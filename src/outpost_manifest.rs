//! Storing the DUT's `outpost-manifest.json`, and rendering a trace against it.
//!
//! `embarch-outpost/design.md` §3 decision 9, and this crate's own §3 decision
//! 30(c). Built 2026-08-25, when Milestone 7 Phase C produced the first real
//! manifest — the named trigger both halves were deliberately parked on.
//!
//! **Two questions, two mechanisms, and each covers the other's blind spot.**
//!
//! *Which manifest?* The one that arrived with the flash that put this image
//! on the DUT ([`ManifestSlot`]). Manifest and image reach Core in **one
//! operation** (`POST /flash` carries both), so there is no interval in which
//! Core holds one without the other, and no "current manifest" registry to go
//! stale. This is not the write-ahead-staleness pattern
//! `embarch-topology/design.md` §3 decision 3 exists to eliminate — what that
//! forbids is a *persisted* record of resolved state consulted at a later,
//! unrelated moment, and this binding's lifetime ends at the next flash of the
//! same chip.
//!
//! *Is it actually the right one?* The build ID the running firmware puts in
//! its own header frame, compared against the manifest's copy. That is what
//! catches a DUT flashed out-of-band between the study's flash and its
//! capture — a bare `west flash` or an IDE button, entirely normal, and the
//! case flash-binding is blind to.
//!
//! **On a mismatch nothing is rendered.** Not "rendered with a warning": a
//! stale manifest against a rebuilt firmware relabels every marker and thread
//! and produces a trace that is entirely readable and entirely wrong. The raw
//! bytes are always on disk either way, so a mismatch costs the rendering and
//! never the capture.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use embarch_study_designer::outpost::{
    self, Frame, ManifestRefusal, OutpostManifest, RecordKind, Unwrapper,
};

/// The file a study's own copy of the manifest is written to, inside that
/// study's results directory.
pub const STUDY_MANIFEST_FILE: &str = "outpost-manifest.json";

/// A manifest, and which flash put it there.
#[derive(Debug, Clone)]
pub struct StoredManifest {
    /// The file's own bytes, kept verbatim so the copy stored with a study is
    /// the artifact the build produced rather than a re-serialization of it.
    pub json: String,
    pub manifest: OutpostManifest,
}

/// Core's memory of what it last flashed onto each chip.
///
/// Keyed by chip rather than held as a single slot because `POST /flash` is
/// also how dev-bench's own firmware gets written, and clearing the DUT's
/// manifest because the bench was reflashed would be wrong. `chip` is on every
/// flash request, and it is the only thing on one that distinguishes the two
/// boards without Core having to resolve a topology role it is not given.
#[derive(Clone, Default)]
pub struct ManifestSlot {
    inner: Arc<Mutex<HashMap<String, StoredManifest>>>,
}

impl ManifestSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// A flash carried a manifest: it replaces whatever that chip had.
    pub fn store(&self, chip: &str, json: String, manifest: OutpostManifest) {
        let mut guard = self.inner.lock().unwrap();
        tracing::info!(
            chip,
            build_id = manifest.build_id.as_str(),
            markers = manifest.markers.len(),
            threads = manifest.threads.len(),
            isrs = manifest.isrs.len(),
            "stored an outpost manifest for this chip"
        );
        // Keyed by chip: a later flash of the *same* chip without a manifest
        // clears this one, because the image it describes is no longer on the
        // board. A flash of a different chip — dev-bench's own, most
        // obviously — leaves it alone.
        guard.insert(chip.to_string(), StoredManifest { json, manifest });
    }

    /// A flash carried **no** manifest: whatever that chip had described an
    /// image that is no longer on it.
    ///
    /// Clearing is the point. Keeping the old manifest would leave Core
    /// holding a plausible, wrong answer for the next study, which is the one
    /// outcome this whole mechanism exists to prevent — and the build-ID check
    /// would catch it only if the new image happens to carry the outpost at
    /// all.
    pub fn clear_for_chip(&self, chip: &str) {
        let mut guard = self.inner.lock().unwrap();
        if guard.remove(chip).is_some() {
            tracing::info!(
                chip,
                "cleared this chip's outpost manifest: it was flashed again with no manifest"
            );
        }
    }

    /// Whichever manifest is currently bound, if exactly one chip has one.
    ///
    /// A `Study` names a *signal*, not a chip (`embarch-outpost/design.md` §3
    /// decision 12), so there is nothing in a study to resolve a chip from.
    /// With one bound manifest that is unambiguous. With more than one it is
    /// not, and this returns `None` rather than picking — an ambiguous choice
    /// rendered confidently is the same failure as a stale one.
    pub fn current(&self) -> Option<StoredManifest> {
        let guard = self.inner.lock().unwrap();
        let mut it = guard.values();
        match (it.next(), it.next()) {
            (Some(one), None) => Some(one.clone()),
            (Some(_), Some(_)) => {
                let chips: Vec<&str> = guard.keys().map(String::as_str).collect();
                tracing::warn!(
                    chips = ?chips,
                    "more than one chip has an outpost manifest bound; refusing to guess which one \
                     this study's trace belongs to"
                );
                None
            }
            _ => None,
        }
    }
}

