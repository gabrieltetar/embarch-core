//! `study_results/<study_id>/streams/` — one file per declared stream tap.
//!
//! `embarch-core/design.md` §3 decision 30(b) is what this implements:
//! `streams/` replaces `data.csv`/`waveform.csv`/`gatt.csv` as *paths* while
//! keeping every one of their row shapes, and **a tap always writes its raw
//! bytes before any decode is attempted** — a decode that fails must not cost
//! the capture, which is the whole difference between a bad afternoon and a
//! lost one.
//!
//! Nothing here decides what a payload *means*. The tap's declared
//! [`StreamEncoding`] does, always, and it is the only thing that does
//! (`embarch-study-designer/design.md` §3 decision 35) — there is deliberately
//! no sniff, no heuristic, and no "looks like text" fallback anywhere in this
//! module. [`StreamEncoding::Raw`] is the honest default for a payload nobody
//! declared, and it renders nothing.
//!
//! **Retention lives here too** (decision 30's own "unbounded in two
//! independent directions" note): per-file segment rotation keeping the tail
//! within one run ([`SegmentedFile`], `EMBARCH_STREAM_MAX_BYTES`), and a
//! keep-last-N sweep across runs ([`sweep_study_results`],
//! `EMBARCH_STUDY_RESULTS_KEEP`). **Both defaults are reasoned and neither is
//! measured** — the first real capture is what sizes them.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use embarch_study_designer::{
    limits::MAX_STREAM_NAME_LEN, GattTranscriptEntry, Sample, StreamEncoding, StreamRef,
    StreamSource, StreamTap, StructLayout,
};
use serde::{Deserialize, Serialize};

/// Cap on one stream file before it rotates (decision 30's retention note).
///
/// **32 MiB is reasoned, not measured.** It is roughly "an hour of a
/// 1 kHz `f32` power capture, or a few minutes of a chatty trace" — big
/// enough that no plausible study loses anything, small enough that a
/// runaway tap can't fill a disk before anyone notices. Two segments are
/// kept, so a tap's worst case on disk is a little under twice this. The
/// first real capture is what replaces this number with a measured one.
pub const DEFAULT_STREAM_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// How many `study_results/<study_id>/` directories survive the sweep at
/// `POST /study` (decision 30's retention note).
///
/// **50 is reasoned, not measured.** Count-based rather than age- or
/// size-based deliberately: a count needs no clock, and this suite's Core
/// runs as a service whose idea of "old" would otherwise have to be
/// maintained. 50 is "more runs than a single debugging session ever
/// produces" — the point is that a bench left running for months does not
/// silently accumulate every capture it ever took.
pub const DEFAULT_STUDY_RESULTS_KEEP: usize = 50;

/// Baud rate for a `Route::Direct` signal tap's own serial port.
///
/// **Settled here because nothing else declares it.**
/// `embarch_topology::hardware::SignalLink` records *where* a signal goes,
/// not how fast it talks, and a `Study`'s tap names the signal rather than
/// the carrier on purpose (`embarch-study-designer/design.md` §3 decision
/// 39) — so the rate had to land somewhere, and it lands as an operator
/// knob with dev-bench's own link rate as the default.
/// `embarch-outpost/design.md` §5.2's worked example configures its UART at
/// exactly this rate.
pub const DEFAULT_SIGNAL_BAUD: u32 = 1_000_000;

pub const STREAM_MAX_BYTES_ENV: &str = "EMBARCH_STREAM_MAX_BYTES";
pub const STUDY_RESULTS_KEEP_ENV: &str = "EMBARCH_STUDY_RESULTS_KEEP";
pub const SIGNAL_BAUD_ENV: &str = "EMBARCH_SIGNAL_BAUD";

/// The subdirectory of a study's results directory this module owns.
pub const STREAMS_DIR: &str = "streams";

/// The index every other reader resolves a tap name through — see
/// [`StreamIndex`].
pub const INDEX_FILE: &str = "index.json";

/// `EMBARCH_STREAM_MAX_BYTES`, or [`DEFAULT_STREAM_MAX_BYTES`]. `0` disables
/// rotation outright (unbounded), the same "0 turns it off" convention
/// [`study_results_keep`] uses. An unparseable value warns and falls back
/// rather than failing a study over a typo in an env var.
pub fn stream_max_bytes() -> u64 {
    match std::env::var(STREAM_MAX_BYTES_ENV) {
        Err(_) => DEFAULT_STREAM_MAX_BYTES,
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    "{STREAM_MAX_BYTES_ENV}='{raw}' isn't a byte count; \
                     using the default of {DEFAULT_STREAM_MAX_BYTES}"
                );
                DEFAULT_STREAM_MAX_BYTES
            }
        },
    }
}

/// `EMBARCH_STUDY_RESULTS_KEEP`, or [`DEFAULT_STUDY_RESULTS_KEEP`]. `0`
/// disables the sweep entirely — nothing is ever deleted.
pub fn study_results_keep() -> usize {
    match std::env::var(STUDY_RESULTS_KEEP_ENV) {
        Err(_) => DEFAULT_STUDY_RESULTS_KEEP,
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    "{STUDY_RESULTS_KEEP_ENV}='{raw}' isn't a count; \
                     using the default of {DEFAULT_STUDY_RESULTS_KEEP}"
                );
                DEFAULT_STUDY_RESULTS_KEEP
            }
        },
    }
}

/// `EMBARCH_SIGNAL_BAUD`, or [`DEFAULT_SIGNAL_BAUD`].
pub fn signal_baud() -> u32 {
    match std::env::var(SIGNAL_BAUD_ENV) {
        Err(_) => DEFAULT_SIGNAL_BAUD,
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(v) if v > 0 => v,
            _ => {
                tracing::warn!(
                    "{SIGNAL_BAUD_ENV}='{raw}' isn't a baud rate; \
                     using the default of {DEFAULT_SIGNAL_BAUD}"
                );
                DEFAULT_SIGNAL_BAUD
            }
        },
    }
}

// ---- the on-disk index -----------------------------------------------------

