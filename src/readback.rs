//! Reads an image back through the probe and says which state holds (decision 142). The smoke
//! check calls it before a bench's first test (decision 143); the run record is a later unit. It reads each segment of a board's newest `flashed-images.jsonl` entry in chunks of
//! [`live::MAX_WORDS`] words, each under its own short halt (`live::read_ranges`, decision 81),
//! writes nothing, and hashes what came back.
//!
//! The DUT's answer is `holds`, `differs` or `claim`; a bench board's is `released`,
//! `unreleased`, `unknown` or `unverified`. The probe sits behind [`CoreAccess`] so every state
//! is tested without a board.

use std::path::Path;

use embarch_study_designer::hil_image::{identity, BenchFirmwareList, Segment, BENCH_FIRMWARE_SCHEMA};
use serde::{Deserialize, Serialize};

use crate::lease::{sha256_hex, FlashedEntry};
use crate::live::{check, read_ranges, CoreAccess, MemReadRequest, Range, MAX_WORDS};

/// The environment variable naming `bench-firmware.json` (decision 142). The owner may swap it
/// for a key in Core's config file.
pub const BENCH_FIRMWARE_LIST_ENV: &str = "EMBARCH_BENCH_FIRMWARE_LIST";

/// Which kind of board is read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    /// The device under test: compared with what Core flashed.
    Dut,
    /// A bench board: compared with the released bench-firmware list.
    Bench,
}

/// What a readback found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ImageStatus {
    /// DUT: every segment read back equals the segment Core flashed.
    Holds,
    /// DUT: the first segment whose bytes differ.
    Differs { address: u64, length: u64 },
    /// DUT: Core cannot say; `reason` names why.
    Claim { reason: String },
    /// Bench: the read-back identity is in the released list.
    Released { app: String, board: String, version: String },
    /// Bench: not in the list; `scope` is the token scope of the flash that put it there.
    Unreleased { scope: String },
    /// Bench: Core has no flash entry for the board, so no segments to read.
    Unknown,
    /// Bench: no probe, a refused read, or no list to compare with; `reason` names which.
    Unverified { reason: String },
}

/// The released list from the path in `EMBARCH_BENCH_FIRMWARE_LIST`; the reason it cannot be
/// had otherwise.
pub fn bench_list_from_env() -> Result<BenchFirmwareList, String> {
    let path = std::env::var_os(BENCH_FIRMWARE_LIST_ENV).ok_or_else(|| format!("{BENCH_FIRMWARE_LIST_ENV} is not set"))?;
    load_bench_list(Path::new(&path))
}

/// Parses `bench-firmware.json` at `path`.
pub fn load_bench_list(path: &Path) -> Result<BenchFirmwareList, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let list: BenchFirmwareList =
        serde_json::from_str(&text).map_err(|e| format!("{} does not parse: {e}", path.display()))?;
    if list.schema != BENCH_FIRMWARE_SCHEMA {
        return Err(format!("{} has schema {}, expected {BENCH_FIRMWARE_SCHEMA}", path.display(), list.schema));
    }
    Ok(list)
}

/// The SHA-256 of the `seg.length` bytes at `seg.address`, read in chunks, one halt each.
fn read_segment(core: &mut impl CoreAccess, seg: &Segment) -> Result<String, String> {
    let start = seg.address & !3;
    let skip = (seg.address - start) as usize;
    let total_words = (skip as u64 + seg.length).div_ceil(4);
    let mut bytes = Vec::with_capacity(total_words as usize * 4);
    let mut done = 0u64;
    while done < total_words {
        let words = (total_words - done).min(u64::from(MAX_WORDS)) as u32;
        let req = MemReadRequest {
            role: None,
            probe_serial: None,
            ranges: vec![Range { address: start + done * 4, words }],
        };
        check(&req)?;
        let out = read_ranges(core, &req).map_err(|e| format!("{e:#}"))?;
        for r in out.ranges {
            bytes.extend(r.words.iter().flat_map(|w| w.to_le_bytes()));
        }
        done += u64::from(words);
    }
    let end = skip + seg.length as usize;
    Ok(sha256_hex(&bytes[skip..end]))
}

