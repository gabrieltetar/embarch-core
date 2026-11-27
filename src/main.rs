mod analog_tap;
mod analog_source;
mod api;
mod bootload;
mod call_log;
mod capture;
mod chip_resolve;
mod confirmations;
mod definitions;
mod dev_bench_link;
mod dev_bench_log;
mod elevate;
mod exchange;
mod flash_backend;
mod hardware;
mod instrument;
mod keep_awake;
mod host_counts;
mod host_parse;
mod host_study;
mod lease;
mod live;
mod logs;
mod manual;
mod measurements;
mod serial;
mod session_run;
mod service;
mod run_log;
mod safe_off;
mod signing;
mod outpost_load;
mod outpost_manifest;
mod outpost_preflight;
mod port_hold;
mod publish;
mod readback;
mod smoke_check;
mod stream_store;
mod test_executor;
mod test_publish;
mod test_run_record;
mod test_runs;
mod study;
mod token_store;

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::fmt::writer::MakeWriterExt;

/// Shared with `service::windows`'s fallback (`run_service` uses this only if
/// `BIND_PORT` was somehow never set — a call path that shouldn't exist in
/// practice, since `try_dispatch` always sets it first) — one constant
/// instead of two places that could disagree.
pub(crate) const DEFAULT_PORT: u16 = 4884;

/// Loopback-only (decision 6's amendment, 2026-08-15): reachable
/// only from processes on this same machine unless something explicitly
/// widens it. Shared between `Run`'s and `Install`'s own `--bind` flags so
/// they can't drift apart.
pub(crate) const DEFAULT_BIND: &str = "127.0.0.1";