/// `streams/index.json` — written once at study start, before a single byte
/// has arrived, and never rewritten.
///
/// **Why this exists at all**, since decision 30 didn't name it: the three
/// retired routes (`/power-data`, `/waveform-data`, `/gatt-data`) are kept as
/// aliases for one release, and an alias has to answer "which tap is the
/// power tap?" from a handler that has no `Study` in hand — Core reads
/// results back off disk, deliberately holding no resident copy of a
/// finished study (`StudyJob`'s own doc comment). The index is that answer,
/// and it doubles as the name → file mapping `GET /study/{id}/stream/{name}`
/// resolves through, which is what makes a tap name incapable of escaping
/// the streams directory: only a name the index already carries resolves to
/// anything at all.
///
/// Written at start rather than at finish so a study that *failed* still
/// says which taps it declared — the same reason the capture files
/// themselves are written incrementally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamIndex {
    /// Bumped only if this file's own shape changes. Not a schema version in
    /// `embarch-study-designer`'s sense — nothing outside Core reads it.
    pub version: u32,
    pub streams: Vec<StreamIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamIndexEntry {
    pub id: u8,
    /// The tap's declared name — what `GET /study/{id}/stream/{name}` takes.
    pub name: String,
    /// Byte-for-byte what arrived, always written before any decode is
    /// attempted (decision 30(b)).
    pub raw_file: String,
    /// The decoded rendering, when the declared encoding has one. `None` for
    /// `Raw` (nothing to decode), `Text` (the decode is the identity, so
    /// `raw_file` already *is* the rendering) and `OutpostTrace` (needs a
    /// manifest Core cannot yet be given — decision 30(c)).
    pub rendered_file: Option<String>,
    /// `frame_index,rx_utc_ms,frame_bytes` — **Core's own receipt time for
    /// every frame of an `OutpostTrace` capture**, and the trace's only clock
    /// (`embarch-outpost/design.md` §3 decisions 17 and 18). `None` for every
    /// other encoding: a sample and a transcript entry carry
    /// `core_rx_utc_ms` in their own rendered rows, so only the encoding
    /// whose rendering happens post-hoc needs the stamps kept beside the
    /// bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrival_file: Option<String>,
    pub encoding: StreamEncoding,
    /// Which of the three retired fixed-path routes this tap answers, if
    /// any: `"power"`, `"waveform"` or `"gatt"`. The mapping is exactly the
    /// one Phase A's interim `write_stream_record` used to pick between the
    /// three CSV files, moved here rather than re-derived.
    pub alias: Option<String>,
    /// Why this tap's rendering is missing, incomplete, unnamed or untimed —
    /// set only when there is something to say. An `OutpostTrace` tap decoded
    /// without an applicable manifest carries the refusal here, so the reason
    /// survives alongside the capture instead of only in a log line nobody
    /// kept.
    ///
    /// **Prose, for a person.** The two facts a caller has to branch on live
    /// in [`named`](Self::named) and [`timed`](Self::timed) precisely so
    /// nothing has to pattern-match on this text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Whether an applicable manifest named this trace's threads, ISRs and
    /// markers. `None` until the capture closes, and on every encoding for
    /// which the question is meaningless.
    ///
    /// Split out from `note` when a trace gained a *second* way of being
    /// incomplete: it can be named and untimed, timed and unnamed, or neither,
    /// and a caller that inferred "named" from "no note" would call an untimed
    /// trace unnamed (`embarch-outpost/design.md` §3 decision 18).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<bool>,
    /// Whether this trace's frames carry Core's receipt time — the trace's
    /// only clock (`embarch-outpost/design.md` §3 decision 17). `false` is an
    /// ordered, untimed trace: a real answer, and one a caller must draw
    /// differently rather than against an axis of milliseconds it does not
    /// have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed: Option<bool>,
    /// Whether the firmware kept **itself** out of this trace —
    /// `CONFIG_EMBARCH_OUTPOST_TRACE_SELF=n`, which is the default, read off
    /// the header frame's own flags byte
    /// (`embarch-outpost/design.md` §3 decision 19).
    ///
    /// The third way a trace can be incomplete, and the only one the *firmware*
    /// decides rather than the host: no record describes the outpost's own
    /// drain thread or its own UART's interrupt, so intervals covered by no
    /// lane are the instrument's own rather than unexplained. `true` is the
    /// normal, useful setting — without it a quiet DUT's trace is half a
    /// description of its own transmission.
    ///
    /// A boolean beside `named`/`timed` rather than the raw flags byte, for the
    /// reason those two exist: this is the fact a caller branches on, and
    /// anything that wanted to branch on a *different* bit would be a caller
    /// this suite does not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_excluded: Option<bool>,
}

impl StreamIndex {
    pub fn find(&self, name: &str) -> Option<&StreamIndexEntry> {
        self.streams.iter().find(|e| e.name == name)
    }

    pub fn find_alias(&self, alias: &str) -> Option<&StreamIndexEntry> {
        self.streams.iter().find(|e| e.alias.as_deref() == Some(alias))
    }
}

/// Reads `streams/index.json`. `Ok(None)` when there is no `streams/`
/// directory at all — a study captured before this existed, or one that
/// never got far enough to write it.
/// Rewrites `index.json` after the capture has closed.
///
/// The index is written *before* any bytes arrive (decision 30(b)), which is
/// what makes a tap that produced nothing still appear. Two things are only
/// knowable afterwards, though: whether a post-hoc rendering exists, and why
/// it does not. This is how those get in without the pre-arrival write having
/// to predict them.
pub fn update_index<F>(streams_dir: &Path, mut edit: F) -> anyhow::Result<()>
where
    F: FnMut(&mut StreamIndexEntry),
{
    use anyhow::Context as _;

    let Some(mut index) = read_index(streams_dir)? else {
        return Ok(());
    };
    for entry in &mut index.streams {
        edit(entry);
    }
    let path = streams_dir.join(INDEX_FILE);
    fs::write(&path, serde_json::to_vec_pretty(&index)?)
        .with_context(|| format!("failed to rewrite {}", path.display()))?;
    Ok(())
}

