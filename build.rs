//! Stamps this binary with the commit it was built from (`embarch-umbrella`
//! decision 57, suite task 047).
//!
//! **The same file, byte for byte, in `embarch-api`, `embarch-core` and
//! `embarch-umbrella`.** Each repo builds alone, so there is no shared crate to
//! put it in that all three already depend on; a copy that drifts is caught by
//! reading the three side by side, and the decision says so.
//!
//! What it emits, read with `env!` by the crate:
//!
//! - `EMBARCH_BUILD_COMMIT` — the full `HEAD` sha, or `unknown`.
//! - `EMBARCH_BUILD_DIRTY` — `1` when tracked files differ from `HEAD`, else `0`.
//! - `EMBARCH_BUILD_SOURCE` — this crate's own directory at build time, so a
//!   caller can find the checkout an installed binary came from without
//!   guessing a layout.
//! - `EMBARCH_BUILD_VERSION` — what `--version` prints after the binary name:
//!   `0.1.0 (commit 744b94b0)`, `0.1.0 (commit 744b94b0, dirty)` or
//!   `0.1.0 (commit unknown)`. The crate version stays the **first** field
//!   after the name, which is what `embarch doctor` reads.
//!
//! **`unknown` rather than a guess.** git is asked only when this directory is
//! the root of its own checkout: a source copy with no `.git` — `deploy-core`'s
//! rsync'd Windows tree — that sits inside some other repository would
//! otherwise report that repository's commit. `EMBARCH_BUILD_COMMIT` and
//! `EMBARCH_BUILD_DIRTY` in the environment win over git, which is how
//! `deploy-core` carries the Linux checkout's identity to a Windows build.
//!
//! **When it re-runs.** On a change to `HEAD`, the ref it points at,
//! `packed-refs`, `src/`, `crates/` (where it exists), `Cargo.toml`,
//! `Cargo.lock`, this file, or the two override variables. Not the index: an
//! editor refreshing it would re-link the binary all day. So an edit anywhere
//! else in the tree (a README, a test) does not refresh the dirty flag until
//! one of those moves.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("--no-optional-locks")
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

/// `git rev-parse --git-path` answers relative to `dir` unless it is absolute.
fn git_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(git(dir, &["rev-parse", "--git-path", name])?);
    Some(if p.is_absolute() { p } else { dir.join(p) })
}

fn watch(path: &Path) {
    // A path that does not exist would make Cargo re-run this on every build.
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn from_git(dir: &Path) -> Option<(String, bool)> {
    let top = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?);
    if std::fs::canonicalize(&top).ok()? != std::fs::canonicalize(dir).ok()? {
        return None;
    }
    let head = git(dir, &["rev-parse", "HEAD"])?;
    let changes = git(dir, &["status", "--porcelain", "--untracked-files=no"])?;

    for name in ["HEAD", "packed-refs"] {
        if let Some(p) = git_path(dir, name) {
            watch(&p);
        }
    }
    if let Some(r) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(p) = git_path(dir, &r) {
            watch(&p);
        }
    }
    Some((head, !changes.is_empty()))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=EMBARCH_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=EMBARCH_BUILD_DIRTY");

    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    for name in ["src", "crates", "Cargo.toml", "Cargo.lock"] {
        watch(&dir.join(name));
    }

    let overridden = std::env::var("EMBARCH_BUILD_COMMIT")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    let (commit, dirty) = match overridden {
        Some(c) => {
            let d = std::env::var("EMBARCH_BUILD_DIRTY").map(|d| d.trim() == "1").unwrap_or(false);
            (c, d)
        }
        None => from_git(&dir).unwrap_or_else(|| ("unknown".to_string(), false)),
    };

    let short: String = if commit == "unknown" { commit.clone() } else { commit.chars().take(8).collect() };
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let line = if dirty {
        format!("{version} (commit {short}, dirty)")
    } else {
        format!("{version} (commit {short})")
    };

    println!("cargo:rustc-env=EMBARCH_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=EMBARCH_BUILD_DIRTY={}", if dirty { "1" } else { "0" });
    println!("cargo:rustc-env=EMBARCH_BUILD_SOURCE={}", dir.display());
    println!("cargo:rustc-env=EMBARCH_BUILD_VERSION={line}");
}