/// Parses and sanity-checks a manifest's bytes.
///
/// Rejected at `POST /flash` rather than at render time: a malformed manifest
/// is a build-tooling problem, and the moment to say so is while the person
/// who ran the build is still watching.
pub fn parse(json: &str) -> Result<OutpostManifest, String> {
    let manifest: OutpostManifest =
        serde_json::from_str(json).map_err(|e| format!("not a valid outpost-manifest.json: {e}"))?;
    if manifest.build_id.is_empty() {
        return Err("outpost-manifest.json has an empty build_id, so nothing could ever be \
                    verified against it"
            .to_string());
    }
    if manifest.record_layout_version != outpost::RECORD_LAYOUT_VERSION {
        return Err(format!(
            "outpost-manifest.json declares record layout version {}, and this Core decodes {} — \
             the record shapes are not compatible and guessing which fields moved is exactly what \
             the manifest mechanism exists to prevent",
            manifest.record_layout_version,
            outpost::RECORD_LAYOUT_VERSION
        ));
    }
    Ok(manifest)
}

/// Writes the study's own copy of the manifest beside its results, so a trace
/// stays readable after the next flash has replaced what Core holds in memory.
pub fn write_study_copy(results_dir: &Path, stored: &StoredManifest) -> std::io::Result<PathBuf> {
    let path = results_dir.join(STUDY_MANIFEST_FILE);
    std::fs::write(&path, stored.json.as_bytes())?;
    Ok(path)
}

/// What rendering one outpost tap produced.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOutcome {
    /// Frames that decoded, of every kind.
    pub frames: usize,
    /// Frames dropped: bad framing, a failed CRC, or a body that did not hold
    /// what its type claimed. Each costs its own frame and nothing else.
    pub bad_frames: usize,
    /// Frames the sequence numbers say never arrived.
    pub lost_frames: u64,
    pub records: usize,
    /// Records the firmware itself reported dropping, summed across every gap
    /// record. **Rendered as gaps, never smoothed over** — the losses
    /// correlate with load, which is exactly when the trace matters.
    pub dropped_at_source: u64,
    /// Set when the trace was decoded into structure but **not named**,
    /// because no manifest was applicable. The rows are still written: a
    /// timeline with numeric IDs is a real answer, and it is honestly
    /// distinguishable from a named one.
    pub refusal: Option<ManifestRefusal>,
    /// The build ID the firmware reported, when a header frame was found.
    pub firmware_build_id: Option<String>,
    /// Frames that got a receipt time out of the arrival log — the trace's only
    /// clock (`embarch-outpost/design.md` §3 decisions 17, 18). Zero means
    /// every row rendered with an empty `rx_utc_ms`: an ordered, untimed
    /// trace, which is a real answer and is never dressed up as a timed one.
    pub stamped_frames: usize,
    /// Why the arrival log did not apply, when there was one and it did not.
    /// Reaches `streams/index.json`'s `note` the same way a manifest refusal
    /// does.
    pub arrival_refusal: Option<String>,
}

