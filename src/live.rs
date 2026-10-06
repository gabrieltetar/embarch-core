//! `POST /live/mem-read` — read words of a role's target memory over its enrolled probe
//! (decision 81). The atlas's live module turns register names into addresses and decodes what
//! comes back; Core only reads.
//!
//! **One-off reads halt the core** (decision 81, amending the live module's never-halt rule):
//! an STM32G0 idling in WFI with no DMA clock on stops its bus matrix in Sleep, and a debug read
//! of the system bus then returns 0 or the previous word with an OK acknowledge (the G0 reference
//! manual, debug chapter). A halted core is never asleep, so every word read halted is good. A
//! core that was running is let run again, and the answer says how long it was stopped. Nothing
//! is written to the target beyond the halt and the resume.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use probe_rs::{MemoryInterface, Permissions};
use serde::{Deserialize, Serialize};

/// The most words one request returns, over all its ranges: 1 KiB, a whole peripheral's register
/// block on the parts the suite targets, and short enough to keep the halt in the low milliseconds.
pub const MAX_WORDS: u32 = 256;

/// The most ranges one request reads under its one halt.
pub const MAX_RANGES: usize = 32;

/// How long a running core gets to stop before the read is refused.
const HALT_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
pub struct Range {
    /// Byte address of the first word; a multiple of 4.
    pub address: u64,
    /// How many 32-bit words to read.
    pub words: u32,
}

#[derive(Debug, Deserialize)]
pub struct MemReadRequest {
    /// The enrolled role whose probe and chip are used.
    #[serde(default = "default_role")]
    pub role: String,
    /// Read in order, all under one halt, so they are one snapshot.
    pub ranges: Vec<Range>,
}

fn default_role() -> String {
    embarch_topology::hardware::DUT_ROLE.to_string()
}

#[derive(Debug, Serialize)]
pub struct RangeRead {
    pub address: u64,
    pub words: Vec<u32>,
}

#[derive(Debug, Serialize)]
pub struct MemReadResponse {
    pub role: String,
    pub chip: String,
    pub probe_serial: String,
    pub ranges: Vec<RangeRead>,
    /// The core was running and this read stopped it.
    pub halted: bool,
    /// How long it was stopped, in microseconds; 0 when it was already halted.
    pub halted_us: u64,
}

/// Refuses a request before any hardware is touched.
pub fn check(req: &MemReadRequest) -> Result<(), String> {
    if req.ranges.is_empty() || req.ranges.len() > MAX_RANGES {
        return Err(format!("ranges must hold 1 to {MAX_RANGES} ranges, got {}", req.ranges.len()));
    }
    let mut total = 0u64;
    for r in &req.ranges {
        if !r.address.is_multiple_of(4) {
            return Err(format!("address {:#x} is not a multiple of 4", r.address));
        }
        if r.words == 0 {
            return Err(format!("the range at {:#x} reads no words", r.address));
        }
        if r.address + 4 * u64::from(r.words) > 1 << 32 {
            return Err(format!("{} words from {:#x} run past the 32-bit address space", r.words, r.address));
        }
        total += u64::from(r.words);
    }
    if total > u64::from(MAX_WORDS) {
        return Err(format!("{total} words asked for; one read returns at most {MAX_WORDS}"));
    }
    Ok(())
}

/// The read itself, behind the same board-identity gate as `/flash` and `/reset`.
pub fn run(chip: &str, probe_serial: &str, role: &str, req: &MemReadRequest) -> Result<MemReadResponse> {
    embarch_topology::hardware::validate_serial(probe_serial)
        .context("board-identity gate refused this read")?;
    let mut probe = crate::hardware::open_probe(Some(probe_serial), "mem-read")?;
    embarch_topology::hardware::check_target_powered(&mut probe).context("can't read")?;
    // Guarded attach (decision 80): probe-rs's own ARMv6 STM32 sequence would read-modify-write
    // a running target's RCC through reads that can come back as garbage.
    let mut session = embarch_topology::hardware::attach::attach(probe, chip, Permissions::default())
        .with_context(|| format!("failed to attach to target '{chip}'"))?;
    let mut core = session.core(0).context("failed to select core 0")?;

    let was_halted = core.core_halted().context("failed to read whether the core is halted")?;
    let start = Instant::now();
    if !was_halted {
        core.halt(HALT_TIMEOUT).context(
            "the core did not halt for the read; in Stop mode a debug halt needs \
             DBGMCU_CR.DBG_STOP, which a read does not set",
        )?;
    }
    let read: Result<Vec<RangeRead>> = req
        .ranges
        .iter()
        .map(|r| {
            let mut words = vec![0u32; r.words as usize];
            core.read_32(r.address, &mut words)
                .with_context(|| format!("failed to read {} words at {:#x}", r.words, r.address))?;
            Ok(RangeRead { address: r.address, words })
        })
        .collect();
    let resumed = if was_halted { Ok(()) } else { core.run() };
    let halted_us = if was_halted { 0 } else { start.elapsed().as_micros() as u64 };

    let ranges = read?;
    resumed.context("failed to let the core run again after the read")?;
    Ok(MemReadResponse {
        role: role.to_string(),
        chip: chip.to_string(),
        probe_serial: probe_serial.to_string(),
        ranges,
        halted: !was_halted,
        halted_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(ranges: &[(u64, u32)]) -> MemReadRequest {
        MemReadRequest {
            role: "dut".into(),
            ranges: ranges.iter().map(|&(address, words)| Range { address, words }).collect(),
        }
    }

    #[test]
    fn aligned_ranges_within_the_caps_pass() {
        assert!(check(&req(&[(0x4002_103C, 1)])).is_ok());
        assert!(check(&req(&[(0x4000_A000, 13), (0x4000_A038, 2)])).is_ok());
        assert!(check(&req(&[(0x4000_A000, MAX_WORDS)])).is_ok());
        assert!(check(&req(&[(0xFFFF_FFFC, 1)])).is_ok());
    }

    #[test]
    fn misaligned_empty_oversized_and_wrapping_reads_are_refused() {
        assert!(check(&req(&[])).unwrap_err().contains("ranges must hold"));
        assert!(check(&req(&[(0x4002_1000, 1); MAX_RANGES + 1])).unwrap_err().contains("ranges must hold"));
        assert!(check(&req(&[(0x4002_103D, 1)])).unwrap_err().contains("multiple of 4"));
        assert!(check(&req(&[(0x4002_1000, 0)])).unwrap_err().contains("reads no words"));
        assert!(check(&req(&[(0x4002_1000, 200), (0x4002_2000, 57)])).unwrap_err().contains("at most"));
        assert!(check(&req(&[(0xFFFF_FFFC, 2)])).unwrap_err().contains("32-bit"));
    }

    #[test]
    fn the_role_defaults_to_the_dut() {
        let r: MemReadRequest =
            serde_json::from_str(r#"{"ranges": [{"address": 1073877052, "words": 1}]}"#).unwrap();
        assert_eq!(r.role, "dut");
        assert_eq!(r.ranges[0].address, 0x4002_103C);
    }
}
