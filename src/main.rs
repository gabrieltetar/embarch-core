mod api;
mod chip_resolve;
mod dev_bench_link;
mod elevate;
mod enroll_page;
mod hardware;
mod serial;
mod service;
mod study;
mod token_store;

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::future::Future;
use std::path::PathBuf;
use tracing_subscriber::fmt::writer::MakeWriterExt;

/// Shared with `service::windows`'s fallback (`run_service` uses this only if
/// `BIND_PORT` was somehow never set — a call path that shouldn't exist in
/// practice, since `try_dispatch` always sets it first) — one constant
/// instead of two places that could disagree.
pub(crate) const DEFAULT_PORT: u16 = 4884;

/// Loopback-only (design.md §3 decision 6's amendment, 2026-08-15): reachable
/// only from processes on this same machine unless something explicitly
/// widens it. Shared between `Run`'s and `Install`'s own `--bind` flags so
/// they can't drift apart.
pub(crate) const DEFAULT_BIND: &str = "127.0.0.1";

/// Filename prefix for the daily-rolling log file (§3 decision 16,
/// `embarch-core/design.md`) — shared between `init_tracing`, which sets the
/// prefix on the `tracing-appender` builder, and `print_log_tail`, which
/// needs the same prefix to recognize which files in the log directory are
/// its own rotated log files (as opposed to anything else that might land in
/// that directory later) and to know where the prefix ends and the
/// lexicographically-sortable ISO date suffix begins.
const LOG_FILE_PREFIX: &str = "core.log";

#[derive(Parser)]
#[command(name = "embarch-core", version)]
#[command(about = "EmbArch Core — the OS-level service that talks to debug/flash hardware")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP service in the foreground. This is also what the
    /// installed background service invokes — `install` just registers
    /// this same command with the OS. On Windows specifically, this first
    /// tries to hand off to the Service Control Manager, and only falls
    /// back to a plain foreground run when that fails because nothing
    /// launched this process as a service in the first place (a human at a
    /// console, same as everywhere else) — see `service::windows`.
    Run {
        /// Bind address. Loopback-only by default (design.md §3 decision 6's
        /// amendment) — reachable only from this same machine unless widened
        /// explicitly. `embarch-umbrella setup` passes an explicit `--bind`
        /// to `install` for the one topology (WSL2⟷Windows, or a genuinely
        /// remote Core) that actually needs a wider address.
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    /// Install embarch-core as a background OS service (Windows Service
    /// via sc.exe, or systemd on Linux) and start it immediately.
    Install {
        /// Bind address the installed service runs with — baked into its
        /// registered start command (survives reboots without umbrella
        /// having to re-run this), not just this one invocation. Same
        /// loopback-only default as `run`; `embarch-umbrella setup` passes
        /// `recommended_bind_address(TopologyClass)`'s answer explicitly for
        /// wsl-host/remote (design.md §3 decision 6's amendment).
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: String,
    },
    /// Stop and remove the background OS service.
    Uninstall,
    /// Start the already-installed background service. Separate from
    /// `install` (which registers *and* starts) so a service that's been
    /// stopped — or that failed to come up at boot — can be started again
    /// without re-registering it.
    Start,
    /// Stop the running background service, leaving it installed so it
    /// still starts at the next boot.
    Stop,
    /// Replace the installed service's binary with a new one and restart
    /// it. Must be run via the currently-installed copy itself (see
    /// `service::update`'s own doc comment for why) — self-elevates like
    /// every other subcommand here.
    Update {
        /// Path to the new embarch-core binary to install in place of this one.
        new_exe: PathBuf,
    },
    /// Print which serial port embarch-dev-bench is on, using the same
    /// SEGGER-VID detection `GET /dev-bench/port` serves — a human's way to
    /// check the bench is visible without an HTTP client or a running service.
    DetectDevBench,
    /// Print the last N lines of embarch-core's own current daily log file
    /// (§3 decision 16) — pure local file read, no hardware access, same
    /// posture as `detect-dev-bench`. Useful for seeing what a Core running
    /// as a background service actually logged (its stderr otherwise goes
    /// nowhere a human can read), without needing a second terminal open on
    /// a foreground `run`.
    Logs {
        /// How many of the most recent lines to print.
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
}

/// Both `main`'s foreground `Run` path and `service::windows`'s SCM-dispatched
/// callback each build their own fresh runtime (see that module's comment on
/// why it can't be nested) — this is the one place both configure it, so they
/// can't drift apart again.
///
/// The explicit 64 MiB `thread_stack_size` (applied to worker *and*
/// `spawn_blocking` threads alike — Tokio's `Builder` doesn't distinguish)
/// is a real fix, not a defensive default: `embarch-study-designer/design.md`
/// §7 tracked a `Study`/`StepResult` stack-overflow risk from large
/// fixed-capacity `heapless` types, previously reproduced only in debug
/// builds and "confirmed release-build-safe" as of that doc's 2026-08-19/20
/// finding. That confirmation didn't hold: the first real `run_study` POST
/// against this milestone's GATT-extended `StepResult` (decisions 31/32's
/// `gatt_services`/`gatt_activity`, larger than anything sized when that
/// finding was written) crashed the real, `--release` Windows service with
/// `STATUS_STACK_OVERFLOW` (0xc00000fd) — a first real release-build
/// occurrence, on `run_study`'s `spawn_blocking(run_study_to_completion)`
/// specifically (`study.rs`), not on a debug build's default `tokio-rt-worker`
/// stack as `design.md` §7 had only ever seen before. Matches the size
/// already known to clear it in tests (`RUST_MIN_STACK=67108864`) rather than
/// picking a new number — made an explicit runtime setting here instead of
/// an ambient env var, since a Windows service's environment isn't something
/// `install`/SCM sets today and shouldn't have to.
fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build()
}