/// Core's own receipt time per frame, read back from the sidecar
/// `stream_store::ArrivalLog` wrote during the capture.
///
/// **The alignment is verified, not assumed.** The raw capture rotates under
/// retention and the arrival log does not, so "frame 0" can mean different
/// frames in the two files. Each row carries its frame's byte length, so this
/// tries the two alignments that can actually occur — the log starting where
/// the capture starts, and the log running ahead of a capture that lost its
/// beginning — and checks the lengths agree. When neither fits, **nothing is
/// stamped**: a whole trace shifted by three frames is readable, wrong, and
/// indistinguishable from a correct one, which is precisely the failure this
/// module exists to refuse.
#[derive(Debug, Default)]
struct ArrivalIndex {
    /// `(rx_utc_ms, frame_bytes)`, in frame-index order.
    rows: Vec<(u64, usize)>,
}

impl ArrivalIndex {
    fn read(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut rows: Vec<(u64, u64, usize)> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("frame_index") {
                continue;
            }
            let mut it = line.split(',');
            let (Some(idx), Some(rx), Some(len)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            match (idx.parse::<u64>(), rx.parse::<u64>(), len.parse::<usize>()) {
                (Ok(idx), Ok(rx), Ok(len)) => rows.push((idx, rx, len)),
                // A half-written final line is the normal shape of a file that
                // was being appended to when the process stopped.
                _ => continue,
            }
        }
        rows.sort_unstable();
        // Rows must be a contiguous run for an offset alignment to mean
        // anything. A hole means some `note_arrival` write failed, and the
        // honest response is to keep the prefix rather than to silently
        // renumber across the hole.
        let mut kept: Vec<(u64, usize)> = Vec::new();
        for (i, (idx, rx, len)) in rows.into_iter().enumerate() {
            if idx != i as u64 {
                break;
            }
            kept.push((rx, len));
        }
        Ok(Self { rows: kept })
    }

    /// How far into [`Self::rows`] the capture's frame 0 sits, or `None` when
    /// no alignment survives the length check.
    fn offset_for(&self, chunk_lens: &[usize]) -> Option<usize> {
        if self.rows.is_empty() || chunk_lens.is_empty() {
            return None;
        }
        let candidates = [0usize, self.rows.len().saturating_sub(chunk_lens.len())];
        candidates.into_iter().find(|&off| {
            let overlap = chunk_lens.len().min(self.rows.len().saturating_sub(off));
            // One frame in common proves nothing — a single length matches by
            // coincidence often enough on a wire whose frames are mostly one
            // of two sizes.
            overlap >= 2
                && (0..overlap).all(|i| self.rows[off + i].1 == chunk_lens[i])
        })
    }
}