#[derive(Parser)]
// `--version` names the commit this binary was built from (decision 100); the
// stamp is `build.rs`, shared with `embarch-api` and `embarch-umbrella`
// (`embarch-umbrella` decision 57). The crate version stays the first field
// after the name, which is what `embarch doctor` reads.
#[command(name = "embarch-core", version = env!("EMBARCH_BUILD_VERSION"))]
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
        /// Bind address. Loopback-only by default (decision 6's
        /// amendment) — reachable only from this same machine unless widened
        /// explicitly. `embarch-umbrella setup` passes an explicit `--bind`
        /// to `install` for the one topology (WSL2⟷Windows, or a genuinely
        /// remote Core) that actually needs a wider address.
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// The attested Core: keep the signing key, both logs and the
        /// identity store in `<data dir>/store/`, which `run` never creates
        /// (the elevated install does). Reported as `service`.
        #[arg(long, conflicts_with = "store")]
        service: bool,
        /// A development Core: keep the token pair, signing key, logs,
        /// results and identity store in this folder, created if missing.
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },
    /// Install embarch-core as a background OS service (Windows Service
    /// via sc.exe, or systemd on Linux) and start it immediately.
    Install {
        /// Bind address the installed service runs with — baked into its
        /// registered start command (survives reboots without umbrella
        /// having to re-run this), not just this one invocation. Same
        /// loopback-only default as `run`; `embarch-umbrella setup` passes
        /// `recommended_bind_address(TopologyClass)`'s answer explicitly for
        /// wsl-host/remote (decision 6's amendment).
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: String,
        /// Register the service. Without it nothing is registered: a source
        /// build runs as a development Core. Refuses unless
        /// `<data dir>/installed-release.json` lists this binary's SHA-256.
        #[arg(long)]
        service: bool,
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
    /// List probe-rs target names, optionally narrowed by a case-insensitive
    /// substring — decision 34. Pure enumeration: no probe is
    /// opened and no board need be attached, same posture as
    /// `detect-dev-bench`. This is where the target name for a **new
    /// `SOC_TO_CHIP` entry** comes from when a SoC isn't in the built-in
    /// table. That table is compiled into this binary (`src/chip_resolve.rs`)
    /// and has no config file anywhere: adding a SoC means editing Core's
    /// source, rebuilding and redeploying. `embarch-api` has no per-project
    /// override field for it. Replaces the `cargo install probe-rs-tools`
    /// detour that onboarding used to require for exactly this.
    ChipList {
        /// Case-insensitive substring; omit to list every target. Substring,
        /// not prefix — someone hunting a "54L15" should find `nRF54L15_xxAA`.
        filter: Option<String>,
    },
    /// Report which program Core would use to flash a given chip, and where
    /// it found it — decision 36. Pure lookup: no probe is
    /// opened, nothing is flashed and no board need be attached, same posture
    /// as `detect-dev-bench` and `chip-list`.
    ///
    /// This is the answer to "is the vendor tool installed?" without
    /// discovering it at the moment of a flash, which is the worst time to
    /// find out. On a chip family Core refuses probe-rs for, this prints the
    /// same refusal a flash would fail with, including what to install.
    FlashBackend {
        /// The probe-rs chip name, as `embarch-api` would send it (e.g.
        /// `nRF54L15`). Omit to report every family this build treats
        /// specially, which is the quicker "is my bench ready" check.
        #[arg(long)]
        chip: Option<String>,
    },
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
    /// Print the last N lines of the dev-bench debug log (§3 decision 37) —
    /// what `embarch-dev-bench`'s own firmware logged over the link, including
    /// its boot record and any fatal-error dump, with each line stamped by
    /// arrival and by the study it belongs to. Separate from `logs` above on
    /// purpose: that one is Core's account of a run, this one is the bench's.
    /// Pure local file read, no hardware access.
    DevBenchLogs {
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
/// is a real fix, not a defensive default: `embarch-study-designer`
/// decision 46 tracked a `Study`/`StepResult` stack-overflow risk from large
/// fixed-capacity `heapless` types, previously reproduced only in debug
/// builds and "confirmed release-build-safe" by that decision's own
/// mitigation. That confirmation didn't hold: the first real `run_study` POST
/// against this milestone's GATT-extended `StepResult` (`embarch-study-designer`
/// decisions 31/32's
/// `gatt_services`/`gatt_activity`, larger than anything sized when decision 46
/// was written) crashed the real, `--release` Windows service with
/// `STATUS_STACK_OVERFLOW` (0xc00000fd) — a first real release-build
/// occurrence, on `run_study`'s `spawn_blocking(run_study_to_completion)`
/// specifically (`study.rs`), not on a debug build's default `tokio-rt-worker`
/// stack as decision 46 had only ever seen before (`embarch-study-designer`
/// decision 49 is where this crash and the 64 MiB figure are recorded on that
/// side). Matches the size
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
        Command::Run { bind, port, service, store } => {
            token_store::init_store(match (service, store) {
                (true, _) => token_store::StoreLayout::Service,
                (false, Some(dir)) => token_store::StoreLayout::Development(dir),
                (false, None) => token_store::StoreLayout::Legacy,
            })?;
            #[cfg(windows)]
            if let Some(result) = service::windows::try_dispatch(bind.clone(), port) {
                return result;
            }
            build_runtime()?.block_on(run(bind, port))?;
        }
        Command::Install { service: false, .. } => {
            service::install_development();
        }
        Command::Install { bind, service: true } => {
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
        Command::FlashBackend { chip } => {
            // A short, representative list rather than every chip probe-rs
            // knows: the question this answers is "which tool does this bench
            // need", and that is decided per family.
            let chips: Vec<String> = match &chip {
                Some(c) => vec![c.clone()],
                None => ["nRF54L15", "nRF52840", "esp32c5"].iter().map(|s| s.to_string()).collect(),
            };
            let mut any_err = false;
            for c in &chips {
                match flash_backend::discover(c) {
                    Ok(backend) => {
                        let where_ = match &backend {
                            flash_backend::Backend::ProbeRs => "built in".to_string(),
                            flash_backend::Backend::JLink { exe }
                            | flash_backend::Backend::NrfUtil { exe } => exe.display().to_string(),
                        };
                        println!("{c}\t{}\t{where_}", backend.name());
                    }
                    Err(e) => {
                        any_err = true;
                        println!("{c}\tUNAVAILABLE");
                        eprintln!("\n{c}: {e:?}\n");
                    }
                }
            }
            if any_err {
                std::process::exit(1);
            }
        }
        Command::ChipList { filter } => {
            let names = chip_resolve::chip_list(filter.as_deref());
            for name in &names {
                println!("{name}");
            }
            // To stderr, so `chip-list | grep` and friends see only names on
            // stdout — this subcommand's output is meant to be piped.
            eprintln!(
                "{} target{} listed{}.",
                names.len(),
                if names.len() == 1 { "" } else { "s" },
                match &filter {
                    Some(f) => format!(" matching {f:?}"),
                    None => String::new(),
                }
            );
        }
        Command::Logs { tail } => {
            for line in logs::read_recent(tail)? {
                println!("{line}");
            }
        }
        Command::DevBenchLogs { tail } => {
            for line in
                logs::read_recent_with_prefix(dev_bench_log::DEV_BENCH_LOG_FILE_PREFIX, tail)?
            {
                println!("{line}");
            }
        }
    }

    Ok(())
}

/// Sets up `tracing` to write to both stderr (unchanged from before this
/// decision) and a daily-rolling file under
/// `token_store::local_data_dir()?.join("logs")` — `%ProgramData%\embarch\logs`
/// / `/var/lib/embarch/logs` (decision 16),
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
///
/// Both arms route through [`build_fallback_subscriber`] (this arm) or an
/// equivalent explicit `.with_writer(...)` (the success arm above) rather
/// than the bare, zero-config `tracing_subscriber::fmt::init()` — that
/// entry point's default `MakeWriter` is `std::io::stdout`, not stderr
/// (`tracing_subscriber` 0.3.23, `SubscriberBuilder<..., W = fn() -> io::Stdout>`),
/// which is what previously put this arm's own "continuing with stderr
/// only" warning on stdout: `--version`'s output is `stdout` alone
/// (clap's built-in handling of the flag), so anything else init_tracing
/// put there rode along in front of it, unparseable
/// (core/tasks/015, `embarch-umbrella`'s check 1 read exactly this). ANSI
/// colouring turned out not to be a second, independent bug: this crate
/// never probes whether a writer is a tty (`tracing_subscriber`'s `fmt`
/// layer decides `is_ansi` purely from the "ansi" cargo feature and the
/// `NO_COLOR` env var — see `Layer::default()` — not from the writer at
/// all), so the escapes in the captured output were simply riding on the
/// same misrouted text; moving that text off stdout removes them from
/// stdout too, with nothing left to change separately. See `decisions.md`
/// for the numbered entry.
fn init_tracing() {
    match build_log_file_writer() {
        Ok(file_writer) => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr.and(file_writer))
                .init();
        }
        Err(e) => {
            let subscriber = build_fallback_subscriber(std::io::stderr);
            tracing::subscriber::set_global_default(subscriber)
                .expect("no global tracing subscriber should be installed yet");
            tracing::warn!(
                "failed to set up daily-rolling log file, continuing with stderr only: {e:?}"
            );
        }
    }
}