// Deliberately not `#[tokio::main]`: the Windows service path below needs to
// call `StartServiceCtrlDispatcherW` (via `service::windows::try_dispatch`)
// as a plain blocking call from `main`'s own thread, before any Tokio
// runtime exists — SCM expects that handshake promptly, and everything
// after it (including the actual async server) runs on a runtime built
// fresh inside the service callback, never nested inside another one.
fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Command::Run { bind, port } => {
            #[cfg(windows)]
            if let Some(result) = service::windows::try_dispatch(bind.clone(), port) {
                return result;
            }
            build_runtime()?.block_on(run(bind, port))?;
        }
        Command::Install { bind } => {
            service::install(&bind)?;
            println!("embarch-core installed and started as a background service, bound to {bind}.");
        }
        Command::Uninstall => {
            service::uninstall()?;
            println!("embarch-core service stopped and removed.");
        }
        Command::Start => {
            service::start()?;
            println!("embarch-core service started.");
        }
        Command::Stop => {
            service::stop()?;
            println!("embarch-core service stopped.");
        }
        Command::Update { new_exe } => {
            service::update(&new_exe)?;
            println!("embarch-core service updated and restarted.");
        }
        Command::DetectDevBench => {
            let port = embarch_topology::hardware::resolve_dev_bench_port()?;
            println!("{}", port.port_name);
            println!(
                "  detected_by: {}\n  serial: {:?}\n  product: {:?}\n  interface: {:?}",
                port.detected_by, port.serial_number, port.product, port.interface
            );
        }
        Command::Logs { tail } => {
            print_log_tail(tail)?;
        }
    }

    Ok(())
}

/// Sets up `tracing` to write to both stderr (unchanged from before this
/// decision) and a daily-rolling file under
/// `token_store::local_data_dir()?.join("logs")` — `%ProgramData%\embarch\logs`
/// / `/var/lib/embarch/logs` (§3 decision 16, `embarch-core/design.md`),
/// retaining the last 7 daily files. Runs once, unconditionally, at the very
/// top of `main`, before `Cli::parse()` or dispatch on the subcommand — so
/// both entry paths that share `build_runtime()` (a plain foreground `Run`,
/// and `service::windows`'s SCM-dispatched callback, which is invoked from
/// inside the same `Run` match arm below) are covered by construction, not
/// by each having to remember to call this themselves.
///
/// Setting up the file writer is allowed to fail without taking the whole
/// CLI down with it — e.g. an unprivileged human running `detect-dev-bench`
/// or `logs` itself on Linux, where `/var/lib/embarch` isn't writable
/// without root (same caveat `token_store.rs`'s own tests already note).
/// stderr output, which every subcommand already depended on before this
/// decision existed, must keep working regardless; a file-logging setup
/// failure is itself reported once, after falling back, rather than silently
/// swallowed.
fn init_tracing() {
    match build_log_file_writer() {
        Ok(file_writer) => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr.and(file_writer))
                .init();
        }
        Err(e) => {
            tracing_subscriber::fmt::init();
            tracing::warn!(
                "failed to set up daily-rolling log file, continuing with stderr only: {e:?}"
            );
        }
    }
}

