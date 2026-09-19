//! The outpost mode pre-flight: read the running DUT's header frame, and
//! refuse to start a study whose declared trace mode it does not match.
//!
//! `embarch-study-designer`'s `Requirements.outpost` is the declaration;
//! this is the check. It runs **before dev-bench is told to do anything**,
//! which is the same ordering property the version gate states for itself —
//! a study that cannot be satisfied leaves a bench that never started a
//! step.
//!
//! # Why this needs no reset in the normal case
//!
//! The header frame is not only a power-on event. `embarch-outpost` emits
//! it at startup and then every `CONFIG_EMBARCH_OUTPOST_HEADER_INTERVAL_MS`
//! — 1000 by default — precisely so a host attaching mid-stream can decode
//! ([`embarch-outpost/interfaces/wire.md`]). So the pre-flight *listens*
//! first: on ordinary firmware a header is under a second away, and the DUT
//! is never rebooted for a question that answers itself.
//!
//! **`0` is a legal build**, and means the header is emitted once at
//! startup and never again. A study against that firmware would otherwise
//! be unverifiable — and worse, unverifiable exactly when it matters,
//! because a run that just reflashed has already sent its power-on header
//! before Core's port was open. So a listen that comes up empty is followed
//! by one reset, with the port already open, and a second listen for the
//! power-on header. One reboot, only in the case where it is the only way
//! to get an answer, and reported as having happened.
//!
//! # Two things that look incidental and are not
//!
//! **The input buffer is cleared, and a failure to clear is fatal here.**
//! `study.rs`'s capture clears too, and there a failure is a warning: a
//! stale prefix is a bad capture, not a reason to refuse to capture. Here
//! the same bytes are a *wrong answer*. A driver buffer holding a header
//! from before the reflash would satisfy a check about firmware that is no
//! longer on the board, which is the one outcome this whole mechanism
//! exists to prevent.
//!
//! **Only the flags byte is compared.** The header also carries a
//! `build_id`, and it is reported and logged — but it is
//! `<app describe>+op<module describe>+m<marker hash>` built from
//! `git describe --always --dirty --tags`, while a study's
//! `requires.firmware_version` is `embarch-core-client`'s own
//! `git describe --always --dirty --abbrev=8`. Those two strings do not
//! match for the same commit, and inventing a comparison rule between them
//! would be guessing at what an engineer meant. The version half stays
//! where it is (`Provenance`), and the build id is carried into the refusal
//! text so a human can read it.
//!
//! [`embarch-outpost/interfaces/wire.md`]: https://github.com/gabrieltetar/embarch-doc/blob/main/embarch-outpost/interfaces/wire.md

use embarch_study_designer::outpost::{self, Frame, HeaderFlags, OutpostHeader};
use embarch_study_designer::streams::{StreamEncoding, StreamSource, StreamTap};
use embarch_study_designer::study::OutpostModeRequirement;
use std::time::{Duration, Instant};

/// How long to listen for a repeating header before concluding there isn't
/// one.
///
/// Three times the 1000 ms default interval. Not tighter: the interval is a
/// Kconfig an application may raise, and the cost of waiting is a few
/// seconds at the start of a run, where the cost of giving up early is a
/// refusal for firmware that was about to answer.
const LISTEN_BUDGET: Duration = Duration::from_secs(3);

/// How long to wait for the power-on header after the reset.
///
/// Longer than [`LISTEN_BUDGET`] because it is waiting on a boot rather than
/// on a timer: the DUT has to come out of reset, bring its UART up and run
/// the outpost's own init before the first frame exists.
const POST_RESET_BUDGET: Duration = Duration::from_secs(6);

/// Per-read timeout, matching what `study.rs`'s capture opens its ports
/// with: short enough that the deadline above is honoured to within one
/// read, long enough not to spin.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// The largest zero-delimited run this will accumulate before dropping it,
/// mirroring `outpost_manifest::MAX_LIVE_FRAME_BYTES`' reasoning: a link
/// that stops delimiting must cost a frame, not memory.
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// What the pre-flight read off the wire.
#[derive(Debug, Clone)]
pub struct HeaderReading {
    pub flags: u8,
    pub build_id: String,
    pub outpost_version: String,
    pub record_layout_version: u8,
    /// Whether Core had to reset the DUT to get this header — true only for
    /// a firmware built with `CONFIG_EMBARCH_OUTPOST_HEADER_INTERVAL_MS=0`,
    /// and worth reporting because it means the run began with a reboot.
    pub after_reset: bool,
    pub signal_name: String,
    pub port_name: String,
}