/// Builds the fallback-arm subscriber with an explicit, injectable writer
/// instead of the bare `tracing_subscriber::fmt::init()` this replaced
/// (whose writer can't be substituted at all — that is the whole point of
/// a zero-config entry point, and exactly why the old code silently
/// defaulted to stdout). Generic so a test can hand it an in-memory writer
/// and observe that events actually flow through the writer that gets
/// passed in, without needing a real unwritable log directory to reach
/// this arm, and without installing a second global default subscriber
/// in the same process (`tracing`'s global default can only be set once).
fn build_fallback_subscriber<W>(writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::fmt().with_writer(writer).finish()
}

/// Builds the `tracing-appender` daily-rolling file writer §3 decision 16
/// specifies: `filename_prefix(logs::LOG_FILE_PREFIX)` + `Rotation::DAILY`
/// names each day's file `<LOG_FILE_PREFIX>.<yyyy-MM-dd>` (`RollingFileAppender`
/// implements `MakeWriter` directly, so no `NonBlocking` wrapper/worker
/// thread is needed — `tracing`'s call sites here are never so hot that a
/// blocking file write matters). `max_log_files(7)` deletes the oldest
/// matching file once an 8th day's worth exist, keeping the most recent 7 —
/// this same file is what `GET /logs/recent` (`api.rs`)
/// and `logs::read_recent` read, not a second,
/// size-capped mechanism — `embarch-ui` decision 7 describes Core's
/// retention that way, but this crate has only ever had the daily-rolling
/// file above.
fn build_log_file_writer() -> anyhow::Result<tracing_appender::rolling::RollingFileAppender> {
    let log_dir = token_store::local_data_dir()?.join("logs");
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(logs::LOG_FILE_PREFIX)
        .max_log_files(7)
        .build(&log_dir)
        .with_context(|| format!("failed to initialize rolling log file appender in {}", log_dir.display()))
}

