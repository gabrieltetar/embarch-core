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
//!
//! **`POST /live/core-state`** (decision 97) answers where the core is, read-only, in two modes: the
//! default never halts (`DHCSR` plus `DWT_PCSR` samples, both PPB reads), `halt=true` stops the
//! core once, reads its registers and `SCB_ICSR`, and lets it run again. Both routes name the
//! state found, the state left, and every halt the request made, the identity gate's included.
//! The probe layer sits behind [`CoreAccess`] so the logic is unit-tested without a board.

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
    /// **Deprecated, kept one release (decision 97).** `true` when the core was running and the
    /// read halted it, so `false` meant "the core was already halted" and never "running". Read
    /// `found` and `left` instead.
    pub halted: bool,
    /// How long this read's own halt lasted, in microseconds; 0 when it was already halted. The
    /// identity gate's halt is not in it.
    pub halted_us: u64,
    /// The core's state when the request arrived.
    pub found: RunState,
    /// The core's state when the request returned.
    pub left: RunState,
    /// Every halt this request made: the identity gate's plus this read's own.
    pub halt_count: u32,
}

/// Whether the core was executing or stopped by a debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Halted,
}

impl RunState {
    fn of(halted: bool) -> Self {
        if halted { Self::Halted } else { Self::Running }
    }
}

/// The probe layer the logic needs, so a unit test can stand in for a board.
pub trait CoreAccess {
    fn halt(&mut self) -> Result<()>;
    fn run(&mut self) -> Result<()>;
    fn read_word(&mut self, address: u64) -> Result<u32>;
    fn read_words(&mut self, address: u64, out: &mut [u32]) -> Result<()>;
    /// A core register by its Cortex-M register id (`R0`-`R15` 0-15, `xPSR` 16, `EXTRA` 20).
    fn read_reg(&mut self, id: u16) -> Result<u32>;
    fn pause(&mut self, d: Duration);
}