/// The one outpost trace tap a mode requirement is checked against, and the
/// signal it names.
///
/// **First rather than only.** A study may declare more than one outpost
/// tap; they are all the same DUT's firmware, so any one of them answers
/// the question, and refusing a study for having two would be a rule with
/// no failure behind it.
pub fn trace_signal_name(streams: &[StreamTap]) -> Option<&str> {
    streams.iter().find_map(|tap| match (&tap.encoding, &tap.source) {
        (StreamEncoding::OutpostTrace, StreamSource::Signal { name }) => Some(name.as_str()),
        _ => None,
    })
}

/// Renders a flags byte as the names the declaration is written in, so a
/// refusal says `trace_self` rather than `0x80`.
///
/// Empty mask renders as `(none)` rather than an empty string: a message
/// reading "missing: " is a message that lost a word somewhere.
pub fn describe_flags(mask: u8) -> String {
    if mask == 0 {
        return "(none)".to_string();
    }
    let named: Vec<&str> = HeaderFlags::NAMED
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if named.is_empty() {
        // Every bit in the byte is named today, so this is unreachable
        // rather than merely unlikely — but a firmware that grows a ninth
        // flag makes it reachable, and "0x00" is a worse answer than the
        // hex of the bits nobody here can name.
        return format!("{mask:#04x}");
    }
    named.join(", ")
}

/// The refusal text for a header that does not satisfy a requirement.
///
/// **Both bytes are named**, not just the verdict: an engineer reading this
/// is deciding whether to change the study or rebuild the firmware, and
/// that decision needs what was asked for beside what is actually running.
pub fn describe_mismatch(req: &OutpostModeRequirement, reading: &HeaderReading) -> String {
    let (missing, present) = req.unmet(reading.flags);
    let mut parts = Vec::new();
    if missing != 0 {
        parts.push(format!("missing {}", describe_flags(missing)));
    }
    if present != 0 {
        parts.push(format!("has {} and must not", describe_flags(present)));
    }
    format!(
        "the DUT's outpost is not in the mode this study requires: {}. \
         The firmware on the board reports flags {:#04x} ({}); the study asks for \
         {:#04x} set ({}) and {:#04x} clear ({}). Its build id is '{}', outpost \
         {}, read from signal '{}' on {}{}. Rebuild the DUT with the hook families \
         this study needs, or change what it requires.",
        parts.join("; "),
        reading.flags,
        describe_flags(reading.flags),
        req.required_set,
        describe_flags(req.required_set),
        req.required_clear,
        describe_flags(req.required_clear),
        reading.build_id,
        reading.outpost_version,
        reading.signal_name,
        reading.port_name,
        if reading.after_reset { " after a reset" } else { "" },
    )
}