pub fn read_index(streams_dir: &Path) -> anyhow::Result<Option<StreamIndex>> {
    let path = streams_dir.join(INDEX_FILE);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ---- one rotating file -----------------------------------------------------

/// One capture file, capped at `max_bytes` and rotated by **renaming, never
/// rewriting** (decision 30's retention note).
///
/// Two segments: `name.ext` is the live one and `name.1.ext` the previous.
/// Rotation renames live → previous and starts a fresh live file; the
/// segment that *was* previous is deleted, and that deletion — the only
/// point at which captured bytes actually stop existing — is what sets
/// [`truncated`](Self::truncated) and so reaches `StreamRef.truncated`. A
/// capture that lost data has to say so; a short capture that reads as a
/// complete one is the failure this flag exists to prevent.
///
/// A single write larger than `max_bytes` is written whole into a fresh
/// segment rather than split: a record is the smallest thing on this wire
/// that means anything, and half of one means nothing.
pub struct SegmentedFile {
    live: PathBuf,
    previous: PathBuf,
    /// Repeated at the top of every segment, so each segment is
    /// independently readable. [`read_capture`] emits it once when it
    /// concatenates them back.
    header: Option<String>,
    max_bytes: u64,
    file: Option<fs::File>,
    live_len: u64,
    total_written: u64,
    truncated: bool,
}

impl SegmentedFile {
    fn new(dir: &Path, file_name: &str, header: Option<String>, max_bytes: u64) -> Self {
        Self {
            live: dir.join(file_name),
            previous: dir.join(previous_segment_name(file_name)),
            header,
            max_bytes,
            file: None,
            live_len: 0,
            total_written: 0,
            truncated: false,
        }
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.max_bytes > 0
            && self.live_len > 0
            && self.live_len + bytes.len() as u64 > self.max_bytes
        {
            self.rotate()?;
        }

        if self.file.is_none() {
            let mut f = fs::OpenOptions::new().create(true).append(true).open(&self.live)?;
            let fresh = f.metadata().map(|m| m.len() == 0).unwrap_or(true);
            if fresh {
                if let Some(header) = &self.header {
                    let line = format!("{header}\n");
                    f.write_all(line.as_bytes())?;
                    self.live_len += line.len() as u64;
                }
            }
            self.file = Some(f);
        }

        let f = self.file.as_mut().expect("just opened");
        f.write_all(bytes)?;
        self.live_len += bytes.len() as u64;
        self.total_written += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        // Close before renaming — Windows will not rename an open file.
        self.file = None;
        if self.previous.exists() {
            fs::remove_file(&self.previous)?;
            // The one place captured bytes actually stop existing.
            self.truncated = true;
        }
        fs::rename(&self.live, &self.previous)?;
        self.live_len = 0;
        Ok(())
    }
}

/// `power.csv` → `power.1.csv`; a name with no extension → `power.1`.
fn previous_segment_name(file_name: &str) -> String {
    match file_name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.1.{ext}"),
        None => format!("{file_name}.1"),
    }
}

/// Reads one capture back as bytes: the previous segment (if it survived)
/// followed by the live one, in that order, so the result is in capture
/// order.
///
/// `csv` de-duplicates the header line each segment carries, so a rotated
/// capture still reads back as one valid CSV. That is a read-time filter,
/// never a rewrite of anything on disk.
///
/// `Ok(None)` when neither segment exists — the tap was declared but nothing
/// ever arrived on it.
pub fn read_capture(streams_dir: &Path, file_name: &str, csv: bool) -> std::io::Result<Option<Vec<u8>>> {
    let live = streams_dir.join(file_name);
    let previous = streams_dir.join(previous_segment_name(file_name));

    let mut out: Vec<u8> = Vec::new();
    let mut found = false;
    for (i, path) in [previous, live].into_iter().enumerate() {
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        found = true;
        if csv && i > 0 && !out.is_empty() {
            // Drop this segment's repeated header line.
            if let Some(nl) = bytes.iter().position(|&b| b == b'\n') {
                out.extend_from_slice(&bytes[nl + 1..]);
            }
        } else {
            out.extend_from_slice(&bytes);
        }
    }

    Ok(found.then_some(out))
}

/// `text/csv` for a rendered CSV, `text/plain` for a `Text` tap's own file,
/// `application/octet-stream` for raw bytes — declared by the file the index
/// named, never sniffed from the content.
pub fn content_type_for(file_name: &str) -> &'static str {
    match file_name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("csv") => "text/csv",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ---- the store -------------------------------------------------------------

struct TapFiles {
    name: String,
    /// Always present. Written before any decode is attempted.
    raw: SegmentedFile,
    rendered: Option<SegmentedFile>,
    /// Only an `OutpostTrace` tap has one: see [`ArrivalLog`].
    arrival: Option<ArrivalLog>,
    /// `StreamClose.dropped > 0` — data the *source* lost before it ever
    /// reached Core. A different loss from a retention rotation, reported
    /// through the same `StreamRef.truncated` flag because a reader's
    /// question is the same either way: is this capture complete?
    lost_at_source: bool,
}

/// Core's receipt time for each frame of an outpost capture, kept beside the
/// raw bytes.
///
/// **This is the trace's clock.** An outpost record carries no timestamp at
/// all (`embarch-outpost/design.md` §3 decision 4), so the only time a trace
/// has is when Core received it — and the rendering happens *post-hoc*, from
/// the complete raw file, long after the read that saw the bytes. Something
/// has to carry the stamps across that gap, and this is it (decision 18).
///
/// **What a row is keyed by.** `frame_index` counts non-empty runs between
/// `0x00` delimiters from the start of the capture — exactly what
/// `embarch_study_designer::outpost::chunks` enumerates, so the writer and the
/// decoder agree on what frame 7 is *without this side decoding anything*. All
/// this does is count delimiters; a frame that later fails its CRC still
/// consumes an index on both sides, which is what keeps them in step.
///
/// **Why `frame_bytes` is in the row.** It makes the join checkable rather
/// than assumed. The raw file rotates under retention and this file does not,
/// so their frame 0 can stop being the same frame; with each frame's own
/// length recorded, the renderer can verify an alignment and **refuse** when
/// none fits, instead of shifting every timestamp by a few frames and
/// producing a trace that is entirely readable and entirely wrong.
struct ArrivalLog {
    file: SegmentedFile,
    /// Non-empty delimiter-separated runs seen so far — the next frame's index.
    frames: u64,
    /// Bytes of the run in progress. A frame split across two reads is the
    /// normal case, not an edge one: this is what makes the count survive it.
    pending: usize,
}