/// Builds the `tracing-appender` daily-rolling file writer §3 decision 16
/// specifies: `filename_prefix(LOG_FILE_PREFIX)` + `Rotation::DAILY` names
/// each day's file `<LOG_FILE_PREFIX>.<yyyy-MM-dd>` (`RollingFileAppender`
/// implements `MakeWriter` directly, so no `NonBlocking` wrapper/worker
/// thread is needed — `tracing`'s call sites here are never so hot that a
/// blocking file write matters). `max_log_files(7)` deletes the oldest
/// matching file once an 8th day's worth exist, keeping the most recent 7.
fn build_log_file_writer() -> anyhow::Result<tracing_appender::rolling::RollingFileAppender> {
    let log_dir = token_store::local_data_dir()?.join("logs");
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .max_log_files(7)
        .build(&log_dir)
        .with_context(|| format!("failed to initialize rolling log file appender in {}", log_dir.display()))
}

/// `Command::Logs`'s implementation: finds the current daily log file and
/// prints its last `tail` lines. "Current" is resolved by picking the
/// lexicographically largest filename among everything in the log directory
/// that starts with `LOG_FILE_PREFIX.` — since `tracing-appender`'s date
/// format is ISO (`yyyy-MM-dd`), lexicographic and chronological order agree,
/// so this needs no date parsing of its own. Pure local file read — same
/// no-hardware-access posture as `detect-dev-bench`.
fn print_log_tail(tail: usize) -> anyhow::Result<()> {
    let log_dir = token_store::local_data_dir()?.join("logs");

    let candidates: Vec<PathBuf> = std::fs::read_dir(&log_dir)
        .with_context(|| format!("failed to read log directory {}", log_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();

    let latest = latest_log_file(&candidates, LOG_FILE_PREFIX)
        .with_context(|| format!("no log files found in {}", log_dir.display()))?;

    let contents = std::fs::read_to_string(latest)
        .with_context(|| format!("failed to read log file {}", latest.display()))?;

    for line in tail_lines(&contents, tail) {
        println!("{line}");
    }

    Ok(())
}

/// Pure selection logic behind `print_log_tail`, split out so it's
/// unit-testable against a synthesized file list, no real log directory
/// needed — same rationale `embarch_topology::hardware`'s own port-list
/// selection logic already established. Picks the lexicographically largest filename among
/// `candidates` that starts with `<prefix>.`; since `tracing-appender`'s date
/// format is ISO (`yyyy-MM-dd`), lexicographic order agrees with chronological
/// order, so the "most recent" file needs no date parsing of its own.
fn latest_log_file<'a>(candidates: &'a [PathBuf], prefix: &str) -> Option<&'a PathBuf> {
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
/// `n` lines — split out from `print_log_tail` for the same reason as
/// `latest_log_file` above.
fn tail_lines(contents: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

/// Build the router and serve it until `shutdown` resolves. `pub(crate)` so
/// `service::windows`'s SCM-dispatched path can drive the exact same server
/// startup, just with a real shutdown signal wired to a Stop control event
/// instead of one that never fires.
pub(crate) async fn serve(
    bind: String,
    port: u16,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let token = token_store::resolve_token()?;

    let state = api::AppState::new(token);

    let app = api::build_router(state);

    let addr = format!("{bind}:{port}");
    tracing::info!("embarch-core listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

/// The plain foreground case (manual `embarch-core.exe run` at a console, or
/// the whole story on Linux/macOS): run forever, no shutdown signal.
async fn run(bind: String, port: u16) -> anyhow::Result<()> {
    serve(bind, port, std::future::pending()).await
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