/// Opens the trace signal's own port, listens for a header, and — only if
/// none arrives — resets through `reset` and listens again.
///
/// Blocking throughout: it owns a serial port for its whole duration. The
/// caller runs it on a blocking thread and holds the hardware lock across
/// it, so nothing can flash the DUT between the header this reads and the
/// study that starts on the strength of it.
///
/// `reset` is passed in rather than performed here so the lock and the
/// enrollment lookup stay with the caller, which is the only place that has
/// `AppState`.
pub fn read_header(
    signal_name: &str,
    reset: impl FnOnce() -> Result<(), String>,
) -> Result<HeaderReading, String> {
    let link = embarch_topology::hardware::find_signal(signal_name)
        .map_err(|e| format!("couldn't read the declared route for '{signal_name}': {e:?}"))?
        .ok_or_else(|| {
            format!(
                "signal '{signal_name}' has no declared route, so there is no port to read the \
                 outpost's header frame from (declare it with POST /signals)"
            )
        })?;

    // A `ViaDevBench` trace arrives relayed over the dev-bench link, which
    // is not carrying stream bytes until `StudyStart` has gone out — and
    // going out is exactly what this check has to happen before. Refused
    // rather than skipped: a mode requirement that silently did not run
    // would be worse than one that cannot.
    if matches!(link.route, embarch_topology::hardware::Route::ViaDevBench { .. }) {
        return Err(format!(
            "signal '{signal_name}' is routed via dev-bench, so its header frame only arrives \
             once the study has started — which is after this check has to run. Route the \
             outpost to its own port (POST /signals) to declare an outpost mode requirement."
        ));
    }

    let port = embarch_topology::hardware::resolve_signal_port(signal_name)
        .map_err(|e| format!("couldn't resolve a carrier for signal '{signal_name}': {e:?}"))?;
    let baud = crate::stream_store::signal_baud();
    let mut serial = serialport::new(&port.port_name, baud)
        .timeout(READ_TIMEOUT)
        .open()
        .map_err(|e| {
            format!(
                "failed to open signal '{signal_name}' on {} at {baud} baud to read the \
                 outpost's header frame: {e:?}",
                port.port_name
            )
        })?;

    // See this module's doc comment: fatal here, a warning in the capture,
    // and the difference is that a stale header is a wrong answer rather
    // than a bad first row.
    serial.clear(serialport::ClearBuffer::Input).map_err(|e| {
        format!(
            "couldn't clear buffered input on {} before reading the outpost's header frame: \
             {e:?} — the bytes already in the driver may predate the firmware now on the \
             board, and a header from the previous image would answer this check wrongly",
            port.port_name
        )
    })?;

    let reading = |header: OutpostHeader, after_reset: bool| HeaderReading {
        flags: header.flags,
        build_id: header.build_id.as_str().to_string(),
        outpost_version: header.outpost_version.as_str().to_string(),
        record_layout_version: header.record_layout_version,
        after_reset,
        signal_name: signal_name.to_string(),
        port_name: port.port_name.clone(),
    };

    if let Some(header) = listen(&mut *serial, LISTEN_BUDGET)? {
        return Ok(reading(header, false));
    }

    tracing::info!(
        signal = signal_name,
        port = port.port_name,
        "no outpost header in {LISTEN_BUDGET:?}; resetting the DUT to get its power-on one \
         (a build with CONFIG_EMBARCH_OUTPOST_HEADER_INTERVAL_MS=0 emits it only at startup)"
    );
    reset()?;

    match listen(&mut *serial, POST_RESET_BUDGET)? {
        Some(header) => Ok(reading(header, true)),
        None => Err(format!(
            "no outpost header frame arrived on signal '{signal_name}' ({}) within {:?}, nor \
             within {:?} of resetting the DUT. Either the firmware on the board has no outpost \
             compiled in, or it is not transmitting on this port — so this study's outpost mode \
             requirement cannot be checked, and it will not be run unchecked.",
            port.port_name, LISTEN_BUDGET, POST_RESET_BUDGET
        )),
    }
}