/// How long a graceful stop waits for the cancelled study's teardown, and then for `hw_lock`,
/// before it runs the safe-off anyway (decision 156). Chosen, not measured: a teardown is a few
/// instrument lines and a device study's own cancel is bounded (decision 128); a stop that waits
/// longer than this is one the SCM would kill.
const STOP_STUDY_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const STOP_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What a graceful stop waits for before the safe-off.
#[derive(Clone, Copy)]
struct StopWaits {
    study: std::time::Duration,
    hw_lock: std::time::Duration,
}

/// Binds, switches the bench's declared outputs off, then serves until `shutdown` resolves and
/// stops gracefully (decision 156). `pub(crate)` so `service::windows`'s SCM-dispatched path
/// drives the exact same start and stop, with a real shutdown signal wired to a Stop control event.
///
/// **The order is the contract:** the port is bound first, so a second Core fails here having
/// sent nothing to the bench; then the tokens, logs and state; then the safe-off and the `start`
/// entry; then the first route is served.
pub(crate) async fn serve(
    bind: String,
    port: u16,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| bind_failure(&addr))?;

    let tokens = token_store::resolve_tokens()?;

    let run_log = crate::run_log::RunLog::resolve()?;
    run_log::install(run_log.clone());
    let state = api::AppState::with_tokens(tokens, signing::Sealer::resolve()?, run_log);

    // A Test run the last Core was holding when it stopped is sealed as interrupted (decision 144).
    test_run_record::seal_interrupted(&state.sealer, &state.run_log);
    let bench: Arc<dyn safe_off::Bench> = Arc::new(safe_off::LiveBench { captures: state.captures.clone() });
    serve_bound(listener, state, bench, shutdown, StopWaits { study: STOP_STUDY_WAIT, hw_lock: STOP_LOCK_WAIT }).await
}

/// What a failed bind says: the address, and that another Core may hold it.
fn bind_failure(addr: &str) -> String {
    format!("could not bind {addr}: another embarch-core may already hold this address; nothing was sent to the bench")
}

/// [`serve`] from a bound listener and a built state, over any [`safe_off::Bench`], so the start
/// and stop sequences are testable with no port.
async fn serve_bound(
    listener: tokio::net::TcpListener,
    state: api::AppState,
    bench: Arc<dyn safe_off::Bench>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    waits: StopWaits,
) -> anyhow::Result<()> {
    let call_log = state.call_log.clone();
    let mode = token_store::mode();

    let results = safe_off_blocking(&bench).await;
    match call_log.record_start(mode, &results) {
        Ok(seq) => tracing::info!("call log: start entry {seq} written, {} safe-off result(s)", results.len()),
        // The same rule as a call: today a log that cannot be written is logged and Core serves on (decision 122).
        Err(e) => tracing::error!("could not enter the start in the call log: {e:#}"),
    }

    let app = api::build_router(state.clone());
    tracing::info!("embarch-core listening on {}", listener.local_addr().map(|a| a.to_string()).unwrap_or_default());
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await?;

    // The last request has been answered. A study in flight is cancelled and its teardown runs, then
    // the safe-off, the fold flush and the stop entry (decision 156).
    let note = stop_studies(&state, waits).await;
    let guard = tokio::time::timeout(waits.hw_lock, state.hw_lock.clone().lock_owned()).await.ok();
    let note = call_log::StopNote { hw_lock_held: guard.is_none(), ..note };
    let results = safe_off_blocking(&bench).await;
    drop(guard);
    if let Err(e) = call_log.record_stop(mode, &note, &results) {
        tracing::error!("could not enter the stop in the call log: {e:#}");
        // A clean stop still writes the pending fold (decision 122).
        call_log.flush();
    }
    Ok(())
}

