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
                let msg = postcard::from_bytes_cobs(&mut frame)
                    .context("failed to decode DevBenchMessage")?;
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