/// Decodes a captured outpost stream into a `*.trace.csv` beside it.
///
/// Post-hoc, from the complete raw file, because that is what the capture
/// model is: `embarch-outpost/design.md` §3 decision 10 settled that a trace
/// is recorded for the duration of a study and drawn afterwards, with no live
/// feed. Decoding at the end also means a header frame that arrived late still
/// names every record before it.
pub fn render(
    raw_path: &Path,
    out_path: &Path,
    arrival_path: Option<&Path>,
    manifest: Option<&OutpostManifest>,
) -> anyhow::Result<RenderOutcome> {
    use std::io::Write as _;

    let raw = std::fs::read(raw_path)?;

    // Frame lengths first, in the same enumeration order the rows will be
    // written in, because the arrival join is checked against them before a
    // single row is stamped.
    let chunk_lens: Vec<usize> = outpost::chunks(&raw).map(<[u8]>::len).collect();
    let mut arrival_refusal: Option<String> = None;
    let (arrivals, arrival_offset) = match arrival_path {
        None => (ArrivalIndex::default(), None),
        Some(path) => match ArrivalIndex::read(path) {
            Ok(index) => match index.offset_for(&chunk_lens) {
                Some(off) => (index, Some(off)),
                None => {
                    arrival_refusal = Some(format!(
                        "{} holds {} frame stamps and this capture decodes into {} frames, and no \
                         alignment of the two agrees on frame lengths — rendering the trace \
                         without times rather than shifting every timestamp",
                        path.display(),
                        index.rows.len(),
                        chunk_lens.len()
                    ));
                    (ArrivalIndex::default(), None)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (ArrivalIndex::default(), None)
            }
            Err(e) => {
                arrival_refusal =
                    Some(format!("could not read {}: {e}", path.display()));
                (ArrivalIndex::default(), None)
            }
        },
    };

    let mut header: Option<embarch_study_designer::outpost::OutpostHeader> = None;
    let mut applied: Option<&OutpostManifest> = None;
    let mut refusal = manifest.map(|_| ManifestRefusal::None);
    if manifest.is_none() {
        refusal = Some(ManifestRefusal::None);
    }

    let mut out = std::fs::File::create(out_path)?;
    writeln!(out, "{}", outpost::csv_header())?;

    let mut scratch = vec![0u8; 4096];
    let mut outcome = RenderOutcome {
        frames: 0,
        bad_frames: 0,
        lost_frames: 0,
        records: 0,
        dropped_at_source: 0,
        refusal: None,
        firmware_build_id: None,
        stamped_frames: 0,
        arrival_refusal: None,
    };
    let mut last_seq: Option<u8> = None;

    // The DUT's cycle counter is 32-bit and absolute, so a long trace wraps.
    // One unwrapper for the whole stream, in frame order, because a wrap is
    // only detectable against the previous record's value -- restarting it per
    // frame would lose every wrap that happens to fall on a frame boundary.
    let mut unwrapper = Unwrapper::new();

    for (frame_index, chunk) in outpost::chunks(&raw).enumerate() {
        // The frame's own receipt time, or none. Looked up by index rather
        // than by position in a filtered list on purpose: a frame that fails
        // its CRC below still consumed an index while the bytes were being
        // stamped (`stream_store::ArrivalLog`), so skipping one here must not
        // shift the frames after it.
        let rx_utc_ms = arrival_offset
            .and_then(|off| arrivals.rows.get(off + frame_index))
            .map(|(rx, _)| *rx);

        // A frame larger than the scratch buffer is a configuration Core
        // cannot decode rather than one it should truncate: grow once and
        // retry, so a DUT with a large `CONFIG_EMBARCH_OUTPOST_BATCH_BYTES`
        // is not silently unreadable.
        if chunk.len() > scratch.len() {
            scratch.resize(chunk.len() * 2, 0);
        }
        let frame = match outpost::decode_frame(chunk, &mut scratch) {
            Ok(frame) => frame,
            Err(_) => {
                outcome.bad_frames += 1;
                continue;
            }
        };
        outcome.frames += 1;

        let seq = match &frame {
            Frame::Header { seq, .. } => *seq,
            Frame::Records { seq, .. } => *seq,
        };
        if let Some(prev) = last_seq {
            outcome.lost_frames += u64::from(seq.wrapping_sub(prev).wrapping_sub(1));
        }
        last_seq = Some(seq);

        match frame {
            Frame::Header { header: h, .. } => {
                if header.is_none() {
                    outcome.firmware_build_id = Some(h.build_id.as_str().to_string());
                    if let Some(m) = manifest {
                        match m.check(h.build_id.as_str(), h.record_layout_version) {
                            Ok(()) => {
                                applied = Some(m);
                                refusal = None;
                            }
                            Err(why) => refusal = Some(why),
                        }
                    }
                    header = Some(h);
                }
            }
            Frame::Records { records, seq } => {
                if rx_utc_ms.is_some() {
                    outcome.stamped_frames += 1;
                }
                for record in records {
                    let Ok(record) = record else {
                        outcome.bad_frames += 1;
                        break;
                    };
                    outcome.records += 1;
                    if RecordKind::from_byte(record.kind) == Some(RecordKind::Gap) {
                        outcome.dropped_at_source += u64::from(record.a);
                    }
                    // `cycles_per_sec` comes from the header frame, so a
                    // stream whose header was lost or corrupt renders an empty
                    // `us` column rather than a wrong one -- the cycle count
                    // itself is still exact, and inventing a rate to divide it
                    // by is the plausible-and-wrong answer this module refuses
                    // everywhere else.
                    let cycles_per_sec = header.as_ref().map(|h| h.cycles_per_sec).unwrap_or(0);
                    let absolute = unwrapper.absolute(record.cycles);
                    writeln!(
                        out,
                        "{}",
                        record.to_csv_row(
                            frame_index as u64,
                            u32::from(seq),
                            rx_utc_ms,
                            absolute,
                            cycles_per_sec,
                            applied,
                        )
                    )?;
                }
            }
        }
    }

    if header.is_none() {
        // Nothing in the stream said what it is. Every record is still
        // structurally decodable, and every one still has whatever time the
        // arrival log gave its frame — what is missing is the build ID that
        // would let a manifest name any of it, and the layout version that
        // would confirm the shapes agree.
        tracing::warn!(
            path = %raw_path.display(),
            "an outpost capture carried no header frame; nothing can be named from it"
        );
    }
    if let Some(why) = &arrival_refusal {
        tracing::warn!(path = %raw_path.display(), "{why}");
    }
    outcome.refusal = refusal;
    outcome.arrival_refusal = arrival_refusal;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST_JSON: &str = r#"{
        "schema": 1,
        "build_id": "v1.0-dirty+opabc+mdef",
        "outpost_version": "abc",
        "record_layout_version": 3,
        "markers": {"0": "WORK_BEGIN"},
        "threads": {"0x20001234": "main"},
        "isrs": {"7": "nrfx_isr"},
        "isr_args": {"7": "nrfx_twim_30_irq_handler"},
        "manifest_crc": 1,
        "notes": []
    }"#;

    #[test]
    fn a_manifest_parses_and_keeps_its_tables() {
        let m = parse(MANIFEST_JSON).expect("parses");
        assert_eq!(m.build_id, "v1.0-dirty+opabc+mdef");
        assert_eq!(m.markers.get("0").map(String::as_str), Some("WORK_BEGIN"));
        assert_eq!(m.isr_args.get("7").map(String::as_str), Some("nrfx_twim_30_irq_handler"));
    }

    #[test]
    fn a_manifest_with_no_build_id_is_refused_at_the_door() {
        let json = MANIFEST_JSON.replace("v1.0-dirty+opabc+mdef", "");
        assert!(parse(&json).unwrap_err().contains("empty build_id"));
    }

    #[test]
    fn a_manifest_from_a_different_record_layout_is_refused_at_the_door() {
        let json =
            MANIFEST_JSON.replace("\"record_layout_version\": 3", "\"record_layout_version\": 2");
        assert!(parse(&json).unwrap_err().contains("record layout version"));
    }

    #[test]
    fn a_flash_with_no_manifest_clears_only_its_own_chip() {
        let slot = ManifestSlot::new();
        let m = parse(MANIFEST_JSON).unwrap();
        slot.store("nRF54L15", MANIFEST_JSON.to_string(), m);

        // dev-bench being reflashed must not take the DUT's manifest with it.
        slot.clear_for_chip("esp32c5");
        assert!(slot.current().is_some(), "another chip's flash cleared this one's manifest");

        slot.clear_for_chip("nRF54L15");
        assert!(slot.current().is_none(), "the chip's own reflash must clear it");
    }

    /// A real capture and the manifest its own build produced, taken verbatim
    /// from `embarch-outpost/tests/native_sim_stream`.
    ///
    /// These bytes come from the **firmware encoder**, not from anything in
    /// Rust: this is the only test in the suite that pins Core's decoder
    /// against the C that actually writes the wire. A round trip through this
    /// crate's own inverse would agree with itself no matter what the firmware
    /// does.
    const REAL_CAPTURE: &[u8] = include_bytes!("../tests/fixtures/outpost-native-sim.bin");
    const REAL_MANIFEST: &str =
        include_str!("../tests/fixtures/outpost-native-sim-manifest.json");

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "embarch-outpost-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An arrival log for the real capture, with **real frame lengths and
    /// synthesised times**.
    ///
    /// The lengths are the capture's own, which is the half the join is
    /// verified against. The times are invented, and have to be: that capture
    /// went to a `native_sim` process's stdout, so no receiver ever stamped it
    /// — no outpost byte has crossed a real UART on any board. `spacing_ms` is
    /// how far apart consecutive frames are pretended to have arrived.
    fn synth_arrivals(raw: &[u8], first_ms: u64, spacing_ms: u64) -> String {
        let mut out = String::from("frame_index,rx_utc_ms,frame_bytes\n");
        for (i, chunk) in outpost::chunks(raw).enumerate() {
            out.push_str(&format!(
                "{i},{},{}\n",
                first_ms + i as u64 * spacing_ms,
                chunk.len()
            ));
        }
        out
    }

    fn render_real(manifest: Option<&OutpostManifest>) -> (RenderOutcome, String) {
        render_real_with_arrivals(manifest, None)
    }

    fn render_real_with_arrivals(
        manifest: Option<&OutpostManifest>,
        arrivals: Option<&str>,
    ) -> (RenderOutcome, String) {
        let dir = scratch_dir("render");
        let raw = dir.join("outpost.bin");
        let out = dir.join("outpost.trace.csv");
        let arrival_path = dir.join("outpost.arrival.csv");
        std::fs::write(&raw, REAL_CAPTURE).unwrap();
        let arrival_arg = arrivals.map(|text| {
            std::fs::write(&arrival_path, text).unwrap();
            arrival_path.as_path()
        });
        let outcome = render(&raw, &out, arrival_arg, manifest).expect("renders");
        let csv = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        (outcome, csv)
    }

    #[test]
    fn a_real_firmware_capture_decodes_and_names_itself() {
        let manifest = parse(REAL_MANIFEST).expect("the real manifest parses");
        let (outcome, csv) = render_real(Some(&manifest));

        assert_eq!(outcome.bad_frames, 0, "a real capture had undecodable frames");
        assert_eq!(outcome.lost_frames, 0, "frame sequence numbers show a gap");
        assert!(outcome.frames > 1, "only one frame decoded out of a real capture");
        assert!(outcome.records > 100, "only {} records decoded", outcome.records);
        assert_eq!(
            outcome.refusal, None,
            "the manifest from this capture's own build was refused"
        );
        assert!(
            outcome.dropped_at_source > 0,
            "this capture deliberately overflows the ring; its gap records must be counted"
        );

        assert!(csv.starts_with("frame_index,frame_seq,rx_utc_ms,cycles,us,kind,a,b,name\n"));
        // Nobody stamped this capture, so every row's time column is empty —
        // stated rather than filled in.
        assert_eq!(outcome.stamped_frames, 0);
        assert_eq!(outcome.arrival_refusal, None, "there was no arrival log to refuse");
        for line in csv.lines().skip(1) {
            let rx = line.split(',').nth(2).expect("an rx_utc_ms column");
            assert!(rx.is_empty(), "an unstamped capture rendered a time: {line}");
        }
        // Names resolved out of the ELF, at zero wire cost.
        for expected in ["outpost_ping", "outpost_pong", "WORK_BEGIN", "BURST"] {
            assert!(csv.contains(expected), "the rendered trace never names {expected}");
        }
        // And the losses are visible as losses.
        assert!(csv.contains(",gap,"), "a gap record was not rendered as a gap");
    }

    #[test]
    fn a_manifest_from_another_build_is_refused_and_the_rows_stay_unnamed() {
        let mut manifest = parse(REAL_MANIFEST).unwrap();
        manifest.build_id = "some-other-build".to_string();
        let (outcome, csv) = render_real(Some(&manifest));

        match outcome.refusal {
            Some(ManifestRefusal::BuildIdMismatch { manifest: m, firmware: f }) => {
                assert_eq!(m, "some-other-build");
                assert!(!f.is_empty(), "the firmware's own build ID was not reported");
            }
            other => panic!("expected a build-ID refusal, got {other:?}"),
        }
        // Still decoded — the structure is real even when the names are not
        // available — but nothing carries a name it did not earn.
        assert!(outcome.records > 100);
        assert!(
            !csv.contains("outpost_ping") && !csv.contains("WORK_BEGIN"),
            "a refused manifest still labelled rows"
        );
    }

    #[test]
    fn with_no_manifest_a_trace_still_decodes_into_a_timeline() {
        let (outcome, csv) = render_real(None);
        assert_eq!(outcome.refusal, Some(ManifestRefusal::None));
        assert!(outcome.records > 100);
        assert!(csv.contains(",marker,"), "record kinds are known without a manifest");
        assert!(!csv.contains("WORK_BEGIN"), "names appeared without a manifest to take them from");
    }

    /// The arrival join, end to end: a frame's stamp lands on every record in
    /// that frame and on no record of another.
    #[test]
    fn arrival_stamps_land_on_every_record_of_their_own_frame() {
        let manifest = parse(REAL_MANIFEST).unwrap();
        let arrivals = synth_arrivals(REAL_CAPTURE, 1_700_000_000_000, 20);
        let (outcome, csv) = render_real_with_arrivals(Some(&manifest), Some(&arrivals));

        assert!(outcome.stamped_frames > 1, "only {} frames got a time", outcome.stamped_frames);
        assert_eq!(outcome.arrival_refusal, None);

        let mut seen = 0usize;
        for line in csv.lines().skip(1) {
            let f: Vec<&str> = line.split(',').collect();
            let frame_index: u64 = f[0].parse().expect("a frame index");
            let rx: u64 = f[2].parse().expect("every row is stamped");
            assert_eq!(
                rx,
                1_700_000_000_000 + frame_index * 20,
                "a record was stamped with another frame's time: {line}"
            );
            seen += 1;
        }
        assert_eq!(seen, outcome.records);
    }

    /// **The failure this join exists to refuse.** An arrival log whose frames
    /// are not this capture's frames does not get applied to it — a trace
    /// shifted by a few frames reads exactly like a correct one.
    #[test]
    fn an_arrival_log_that_does_not_fit_the_capture_stamps_nothing() {
        let manifest = parse(REAL_MANIFEST).unwrap();
        // Same row count, deliberately wrong lengths.
        let mut bogus = String::from("frame_index,rx_utc_ms,frame_bytes\n");
        for (i, _) in outpost::chunks(REAL_CAPTURE).enumerate() {
            bogus.push_str(&format!("{i},{},7\n", 1_700_000_000_000u64 + i as u64));
        }
        let (outcome, csv) = render_real_with_arrivals(Some(&manifest), Some(&bogus));

        assert_eq!(outcome.stamped_frames, 0, "a mismatched arrival log was applied anyway");
        let why = outcome.arrival_refusal.expect("the refusal has to be reported");
        assert!(why.contains("no alignment"), "{why}");
        // And the trace is still there, ordered and untimed.
        assert!(outcome.records > 100);
        assert!(csv.contains("outpost_ping"), "the refusal cost the names too");
    }

    /// A capture that lost its beginning to retention rotation: the arrival log
    /// is not rotated, so it runs ahead of the bytes. The lengths are what say
    /// by how much.
    #[test]
    fn a_rotated_capture_aligns_from_the_end_it_still_has() {
        let manifest = parse(REAL_MANIFEST).unwrap();
        let arrivals = synth_arrivals(REAL_CAPTURE, 1_700_000_000_000, 20);
        // Drop the first three frames' bytes, keeping every stamp.
        let mut cut = 0usize;
        let mut seen = 0usize;
        for (i, b) in REAL_CAPTURE.iter().enumerate() {
            if *b == 0 {
                seen += 1;
                if seen == 3 {
                    cut = i + 1;
                    break;
                }
            }
        }
        let dir = scratch_dir("rotated");
        let raw = dir.join("outpost.bin");
        let out = dir.join("outpost.trace.csv");
        let arrival_path = dir.join("outpost.arrival.csv");
        std::fs::write(&raw, &REAL_CAPTURE[cut..]).unwrap();
        std::fs::write(&arrival_path, &arrivals).unwrap();
        let outcome =
            render(&raw, &out, Some(&arrival_path), Some(&manifest)).expect("renders");
        let csv = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(outcome.arrival_refusal, None, "a rotated capture refused its own stamps");
        assert!(outcome.stamped_frames > 1);
        // Frame 0 of what survived is frame 3 of what was stamped, so the
        // first row must carry frame 3's time and not frame 0's.
        let first = csv.lines().nth(1).expect("a row");
        let rx: u64 = first.split(',').nth(2).unwrap().parse().unwrap();
        assert_eq!(rx, 1_700_000_000_000 + 3 * 20, "{first}");
    }

    #[test]
    fn a_truncated_capture_costs_the_frames_it_lost_and_nothing_else() {
        // Cut the capture mid-frame, the way a study that ended while bytes
        // were in flight would.
        let dir = std::env::temp_dir().join(format!("embarch-outpost-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("outpost.bin");
        let out = dir.join("outpost.trace.csv");
        std::fs::write(&raw, &REAL_CAPTURE[..REAL_CAPTURE.len() / 2]).unwrap();
        let manifest = parse(REAL_MANIFEST).unwrap();
        let outcome = render(&raw, &out, None, Some(&manifest)).expect("renders");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(outcome.refusal, None, "a truncated capture still has its header");
        assert!(outcome.records > 0, "a truncated capture decoded nothing at all");
        assert!(
            outcome.bad_frames <= 1,
            "cutting one frame in half cost {} frames",
            outcome.bad_frames
        );
    }

    /// Regenerates `embarch-ui`'s committed trace fixtures from this crate's
    /// own renderer, so that view is tested against Core's real output rather
    /// than against a CSV somebody typed.
    ///
    /// `#[ignore]`d because it writes into a sibling repo. Run it deliberately,
    /// after any change to the row shape:
    ///
    /// ```text
    /// EMBARCH_UI_FIXTURES=../embarch-ui/tests/fixtures \
    ///     cargo test regenerate_the_ui_trace_fixtures -- --ignored --nocapture
    /// ```
    ///
    /// It emits both halves, because a trace has two honest states and the
    /// view has to draw each: `outpost-native-sim.trace.csv` (**untimed** —
    /// nobody stamped that capture, and nobody could have: it went to a
    /// simulator's stdout) and `outpost-native-sim-stamped.trace.csv` (the
    /// same real frames with **synthesised** 20 ms arrival stamps, which is
    /// the only way a stamped fixture can exist until an outpost byte crosses
    /// a real UART).
    #[test]
    #[ignore = "writes fixtures into ../embarch-ui"]
    fn regenerate_the_ui_trace_fixtures() {
        let Ok(out_dir) = std::env::var("EMBARCH_UI_FIXTURES") else {
            panic!("set EMBARCH_UI_FIXTURES to embarch-ui/tests/fixtures");
        };
        let out_dir = Path::new(&out_dir);
        let manifest = parse(REAL_MANIFEST).expect("the real manifest parses");

        let dir = scratch_dir("ui-fixtures");
        let raw = dir.join("outpost.bin");
        std::fs::write(&raw, REAL_CAPTURE).unwrap();

        let untimed = dir.join("untimed.csv");
        let outcome = render(&raw, &untimed, None, Some(&manifest)).expect("renders");
        assert_eq!(outcome.stamped_frames, 0);
        std::fs::copy(&untimed, out_dir.join("outpost-native-sim.trace.csv")).unwrap();

        let arrival = dir.join("outpost.arrival.csv");
        std::fs::write(&arrival, synth_arrivals(REAL_CAPTURE, 1_700_000_000_000, 20)).unwrap();
        let stamped = dir.join("stamped.csv");
        let outcome =
            render(&raw, &stamped, Some(&arrival), Some(&manifest)).expect("renders");
        assert!(outcome.stamped_frames > 1);
        assert_eq!(outcome.arrival_refusal, None);
        std::fs::copy(&stamped, out_dir.join("outpost-native-sim-stamped.trace.csv")).unwrap();

        println!(
            "wrote both fixtures: {} records over {} frames, {} of them stamped",
            outcome.records, outcome.frames, outcome.stamped_frames
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_bound_manifests_resolve_to_none_rather_than_to_a_guess() {
        let slot = ManifestSlot::new();
        let m = parse(MANIFEST_JSON).unwrap();
        slot.store("a", MANIFEST_JSON.to_string(), m.clone());
        assert!(slot.current().is_some());
        slot.store("b", MANIFEST_JSON.to_string(), m);
        assert!(
            slot.current().is_none(),
            "with two candidates Core must decline rather than pick one"
        );
    }
}
