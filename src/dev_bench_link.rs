//! The serial transport `study.rs` runs the `DevBenchMessage` protocol over.
//!
//! One `DevBenchLink` is opened per `POST /study` (open-per-study, not a
//! persistent background connection — `embarch-study-designer/design.md`'s
//! own language, "`Hello`... sent by Core when it opens the serial port,
//! before any `Study` traffic", and this suite's "don't build machinery
//! nothing needs yet" posture both point at this being simpler and safer
//! than a long-lived connection with its own separate lifecycle to manage).
//!
//! Blocking, like `serial.rs`'s `read_log` — every method here does real I/O
//! and must be called from `tokio::task::spawn_blocking`, never directly from
//! an async context.

use anyhow::{Context, Result};
use embarch_study_designer::DevBenchMessage;
use serialport::SerialPort;
use std::io::{Read, Write};
use std::time::Instant;

/// The Core↔dev-bench UART link runs at 1 Mbaud
/// (`embarch-study-designer/design.md` §3 decision 25) — a fact that
/// document states about dev-bench firmware's own UART configuration, not a
/// choice made here.
pub const DEV_BENCH_BAUD: u32 = 1_000_000;

/// Per-read timeout passed to `serialport`. Bounded so [`DevBenchLink::recv`]
/// can poll in a loop against a caller-supplied deadline (`study.rs`'s
/// host-side watchdog) rather than blocking forever on one read — the same
/// pattern `serial.rs`'s `read_log` already uses.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// How much of an unframed tail rides in a study's failure reason. The full
/// text goes to the bench's own debug file instead — see
/// [`DevBenchLink::unframed_tail_full`] for why the two differ.
const UNFRAMED_TAIL_REASON_CAP: usize = 192;

/// One open serial connection to `embarch-dev-bench`, speaking
/// COBS-framed/postcard-encoded `DevBenchMessage`s one at a time.
pub struct DevBenchLink {
    port: Box<dyn SerialPort>,
    /// Bytes read off the wire but not yet consumed into a complete,
    /// delimited frame — a blocking serial read can return partial data (or
    /// more than one frame's worth at once), so this carries the remainder
    /// across calls to [`Self::recv`].
    buf: Vec<u8>,
    /// How many empty frames (a bare `0x00` with nothing before it) this
    /// link has skipped — see [`Self::recv`] for what produces one. Counted
    /// rather than only logged so a link that is quietly resyncing over and
    /// over is visible as a number, not as a pattern someone has to notice
    /// in a log.
    empty_frames: u64,
    /// How many frames arrived and would not decode. Counted for the same
    /// reason as `empty_frames`, and read by the caller to decide when a link
    /// has stopped being worth reading — see [`Received::Undecodable`].
    undecodable_frames: u64,
}

/// What one [`DevBenchLink::recv`] produced.
///
/// Three outcomes rather than `Result<Option<_>>`'s two, because **an
/// undecodable frame is not a dead link and treating it as one cost a real
/// diagnosis** (`embarch-dev-bench/design.md` §4). A `StepResult` carrying a
/// step's failure reason arrived short of its own declared length; Core
/// refused it, tore the link down, and reported the study as a *transport*
/// error that never mentioned a step had failed. The one message whose job
/// was to explain a failure was the one message that could kill the link.
///
/// So the posture here is the suite's posture everywhere else — an
/// undecodable frame costs *the frame*, not the link, which is exactly why an
/// outpost frame carries its own CRC ([`embarch-outpost/design.md`] §3
/// decision 5). The frames after it are still worth having: dev-bench's own
/// account of what went wrong arrives as ordinary `LogLine`s, and a
/// `StudyDone` still says whether the run ended on its own terms.
///
/// `DevBenchMessage` is a ~2 KB `no_std` type sized by its largest variant, so
/// clippy would rather see it boxed here. **It is deliberately not.** The
/// signature this replaced was `Result<Option<DevBenchMessage>>`, which carries
/// exactly the same 2 KB by value, so boxing would not remove a cost — it would
/// add a heap allocation per received frame, on the path `StreamChunkBatch`
/// traffic runs through at full link rate. The value is constructed and matched
/// out in the same breath; it never lives in a collection.
#[allow(clippy::large_enum_variant)]
pub enum Received {
    /// A frame arrived and decoded.
    Message(DevBenchMessage),
    /// A frame arrived and did not decode; the string is
    /// [`DevBenchLink::describe_undecodable_frame`]'s account of it, ready to
    /// log. The link is still live and the caller should keep reading.
    Undecodable(String),
    /// The deadline passed with no complete frame buffered. The caller's
    /// watchdog case, not an error.
    Deadline,
}