/// Reads back what `entry` flashed through `core` (`None`: no probe) and says which state
/// holds. `entry` is the board's newest flash entry, `list` the released bench firmware.
pub fn readback(
    core: Option<&mut impl CoreAccess>,
    board: Board,
    entry: Option<&FlashedEntry>,
    list: Option<&BenchFirmwareList>,
) -> ImageStatus {
    let refuse = |reason: String| match board {
        Board::Dut => ImageStatus::Claim { reason },
        Board::Bench => ImageStatus::Unverified { reason },
    };
    let Some(core) = core else { return refuse("the board has no probe".to_string()) };
    let Some(entry) = entry else {
        return match board {
            Board::Dut => ImageStatus::Claim { reason: "no flash is recorded for this board since this store began".into() },
            Board::Bench => ImageStatus::Unknown,
        };
    };
    let segments = match entry.segments.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return match board {
                Board::Dut => ImageStatus::Claim { reason: "the flash entry records no segments to compare".into() },
                Board::Bench => ImageStatus::Unknown,
            }
        }
    };
    let list = match (board, list) {
        (Board::Bench, None) => return refuse("no bench-firmware list to compare with".to_string()),
        (_, l) => l,
    };
    let mut read_back = Vec::with_capacity(segments.len());
    for seg in segments {
        match read_segment(core, seg) {
            Ok(sha256) => read_back.push(Segment { address: seg.address, length: seg.length, sha256 }),
            Err(why) => {
                return refuse(format!("the read at {:#x} ({} bytes) was refused: {why}", seg.address, seg.length));
            }
        }
    }
    match board {
        Board::Dut => match segments.iter().zip(&read_back).find(|(want, got)| want.sha256 != got.sha256) {
            None => ImageStatus::Holds,
            Some((want, _)) => ImageStatus::Differs { address: want.address, length: want.length },
        },
        Board::Bench => {
            let id = identity(&read_back);
            match list.and_then(|l| l.images.iter().find(|i| i.identity == id)) {
                Some(i) => ImageStatus::Released { app: i.app.clone(), board: i.board.clone(), version: i.version.clone() },
                None => ImageStatus::Unreleased { scope: entry.scope.clone() },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::Result;
    use embarch_study_designer::hil_image::BenchImage;

    use super::*;

    /// A flat memory: `(base, bytes)` blocks; a read outside them, or with `refuse`, fails.
    struct Fake {
        mem: Vec<(u64, Vec<u8>)>,
        refuse: bool,
        writes: u32,
        halts: u32,
        max_chunk_words: usize,
    }

    impl Fake {
        fn new(mem: Vec<(u64, Vec<u8>)>) -> Self {
            Self { mem, refuse: false, writes: 0, halts: 0, max_chunk_words: 0 }
        }
        fn byte(&self, a: u64) -> Option<u8> {
            self.mem.iter().find(|(b, v)| a >= *b && a < b + v.len() as u64).map(|(b, v)| v[(a - b) as usize])
        }
    }

    impl CoreAccess for Fake {
        fn halt(&mut self) -> Result<()> {
            self.halts += 1;
            Ok(())
        }
        fn run(&mut self) -> Result<()> {
            Ok(())
        }
        fn read_word(&mut self, _: u64) -> Result<u32> {
            Ok(0) // DHCSR: running
        }
        fn read_words(&mut self, address: u64, out: &mut [u32]) -> Result<()> {
            if self.refuse {
                anyhow::bail!("read-out protection is enabled");
            }
            self.max_chunk_words = self.max_chunk_words.max(out.len());
            for (i, w) in out.iter_mut().enumerate() {
                let mut b = [0u8; 4];
                for (j, x) in b.iter_mut().enumerate() {
                    *x = self.byte(address + 4 * i as u64 + j as u64).ok_or_else(|| anyhow::anyhow!("unmapped"))?;
                }
                *w = u32::from_le_bytes(b);
            }
            Ok(())
        }
        fn read_reg(&mut self, _: u16) -> Result<u32> {
            Ok(0)
        }
        fn pause(&mut self, _: Duration) {}
    }

    fn seg_of(address: u64, bytes: &[u8]) -> Segment {
        Segment { address, length: bytes.len() as u64, sha256: sha256_hex(bytes) }
    }

    fn entry(segments: Option<Vec<Segment>>) -> FlashedEntry {
        FlashedEntry {
            chip: "c".into(),
            probe_serial: Some("0010".into()),
            role: None,
            path: "p".into(),
            sha256: None,
            flashed_at: String::new(),
            lease_holder: None,
            identity: None,
            segments,
            scope: "agent".into(),
            at_utc_ms: 0,
            call_log_seq: None,
        }
    }

    /// A 600-byte segment at an aligned address (three chunks of at most 256 words are not
    /// needed; 1100 bytes forces two) and a 6-byte one at an unaligned address.
    fn image() -> (Vec<(u64, Vec<u8>)>, Vec<Segment>) {
        let big: Vec<u8> = (0..1100u32).map(|i| (i * 7 + 1) as u8).collect();
        let small = vec![9u8, 8, 7, 6, 5, 4];
        let segs = vec![seg_of(0x1000, &big), seg_of(0x2002, &small)];
        (vec![(0x1000, big), (0x2000, vec![0, 0, 9, 8, 7, 6, 5, 4])], segs)
    }

    #[test]
    fn every_segment_matching_holds_and_nothing_is_written() {
        let (mem, segs) = image();
        let mut core = Fake::new(mem);
        let got = readback(Some(&mut core), Board::Dut, Some(&entry(Some(segs))), None);
        assert_eq!(got, ImageStatus::Holds);
        assert!(core.max_chunk_words <= MAX_WORDS as usize, "chunked");
        assert!(core.halts >= 3, "one halt per chunk, not one for the image");
        assert_eq!(core.writes, 0);
    }

    #[test]
    fn one_changed_word_differs_naming_its_range() {
        let (mut mem, segs) = image();
        mem[1].1[4] ^= 0xFF; // a byte of the second segment
        let mut core = Fake::new(mem);
        let got = readback(Some(&mut core), Board::Dut, Some(&entry(Some(segs))), None);
        assert_eq!(got, ImageStatus::Differs { address: 0x2002, length: 6 });
    }

    #[test]
    fn a_refused_read_or_missing_ground_is_a_claim_with_the_reason() {
        let (mem, segs) = image();
        let mut core = Fake::new(mem.clone());
        core.refuse = true;
        match readback(Some(&mut core), Board::Dut, Some(&entry(Some(segs.clone()))), None) {
            ImageStatus::Claim { reason } => assert!(reason.contains("read-out protection"), "{reason}"),
            o => panic!("{o:?}"),
        }
        let mut core = Fake::new(mem);
        let claim = |o: ImageStatus| matches!(o, ImageStatus::Claim { .. });
        assert!(claim(readback(None::<&mut Fake>, Board::Dut, Some(&entry(Some(segs))), None)));
        assert!(claim(readback(Some(&mut core), Board::Dut, None, None)));
        assert!(claim(readback(Some(&mut core), Board::Dut, Some(&entry(None)), None)));
        assert!(claim(readback(Some(&mut core), Board::Dut, Some(&entry(Some(vec![]))), None)));
    }

    fn list_for(segs: &[Segment]) -> BenchFirmwareList {
        BenchFirmwareList {
            schema: BENCH_FIRMWARE_SCHEMA,
            release: "1.0".into(),
            images: vec![BenchImage {
                app: "dev-bench".into(),
                board: "b".into(),
                version: "1.2.3".into(),
                commit: "abc".into(),
                identity: identity(segs),
                segments: segs.to_vec(),
            }],
        }
    }

    #[test]
    fn a_bench_board_is_released_unreleased_unknown_or_unverified() {
        let (mem, segs) = image();
        let list = list_for(&segs);
        let mut core = Fake::new(mem.clone());
        let e = entry(Some(segs.clone()));
        assert_eq!(
            readback(Some(&mut core), Board::Bench, Some(&e), Some(&list)),
            ImageStatus::Released { app: "dev-bench".into(), board: "b".into(), version: "1.2.3".into() }
        );
        // The same entry flashed, but the chip holds something else: its identity is not listed.
        let mut other = mem.clone();
        other[0].1[0] ^= 1;
        let mut core = Fake::new(other);
        assert_eq!(
            readback(Some(&mut core), Board::Bench, Some(&e), Some(&list)),
            ImageStatus::Unreleased { scope: "agent".into() }
        );
        let mut core = Fake::new(mem);
        assert_eq!(readback(Some(&mut core), Board::Bench, None, Some(&list)), ImageStatus::Unknown);
        let unverified = |o: ImageStatus| matches!(o, ImageStatus::Unverified { .. });
        assert!(unverified(readback(None::<&mut Fake>, Board::Bench, Some(&e), Some(&list))));
        assert!(unverified(readback(Some(&mut core), Board::Bench, Some(&e), None)));
        core.refuse = true;
        assert!(unverified(readback(Some(&mut core), Board::Bench, Some(&e), Some(&list))));
    }

    #[test]
    fn the_list_loads_from_a_path_and_a_wrong_schema_is_refused() {
        let (_, segs) = image();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bench-firmware.json");
        std::fs::write(&p, serde_json::to_string(&list_for(&segs)).unwrap()).unwrap();
        assert_eq!(load_bench_list(&p).unwrap().images.len(), 1);
        let mut bad = list_for(&segs);
        bad.schema = 99;
        std::fs::write(&p, serde_json::to_string(&bad).unwrap()).unwrap();
        assert!(load_bench_list(&p).unwrap_err().contains("schema 99"));
        assert!(load_bench_list(&dir.path().join("nope.json")).is_err());
    }
}