impl ArrivalLog {
    /// Scans one arrival's bytes for frame boundaries and appends a row per
    /// frame that *completed* in it.
    ///
    /// The stamp is the read's, so several frames completing in one read all
    /// carry the same time. That is honest — they did arrive together, in one
    /// buffer, and the interval between them is not something Core observed.
    fn note(&mut self, bytes: &[u8], rx_utc_ms: u64) -> Option<String> {
        let mut rows = String::new();
        for byte in bytes {
            if *byte == 0 {
                if self.pending > 0 {
                    rows.push_str(&format!("{},{rx_utc_ms},{}\n", self.frames, self.pending));
                    self.frames += 1;
                }
                self.pending = 0;
            } else {
                self.pending += 1;
            }
        }
        (!rows.is_empty()).then_some(rows)
    }
}

/// Every declared tap's files for one study, plus the index that maps a tap
/// name to them.
pub struct StreamStore {
    /// Read only by [`StreamStore::dir`], which is test-only — every writer
    /// below holds its own resolved paths, and the serving path resolves the
    /// same directory from a `study_id` instead of from a live store.
    #[cfg_attr(not(test), allow(dead_code))]
    dir: PathBuf,
    /// Indexed by `StreamTap.id`, which `validate_taps` has already
    /// guaranteed equals the tap's own index in `Study.streams`.
    taps: Vec<TapFiles>,
}

impl StreamStore {
    /// Creates `streams/` and writes `index.json`, before any bytes arrive.
    pub fn create(
        results_dir: &Path,
        taps: &[StreamTap],
        decoders: &[StructLayout],
        max_bytes: u64,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let dir = results_dir.join(STREAMS_DIR);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;

        let mut used_stems: Vec<String> = Vec::new();
        let mut entries: Vec<StreamIndexEntry> = Vec::new();
        let mut files: Vec<TapFiles> = Vec::new();

        for tap in taps {
            let stem = unique_stem(tap, &used_stems);
            used_stems.push(stem.clone());

            let raw_file = format!("{stem}.{}", raw_extension(&tap.encoding));
            let rendered_file = rendered_extension(&tap.encoding).map(|ext| format!("{stem}.{ext}"));
            let arrival_file = matches!(tap.encoding, StreamEncoding::OutpostTrace)
                .then(|| format!("{stem}.arrival.csv"));

            files.push(TapFiles {
                name: tap.name.as_str().to_string(),
                raw: SegmentedFile::new(&dir, &raw_file, None, max_bytes),
                rendered: rendered_file
                    .as_ref()
                    .map(|f| {
                        SegmentedFile::new(&dir, f, rendered_header(&tap.encoding, decoders), max_bytes)
                    }),
                arrival: arrival_file.as_ref().map(|f| ArrivalLog {
                    // **Deliberately unrotated** (`max_bytes` 0), unlike every
                    // other file here. A row is ~24 bytes against a frame of a
                    // few hundred, so this is a rounding error on the capture
                    // it describes — and rotating it would delete the low
                    // frame indices the join starts from, turning a bounded
                    // retention loss into an unalignable one.
                    file: SegmentedFile::new(
                        &dir,
                        f,
                        Some("frame_index,rx_utc_ms,frame_bytes".to_string()),
                        0,
                    ),
                    frames: 0,
                    pending: 0,
                }),
                lost_at_source: false,
            });

            entries.push(StreamIndexEntry {
                id: tap.id,
                name: tap.name.as_str().to_string(),
                raw_file,
                rendered_file,
                arrival_file,
                encoding: tap.encoding,
                alias: alias_for(&tap.source, &tap.encoding).map(str::to_string),
                note: None,
                named: None,
                timed: None,
                self_excluded: None,
            });
        }

        let index = StreamIndex { version: 1, streams: entries };
        let index_path = dir.join(INDEX_FILE);
        fs::write(&index_path, serde_json::to_vec_pretty(&index)?)
            .with_context(|| format!("failed to write {}", index_path.display()))?;

        tracing::debug!(dir = %dir.display(), taps = taps.len(), "opened streams/ for this study");
        Ok(Self { dir, taps: files })
    }

    /// Where this study's stream files live. Test-only: the serving path
    /// resolves the same directory from a `study_id` instead.
    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Byte-for-byte what arrived. **Always called before any decode is
    /// attempted** (decision 30(b)) — a decode that fails must not cost the
    /// capture.
    pub fn write_raw(&mut self, id: u8, bytes: &[u8]) {
        let Some(tap) = self.taps.get_mut(usize::from(id)) else {
            return;
        };
        if let Err(e) = tap.raw.write(bytes) {
            tracing::error!(
                name = tap.name.as_str(),
                "failed to append raw bytes to {}: {e:?}",
                tap.raw.live.display()
            );
        }
    }

    /// Appends one already-rendered row (the crate's own `to_csv_row` output
    /// plus Core's `core_rx_utc_ms` column) to the tap's rendered file.
    pub fn write_rendered_row(&mut self, id: u8, row: &str) {
        let Some(tap) = self.taps.get_mut(usize::from(id)) else {
            return;
        };
        let Some(rendered) = tap.rendered.as_mut() else {
            return;
        };
        if let Err(e) = rendered.write(format!("{row}\n").as_bytes()) {
            tracing::error!(
                name = tap.name.as_str(),
                "failed to append a row to {}: {e:?}",
                rendered.live.display()
            );
        }
    }

