use anyhow::{Context, Result};
use std::io::Read;
use std::time::{Duration, Instant};

/// Open a serial port, capture output for `duration_ms`, and return it as lines.
///
/// This is a UART/USB-serial console (the target's stdout/log output) — a
/// separate physical connection from the JTAG/SWD debug probe in `hardware.rs`.
/// Most boards expose both: a probe for flashing/debug, and a serial adapter
/// for the running firmware's log output.
pub fn read_log(port: &str, baud: u32, duration_ms: u64) -> Result<Vec<String>> {
    let mut conn = serialport::new(port, baud)
        .timeout(Duration::from_millis(200))
        .open()
        .with_context(|| format!("failed to open serial port '{port}'"))?;

    let deadline = Instant::now() + Duration::from_millis(duration_ms);
    let mut buf = [0u8; 1024];
    let mut collected = Vec::new();

    while Instant::now() < deadline {
        match conn.read(&mut buf) {
            Ok(n) if n > 0 => collected.extend_from_slice(&buf[..n]),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e).context("error reading from serial port"),
        }
    }

    let text = String::from_utf8_lossy(&collected);
    Ok(text.lines().map(|l| l.to_string()).collect())
}