impl DevBenchLink {
    /// Opens `port_name` at [`DEV_BENCH_BAUD`]. Does not send `Hello` or do
    /// anything else protocol-level — that's `study.rs`'s job.
    pub fn open(port_name: &str) -> Result<Self> {
        let port = serialport::new(port_name, DEV_BENCH_BAUD)
            .timeout(READ_TIMEOUT)
            .open()
            .with_context(|| format!("failed to open dev-bench serial port '{port_name}'"))?;
        Ok(Self { port, buf: Vec::new(), empty_frames: 0, undecodable_frames: 0 })
    }

    /// How many frames this link has read that would not decode. Read by
    /// `study.rs` to stop reading a link that has turned to noise, rather
    /// than looping on garbage until the step deadline.
    pub fn undecodable_frames(&self) -> u64 {
        self.undecodable_frames
    }

    /// Bytes that arrived and never became a frame, rendered as hex and
    /// ASCII — or `None` when there are none.
    ///
    /// **These were completely invisible, and they are the one thing on this
    /// link that explains a bench that stopped talking.** `recv` buffers until
    /// a `0x00` delimiter, so anything arriving without one accumulates in
    /// `buf` forever and is never logged, never counted, never reported. That
    /// is precisely the shape of the evidence that matters here: dev-bench's
    /// `zephyr,console` *is* this UART, and an ESP32 that resets puts its ROM
    /// and bootloader banner on it as plain ASCII at a different baud —
    /// text, no nulls anywhere in it. So the run that finally proved dev-bench
    /// was rebooting mid-frame
    /// (`embarch-dev-bench/design.md` §4) reported "no message received from
    /// dev-bench before the deadline" while holding the bench's own account of
    /// the reset in a private `Vec`. The uptime comparison that cracked it
    /// took another handshake to do what these bytes could have said outright.
    ///
    /// Rendered rather than returned raw so the caller cannot accidentally
    /// treat it as protocol data, and capped for the same reason
    /// `describe_undecodable_frame` caps: a link that has been quietly
    /// filling this for a whole study should not put a kilobyte in one log
    /// line.
    pub fn unframed_tail(&self) -> Option<String> {
        Self::describe_unframed_tail(&self.buf, UNFRAMED_TAIL_REASON_CAP)
    }

    /// The same text with nothing elided, for the study's own `dev-bench`
    /// file. **The cap and the absence of one are both deliberate**: a
    /// failure reason travels through an HTTP response and a job registry and
    /// must stay a sentence, while the bench's debug file is exactly where the
    /// whole banner belongs — and the line naming the reset cause can sit
    /// several hundred bytes into it, past any cap a reason could carry.
    pub fn unframed_tail_full(&self) -> Option<String> {
        Self::describe_unframed_tail(&self.buf, usize::MAX)
    }