    /// Records **when** these bytes arrived, for the one encoding whose
    /// timeline is Core's receipt time rather than the DUT's own clock.
    ///
    /// Called right after [`write_raw`](Self::write_raw) with the same bytes
    /// and that record's `rx_utc_ms`; a no-op for every tap that has no
    /// arrival log. Losing a row here costs the *time* on those frames and
    /// never the capture — which is why it warns and returns rather than
    /// propagating.
    pub fn note_arrival(&mut self, id: u8, bytes: &[u8], rx_utc_ms: u64) {
        let Some(tap) = self.taps.get_mut(usize::from(id)) else {
            return;
        };
        let Some(arrival) = tap.arrival.as_mut() else {
            return;
        };
        let Some(rows) = arrival.note(bytes, rx_utc_ms) else {
            return;
        };
        if let Err(e) = arrival.file.write(rows.as_bytes()) {
            tracing::error!(
                name = tap.name.as_str(),
                "failed to append arrival stamps to {}: {e:?}; those frames will render \
                 without a time",
                arrival.file.live.display()
            );
        }
    }

    /// `StreamClose` reported a non-zero `dropped`.
    pub fn mark_lost_at_source(&mut self, id: u8) {
        if let Some(tap) = self.taps.get_mut(usize::from(id)) {
            tap.lost_at_source = true;
        }
    }

    /// One [`StreamRef`] per declared tap, in declaration order — including
    /// taps that produced nothing, which report `bytes_written: 0` rather
    /// than being absent. A missing entry and an empty one are different
    /// facts and a result that conflated them would be lying about one of
    /// them.
    ///
    /// `bytes_written` counts every raw byte this run wrote, including bytes
    /// a later rotation deleted — it is what the capture produced, and
    /// `truncated` is what says the file no longer holds all of it.
    pub fn refs(&self) -> Vec<StreamRef> {
        self.taps
            .iter()
            .map(|tap| StreamRef {
                name: heapless::String::<MAX_STREAM_NAME_LEN>::try_from(tap.name.as_str())
                    .unwrap_or_default(),
                bytes_written: tap.raw.total_written,
                truncated: tap.lost_at_source
                    || tap.raw.truncated
                    || tap.rendered.as_ref().is_some_and(|r| r.truncated),
            })
            .collect()
    }
}

/// Which of the three retired fixed-path routes a tap answers.
///
/// Exactly Phase A's interim mapping, moved rather than re-derived:
/// `GattTranscript` → `gatt.csv`, `Samples` on a `PowerFrontEnd` source →
/// `data.csv`, `Samples` on anything else → `waveform.csv`.
fn alias_for(source: &StreamSource, encoding: &StreamEncoding) -> Option<&'static str> {
    match encoding {
        StreamEncoding::GattTranscript => Some("gatt"),
        StreamEncoding::Samples { .. } => Some(match source {
            StreamSource::PowerFrontEnd { .. } => "power",
            _ => "waveform",
        }),
        _ => None,
    }
}

/// `Text` is the one encoding whose decode is the identity, so its raw file
/// *is* its rendering and it gets `.txt` rather than `.bin` — writing the
/// same bytes twice under two names would double the disk cost of the one
/// render that adds nothing. The extension records the declared encoding;
/// the content is byte-for-byte what arrived either way.
fn raw_extension(encoding: &StreamEncoding) -> &'static str {
    match encoding {
        StreamEncoding::Text => "txt",
        _ => "bin",
    }
}

fn rendered_extension(encoding: &StreamEncoding) -> Option<&'static str> {
    match encoding {
        StreamEncoding::Samples { .. }
        | StreamEncoding::GattTranscript
        | StreamEncoding::Struct { .. } => Some("csv"),
        // `Raw` has nothing declared to decode against; `Text`'s raw file is
        // already its rendering; `OutpostTrace` needs a manifest Core cannot
        // yet be given, and renders nothing rather than guessing (decision
        // 30(c)).
        // `Raw` has nothing declared to decode against, and `Text`'s raw file
        // is already its rendering. `OutpostTrace` renders too, but **not from
        // the streaming path**: it is decoded post-hoc from the complete raw
        // file once the capture closes, and `set_rendered` fills its entry in
        // afterwards.
        StreamEncoding::Raw | StreamEncoding::Text | StreamEncoding::OutpostTrace => None,
    }
}

/// The header line a rendered file opens with — the crate's own column list
/// plus the one column Core itself appends (`core_rx_utc_ms`, decision 30).
/// Core holds no other column knowledge, here or anywhere.
fn rendered_header(encoding: &StreamEncoding, decoders: &[StructLayout]) -> Option<String> {
    match encoding {
        StreamEncoding::Samples { .. } => Some(format!("{},core_rx_utc_ms", Sample::csv_header())),
        StreamEncoding::GattTranscript => {
            Some(format!("{},core_rx_utc_ms", GattTranscriptEntry::csv_header()))
        }
        // The decoded columns come from the engineer's own declared layout
        // (`embarch-study-designer/design.md` §3 decision 52) — Core supplies
        // the fixed columns around them and nothing else, exactly as it does
        // for the two above.
        //
        // `payload_hex`/`decode_note` are always present, not only on a
        // failed row: a reader must be able to tell a payload that didn't fit
        // the layout from one that did without the columns shifting under
        // them mid-file.
        StreamEncoding::Struct { decoder } => {
            let layout = decoders.get(usize::from(*decoder))?;
            let columns = layout.column_header().ok()?;
            Some(format!(
                "rx_utc_ms,step_index,step_name,{columns},payload_hex,decode_note,core_rx_utc_ms"
            ))
        }
        _ => None,
    }
}

/// A tap name is authored, not typed by an operating system: it can hold
/// characters no filesystem accepts. Everything outside `[A-Za-z0-9._-]`
/// becomes `_`, and a stem that collides with one already taken gains its
/// tap id — `validate_taps` guarantees names are unique, so only the
/// *sanitized* form can collide.
fn unique_stem(tap: &StreamTap, used: &[String]) -> String {
    let mut stem: String = tap
        .name
        .as_str()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    // A leading dot would make the whole file a hidden one, and a stem of
    // dots alone would collide with `.`/`..`.
    if stem.trim_matches('.').is_empty() {
        stem = format!("stream-{}", tap.id);
    }
    if used.iter().any(|u| u == &stem) {
        stem = format!("{stem}-{}", tap.id);
    }
    stem
}

