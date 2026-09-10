//! Shared log-reading logic behind `embarch-core logs` (CLI, `main.rs`) and
//! `GET /logs/recent`/`GET /logs/stream` (HTTP, `api.rs`) — one
//! implementation, multiple call sites, matching this suite's own
//! established shape (`embarch-topology` decisions 2/8/14) rather
//! than the CLI and HTTP paths growing separate copies of "find the current
//! log file." Moved out of `main.rs` unchanged (`embarch-ui` decision 7)
//! when the HTTP surface
//! needed the same logic `main.rs`'s `Logs` subcommand already had.
//!
//! Reuses the existing daily-rolling logfile (`main.rs`'s `init_tracing`,
//! decision 16) rather than introducing a second, size-capped log
//! mechanism — `embarch-ui` decision 7 originally described a
//! new size-capped rotating logfile, written before this session noticed
//! Core already had a real, tested, daily-rotating one (7-file retention).
//! That decision is corrected in place rather than building a redundant
//! second mechanism (see this crate's own decisions for the full account).

use anyhow::{Context, Result};
use std::path::PathBuf;

pub(crate) const LOG_FILE_PREFIX: &str = "core.log";

pub(crate) fn log_dir() -> Result<PathBuf> {
    Ok(crate::token_store::local_data_dir()?.join("logs"))
}

/// Picks the lexicographically largest filename among `candidates` that
/// starts with `<prefix>.`; since `tracing-appender`'s date format is ISO
/// (`yyyy-MM-dd`), lexicographic order agrees with chronological order, so
/// the "most recent" file needs no date parsing of its own.
pub(crate) fn latest_log_file<'a>(candidates: &'a [PathBuf], prefix: &str) -> Option<&'a PathBuf> {
    let prefix_with_sep = format!("{prefix}.");
    candidates
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix_with_sep))
        })
        .max_by_key(|path| path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
}