/// Reads until a header frame decodes or `budget` runs out.
///
/// Frames that are not headers, and frames that fail their CRC, are skipped
/// — a bad frame costs itself and nothing else, the same rule the live
/// decoder and the post-hoc render both hold. A read error that is not a
/// timeout ends the listen: the port has gone, and waiting out the budget
/// on a dead port would turn a clear failure into a slow one.
fn listen(
    port: &mut dyn serialport::SerialPort,
    budget: Duration,
) -> Result<Option<OutpostHeader>, String> {
    let deadline = Instant::now() + budget;
    let mut buf = [0u8; 1024];
    let mut pending: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; 4096];

    while Instant::now() < deadline {
        let n = match port.read(&mut buf) {
            Ok(n) => n,
            // A debug UART with nothing on it is the normal state, not an
            // error.
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(format!("reading the outpost's port failed: {e:?}")),
        };

        for byte in &buf[..n] {
            if *byte != 0 {
                if pending.len() < MAX_FRAME_BYTES {
                    pending.push(*byte);
                }
                continue;
            }
            if pending.is_empty() {
                continue;
            }
            let chunk = std::mem::take(&mut pending);
            if chunk.len() > scratch.len() {
                scratch.resize(chunk.len() * 2, 0);
            }
            if let Ok(Frame::Header { header, .. }) = outpost::decode_frame(&chunk, &mut scratch) {
                return Ok(Some(header));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::streams::{StreamScope, StreamSource};
    use heapless::String as HString;

    fn tap(id: u8, encoding: StreamEncoding, signal: &str) -> StreamTap {
        StreamTap {
            id,
            name: HString::try_from("t").unwrap(),
            source: StreamSource::Signal { name: HString::try_from(signal).unwrap() },
            encoding,
            scope: StreamScope::WholeStudy,
        }
    }

    #[test]
    fn the_trace_tap_is_found_among_others() {
        let streams = vec![
            tap(0, StreamEncoding::Text, "console"),
            tap(1, StreamEncoding::OutpostTrace, "outpost"),
        ];
        assert_eq!(trace_signal_name(&streams), Some("outpost"));
        assert_eq!(trace_signal_name(&streams[..1]), None);
        assert_eq!(trace_signal_name(&[]), None);
    }

    /// An outpost-encoded tap whose source is not a `Signal` names no
    /// signal at all, so there is nothing for this to return. (A `Signal`
    /// tap whose *route* goes via dev-bench is a different case, judged in
    /// `read_header` where the route is known.)
    #[test]
    fn a_tap_that_is_not_a_signal_is_not_a_trace_signal() {
        let mut t = tap(0, StreamEncoding::OutpostTrace, "outpost");
        t.source = StreamSource::DevBenchLog;
        assert_eq!(trace_signal_name(&[t]), None);
    }

    #[test]
    fn flags_render_as_the_names_a_declaration_is_written_in() {
        assert_eq!(describe_flags(0), "(none)");
        assert_eq!(describe_flags(HeaderFlags::TRACE_SELF), "trace_self");
        assert_eq!(
            describe_flags(HeaderFlags::TRACE_THREADS | HeaderFlags::TRACE_ISRS),
            "trace_threads, trace_isrs"
        );
        // Bit order, not the order they were OR'd in.
        assert_eq!(
            describe_flags(HeaderFlags::TRACE_SELF | HeaderFlags::TRACE_THREADS),
            "trace_threads, trace_self"
        );
    }

    fn reading(flags: u8) -> HeaderReading {
        HeaderReading {
            flags,
            build_id: "v1.2-3-gdeadbeef+opv0.4+mabcd1234".to_string(),
            outpost_version: "v0.4".to_string(),
            record_layout_version: 3,
            after_reset: false,
            signal_name: "outpost".to_string(),
            port_name: "COM9".to_string(),
        }
    }

    /// The refusal has to carry enough for the next decision — change the
    /// study, or rebuild the firmware — which means both bytes and the
    /// build id, not a verdict.
    #[test]
    fn a_mismatch_names_what_is_missing_and_what_must_not_be_there() {
        let req = OutpostModeRequirement {
            required_set: HeaderFlags::TRACE_MARKERS,
            required_clear: HeaderFlags::TRACE_SELF,
        };
        let msg = describe_mismatch(&req, &reading(HeaderFlags::TRACE_SELF));
        assert!(msg.contains("missing trace_markers"), "{msg}");
        assert!(msg.contains("has trace_self and must not"), "{msg}");
        assert!(msg.contains("v1.2-3-gdeadbeef+opv0.4+mabcd1234"), "{msg}");
        assert!(msg.contains("COM9"), "{msg}");
        assert!(msg.contains("0x80"), "{msg}");
    }

    #[test]
    fn a_mismatch_in_only_one_direction_says_only_that() {
        let req = OutpostModeRequirement {
            required_set: HeaderFlags::TRACE_MARKERS,
            required_clear: 0,
        };
        let msg = describe_mismatch(&req, &reading(0));
        assert!(msg.contains("missing trace_markers"), "{msg}");
        assert!(!msg.contains("must not"), "{msg}");
    }

    #[test]
    fn a_reset_is_reported_in_the_refusal_because_the_run_began_with_a_reboot() {
        let req = OutpostModeRequirement { required_set: HeaderFlags::TRACE_GPIO, required_clear: 0 };
        let mut r = reading(0);
        r.after_reset = true;
        assert!(describe_mismatch(&req, &r).contains("after a reset"));
        r.after_reset = false;
        assert!(!describe_mismatch(&req, &r).contains("after a reset"));
    }
}