impl CoreAccess for probe_rs::Core<'_> {
    fn halt(&mut self) -> Result<()> {
        probe_rs::Core::halt(self, HALT_TIMEOUT).map(|_| ()).map_err(Into::into)
    }
    fn run(&mut self) -> Result<()> {
        probe_rs::Core::run(self).map_err(Into::into)
    }
    fn read_word(&mut self, address: u64) -> Result<u32> {
        self.read_word_32(address).map_err(Into::into)
    }
    fn read_words(&mut self, address: u64, out: &mut [u32]) -> Result<()> {
        self.read_32(address, out).map_err(Into::into)
    }
    fn read_reg(&mut self, id: u16) -> Result<u32> {
        self.read_core_reg::<u32>(probe_rs::RegisterId(id)).map_err(Into::into)
    }
    fn pause(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

const DHCSR: u64 = 0xE000_EDF0;
const SCB_ICSR: u64 = 0xE000_ED04;
const DWT_PCSR: u64 = 0xE000_101C;
const S_HALT: u32 = 1 << 17;

/// How many halts the identity gate makes before this request's own. The gate reads an STM32G0's
/// UID with the core halted, and only when it found the core running (`embarch-topology`
/// `hardware_id.rs`, `read_halted`); every other family reads its ID without halting. Mirrors
/// topology's `Stm32G0Uid` arm by chip prefix: assumed, not asked of topology (decision 97).
pub fn gate_halts(chip: &str, found: RunState) -> u32 {
    u32::from(chip.to_ascii_lowercase().starts_with("stm32g0") && found == RunState::Running)
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

/// What `mem-read` found out, apart from the words.
#[derive(Debug)]
pub struct ReadOutcome {
    pub ranges: Vec<RangeRead>,
    pub found: RunState,
    pub left: RunState,
    pub halted_us: u64,
    /// This read's own halts, the identity gate's not included.
    pub halts: u32,
}

/// Reads the ranges under one halt of a running core, then lets it run; a halted core is read
/// as it is and left halted.
pub fn read_ranges(core: &mut impl CoreAccess, req: &MemReadRequest) -> Result<ReadOutcome> {
    let was_halted = core.read_word(DHCSR).context("failed to read whether the core is halted")? & S_HALT != 0;
    let start = Instant::now();
    if !was_halted {
        core.halt().context(
            "the core did not halt for the read; in Stop mode a debug halt needs \
             DBGMCU_CR.DBG_STOP, which a read does not set",
        )?;
    }
    let read: Result<Vec<RangeRead>> = req
        .ranges
        .iter()
        .map(|r| {
            let mut words = vec![0u32; r.words as usize];
            core.read_words(r.address, &mut words)
                .with_context(|| format!("failed to read {} words at {:#x}", r.words, r.address))?;
            Ok(RangeRead { address: r.address, words })
        })
        .collect();
    let resumed = if was_halted { Ok(()) } else { core.run() };
    let halted_us = if was_halted { 0 } else { start.elapsed().as_micros() as u64 };
    let ranges = read?;
    resumed.context("failed to let the core run again after the read")?;
    Ok(ReadOutcome {
        ranges,
        found: RunState::of(was_halted),
        left: RunState::of(was_halted),
        halted_us,
        halts: u32::from(!was_halted),
    })
}

/// Opens the role's probe behind the identity gate and the guarded attach, and hands the core to
/// `f` together with the number of halts the gate made.
fn with_core<T>(
    chip: &str,
    probe_serial: &str,
    owner: &str,
    f: impl FnOnce(&mut probe_rs::Core<'_>, &dyn Fn(RunState) -> u32) -> Result<T>,
) -> Result<T> {
    embarch_topology::hardware::validate_serial_for_chip(probe_serial, chip)
        .context("board-identity gate refused this read")?;
    let mut probe = crate::hardware::open_probe(Some(probe_serial), owner)?;
    embarch_topology::hardware::check_target_powered(&mut probe).context("can't read")?;
    // Guarded attach (decision 80): probe-rs's own ARMv6 STM32 sequence would read-modify-write
    // a running target's RCC through reads that can come back as garbage.
    let mut session = embarch_topology::hardware::attach::attach(probe, chip, Permissions::default())
        .with_context(|| format!("failed to attach to target '{chip}'"))?;
    let mut core = session.core(0).context("failed to select core 0")?;
    let chip = chip.to_string();
    f(&mut core, &move |found| gate_halts(&chip, found))
}

/// The read itself, behind the same board-identity gate as `/flash` and `/reset`.
pub fn run(chip: &str, probe_serial: &str, role: &str, req: &MemReadRequest) -> Result<MemReadResponse> {
    let (out, gate) = with_core(chip, probe_serial, "mem-read", |core, gate| {
        let out = read_ranges(core, req)?;
        let g = gate(out.found);
        Ok((out, g))
    })?;
    Ok(MemReadResponse {
        role: role.to_string(),
        chip: chip.to_string(),
        probe_serial: probe_serial.to_string(),
        ranges: out.ranges,
        halted: out.halts > 0,
        halted_us: out.halted_us,
        found: out.found,
        left: out.left,
        halt_count: gate + out.halts,
    })
}

/// The most `DWT_PCSR` samples one request takes.
pub const MAX_SAMPLES: u32 = 64;

#[derive(Debug, Deserialize)]
pub struct CoreStateRequest {
    /// The enrolled role whose probe and chip are used.
    #[serde(default = "default_role")]
    pub role: String,
    /// Stop the core once and read its registers; the default never halts.
    #[serde(default)]
    pub halt: bool,
    /// How many `DWT_PCSR` samples the non-halting mode takes, 1 ms apart.
    #[serde(default = "default_samples")]
    pub samples: u32,
}

fn default_samples() -> u32 {
    8
}

/// Refuses a request before any hardware is touched.
pub fn check_core_state(req: &CoreStateRequest) -> Result<(), String> {
    if req.samples == 0 || req.samples > MAX_SAMPLES {
        return Err(format!("samples must be 1 to {MAX_SAMPLES}, got {}", req.samples));
    }
    Ok(())
}

/// `DHCSR` as the core reported it.
#[derive(Debug, PartialEq, Serialize)]
pub struct Dhcsr {
    pub raw: u32,
    pub s_halt: bool,
    pub s_sleep: bool,
    pub s_lockup: bool,
    /// An instruction retired since the last `DHCSR` read (clears on read).
    pub s_retire_st: bool,
    /// The core was reset since the last `DHCSR` read (clears on read).
    pub s_reset_st: bool,
    /// `lockup`, `halted`, `sleeping` or `running`, in that order of precedence.
    pub state: &'static str,
}

impl Dhcsr {
    fn decode(raw: u32) -> Self {
        let bit = |n: u32| raw & (1 << n) != 0;
        let (s_halt, s_sleep, s_lockup) = (bit(17), bit(18), bit(19));
        let state = if s_lockup {
            "lockup"
        } else if s_halt {
            "halted"
        } else if s_sleep {
            "sleeping"
        } else {
            "running"
        };
        Self { raw, s_halt, s_sleep, s_lockup, s_retire_st: bit(24), s_reset_st: bit(25), state }
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct CoreRegisters {
    pub r: [u32; 13],
    /// The active stack pointer (`R13`).
    pub sp: u32,
    pub lr: u32,
    pub pc: u32,
    pub xpsr: u32,
    pub control: u32,
    pub primask: u32,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Icsr {
    pub raw: u32,
    /// The active exception number; 0 in thread mode.
    pub vectactive: u32,
    pub vectpending: u32,
    pub isrpending: bool,
    pub pendstset: bool,
    pub nmipendset: bool,
}

impl Icsr {
    fn decode(raw: u32) -> Self {
        Self {
            raw,
            vectactive: raw & 0x1FF,
            vectpending: (raw >> 12) & 0x1FF,
            isrpending: raw & (1 << 22) != 0,
            pendstset: raw & (1 << 26) != 0,
            nmipendset: raw & (1 << 31) != 0,
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct CoreStateResponse {
    pub role: String,
    pub chip: String,
    pub probe_serial: String,
    pub found: RunState,
    pub left: RunState,
    /// Every halt this request made, the identity gate's included; 0 in the default mode on a
    /// non-STM32G0 or an already-halted core.
    pub halt_count: u32,
    /// How long this request's own halt lasted; 0 when it made none.
    pub halted_us: u64,
    /// `DHCSR` as found.
    pub dhcsr: Dhcsr,
    /// `DHCSR` again after the samples (default mode only), where `s_retire_st` says whether
    /// instructions retired in between.
    pub dhcsr_after: Option<Dhcsr>,
    /// `DWT_PCSR` samples, 1 ms apart; `null` is a sample the core did not give (asleep, or
    /// halted). Empty when the core was halted or the part has no PC sampling register.
    pub pc_samples: Vec<Option<u32>>,
    /// Why `pc_samples` is empty, when it is.
    pub pc_note: Option<String>,
    /// `halt=true` only.
    pub registers: Option<CoreRegisters>,
    /// `halt=true` only.
    pub scb_icsr: Option<Icsr>,
}

/// Where the core is. Default: `DHCSR`, then `DWT_PCSR` samples, no halt. `halt`: one halt (none
/// when the core was already halted), the registers and `SCB_ICSR`, then back to the state found.
/// `gate` is how many halts the identity gate made.
pub fn read_core_state(
    core: &mut impl CoreAccess,
    req: &CoreStateRequest,
    gate: impl FnOnce(RunState) -> u32,
) -> Result<CoreStateResponse> {
    let dhcsr = Dhcsr::decode(core.read_word(DHCSR).context("failed to read DHCSR")?);
    let found = RunState::of(dhcsr.s_halt);
    let gate_halts = gate(found);
    let mut out = CoreStateResponse {
        role: req.role.clone(),
        chip: String::new(),
        probe_serial: String::new(),
        found,
        left: found,
        halt_count: gate_halts,
        halted_us: 0,
        dhcsr,
        dhcsr_after: None,
        pc_samples: Vec::new(),
        pc_note: None,
        registers: None,
        scb_icsr: None,
    };
    if !req.halt {
        if found == RunState::Halted {
            out.pc_note = Some("the core is halted: DWT_PCSR returns nothing on a halted core, use halt=true for the registers".into());
            return Ok(out);
        }
        for i in 0..req.samples {
            if i > 0 {
                core.pause(Duration::from_millis(1));
            }
            match core.read_word(DWT_PCSR) {
                Ok(v) => out.pc_samples.push((v != u32::MAX).then_some(v)),
                Err(e) => {
                    out.pc_samples.clear();
                    out.pc_note = Some(format!("DWT_PCSR could not be read (the part may have no PC sampling): {e:#}"));
                    break;
                }
            }
        }
        out.dhcsr_after = Some(Dhcsr::decode(core.read_word(DHCSR).context("failed to read DHCSR")?));
        return Ok(out);
    }
    let start = Instant::now();
    if found == RunState::Running {
        core.halt().context(
            "the core did not halt; in Stop mode a debug halt needs DBGMCU_CR.DBG_STOP, \
             which a read does not set",
        )?;
        out.halt_count += 1;
    }
    let regs = read_registers(core);
    let icsr = core.read_word(SCB_ICSR).context("failed to read SCB_ICSR");
    let resumed = if found == RunState::Running { core.run() } else { Ok(()) };
    if found == RunState::Running {
        out.halted_us = start.elapsed().as_micros() as u64;
    }
    resumed.context("failed to let the core run again after the read")?;
    out.registers = Some(regs?);
    out.scb_icsr = Some(Icsr::decode(icsr?));
    Ok(out)
}

fn read_registers(core: &mut impl CoreAccess) -> Result<CoreRegisters> {
    let mut r = [0u32; 13];
    for (i, slot) in r.iter_mut().enumerate() {
        *slot = core.read_reg(i as u16).with_context(|| format!("failed to read R{i}"))?;
    }
    let mut reg = |id: u16, name: &str| core.read_reg(id).with_context(|| format!("failed to read {name}"));
    let sp = reg(13, "SP")?;
    let lr = reg(14, "LR")?;
    let pc = reg(15, "PC")?;
    let xpsr = reg(16, "xPSR")?;
    // probe-rs packs CONTROL [31:24], FAULTMASK [23:16], BASEPRI [15:8], PRIMASK [7:0] in one id.
    let extra = reg(20, "CONTROL/PRIMASK")?;
    Ok(CoreRegisters { r, sp, lr, pc, xpsr, control: extra >> 24, primask: extra & 1 })
}

/// `POST /live/core-state` behind the identity gate and the guarded attach.
pub fn run_core_state(chip: &str, probe_serial: &str, role: &str, req: &CoreStateRequest) -> Result<CoreStateResponse> {
    let mut out = with_core(chip, probe_serial, "core-state", |core, gate| read_core_state(core, req, gate))?;
    out.chip = chip.to_string();
    out.probe_serial = probe_serial.to_string();
    out.role = role.to_string();
    Ok(out)
}

/// `DHCSR`, the core's debug status register, read twice a few milliseconds apart so
/// `S_RETIRE_ST` (clear on read) in the second read says whether instructions are retiring.
/// A PPB read through the guarded attach: it never halts and needs no DMA clock (decision 85).
pub fn read_dhcsr(chip: &str, probe_serial: &str) -> Result<u32> {
    embarch_topology::hardware::validate_serial_for_chip(probe_serial, chip).context("board-identity gate refused this read")?;
    let mut probe = crate::hardware::open_probe(Some(probe_serial), "exchange")?;
    embarch_topology::hardware::check_target_powered(&mut probe).context("can't read")?;
    let mut session = embarch_topology::hardware::attach::attach(probe, chip, Permissions::default())
        .with_context(|| format!("failed to attach to target '{chip}'"))?;
    let mut core = session.core(0).context("failed to select core 0")?;
    core.read_word_32(DHCSR).context("failed to read DHCSR")?;
    std::thread::sleep(std::time::Duration::from_millis(3));
    core.read_word_32(DHCSR).context("failed to read DHCSR")
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

    /// A scripted core: answers by address, counts halts and resumes.
    struct Fake {
        halted: bool,
        halts: u32,
        runs: u32,
        pcsr: Vec<u32>,
        pcsr_err: bool,
        fail_reg: Option<u16>,
        log: Vec<&'static str>,
    }

    impl Fake {
        fn new(halted: bool) -> Self {
            Self { halted, halts: 0, runs: 0, pcsr: vec![0x0800_1234, 0x0800_1236], pcsr_err: false, fail_reg: None, log: vec![] }
        }
    }

    impl CoreAccess for Fake {
        fn halt(&mut self) -> Result<()> {
            self.halted = true;
            self.halts += 1;
            self.log.push("halt");
            Ok(())
        }
        fn run(&mut self) -> Result<()> {
            self.halted = false;
            self.runs += 1;
            self.log.push("run");
            Ok(())
        }
        fn read_word(&mut self, address: u64) -> Result<u32> {
            match address {
                DHCSR => Ok(if self.halted { S_HALT } else { 1 << 24 }),
                SCB_ICSR => Ok((1 << 22) | (15 << 12) | 3),
                DWT_PCSR if self.pcsr_err => anyhow::bail!("no such register"),
                DWT_PCSR => Ok(if self.pcsr.is_empty() { u32::MAX } else { self.pcsr.remove(0) }),
                _ => Ok(0xAAAA_0000),
            }
        }
        fn read_words(&mut self, address: u64, out: &mut [u32]) -> Result<()> {
            for (i, w) in out.iter_mut().enumerate() {
                *w = address as u32 + 4 * i as u32;
            }
            Ok(())
        }
        fn read_reg(&mut self, id: u16) -> Result<u32> {
            assert!(self.halted, "registers are only read halted");
            if self.fail_reg == Some(id) {
                anyhow::bail!("register read failed");
            }
            Ok(match id {
                20 => 0x0200_0001,
                n => 0x1000 + u32::from(n),
            })
        }
        fn pause(&mut self, _: Duration) {}
    }

    fn state_req(halt: bool, samples: u32) -> CoreStateRequest {
        CoreStateRequest { role: "dut".into(), halt, samples }
    }

    #[test]
    fn core_state_requests_are_checked_and_default_to_no_halt() {
        let r: CoreStateRequest = serde_json::from_str("{}").unwrap();
        assert_eq!((r.role.as_str(), r.halt, r.samples), ("dut", false, 8));
        assert!(check_core_state(&r).is_ok());
        assert!(check_core_state(&state_req(false, 0)).unwrap_err().contains("samples must be"));
        assert!(check_core_state(&state_req(true, MAX_SAMPLES + 1)).unwrap_err().contains("samples must be"));
        assert!(check_core_state(&state_req(false, MAX_SAMPLES)).is_ok());
    }

    #[test]
    fn default_mode_never_halts_and_samples_the_pc() {
        let mut core = Fake::new(false);
        let out = read_core_state(&mut core, &state_req(false, 3), |f| gate_halts("stm32g0b1ve", f)).unwrap();
        assert_eq!((core.halts, core.runs), (0, 0));
        assert_eq!((out.found, out.left), (RunState::Running, RunState::Running));
        assert_eq!(out.pc_samples, vec![Some(0x0800_1234), Some(0x0800_1236), None]);
        assert_eq!(out.dhcsr.state, "running");
        assert!(out.dhcsr.s_retire_st && out.dhcsr_after.is_some());
        // the gate's halt on an STM32G0 is the only one, and the response says so
        assert_eq!(out.halt_count, 1);
        assert!(out.registers.is_none() && out.scb_icsr.is_none());
        let nrf = read_core_state(&mut Fake::new(false), &state_req(false, 1), |f| gate_halts("nrf54l15", f)).unwrap();
        assert_eq!(nrf.halt_count, 0);
    }

    #[test]
    fn default_mode_on_a_halted_core_samples_nothing() {
        let mut core = Fake::new(true);
        let out = read_core_state(&mut core, &state_req(false, 4), |f| gate_halts("stm32g0b1ve", f)).unwrap();
        assert_eq!((out.found, out.left, out.halt_count), (RunState::Halted, RunState::Halted, 0));
        assert!(out.pc_samples.is_empty() && out.pc_note.unwrap().contains("halted"));
        assert_eq!(out.dhcsr.state, "halted");
    }

    #[test]
    fn a_part_without_pc_sampling_says_so() {
        let mut core = Fake::new(false);
        core.pcsr_err = true;
        let out = read_core_state(&mut core, &state_req(false, 4), |_| 0).unwrap();
        assert!(out.pc_samples.is_empty() && out.pc_note.unwrap().contains("DWT_PCSR"));
    }

    #[test]
    fn running_halted_let_run_counts_the_gate_and_the_read() {
        let mut core = Fake::new(false);
        let out = read_core_state(&mut core, &state_req(true, 1), |f| gate_halts("STM32G0B1VE", f)).unwrap();
        assert_eq!(core.log, vec!["halt", "run"]);
        assert_eq!((out.found, out.left), (RunState::Running, RunState::Running));
        assert_eq!(out.halt_count, 2);
        let regs = out.registers.unwrap();
        assert_eq!((regs.r[0], regs.r[12], regs.sp, regs.lr, regs.pc, regs.xpsr), (0x1000, 0x100C, 0x100D, 0x100E, 0x100F, 0x1010));
        assert_eq!((regs.control, regs.primask), (2, 1));
        let icsr = out.scb_icsr.unwrap();
        assert_eq!((icsr.vectactive, icsr.vectpending, icsr.isrpending), (3, 15, true));
    }

    #[test]
    fn found_halted_is_left_halted_without_a_resume() {
        let mut core = Fake::new(true);
        let out = read_core_state(&mut core, &state_req(true, 1), |f| gate_halts("stm32g0b1ve", f)).unwrap();
        assert_eq!((core.halts, core.runs), (0, 0));
        assert_eq!((out.found, out.left, out.halt_count, out.halted_us), (RunState::Halted, RunState::Halted, 0, 0));
        assert!(out.registers.is_some());
    }

    #[test]
    fn a_failed_register_read_still_lets_the_core_run() {
        let mut core = Fake::new(false);
        core.fail_reg = Some(5);
        let err = read_core_state(&mut core, &state_req(true, 1), |_| 0).unwrap_err();
        assert!(format!("{err:#}").contains("R5"));
        assert_eq!(core.log, vec!["halt", "run"]);
    }

    #[test]
    fn mem_read_names_what_it_found_and_left() {
        let r = req(&[(0x4002_103C, 2)]);
        let mut running = Fake::new(false);
        let out = read_ranges(&mut running, &r).unwrap();
        assert_eq!((out.found, out.left, out.halts), (RunState::Running, RunState::Running, 1));
        assert_eq!(out.ranges[0].words, vec![0x4002_103C, 0x4002_1040]);
        assert_eq!(running.log, vec!["halt", "run"]);
        let mut halted = Fake::new(true);
        let out = read_ranges(&mut halted, &r).unwrap();
        assert_eq!((out.found, out.left, out.halts, out.halted_us), (RunState::Halted, RunState::Halted, 0, 0));
        assert!(halted.log.is_empty());
    }

    #[test]
    fn mem_read_json_keeps_the_old_fields_and_adds_the_new() {
        let v = serde_json::to_value(MemReadResponse {
            role: "dut".into(),
            chip: "c".into(),
            probe_serial: "p".into(),
            ranges: vec![],
            halted: true,
            halted_us: 15_000,
            found: RunState::Running,
            left: RunState::Running,
            halt_count: 2,
        })
        .unwrap();
        assert_eq!(v["halted"], true);
        assert_eq!(v["halted_us"], 15_000);
        assert_eq!((v["found"].as_str(), v["left"].as_str(), v["halt_count"].as_u64()), (Some("running"), Some("running"), Some(2)));
    }
}
