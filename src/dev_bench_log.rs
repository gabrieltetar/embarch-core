//! The dev-bench debug file: every `DevBenchMessage::LogLine` Core receives,
//! appended to a daily-rolling file of its own (§3 decision 37).
//!
//! **Why a third destination for these lines.** Before this, a `LogLine`
//! reached two places: Core's own `core.log` (via `tracing`) and — since the
//! reserved `dev-bench` stream tap — the running study's results directory.
//! Both were right for what they were, and neither is a debug log of the
//! bench:
//!
//! - `core.log` is Core's own operational log. It is what a human reads to see
//!   what the *service* did, and interleaving a firmware's full `CONFIG_LOG`
//!   output into it at `info` would drown that. It also rotates on Core's
//!   schedule, mixed with everything else.
//! - the study's `streams/dev-bench` file is scoped to one study by
//!   construction. It cannot hold a line from the handshake that failed before
//!   the study started, and it does not exist at all for the
//!   `GET /dev-bench/hello` probe.
//!
//! `embarch-dev-bench/design.md` §3 decision 38 turned `CONFIG_LOG` on in the
//! firmware, which changes the volume and the value of this channel: it now
//! carries Zephyr's own subsystem output and, on a crash, the fatal-error
//! dump. That wants one continuous file, spanning studies, that exists whether
//! or not a study is running — which is what this module is.
//!
//! Deliberately *not* a second log mechanism in the sense `logs.rs`'s own
//! header warns about: it reuses the same `tracing-appender` daily rotation
//! with the same 7-file retention, in the same directory, and `logs.rs`'s
//! reader functions serve it by prefix. Only the file is separate.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

use tracing_appender::rolling::RollingFileAppender;

/// Names each day's file `dev-bench.log.<yyyy-MM-dd>`, alongside
/// `core.log.<yyyy-MM-dd>` (`logs::LOG_FILE_PREFIX`).
pub(crate) const DEV_BENCH_LOG_FILE_PREFIX: &str = "dev-bench.log";

/// What a line's own `<lvl>` marker says about it.
///
/// dev-bench's log backend emits log_output's standard `<err>`/`<wrn>`/
/// `<inf>`/`<dbg>` prefix (`embarch-dev-bench/app/src/dev_bench_log.c`), so
/// Core can mirror a firmware line into `core.log` at a level that matches
/// what the firmware said, instead of picking one level for all of them.
///
/// `Unmarked` is the hand-written `send_log_line()` diagnostics (design.md
/// §3 decision 7's original use of this channel) — a truncated transcript, a
/// link RX overrun, the uptime line at handshake. Those are deliberate
/// statements to the host, not log records, and they keep the `info` treatment
/// decision 37 found them already deserving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineLevel {
    Error,
    Warn,
    Info,
    Debug,
    Unmarked,
}

/// Reads the leading `<lvl>` token, if there is one. Pure, and separate from
/// everything that touches a file, so the mapping is testable directly.
pub(crate) fn classify(line: &str) -> LineLevel {
    match line.as_bytes().first() {
        // Cheap reject first: the overwhelming majority of lines either start
        // with '<' or are not marked at all.
        Some(b'<') => {}
        _ => return LineLevel::Unmarked,
    }
    match &line[..line.len().min(6)] {
        s if s.starts_with("<err> ") => LineLevel::Error,
        s if s.starts_with("<wrn> ") => LineLevel::Warn,
        s if s.starts_with("<inf> ") => LineLevel::Info,
        s if s.starts_with("<dbg> ") => LineLevel::Debug,
        // A line that merely happens to start with '<'. Treated as what it is
        // rather than guessed at.
        _ => LineLevel::Unmarked,
    }
}

/// One record's rendered form: arrival timestamp, the study it belongs to (or
/// `-`), then the line exactly as dev-bench sent it.
///
/// **Core stamps arrival rather than dev-bench stamping departure**, the same
/// choice and for the same reason `study.rs` makes it for the reserved
/// `dev-bench` tap: this is when the line was received, on the same clock every
/// other Core-mediated record uses, and dev-bench spends none of its 128-byte
/// line budget on a timestamp Core would have to re-base anyway.
///
/// Pure, so the format is tested without a filesystem.
pub(crate) fn render(now_rfc3339: &str, study_id: Option<&str>, line: &str) -> String {
    format!("{now_rfc3339} {} {line}", study_id.unwrap_or("-"))
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        // Unreachable for a well-known format against a valid instant; a
        // timestamp that cannot be rendered must not cost the line it was
        // going to label.
        .unwrap_or_else(|_| "0000-00-00T00:00:00Z".to_string())
}

