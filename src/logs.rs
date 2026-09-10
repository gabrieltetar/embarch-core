//! Shared log-reading logic behind `embarch-core logs` (CLI, `main.rs`) and
//! `GET /logs/recent` (HTTP, `api.rs`) — one
//! implementation, multiple call sites, matching this suite's own
//! established shape (`embarch-topology` decisions 2/8/14) rather
//! than the CLI and HTTP paths growing separate copies of "find the current
//! log file." Moved out of `main.rs` unchanged (`embarch-ui` decision 7)
//! when the HTTP surface
//! needed the same logic `main.rs`'s `Logs` subcommand already had.
//!
//! `GET /logs/stream`'s poll-follow machinery (`FollowState`) lived here
//! too, until it was retired (`tasks/core/021`) for having no caller
//! anywhere in the suite; `read_recent`/`tail_lines` below are what
//! `/logs/recent` still uses and are unaffected.
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

}