// ---- keep-last-N sweep across runs -----------------------------------------

/// Deletes all but the newest `keep` study result directories under `root`
/// (decision 30's retention note). `keep == 0` disables the sweep entirely
/// and deletes nothing.
///
/// **Count-based, not age-based**, which is what lets it run without a clock
/// of its own: it only ever needs to *order* directories, and the
/// filesystem's own mtime already orders them.
///
/// Only directories whose name is a 32-hex-character study id are
/// considered. A results root is a directory Core owns, but "delete
/// everything I did not expect to find" is not a posture a retention sweep
/// should ever take.
///
/// Returns how many directories it removed. Takes `root` explicitly rather
/// than resolving it, so it is testable against a temporary directory.
pub fn sweep_study_results(root: &Path, keep: usize) -> anyhow::Result<usize> {
    if keep == 0 {
        return Ok(0);
    }

    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        // No results have ever been written — nothing to sweep, not a
        // failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };

    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_study_id(name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        dirs.push((modified, entry.path()));
    }

    if dirs.len() <= keep {
        return Ok(0);
    }

    // Newest first, then drop everything past `keep`. Ties break on path so
    // the order is total and the sweep is deterministic.
    dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut removed = 0;
    for (_, path) in dirs.into_iter().skip(keep) {
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                tracing::info!("retention: removed old study results at {}", path.display());
                removed += 1;
            }
            Err(e) => tracing::warn!("retention: failed to remove {}: {e:?}", path.display()),
        }
    }
    Ok(removed)
}