    /// Pure, and separate from [`Self::unframed_tail`], for the same reason
    /// `describe_undecodable_frame` and `take_frame` are: testable with no
    /// serial port in the way.
    fn describe_unframed_tail(buf: &[u8], cap: usize) -> Option<String> {
        if buf.is_empty() {
            return None;
        }
        let head: Vec<u8> = buf.iter().take(cap).copied().collect();
        let hex: Vec<String> = head.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = head
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        Some(format!(
            "{} byte(s) arrived without a frame delimiter; first {}: {} | {ascii}",
            buf.len(),
            head.len(),
            hex.join(" ")
        ))
    }

    /// Postcard-encodes `msg`, COBS-frames it (trailing `0x00` delimiter
    /// included), and writes it out in full.
    pub fn send(&mut self, msg: &DevBenchMessage) -> Result<()> {
        let framed = postcard::to_stdvec_cobs(msg).context("failed to encode DevBenchMessage")?;
        self.port
            .write_all(&framed)
            .context("failed to write to dev-bench serial port")?;
        Ok(())
    }

    /// What to say when a frame arrives and will not decode.
    ///
    /// **"failed to decode DevBenchMessage" on its own is close to useless,
    /// and this was found the hard way.** A postcard error of "Hit the end of
    /// buffer, expected more data" tells you a frame was short of what the
    /// type needs — and says nothing about *which* message, how big it was,
    /// or how far the two ends disagree. Diagnosing one cost a session of
    /// isolating studies step by step to find out which action produced it.
    /// The frame's own first byte is the variant index, which is the single
    /// most useful fact available at this point and is free.
    ///
    /// A prefix rather than the whole frame: a `StepResult` carrying a full
    /// GATT table runs to kilobytes, and a log line that long is its own
    /// problem. **Raised from 24 bytes to 192 on 2026-08-27, because 24 was
    /// short of the one thing that mattered.** A real failure arrived as an
    /// 88-byte frame whose last variable-length field was a 64-character
    /// `Fail` reason — the actual explanation of why a study step failed, sat
    /// inside the frame Core was holding, and the log threw it away after the
    /// first byte of it. 192 bytes covers a `StepResult` with a full-length
    /// reason and its trailing options with room to spare, and still refuses
    /// to print a kilobyte of GATT table.
    ///
    /// **The ASCII column is not decoration.** The fields that identify a
    /// frame are two strings — the step name and the fail reason — and
    /// reading either out of hex by hand is exactly the friction that made a
    /// truncated frame take three studies to characterise.
    ///
    /// Pure, and separate from `recv`, so the message is testable without a
    /// serial port.
    fn describe_undecodable_frame(head: &[u8], framed_len: usize) -> String {
        let tag = match head.first() {
            // The COBS code byte comes first, so the variant index is the
            // *second* byte of the framed bytes — index 1, not 0. Getting
            // this wrong would name a plausible wrong variant, which is
            // worse than naming none.
            Some(_) => head.get(1).copied(),
            None => None,
        };
        let tag_name = tag.map(Self::describe_tag).unwrap_or("(frame too short to hold a tag)");
        let hex: Vec<String> = head.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = head
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        // What the COBS code byte claims against what actually turned up. A
        // frame short of its own declared length is a *transport* fault, and
        // it reads identically to a field-layout disagreement unless the two
        // numbers are put side by side -- which is how a flat 16-byte
        // truncation was first misread as an encoder omitting trailing
        // fields (`embarch-dev-bench/design.md` §4).
        let claim = match head.first() {
            // COBS: the code byte is 1 + the count of non-zero bytes that
            // follow, so the block it opens should run to that many bytes --
            // the code byte itself included, the `0x00` delimiter *not*.
            //
            // **`framed_len` counts the delimiter and the block does not,
            // which is a one-byte lie worth not telling.** `take_frame`
            // drains `..=pos`, so every length that reaches here is one more
            // than the block's. Reporting "SHORT BY 12" for a block that lost
            // thirteen bytes is the kind of number someone lines up against a
            // field layout, and being out by one there is how an afternoon
            // goes missing.
            Some(&code) if code > 0 => {
                let expected = usize::from(code);
                let block_len = framed_len.saturating_sub(1);
                if expected > block_len {
                    format!(
                        ", COBS code byte claims a {expected}-byte block and {block_len} \
                         arrived (plus the delimiter) -- SHORT BY {}",
                        expected - block_len
                    )
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };
        format!(
            "failed to decode DevBenchMessage: {framed_len} COBS-framed bytes, \
             variant index {} ({tag_name}){claim}, first {} bytes {} | {ascii}",
            tag.map(|t| t.to_string()).unwrap_or_else(|| "?".to_string()),
            head.len(),
            hex.join(" ")
        )
    }

    /// `DevBenchMessage`'s variant index -> its name. Hand-maintained
    /// against `embarch-study-designer`'s `protocol.rs`, in the same
    /// append-only order postcard encodes positionally — an unknown index is
    /// named as unknown rather than guessed at, which is itself a useful
    /// finding (a bench newer than this Core).
    fn describe_tag(tag: u8) -> &'static str {
        match tag {
            0 => "Hello",
            1 => "HelloAck",
            2 => "StreamOpen",
            3 => "StreamChunkBatch",
            4 => "StreamClose",
            5 => "LogLine",
            6 => "StudyStart",
            7 => "StepResult",
            8 => "StudyDone",
            _ => "unknown variant — a bench newer than this Core?",
        }
    }

    /// Splits the next complete `0x00`-delimited frame off the front of
    /// `buf`, skipping any *empty* ones and counting them into
    /// `empty_frames`. Returns `None` when no complete frame is buffered yet.
    ///
    /// **An empty segment — a `0x00` with nothing before it — is not a frame
    /// that failed to decode, it is the absence of one.** COBS never encodes
    /// a message to zero bytes, so no sender can legitimately produce this.
    /// What does produce it is the wire going quiet mid-stream: a bench that
    /// reset (the line idles, and the first thing the driver hands back is a
    /// null), or a delimiter arriving twice across a resync.
    ///
    /// Skipping it rather than failing the study is what lets the *next*
    /// frames through, and those are the ones worth having — a bench that
    /// just rebooted sends its panic dump and its boot record as ordinary
    /// `LogLine`s. Treating this as a fatal decode error tore the link down
    /// before any of that could arrive, which is precisely how a firmware
    /// crash spent a session presenting as a protocol bug.
    ///
    /// Pure, and separate from `recv`, so the framing is testable without a
    /// serial port — the same reason `describe_undecodable_frame` is.
    fn take_frame(buf: &mut Vec<u8>, empty_frames: &mut u64) -> Option<Vec<u8>> {
        loop {
            let pos = buf.iter().position(|&b| b == 0)?;

            if pos == 0 {
                buf.drain(..=pos);
                *empty_frames += 1;
                tracing::debug!(
                    empty_frames = *empty_frames,
                    "skipped an empty frame on the dev-bench link (a stray delimiter — \
                     usually a bench that reset mid-stream)"
                );
                continue;
            }
            return Some(buf.drain(..=pos).collect());
        }
    }

    /// Reads and decodes one `DevBenchMessage`, blocking (via bounded,
    /// polled reads) until either a complete `0x00`-delimited frame arrives
    /// or `deadline` passes.
    ///
    /// `Err` means the *connection* failed — a read error on the port, and
    /// nothing else. A frame that arrived and would not decode comes back as
    /// [`Received::Undecodable`]; see that variant for why it is not an
    /// error.
    pub fn recv(&mut self, deadline: Instant) -> Result<Received> {
        loop {
            if let Some(mut frame) = Self::take_frame(&mut self.buf, &mut self.empty_frames) {
                // Kept for the error path below: `from_bytes_cobs` decodes
                // in place, so by the time it fails `frame` no longer holds
                // what arrived on the wire.
                let framed_len = frame.len();
                let head: Vec<u8> = frame.iter().take(192).copied().collect();
                return match postcard::from_bytes_cobs(&mut frame) {
                    Ok(msg) => Ok(Received::Message(msg)),
                    Err(_) => {
                        self.undecodable_frames += 1;
                        Ok(Received::Undecodable(Self::describe_undecodable_frame(
                            &head, framed_len,
                        )))
                    }
                };
            }

            if Instant::now() >= deadline {
                return Ok(Received::Deadline);
            }

            let mut chunk = [0u8; 256];
            match self.port.read(&mut chunk) {
                Ok(0) => {}
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e).context("error reading from dev-bench serial port"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::{StreamRecord, DEV_BENCH_WIRE_SCHEMA_VERSION};

    /// The failure this whole path exists for, at the framing layer: a bench
    /// that resets mid-stream leaves a bare delimiter behind, and the frames
    /// *after* it — the panic dump and boot record that say why it reset —
    /// are the ones worth reading. Before this, the stray null was a fatal
    /// decode error and took the link down before any of them arrived.
    #[test]
    fn a_stray_delimiter_is_skipped_and_the_next_frame_still_arrives() {
        let log = DevBenchMessage::LogLine {
            text: heapless::String::try_from("<err> os: E_CPU_EXCEPTION").unwrap(),
        };
        let mut buf: Vec<u8> = vec![0x00];
        buf.extend(postcard::to_stdvec_cobs(&log).unwrap());

        let mut empty = 0u64;
        let mut frame = DevBenchLink::take_frame(&mut buf, &mut empty)
            .expect("the frame after the stray delimiter must still be readable");
        assert_eq!(empty, 1, "the skipped delimiter must be counted");

        let decoded: DevBenchMessage = postcard::from_bytes_cobs(&mut frame).unwrap();
        match decoded {
            DevBenchMessage::LogLine { text } => {
                assert_eq!(text.as_str(), "<err> os: E_CPU_EXCEPTION")
            }
            other => panic!("expected the LogLine back, got {other:?}"),
        }
    }

    /// A run of them — what a line that stays idle for a while actually
    /// produces — collapses to one skip per null, not to a lost frame.
    #[test]
    fn a_run_of_stray_delimiters_is_skipped_as_a_run() {
        let log = DevBenchMessage::LogLine { text: heapless::String::try_from("up").unwrap() };
        let mut buf: Vec<u8> = vec![0x00, 0x00, 0x00];
        buf.extend(postcard::to_stdvec_cobs(&log).unwrap());

        let mut empty = 0u64;
        assert!(DevBenchLink::take_frame(&mut buf, &mut empty).is_some());
        assert_eq!(empty, 3);
    }

    /// The other half: with nothing but nulls buffered there is no frame yet,
    /// and `take_frame` must say so rather than hand back an empty one — a
    /// `recv` that got `Some(vec![])` here would try to decode it and fail
    /// the study, which is the bug this fixes wearing a different hat.
    #[test]
    fn nothing_but_delimiters_yields_no_frame() {
        let mut buf: Vec<u8> = vec![0x00, 0x00];
        let mut empty = 0u64;

        assert!(DevBenchLink::take_frame(&mut buf, &mut empty).is_none());
        assert_eq!(empty, 2);
        assert!(buf.is_empty(), "every consumed null must be drained");
    }

    /// The one thing the old bare "failed to decode DevBenchMessage" could
    /// not tell you: which message, and how big. Pinned because the *offset*
    /// of the variant index is the part that is easy to get wrong — the COBS
    /// code byte comes first, so it is byte 1 and not byte 0, and naming a
    /// plausible wrong variant would be worse than naming none.
    #[test]
    fn an_undecodable_frame_names_its_variant_and_its_size() {
        // A real COBS-framed `StepResult` (variant 7): code byte, then the
        // tag, then the rest.
        let framed = [0x0du8, 0x07, 0x01, 0x09, 0x61, 0x64, 0x76];
        let msg = DevBenchLink::describe_undecodable_frame(&framed, 23);
        assert!(msg.contains("23 COBS-framed bytes"), "{msg}");
        assert!(msg.contains("variant index 7"), "{msg}");
        assert!(msg.contains("StepResult"), "{msg}");
        assert!(msg.contains("0d 07 01 09"), "the hex prefix is the point: {msg}");
    }

    /// **A frame short of its own declared length says so, and says by how
    /// much.** This is the fault that cost three studies to characterise: a
    /// `StepResult` arriving short of what its COBS code byte promised, which
    /// reads identically to a field-layout disagreement until the two numbers
    /// are put side by side (`embarch-dev-bench/design.md` §4).
    ///
    /// **The shortfall is counted against the block, not the frame.** The
    /// numbers reaching this function include the `0x00` delimiter and the
    /// code byte's claim does not, so a naive subtraction is out by one — and
    /// this is a number people line up against a field layout by hand.
    #[test]
    fn a_frame_shorter_than_its_cobs_code_claims_reports_the_shortfall() {
        // Code byte 0x58 = 88, so the block should run to 88 bytes, the code
        // byte included and the delimiter excluded. Hand it the 72 the bench
        // did -- 71 of the block, plus the delimiter.
        let mut framed = vec![0x58u8, 0x07, 0x05, 0x12];
        framed.extend_from_slice(b"nus-sensor01-start");
        let msg = DevBenchLink::describe_undecodable_frame(&framed, 72);
        assert!(msg.contains("SHORT BY 17"), "{msg}");
        assert!(msg.contains("claims a 88-byte block and 71 arrived"), "{msg}");
        // And the strings that identify the frame are readable without
        // decoding hex by hand.
        assert!(msg.contains("nus-sensor01-start"), "{msg}");
    }

    /// The exact frame from the run that finally read its own failure reason
    /// (2026-08-27, study `61e3b5a0`), pinned end to end: the block claims 81
    /// bytes, 68 of it arrived, and the thirteen bytes missing are the tail of
    /// the reason string. **13, not the 12 a delimiter-inclusive subtraction
    /// gives** — and the difference matters because the field layout this gets
    /// compared against is exact: variant (1) + step index (1) + name length
    /// (1) + `nus-sensor01-start` (18) + `Fail` (1) + reason length (1) +
    /// reason (57) = 80 data bytes, which is precisely the 81-byte block the
    /// code byte claims. The encoder wrote all of it; the wire lost the end.
    #[test]
    fn the_real_truncated_step_result_reports_thirteen_missing_bytes() {
        let mut framed = vec![0x51u8, 0x07, 0x05, 0x12];
        framed.extend_from_slice(b"nus-sensor01-start");
        framed.extend_from_slice(&[0x01, 0x39]);
        framed.extend_from_slice(b"disconnected during write (HCI 0x08, supervi");
        framed.push(0x00);
        assert_eq!(framed.len(), 69, "this is the frame as it arrived");

        let msg = DevBenchLink::describe_undecodable_frame(&framed, framed.len());
        assert!(msg.contains("SHORT BY 13"), "{msg}");
        // The whole point of the 192-byte dump and the ASCII column: the
        // reason is readable without decoding anything by hand.
        assert!(msg.contains("disconnected during write (HCI 0x08, supervi"), "{msg}");
    }

    /// A frame that is not short must not claim a shortfall — the absence of
    /// the phrase is what makes its presence meaningful.
    ///
    /// **The delimiter is part of the fixture, and it was not before.** Every
    /// length that reaches `describe_undecodable_frame` comes from
    /// `take_frame`, which drains through the `0x00`; a fixture without one
    /// is a frame that cannot occur, and this test passed on the old
    /// arithmetic only because both sides were out by the same byte.
    #[test]
    fn a_complete_frame_reports_no_shortfall() {
        let framed = vec![0x04u8, 0x01, 0x02, 0x03, 0x00];
        let msg = DevBenchLink::describe_undecodable_frame(&framed, framed.len());
        assert!(!msg.contains("SHORT BY"), "{msg}");
    }

    #[test]
    fn a_frame_too_short_to_hold_a_tag_says_so_rather_than_guessing() {
        let msg = DevBenchLink::describe_undecodable_frame(&[0x01], 1);
        assert!(msg.contains("frame too short"), "{msg}");
        assert!(!msg.contains("Hello"), "must not name a variant it cannot read: {msg}");
    }

    /// **An unframed tail is reported, because it is what a reset looks like
    /// on this wire.** dev-bench's console is this same UART, so an ESP32 that
    /// reboots writes its bootloader banner here as plain ASCII with no `0x00`
    /// anywhere in it — which `recv` buffers and, before this, never mentioned.
    #[test]
    fn bytes_that_never_form_a_frame_are_reported_with_their_text() {
        let banner = b"rst:0xc (RTC_SW_CPU_RST),boot:0x8";
        let tail = DevBenchLink::describe_unframed_tail(banner, UNFRAMED_TAIL_REASON_CAP)
            .expect("a non-empty buffer must be reported");
        assert_eq!(
            banner.iter().position(|&b| b == 0),
            None,
            "the fixture must have no delimiter — that is the whole reason these bytes hide"
        );
        assert!(tail.contains("33 byte(s)"), "{tail}");
        // The ASCII column is the whole point: this is the line that names the
        // reset, and nobody should have to decode it out of hex.
        assert!(tail.contains("rst:0xc (RTC_SW_CPU_RST)"), "{tail}");

        assert!(
            DevBenchLink::describe_unframed_tail(&[], UNFRAMED_TAIL_REASON_CAP).is_none(),
            "an empty buffer reports nothing"
        );
    }

    /// **The reason elides and the debug file does not**, and the line naming
    /// a reset cause is exactly the kind that sits past the cap: the real
    /// capture that proved this fault was 699 bytes whose first 192 were still
    /// inside the bootloader's SPI-flash preamble.
    #[test]
    fn the_uncapped_tail_keeps_what_the_reason_elides() {
        let mut banner = b"I (soc_init): ESP Simple boot\r\n".repeat(8);
        banner.extend_from_slice(b"rst:0xc (RTC_SW_CPU_RST)");
        assert!(banner.len() > UNFRAMED_TAIL_REASON_CAP);

        let capped = DevBenchLink::describe_unframed_tail(&banner, UNFRAMED_TAIL_REASON_CAP)
            .expect("reported");
        let full = DevBenchLink::describe_unframed_tail(&banner, usize::MAX).expect("reported");

        assert!(!capped.contains("RTC_SW_CPU_RST"), "the cap must actually elide: {capped}");
        assert!(full.contains("rst:0xc (RTC_SW_CPU_RST)"), "{full}");
        // Both still state the true total, so a capped reason never reads as
        // the whole of what arrived.
        let total = format!("{} byte(s)", banner.len());
        assert!(capped.contains(&total) && full.contains(&total));
    }

    /// A bench flashed from a newer `main` than this Core is a real and
    /// recurring situation in this suite (`embarch-dev-workflow.md` §4a's
    /// coupling 1), so an out-of-range variant index gets a message that
    /// points at it instead of at the bytes.
    #[test]
    fn an_unknown_variant_index_points_at_a_version_skew() {
        let msg = DevBenchLink::describe_undecodable_frame(&[0x02, 0x63], 9);
        assert!(msg.contains("newer than this Core"), "{msg}");
    }

    /// The framing/encoding round trip this module is responsible for —
    /// exercised directly against `postcard`'s COBS helpers, with no serial
    /// port involved, mirroring what `DevBenchLink::send`/`recv` do
    /// internally.
    fn round_trip(msg: &DevBenchMessage) {
        let mut framed = postcard::to_stdvec_cobs(msg).unwrap();
        let decoded: DevBenchMessage = postcard::from_bytes_cobs(&mut framed).unwrap();
        assert_eq!(*msg, decoded);
    }

    #[test]
    fn hello_round_trips() {
        round_trip(&DevBenchMessage::Hello {
            schema_version: DEV_BENCH_WIRE_SCHEMA_VERSION,
            host_utc_ms: 1_753_000_000_000,
        });
    }

    #[test]
    fn hello_ack_round_trips() {
        round_trip(&DevBenchMessage::HelloAck {
            schema_version: DEV_BENCH_WIRE_SCHEMA_VERSION,
            compatible: true,
            firmware_version: heapless::String::try_from("nrf54l15dk-g1a2b3c").unwrap(),
            hardware_id: heapless::String::try_from("aaaaaaaabbbbbbbb").unwrap(),
        });
        // A bench whose build has no `hwinfo` driver — the empty ID has to
        // survive Core's own COBS+postcard round trip too, not just the
        // crate's (schema v10, `embarch-study-designer/design.md` §3
        // decision 47).
        round_trip(&DevBenchMessage::HelloAck {
            schema_version: DEV_BENCH_WIRE_SCHEMA_VERSION,
            compatible: true,
            firmware_version: heapless::String::try_from("nrf54l15dk-g1a2b3c").unwrap(),
            hardware_id: heapless::String::new(),
        });
    }

    #[test]
    fn stream_open_chunk_and_close_round_trip() {
        // The generic tap trio that replaced StreamStart/StreamChunk/
        // StreamEnd at schema v8 (`embarch-study-designer/design.md` §3
        // decision 39). Records carry arrival-stamped bytes, never decoded
        // values.
        round_trip(&DevBenchMessage::StreamOpen { id: 3 });

        let mut records: heapless::Vec<StreamRecord, 4> = heapless::Vec::new();
        records
            .push(StreamRecord {
                rx_utc_ms: 1_753_000_000_000,
                bytes: heapless::Vec::from_slice(b"ok\r\n").unwrap(),
            })
            .unwrap();
        round_trip(&DevBenchMessage::StreamChunkBatch { id: 3, records });

        round_trip(&DevBenchMessage::StreamClose { id: 3, dropped: 0 });
        round_trip(&DevBenchMessage::StreamClose { id: 3, dropped: 12 });
    }

    #[test]
    fn study_done_round_trips() {
        round_trip(&DevBenchMessage::StudyDone { completed: true });
        round_trip(&DevBenchMessage::StudyDone { completed: false });
    }

    #[test]
    fn log_line_round_trips() {
        round_trip(&DevBenchMessage::LogLine {
            text: heapless::String::try_from("ble: connected").unwrap(),
        });
    }

    /// Two frames arriving back-to-back in one physical read (the case
    /// [`DevBenchLink::buf`] exists for) must still decode as two separate
    /// messages, in order, with nothing dropped.
    #[test]
    fn two_frames_in_one_buffer_decode_in_order() {
        let a = DevBenchMessage::StudyDone { completed: true };
        let b = DevBenchMessage::LogLine { text: heapless::String::try_from("hi").unwrap() };

        let mut combined = postcard::to_stdvec_cobs(&a).unwrap();
        combined.extend(postcard::to_stdvec_cobs(&b).unwrap());

        let mut buf = combined;
        let first_pos = buf.iter().position(|&x| x == 0).unwrap();
        let mut first_frame: Vec<u8> = buf.drain(..=first_pos).collect();
        let decoded_a: DevBenchMessage = postcard::from_bytes_cobs(&mut first_frame).unwrap();
        assert_eq!(decoded_a, a);

        let decoded_b: DevBenchMessage = postcard::from_bytes_cobs(&mut buf).unwrap();
        assert_eq!(decoded_b, b);
    }
}