async fn safe_off_blocking(bench: &Arc<dyn safe_off::Bench>) -> Vec<safe_off::SafeOffResult> {
    let bench = bench.clone();
    match tokio::task::spawn_blocking(move || safe_off::run(bench.as_ref())).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("the safe-off panicked: {e}");
            Vec::new()
        }
    }
}

/// Raises the cancel flag of the host study, Test run or device study in flight and waits, up to
/// `waits.study`, for it to release `study_lock` and `run_lock`: its teardown has run by then.
async fn stop_studies(state: &api::AppState, waits: StopWaits) -> call_log::StopNote {
    let mut cancelled = None;
    for slot in [&state.host_cancel, &state.device_cancel] {
        if let Some((id, flag)) = slot.lock().unwrap().as_ref() {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            cancelled.get_or_insert_with(|| id.clone());
        }
    }
    let busy = || state.study_lock.lock().unwrap().is_some() || state.run_lock.lock().unwrap().is_some();
    let deadline = tokio::time::Instant::now() + waits.study;
    while busy() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let study_ended = !busy();
    if !study_ended {
        tracing::error!("a study did not end within {:?} of the stop; the safe-off runs regardless", waits.study);
    }
    call_log::StopNote { cancelled_study: cancelled, study_ended, hw_lock_held: false }
}

/// The plain foreground case (manual `embarch-core.exe run` at a console, or the whole story on
/// Linux/macOS): serve until Ctrl-C, or SIGTERM on Unix, then stop gracefully (decision 156).
async fn run(bind: String, port: u16) -> anyhow::Result<()> {
    serve(bind, port, shutdown_signal()).await
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM (what `systemctl stop` sends). A handler that cannot be
/// installed is logged and that signal is then never waited on.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("could not listen for Ctrl-C: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("could not listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => tracing::info!("Ctrl-C: stopping"),
        _ = term => tracing::info!("SIGTERM: stopping"),
    }
}

// Log-selection unit tests (`latest_log_file`/`tail_lines`)
// moved to `logs.rs` alongside the code they test.

#[cfg(test)]
mod init_tracing_tests {
    use super::build_fallback_subscriber;
    use std::sync::{Arc, Mutex};