fn is_study_id(name: &str) -> bool {
    name.len() == 32 && name.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::{SampleLayout, StreamScope, Unit};

    fn tap(id: u8, name: &str, source: StreamSource, encoding: StreamEncoding) -> StreamTap {
        StreamTap {
            id,
            name: heapless::String::try_from(name).unwrap(),
            source,
            encoding,
            scope: StreamScope::WholeStudy,
        }
    }

    fn samples() -> StreamEncoding {
        StreamEncoding::Samples { layout: SampleLayout::F32Le, unit: Unit::Volts, channel_id: 0 }
    }

    // ---- file layout: one file per tap, named by the tap -------------------

    #[test]
    fn create_writes_one_index_entry_per_declared_tap_before_any_bytes_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![
            tap(0, "power", StreamSource::PowerFrontEnd { sample_hz: 1000 }, samples()),
            tap(1, "trace", StreamSource::Signal { name: heapless::String::try_from("outpost").unwrap() }, StreamEncoding::Raw),
            tap(2, "gatt", StreamSource::GattTranscript, StreamEncoding::GattTranscript),
        ];

        let store = StreamStore::create(dir.path(), &taps, &[], 0).unwrap();

        let index = read_index(store.dir()).unwrap().unwrap();
        assert_eq!(index.streams.len(), 3);
        assert_eq!(index.streams[0].raw_file, "power.bin");
        assert_eq!(index.streams[0].rendered_file.as_deref(), Some("power.csv"));
        assert_eq!(index.streams[0].alias.as_deref(), Some("power"));
        // A `Raw` tap renders nothing — no sniff, no "looks like text"
        // fallback, and no alias to any of the three retired routes.
        assert_eq!(index.streams[1].raw_file, "trace.bin");
        assert_eq!(index.streams[1].rendered_file, None);
        assert_eq!(index.streams[1].alias, None);
        assert_eq!(index.streams[2].alias.as_deref(), Some("gatt"));

        // Written before a byte arrives, so a study that failed still says
        // which taps it declared.
        assert!(store.dir().join(INDEX_FILE).exists());
        assert!(!store.dir().join("power.bin").exists());
    }

    #[test]
    fn a_text_taps_raw_file_is_its_rendering_and_gets_no_second_copy() {
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![tap(0, "console", StreamSource::DevBenchLog, StreamEncoding::Text)];
        let store = StreamStore::create(dir.path(), &taps, &[], 0).unwrap();
        let index = read_index(store.dir()).unwrap().unwrap();
        assert_eq!(index.streams[0].raw_file, "console.txt");
        assert_eq!(index.streams[0].rendered_file, None);
    }

    #[test]
    fn an_outpost_trace_taps_raw_bytes_are_written_before_any_rendering_exists() {
        // An outpost tap's rendering is produced **post-hoc**, from the
        // complete raw file once the capture closes
        // (`study::render_outpost_traces`), so the streaming path writes only
        // raw bytes and the index carries no `rendered_file` yet. That is the
        // ordering decision 30(b) requires either way: raw before decode, so a
        // decode that fails never costs the capture.
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![tap(
            0,
            "outpost",
            StreamSource::Signal { name: heapless::String::try_from("outpost-uart").unwrap() },
            StreamEncoding::OutpostTrace,
        )];
        let mut store = StreamStore::create(dir.path(), &taps, &[], 0).unwrap();
        store.write_raw(0, b"\x01\x02\x03");

        let index = read_index(store.dir()).unwrap().unwrap();
        assert_eq!(index.streams[0].rendered_file, None);
        // The one encoding that gets an arrival log, declared before any bytes
        // arrive like everything else in the index.
        assert_eq!(index.streams[0].arrival_file.as_deref(), Some("outpost.arrival.csv"));
        assert_eq!(fs::read(store.dir().join("outpost.bin")).unwrap(), b"\x01\x02\x03");
        assert_eq!(store.refs()[0].bytes_written, 3);
        assert!(!store.refs()[0].truncated);
    }

    fn outpost_store(dir: &Path) -> StreamStore {
        let taps = vec![
            tap(
                0,
                "outpost",
                StreamSource::Signal { name: heapless::String::try_from("outpost-uart").unwrap() },
                StreamEncoding::OutpostTrace,
            ),
            tap(1, "power", StreamSource::PowerFrontEnd { sample_hz: 1000 }, samples()),
        ];
        StreamStore::create(dir, &taps, &[], 0).unwrap()
    }

    /// The arrival log's whole job: a row per frame, keyed by the same frame
    /// index `outpost::chunks` enumerates, with Core's receipt time on it.
    ///
    /// The three cases that decide whether the index stays in step with the
    /// decoder are all here, because getting any of them wrong shifts every
    /// timestamp in a trace: **a frame split across two reads** (the normal
    /// case on a serial port), **several frames completing in one read** (they
    /// share a stamp, because they genuinely arrived together), and **an empty
    /// run between two delimiters**, which `chunks` skips and this must skip
    /// too.
    #[test]
    fn the_arrival_log_records_one_stamped_row_per_frame() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = outpost_store(dir.path());

        // Frame A arrives in two reads; then B and C complete in one read,
        // with a doubled delimiter between them.
        store.note_arrival(0, b"\x03\xaa", 1_000);
        store.note_arrival(0, b"\xbb\x00", 1_020);
        store.note_arrival(0, b"\x02\xcc\x00\x00\x04\xdd\xee\xff\x00", 1_040);

        let log = fs::read_to_string(dir.path().join("streams").join("outpost.arrival.csv")).unwrap();
        let rows: Vec<&str> = log.lines().collect();
        assert_eq!(rows[0], "frame_index,rx_utc_ms,frame_bytes");
        assert_eq!(
            &rows[1..],
            [
                // 3 bytes — the delimiter is not part of the frame — and the
                // stamp of the read that *completed* it.
                "0,1020,3",
                // Both completed in the same read, so both carry its time.
                "1,1040,2",
                "2,1040,4",
            ],
            "the log did not match the frames those reads carried"
        );

        // And the frame lengths are exactly what the decoder will see, which
        // is what makes the join checkable rather than assumed.
        let raw = b"\x03\xaa\xbb\x00\x02\xcc\x00\x00\x04\xdd\xee\xff\x00";
        let lens: Vec<usize> = embarch_study_designer::outpost::chunks(raw)
            .map(<[u8]>::len)
            .collect();
        assert_eq!(lens, vec![3, 2, 4]);
    }

    /// Only the encoding whose rendering happens post-hoc gets a log. A sample
    /// row carries `core_rx_utc_ms` itself, so a second copy of the same fact
    /// would be a file to keep in sync for nothing.
    #[test]
    fn a_tap_that_is_not_an_outpost_trace_has_no_arrival_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = outpost_store(dir.path());
        store.note_arrival(1, b"\x01\x02\x00", 1_000);

        let index = read_index(store.dir()).unwrap().unwrap();
        assert_eq!(index.streams[1].arrival_file, None);
        assert!(!dir.path().join("streams").join("power.arrival.csv").exists());
    }

    #[test]
    fn update_index_fills_in_what_only_the_end_of_a_capture_knows() {
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![tap(
            0,
            "outpost",
            StreamSource::Signal { name: heapless::String::try_from("outpost-uart").unwrap() },
            StreamEncoding::OutpostTrace,
        )];
        let store = StreamStore::create(dir.path(), &taps, &[], 0).unwrap();

        update_index(store.dir(), |entry| {
            entry.rendered_file = Some("outpost.trace.csv".to_string());
            entry.note = Some("decoded but NOT named".to_string());
        })
        .unwrap();

        let index = read_index(store.dir()).unwrap().unwrap();
        assert_eq!(index.streams[0].rendered_file.as_deref(), Some("outpost.trace.csv"));
        assert_eq!(index.streams[0].note.as_deref(), Some("decoded but NOT named"));
        // The pre-arrival facts are untouched.
        assert_eq!(index.streams[0].raw_file, "outpost.bin");
        assert_eq!(index.streams[0].name, "outpost");
    }

    #[test]
    fn a_name_no_filesystem_would_accept_still_gets_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![
            tap(0, "a/b:c", StreamSource::DevBenchLog, StreamEncoding::Raw),
            tap(1, "a b c", StreamSource::DevBenchLog, StreamEncoding::Raw),
        ];
        let mut store = StreamStore::create(dir.path(), &taps, &[], 0).unwrap();
        store.write_raw(0, b"x");
        store.write_raw(1, b"y");

        let index = read_index(store.dir()).unwrap().unwrap();
        // Both sanitize to `a_b_c`; the second gains its tap id rather than
        // interleaving into the first tap's file.
        assert_eq!(index.streams[0].raw_file, "a_b_c.bin");
        assert_eq!(index.streams[1].raw_file, "a_b_c-1.bin");
        assert_eq!(fs::read(store.dir().join("a_b_c.bin")).unwrap(), b"x");
        assert_eq!(fs::read(store.dir().join("a_b_c-1.bin")).unwrap(), b"y");
        // The *name* is unchanged — it is what a Study taps by, and what
        // `GET /study/{id}/stream/{name}` resolves through.
        assert_eq!(index.streams[0].name, "a/b:c");
    }

    // ---- retention within a run: segment rotation --------------------------

    #[test]
    fn rotation_keeps_the_tail_and_reports_truncation_only_once_bytes_are_lost() {
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![tap(0, "raw", StreamSource::DevBenchLog, StreamEncoding::Raw)];
        // 4-byte cap: every 4-byte write past the first rotates.
        let mut store = StreamStore::create(dir.path(), &taps, &[], 4).unwrap();

        store.write_raw(0, b"aaaa");
        assert!(!store.refs()[0].truncated, "nothing has been lost yet");

        // First rotation: `raw.bin` becomes `raw.1.bin`, nothing is deleted.
        store.write_raw(0, b"bbbb");
        assert!(!store.refs()[0].truncated, "a first rotation loses nothing");
        assert_eq!(fs::read(store.dir().join("raw.1.bin")).unwrap(), b"aaaa");
        assert_eq!(fs::read(store.dir().join("raw.bin")).unwrap(), b"bbbb");

        // Second rotation drops the oldest segment — the first point at
        // which captured bytes actually stop existing.
        store.write_raw(0, b"cccc");
        assert!(store.refs()[0].truncated, "a dropped segment must reach StreamRef.truncated");

        // The tail is what survives, in capture order, and `bytes_written`
        // still counts everything this run wrote.
        let served = read_capture(store.dir(), "raw.bin", false).unwrap().unwrap();
        assert_eq!(served, b"bbbbcccc");
        assert_eq!(store.refs()[0].bytes_written, 12);
    }

    #[test]
    fn a_rotated_csv_reads_back_as_one_csv_with_one_header() {
        let dir = tempfile::tempdir().unwrap();
        let taps =
            vec![tap(0, "power", StreamSource::PowerFrontEnd { sample_hz: 1 }, samples())];
        let mut store = StreamStore::create(dir.path(), &taps, &[], 60).unwrap();

        for i in 0..8 {
            store.write_rendered_row(0, &format!("{i},step,1.0,volts,0,{i}"));
        }

        let served = read_capture(store.dir(), "power.csv", true).unwrap().unwrap();
        let text = String::from_utf8(served).unwrap();
        let header = format!("{},core_rx_utc_ms", Sample::csv_header());
        assert_eq!(text.lines().next().unwrap(), header);
        assert_eq!(
            text.lines().filter(|l| **l == header).count(),
            1,
            "a rotated capture must still read back as one valid CSV: {text}"
        );
        assert!(store.refs()[0].truncated);
    }

    #[test]
    fn a_zero_cap_disables_rotation_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![tap(0, "raw", StreamSource::DevBenchLog, StreamEncoding::Raw)];
        let mut store = StreamStore::create(dir.path(), &taps, &[], 0).unwrap();
        for _ in 0..64 {
            store.write_raw(0, b"0123456789");
        }
        assert!(!store.refs()[0].truncated);
        assert!(!store.dir().join("raw.1.bin").exists());
        assert_eq!(store.refs()[0].bytes_written, 640);
    }

    #[test]
    fn a_write_larger_than_the_cap_is_written_whole_rather_than_split() {
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![tap(0, "raw", StreamSource::DevBenchLog, StreamEncoding::Raw)];
        let mut store = StreamStore::create(dir.path(), &taps, &[], 4).unwrap();
        store.write_raw(0, b"0123456789");
        assert_eq!(fs::read(store.dir().join("raw.bin")).unwrap(), b"0123456789");
    }

    #[test]
    fn dropped_records_at_the_source_reach_truncated_too() {
        let dir = tempfile::tempdir().unwrap();
        let taps = vec![tap(0, "raw", StreamSource::DevBenchLog, StreamEncoding::Raw)];
        let mut store = StreamStore::create(dir.path(), &taps, &[], 0).unwrap();
        store.write_raw(0, b"ok");
        assert!(!store.refs()[0].truncated);
        store.mark_lost_at_source(0);
        assert!(store.refs()[0].truncated);
    }

    // ---- retention across runs: the keep-last-N sweep ----------------------

    /// Creates one result directory with a distinct, controlled mtime, so
    /// the sweep's ordering is exercised without the test depending on how
    /// fast it runs.
    fn make_result_dir(root: &Path, id: &str, mtime_rank: u64) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("events.json"), b"{}").unwrap();
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(mtime_rank);
        set_dir_mtime(&dir, t);
    }

    #[cfg(unix)]
    fn set_dir_mtime(dir: &Path, t: std::time::SystemTime) {
        let f = fs::File::open(dir).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(t).set_accessed(t)).unwrap();
    }

    /// Windows needs `FILE_FLAG_BACKUP_SEMANTICS` to open a directory
    /// handle at all. Best-effort: if it fails, the sweep still runs, it
    /// just orders by whatever mtimes creation happened to produce.
    #[cfg(windows)]
    fn set_dir_mtime(dir: &Path, t: std::time::SystemTime) {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        if let Ok(f) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
        {
            let _ = f.set_times(fs::FileTimes::new().set_modified(t).set_accessed(t));
        }
    }

    #[test]
    fn the_sweep_keeps_the_newest_n_and_deletes_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let ids: Vec<String> = (0..5u8).map(|i| format!("{:032x}", i)).collect();
        for (rank, id) in ids.iter().enumerate() {
            make_result_dir(root.path(), id, 1_000 + rank as u64);
        }

        let removed = sweep_study_results(root.path(), 2).unwrap();
        assert_eq!(removed, 3);
        // The two newest survive; the three oldest are gone.
        assert!(root.path().join(&ids[4]).exists());
        assert!(root.path().join(&ids[3]).exists());
        for id in &ids[..3] {
            assert!(!root.path().join(id).exists(), "{id} should have been swept");
        }
    }

    #[test]
    fn a_keep_of_zero_disables_the_sweep() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..3u8 {
            make_result_dir(root.path(), &format!("{:032x}", i), 1_000 + u64::from(i));
        }
        assert_eq!(sweep_study_results(root.path(), 0).unwrap(), 0);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    }

    #[test]
    fn the_sweep_never_touches_anything_that_is_not_a_study_id() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..3u8 {
            make_result_dir(root.path(), &format!("{:032x}", i), 1_000 + u64::from(i));
        }
        fs::create_dir_all(root.path().join("not-a-study")).unwrap();
        fs::write(root.path().join("README"), b"hi").unwrap();

        sweep_study_results(root.path(), 1).unwrap();
        assert!(root.path().join("not-a-study").exists());
        assert!(root.path().join("README").exists());
    }

    #[test]
    fn sweeping_a_results_root_that_does_not_exist_yet_is_not_a_failure() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(sweep_study_results(&root.path().join("never-written"), 5).unwrap(), 0);
    }

    // ---- env knobs ---------------------------------------------------------

    #[test]
    fn previous_segment_names_are_derived_from_the_extension() {
        assert_eq!(previous_segment_name("power.csv"), "power.1.csv");
        assert_eq!(previous_segment_name("trace.bin"), "trace.1.bin");
        assert_eq!(previous_segment_name("noext"), "noext.1");
    }

    #[test]
    fn content_types_come_from_the_declared_file_never_from_the_bytes() {
        assert_eq!(content_type_for("a.csv"), "text/csv");
        assert_eq!(content_type_for("a.txt"), "text/plain; charset=utf-8");
        assert_eq!(content_type_for("a.bin"), "application/octet-stream");
        assert_eq!(content_type_for("a"), "application/octet-stream");
    }
}