/// The last `n` lines of `contents`, or all of it if there are fewer than
/// `n` lines.
pub(crate) fn tail_lines(contents: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

fn read_dir_candidates(dir: &std::path::Path) -> Result<Vec<PathBuf>> {
    Ok(std::fs::read_dir(dir)
        .with_context(|| format!("failed to read log directory {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect())
}

/// `GET /logs/recent` and `embarch-core logs`'s shared implementation:
/// finds the current daily log file and returns its last `tail` lines.
pub(crate) fn read_recent(tail: usize) -> Result<Vec<String>> {
    read_recent_with_prefix(LOG_FILE_PREFIX, tail)
}

/// The same read against any of this directory's daily-rolling files, by
/// prefix — `embarch-core dev-bench-logs` reads
/// `dev_bench_log::DEV_BENCH_LOG_FILE_PREFIX` through this (§3 decision 37).
/// Both files rotate identically, so one reader serves both; only the prefix
/// differs.
pub(crate) fn read_recent_with_prefix(prefix: &str, tail: usize) -> Result<Vec<String>> {
    let dir = log_dir()?;
    let candidates = read_dir_candidates(&dir)?;
    let latest = latest_log_file(&candidates, prefix).with_context(|| {
        format!("no {prefix}.<date> files found in {} — nothing has been written there yet", dir.display())
    })?;
    let contents = std::fs::read_to_string(latest)
        .with_context(|| format!("failed to read log file {}", latest.display()))?;
    Ok(tail_lines(&contents, tail).into_iter().map(String::from).collect())
}

/// Poll-based tail-follow behind `GET /logs/stream` — `embarch-ui`
/// decision 7's "live tail," implemented the same way `embarch-core`'s
/// own `serial::read_log` already reads a live source (a poll loop, not an
/// OS-level file-change notification), and the same way `embarch-ui`'s own
/// Dashboard/Study-Designer tabs already relay a server-side poll over SSE
/// (`embarch-ui` decision 6) — chosen over adding a custom
/// `tracing` layer that broadcasts each formatted line live, specifically
/// to avoid touching `main.rs`'s `init_tracing` (a foundational, already-
/// deployed piece of a real running service) for a debug-tooling feature.
///
/// Never replays a file's backlog on first use — `poll` on a freshly
/// rotated (or never-yet-seen) file starts from its current end, since
/// backlog is `read_recent`'s job, not this one's.
pub(crate) struct FollowState {
    path: Option<PathBuf>,
    offset: u64,
}

impl FollowState {
    pub(crate) fn new() -> Self {
        FollowState { path: None, offset: 0 }
    }

    /// One tick against the real log directory (`log_dir()`) — what
    /// `GET /logs/stream` actually calls.
    pub(crate) fn poll(&mut self) -> Result<Vec<String>> {
        self.poll_in(&log_dir()?)
    }

    /// One tick against an arbitrary directory — split out from `poll` so
    /// this is testable against a real temp directory without depending on
    /// `token_store::local_data_dir()`'s machine-specific resolution.
    /// Resolves the current log file (which may have rotated since the
    /// last tick) and returns any **complete** lines appended since the
    /// last recorded (path, byte offset).
    ///
    /// **The offset advances only past a `\n` this reader has actually
    /// seen** (decision 44). A tick can land inside the writer's own
    /// `write` — `tracing-appender`'s worker formats an event and writes
    /// it, and nothing makes that one atomic append — so the bytes read
    /// here may end mid-line. Whatever follows the last `\n` in the read
    /// is left in the file, unconsumed, and re-read on the next tick once
    /// its newline has arrived; only the newline-terminated prefix is
    /// published and counted into `offset`. Advancing to EOF instead
    /// would publish the half-line as if it were a whole one and then
    /// publish its remainder as a second "line," which is what this code
    /// used to do while claiming it could not.
    ///
    /// That is also what makes the decode safe: `0x0A` cannot occur
    /// inside a UTF-8 multi-byte sequence, so a slice ending at a `\n` is
    /// always character-boundary-clean, and the read cannot tear a
    /// character in half. Genuinely invalid bytes in the file (not a torn
    /// character — nothing this crate writes produces them) are replaced
    /// rather than raised, because erroring without advancing `offset`
    /// would re-read the same bad bytes every 750 ms forever.
    ///
    /// **One anchor is not covered by that rule, deliberately:** the
    /// file's length taken below whenever this state is not already
    /// following that file. If *that* read lands inside a torn write, the
    /// anchor sits mid-line and the remainder of that one line surfaces
    /// as one short line. **Note which case that is: the branch is "first
    /// tick" as well as "rotated", so it is reached once per subscriber,
    /// on whatever file Core is writing — not only on a file that has
    /// just been created.** Closing it would need a "we started mid-line"
    /// flag carried across ticks; the reason not to is the size of the
    /// loss — one short line at the head of a tail nobody has read yet —
    /// and not the rarity of the case.
    fn poll_in(&mut self, dir: &std::path::Path) -> Result<Vec<String>> {
        use std::io::{Read, Seek, SeekFrom};

        let candidates = read_dir_candidates(dir)?;
        let Some(latest) = latest_log_file(&candidates, LOG_FILE_PREFIX) else {
            return Ok(Vec::new());
        };

        if self.path.as_deref() != Some(latest.as_path()) {
            // First tick, or the file rotated since the last one — start
            // following from the current end, not the beginning.
            let len = std::fs::metadata(latest).map(|m| m.len()).unwrap_or(0);
            self.path = Some(latest.clone());
            self.offset = len;
            return Ok(Vec::new());
        }

        let mut file = std::fs::File::open(latest)
            .with_context(|| format!("failed to open log file {}", latest.display()))?;
        file.seek(SeekFrom::Start(self.offset))
            .with_context(|| format!("failed to seek log file {}", latest.display()))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .with_context(|| format!("failed to read log file {}", latest.display()))?;

        // Only the newline-terminated prefix is ours this tick; a trailing
        // partial line stays in the file for the next one.
        let Some(last_newline) = buf.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        let complete = &buf[..=last_newline];
        self.offset += complete.len() as u64;
        Ok(String::from_utf8_lossy(complete).lines().map(String::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_log_file_picks_the_lexicographically_largest_iso_date() {
        let candidates = vec![
            PathBuf::from("/logs/core.log.2026-08-18"),
            PathBuf::from("/logs/core.log.2026-08-20"),
            PathBuf::from("/logs/core.log.2026-08-19"),
        ];
        assert_eq!(
            latest_log_file(&candidates, "core.log"),
            Some(&PathBuf::from("/logs/core.log.2026-08-20"))
        );
    }

    #[test]
    fn latest_log_file_ignores_files_with_a_different_prefix() {
        let candidates = vec![
            PathBuf::from("/logs/core.log.2026-08-19"),
            PathBuf::from("/logs/some-other-file.txt"),
            PathBuf::from("/logs/token"),
        ];
        assert_eq!(
            latest_log_file(&candidates, "core.log"),
            Some(&PathBuf::from("/logs/core.log.2026-08-19"))
        );
    }

    #[test]
    fn latest_log_file_is_none_when_nothing_matches() {
        let candidates = vec![PathBuf::from("/logs/token")];
        assert_eq!(latest_log_file(&candidates, "core.log"), None);
    }

    #[test]
    fn tail_lines_returns_only_the_last_n() {
        let contents = "one\ntwo\nthree\nfour\nfive";
        assert_eq!(tail_lines(contents, 2), vec!["four", "five"]);
    }

    #[test]
    fn tail_lines_returns_everything_when_fewer_lines_than_requested() {
        let contents = "one\ntwo";
        assert_eq!(tail_lines(contents, 50), vec!["one", "two"]);
    }

    fn temp_log_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "embarch-core-logs-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    #[test]
    fn follow_state_never_replays_existing_content_on_first_tick() {
        let dir = temp_log_dir("follow-first-tick");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{LOG_FILE_PREFIX}.2026-08-24"));
        std::fs::write(&path, "existing line one\nexisting line two\n").unwrap();

        let mut follow = FollowState::new();
        let lines = follow.poll_in(&dir).unwrap();
        assert!(lines.is_empty(), "first tick must not replay backlog — read_recent's job, not this one's");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn follow_state_reports_lines_appended_after_the_first_tick() {
        use std::io::Write as _;

        let dir = temp_log_dir("follow-new-lines");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{LOG_FILE_PREFIX}.2026-08-24"));
        std::fs::write(&path, "existing line one\n").unwrap();

        let mut follow = FollowState::new();
        assert!(follow.poll_in(&dir).unwrap().is_empty());

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "brand new line").unwrap();
        drop(file);

        assert_eq!(follow.poll_in(&dir).unwrap(), vec!["brand new line".to_string()]);
        // A tick with nothing new appended reports no lines, not the same
        // ones again.
        assert!(follow.poll_in(&dir).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn follow_state_starts_fresh_from_the_end_of_a_rotated_file() {
        let dir = temp_log_dir("follow-rotation");
        std::fs::create_dir_all(&dir).unwrap();
        let day1 = dir.join(format!("{LOG_FILE_PREFIX}.2026-08-23"));
        std::fs::write(&day1, "yesterday's last line\n").unwrap();

        let mut follow = FollowState::new();
        assert!(follow.poll_in(&dir).unwrap().is_empty());

        // Rotate: a new day's file appears with its own content already
        // written by the time this tick runs.
        let day2 = dir.join(format!("{LOG_FILE_PREFIX}.2026-08-24"));
        std::fs::write(&day2, "today's first line\n").unwrap();
        // The rotation tick itself reports nothing — it only starts
        // following from this new file's current end.
        assert!(follow.poll_in(&dir).unwrap().is_empty());

        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().append(true).open(&day2).unwrap();
        writeln!(file, "today's second line").unwrap();
        drop(file);
        assert_eq!(follow.poll_in(&dir).unwrap(), vec!["today's second line".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A tick can land inside the writer's own `write` (decision 44). The
    /// half-line it sees is not a line, and must not be published as one —
    /// nor must its remainder arrive next tick as a second "line."
    #[test]
    fn follow_state_holds_a_trailing_partial_line_until_its_newline_arrives() {
        use std::io::Write as _;

        let dir = temp_log_dir("follow-partial-line");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{LOG_FILE_PREFIX}.2026-08-24"));
        std::fs::write(&path, "existing line one\n").unwrap();

        let mut follow = FollowState::new();
        assert!(follow.poll_in(&dir).unwrap().is_empty());

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();

        // A torn write: the writer got as far as the middle of a line.
        write!(file, "INFO first half of a").unwrap();
        file.flush().unwrap();
        assert!(
            follow.poll_in(&dir).unwrap().is_empty(),
            "a line with no newline yet is not a line — it must not be published"
        );

        // The rest of that line, plus a whole one, plus another tear.
        write!(file, " torn line\nINFO a whole line\nINFO the next tor").unwrap();
        file.flush().unwrap();
        assert_eq!(
            follow.poll_in(&dir).unwrap(),
            vec![
                "INFO first half of a torn line".to_string(),
                "INFO a whole line".to_string(),
            ],
            "the completed line must arrive once and whole, and the new partial must be held back"
        );

        // And the held-back partial completes in its turn — once, whole.
        writeln!(file, "n line").unwrap();
        file.flush().unwrap();
        drop(file);
        assert_eq!(follow.poll_in(&dir).unwrap(), vec!["INFO the next torn line".to_string()]);
        assert!(follow.poll_in(&dir).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