/// The process-wide appender, built on first use.
///
/// `Option` rather than a panic or a `Result` at every call site: a debug log
/// that cannot be opened must not be able to fail a study. The failure is
/// reported once, into `core.log`, and every later call is a no-op.
static WRITER: OnceLock<Option<Mutex<RollingFileAppender>>> = OnceLock::new();

fn writer() -> Option<&'static Mutex<RollingFileAppender>> {
    WRITER
        .get_or_init(|| {
            let dir = match crate::logs::log_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    tracing::warn!(
                        "cannot resolve the log directory; dev-bench log lines will reach \
                         core.log only: {e:#}"
                    );
                    return None;
                }
            };
            match tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix(DEV_BENCH_LOG_FILE_PREFIX)
                .max_log_files(7)
                .build(&dir)
            {
                Ok(appender) => {
                    tracing::info!(
                        dir = %dir.display(),
                        "dev-bench debug log open ({DEV_BENCH_LOG_FILE_PREFIX}.<date>, 7 days retained)"
                    );
                    Some(Mutex::new(appender))
                }
                Err(e) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        "failed to open the dev-bench debug log; its lines will reach core.log \
                         only: {e:#}"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Appends one line dev-bench sent. Never fails the caller: a write error is
/// reported into `core.log` and dropped.
///
/// Blocking file I/O, like everything else on the dev-bench path — call it
/// from `spawn_blocking`, not from an async context.
pub(crate) fn append(study_id: Option<&str>, line: &str) {
    let Some(writer) = writer() else {
        return;
    };
    let record = render(&now_rfc3339(), study_id, line);
    // A poisoned mutex means some earlier writer panicked mid-write. The file
    // is still perfectly usable and the alternative is losing every line from
    // here on, so the guard is recovered rather than propagated.
    let mut guard = match writer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Err(e) = writeln!(guard, "{record}") {
        tracing::warn!("failed to append to the dev-bench debug log: {e:#}");
    }
}

/// Core's own marker in dev-bench's file — a link opening or closing, a study
/// starting or ending. Written in the same shape as a real line but bracketed
/// so it can never be mistaken for something the firmware said.
///
/// These are what make the file readable across a reboot: without them, a
/// bench that reset between two studies produces two runs of boot lines with
/// nothing marking where one run ended.
pub(crate) fn note(study_id: Option<&str>, text: &str) {
    append(study_id, &format!("--- {text} ---"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_reads_the_firmware_level_marker() {
        assert_eq!(classify("<err> bt_conn: oh no"), LineLevel::Error);
        assert_eq!(classify("<wrn> log: 3 record(s) dropped"), LineLevel::Warn);
        assert_eq!(classify("<inf> dev_bench: up"), LineLevel::Info);
        assert_eq!(classify("<dbg> gatt: subscribed"), LineLevel::Debug);
    }

    #[test]
    fn classify_leaves_a_hand_written_diagnostic_unmarked() {
        // design.md §3 decision 7's original users of this channel: no level
        // marker, and they must not be demoted to `debug` by accident.
        assert_eq!(
            classify("uptime 412 ms at handshake, reset cause 0x00000001"),
            LineLevel::Unmarked
        );
        assert_eq!(
            classify("this transcript is NOT exhaustive: 2 row(s) dropped"),
            LineLevel::Unmarked
        );
    }

    #[test]
    fn classify_does_not_guess_at_a_line_that_merely_starts_with_a_bracket() {
        assert_eq!(classify("<not a level> hello"), LineLevel::Unmarked);
        assert_eq!(classify("<"), LineLevel::Unmarked);
        assert_eq!(classify(""), LineLevel::Unmarked);
    }

    #[test]
    fn render_puts_the_study_id_between_the_timestamp_and_the_line() {
        assert_eq!(
            render("2026-08-26T18:22:03Z", Some("abc123"), "<inf> dev_bench: up"),
            "2026-08-26T18:22:03Z abc123 <inf> dev_bench: up"
        );
    }

    #[test]
    fn render_marks_a_line_that_belongs_to_no_study() {
        // The handshake probe (`GET /dev-bench/hello`) and every boot line
        // flushed before a study starts land here, and a reader has to be able
        // to tell them from a study's own lines.
        assert_eq!(
            render("2026-08-26T18:22:03Z", None, "<inf> dev_bench: up"),
            "2026-08-26T18:22:03Z - <inf> dev_bench: up"
        );
    }

    #[test]
    fn a_line_is_never_split_across_records() {
        // dev_bench_log.c replaces control bytes and splits on '\n' precisely
        // so this holds; asserted here because it is Core's file format that
        // depends on it.
        let rendered = render("2026-08-26T18:22:03Z", Some("s"), "one line only");
        assert_eq!(rendered.lines().count(), 1);
    }
}