    /// An in-memory `MakeWriter` so the test can observe where
    /// `build_fallback_subscriber` actually sends bytes, without touching a
    /// real fd — real stdout/stderr can't be captured for assertions from
    /// inside a `cargo test` process, and that is exactly why this seam
    /// exists (see `build_fallback_subscriber`'s doc comment).
    #[derive(Clone, Default)]
    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RecordingWriter {
        type Writer = RecordingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Pins the fallback arm's writer choice: `init_tracing`'s failure arm
    /// must route events through whatever `MakeWriter` it is handed (in
    /// production, `std::io::stderr` — never the bare, stdout-defaulting
    /// `tracing_subscriber::fmt::init()` this replaced, which offered no
    /// such seam at all). This is what a packaging script or `--version`
    /// caller relies on: nothing this arm emits can land on stdout, because
    /// there is no code path here that can reach it.
    #[test]
    fn fallback_subscriber_routes_through_its_injected_writer() {
        let writer = RecordingWriter::default();
        let subscriber = build_fallback_subscriber(writer.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("continuing with stderr only: probe");
        });
        let captured = String::from_utf8(writer.0.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains("continuing with stderr only: probe"),
            "expected the warning to reach the injected writer, got: {captured:?}"
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use embarch_topology::hardware::{Instrument, InstrumentProtocol, SignalLink};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TOKEN: &str = "test-token";
    const SHORT: StopWaits = StopWaits { study: Duration::from_millis(300), hw_lock: Duration::from_millis(300) };

    /// A bench with one instrument and no signals that records, per line sent, the call log's
    /// length at that moment: what had been answered before the line went out.
    struct Fake {
        log: call_log::CallLog,
        sent: Mutex<Vec<(String, u64)>>,
    }

    impl safe_off::Bench for Fake {
        fn instruments(&self) -> anyhow::Result<Vec<Instrument>> {
            let mk = |name: &str, lines: &[&str]| Instrument {
                name: name.into(),
                protocol: InstrumentProtocol::Scpi,
                port: "1a86:7523".parse().unwrap(),
                baud: 9600,
                write_terminator: "\n".into(),
                read_terminator: "\r\n".into(),
                min_interval_ms: 0,
                ack_ok: None,
                ack_err: None,
                safe_off: lines.iter().map(|l| l.to_string()).collect(),
                metrology: Vec::new(),
                calibration: None,
            };
            Ok(vec![mk("eload", &["INP OFF"]), mk("dmm", &[])])
        }
        fn signals(&self) -> anyhow::Result<Vec<SignalLink>> {
            Ok(Vec::new())
        }
        fn send_instrument(&self, inst: &Instrument, line: &str) -> safe_off::Sent {
            let len = self.log.mark(false).unwrap().len;
            self.sent.lock().unwrap().push((format!("{}: {line}", inst.name), len));
            safe_off::Sent::Reply("ok".into())
        }
        fn send_signal(&self, _: &SignalLink, _: &str) -> safe_off::Sent {
            unreachable!("no signal is declared")
        }
    }

    /// One request over a plain socket, so the server is driven as a client would: the answer's status line.
    async fn request(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out.lines().next().unwrap_or_default().to_string()
    }

    fn entries(log: &call_log::CallLog) -> Vec<serde_json::Value> {
        log.lines().unwrap().into_iter().map(|l| l["entry"].clone()).collect()
    }

    /// Runs `serve_bound` to the point `body` says, then shuts it down and returns when it ends.
    async fn drive<F, Fut>(state: api::AppState, fake: Arc<Fake>, body: F)
    where
        F: FnOnce(std::net::SocketAddr, api::AppState) -> Fut,
        Fut: Future<Output = ()>,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_bound(listener, state.clone(), fake, async move { let _ = rx.await; }, SHORT));
        // The start entry is written before the first route is served: wait for it.
        for _ in 0..100 {
            if request(addr, "GET", "/call-log", "").await.starts_with("HTTP/1.1 200") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        body(addr, state).await;
        tx.send(()).unwrap();
        server.await.unwrap().unwrap();
    }

    fn fake_for(state: &api::AppState) -> Arc<Fake> {
        Arc::new(Fake { log: state.call_log.clone(), sent: Mutex::new(Vec::new()) })
    }

    #[tokio::test]
    async fn a_start_sends_every_declared_line_before_any_route_answers_and_a_shutdown_sends_them_again_before_the_stop_entry() {
        let state = api::AppState::new(TOKEN.to_string());
        let fake = fake_for(&state);
        drive(state.clone(), fake.clone(), |addr, _| async move {
            // A lease held by someone else: this gated call is refused and logged, and reaches no board.
            request(addr, "POST", "/lease", r#"{"holder":"other","purpose":"p","ttl_s":60}"#).await;
            assert!(request(addr, "POST", "/flash", "{}").await.contains(" 409 "));
        })
        .await;

        let e = entries(&state.call_log);
        let types: Vec<_> = e.iter().map(|x| x["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(types, ["start", "call", "stop"]);
        // Sent twice, each time with the log as it stood: empty at the start, start and the call at the stop.
        assert_eq!(*fake.sent.lock().unwrap(), [("eload: INP OFF".to_string(), 0), ("eload: INP OFF".to_string(), 2)]);
        for entry in [&e[0], &e[2]] {
            let r = &entry["safe_off"];
            assert_eq!((r[0]["name"].as_str(), r[0]["result"].as_str(), r[0]["reply"].as_str()), (Some("eload"), Some("sent"), Some("ok")));
            assert_eq!((r[1]["name"].as_str(), r[1]["result"].as_str()), (Some("dmm"), Some("no safe-off declared")));
        }
        assert_eq!((e[0]["previous"].as_str(), e[0]["last_images"].as_str()), (Some("log started anew"), Some("unknown")));
        assert_eq!((e[2]["study_ended"].as_bool(), e[2]["cancelled_study"].is_null()), (Some(true), true));
        assert!(state.call_log.verify().unwrap().is_ok());
    }

    #[tokio::test]
    async fn the_next_start_after_a_clean_stop_names_its_time_and_after_a_kill_says_not_a_clean_stop() {
        let state = api::AppState::new(TOKEN.to_string());
        drive(state.clone(), fake_for(&state), |_, _| async {}).await;
        let stopped_at = entries(&state.call_log)[1]["at_utc_ms"].as_u64().unwrap();

        // A second Core on the same log: its start follows a clean stop, then it is killed (the
        // task is dropped, so no stop entry is written), and a third start follows that.
        let again = api::AppState::new(TOKEN.to_string());
        let again = api::AppState { call_log: state.call_log.clone(), ..again };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let killed = tokio::spawn(serve_bound(listener, again.clone(), fake_for(&again), std::future::pending(), SHORT));
        for _ in 0..100 {
            if request(addr, "GET", "/call-log", "").await.starts_with("HTTP/1.1 200") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        killed.abort();
        let _ = killed.await;
        // The log is read again from the file, as a restarted Core would.
        let restarted = api::AppState::new(TOKEN.to_string());
        let restarted = api::AppState { call_log: call_log::CallLog::at(state.call_log.path_dir()), ..restarted };
        drive(restarted.clone(), fake_for(&restarted), |_, _| async {}).await;

        let e = entries(&restarted.call_log);
        let types: Vec<_> = e.iter().map(|x| x["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["start", "stop", "start", "start", "stop"]);
        assert_eq!((e[2]["previous"].as_str(), e[2]["previous_stop_utc_ms"].as_u64()), (Some("clean stop"), Some(stopped_at)));
        assert_eq!((e[3]["previous"].as_str(), e[3]["previous_stop_utc_ms"].as_u64()), (Some("not a clean stop"), Some(stopped_at)));
        assert!(restarted.call_log.verify().unwrap().is_ok(), "the chain still recomputes after a kill");
    }

    #[tokio::test]
    async fn a_study_in_flight_is_cancelled_and_ends_before_the_safe_off_runs() {
        let state = api::AppState::new(TOKEN.to_string());
        let fake = fake_for(&state);
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *state.host_cancel.lock().unwrap() = Some(("st-1".into(), flag.clone()));
        *state.study_lock.lock().unwrap() = Some("st-1".into());
        // The study's thread: on its cancel it runs a teardown (a pause here), then releases the lock.
        let (lock, seen) = (state.study_lock.clone(), flag.clone());
        std::thread::spawn(move || {
            while !seen.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(100));
            *lock.lock().unwrap() = None;
        });
        let (ended_before_send, cancelled) = (Arc::new(Mutex::new(Vec::new())), flag.clone());
        drive(state.clone(), fake.clone(), |_, s| {
            let ended = ended_before_send.clone();
            async move {
                ended.lock().unwrap().push(cancelled.load(std::sync::atomic::Ordering::SeqCst));
                assert!(s.study_lock.lock().unwrap().is_some());
            }
        })
        .await;
        assert_eq!(*ended_before_send.lock().unwrap(), [false], "nothing cancelled the study while the server ran");
        assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
        let stop = entries(&state.call_log).pop().unwrap();
        assert_eq!((stop["cancelled_study"].as_str(), stop["study_ended"].as_bool()), (Some("st-1"), Some(true)));
        assert!(state.study_lock.lock().unwrap().is_none(), "the lock was released before the stop entry");
    }

    #[tokio::test]
    async fn a_study_that_never_ends_is_waited_for_only_so_long_and_the_stop_says_so() {
        let state = api::AppState::new(TOKEN.to_string());
        *state.study_lock.lock().unwrap() = Some("stuck".into());
        let fake = fake_for(&state);
        drive(state.clone(), fake.clone(), |_, _| async {}).await;
        let stop = entries(&state.call_log).pop().unwrap();
        assert_eq!(stop["study_ended"].as_bool(), Some(false));
        assert_eq!(fake.sent.lock().unwrap().len(), 2, "the safe-off ran regardless");
    }

    #[tokio::test]
    async fn a_start_whose_port_is_taken_fails_naming_the_address_and_sends_nothing() {
        let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = held.local_addr().unwrap().port();
        let err = serve("127.0.0.1".to_string(), port, std::future::pending()).await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(&format!("127.0.0.1:{port}")) && text.contains("another embarch-core"), "{text}");
        assert!(text.contains("nothing was sent to the bench"), "{text}");
    }
}
