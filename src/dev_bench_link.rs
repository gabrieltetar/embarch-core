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

/// One open serial connection to `embarch-dev-bench`, speaking
/// COBS-framed/postcard-encoded `DevBenchMessage`s one at a time.
pub struct DevBenchLink {
    port: Box<dyn SerialPort>,
    /// Bytes read off the wire but not yet consumed into a complete,
    /// delimited frame — a blocking serial read can return partial data (or
    /// more than one frame's worth at once), so this carries the remainder
    /// across calls to [`Self::recv`].
    buf: Vec<u8>,
}

impl DevBenchLink {
    /// Opens `port_name` at [`DEV_BENCH_BAUD`]. Does not send `Hello` or do
    /// anything else protocol-level — that's `study.rs`'s job.
    pub fn open(port_name: &str) -> Result<Self> {
        let port = serialport::new(port_name, DEV_BENCH_BAUD)
            .timeout(READ_TIMEOUT)
            .open()
            .with_context(|| format!("failed to open dev-bench serial port '{port_name}'"))?;
        Ok(Self { port, buf: Vec::new() })
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
    /// problem. Twenty-four bytes reaches past the tag, the step index and
    /// most step names.
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
        format!(
            "failed to decode DevBenchMessage: {framed_len} COBS-framed bytes, \
             variant index {} ({tag_name}), first {} bytes {}",
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

    /// Reads and decodes one `DevBenchMessage`, blocking (via bounded,
    /// polled reads) until either a complete `0x00`-delimited frame arrives
    /// or `deadline` passes.
    ///
    /// Returns `Ok(None)` on a plain deadline expiry — the caller's watchdog
    /// case, not an error. An `Err` means the connection itself failed
    /// (a read error) or a frame arrived but didn't decode.
    pub fn recv(&mut self, deadline: Instant) -> Result<Option<DevBenchMessage>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == 0) {
                let mut frame: Vec<u8> = self.buf.drain(..=pos).collect();
                // Kept for the error path below: `from_bytes_cobs` decodes
                // in place, so by the time it fails `frame` no longer holds
                // what arrived on the wire.
                let framed_len = frame.len();
                let head: Vec<u8> = frame.iter().take(24).copied().collect();
                let msg = postcard::from_bytes_cobs(&mut frame).with_context(|| {
                    Self::describe_undecodable_frame(&head, framed_len)
                })?;
                return Ok(Some(msg));
            }

            if Instant::now() >= deadline {
                return Ok(None);
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

    #[test]
    fn a_frame_too_short_to_hold_a_tag_says_so_rather_than_guessing() {
        let msg = DevBenchLink::describe_undecodable_frame(&[0x01], 1);
        assert!(msg.contains("frame too short"), "{msg}");
        assert!(!msg.contains("Hello"), "must not name a variant it cannot read: {msg}");
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
