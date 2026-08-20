mod api;
mod chip_resolve;
mod dev_bench;
mod dev_bench_link;
mod elevate;
mod hardware;
mod serial;
mod service;
mod study;
mod token_store;

use clap::{Parser, Subcommand};
use std::future::Future;
use std::path::PathBuf;

/// Shared with `service::windows`, which has no CLI args of its own to read
/// a `--port` from (SCM launches the installed service with just `run`, no
/// flags) — one constant instead of two places that could disagree.
pub(crate) const DEFAULT_PORT: u16 = 4884;

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
        /// Bind address. Use 0.0.0.0 (the default) so this is reachable
        /// from WSL2 or a LAN, not just from processes on this same box.
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    /// Install embarch-core as a background OS service (Windows Service
    /// via sc.exe, or systemd on Linux) and start it immediately.
    Install,
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
}

// Deliberately not `#[tokio::main]`: the Windows service path below needs to
// call `StartServiceCtrlDispatcherW` (via `service::windows::try_dispatch`)
// as a plain blocking call from `main`'s own thread, before any Tokio
// runtime exists — SCM expects that handshake promptly, and everything
// after it (including the actual async server) runs on a runtime built
// fresh inside the service callback, never nested inside another one.
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Command::Run { bind, port } => {
            #[cfg(windows)]
            if let Some(result) = service::windows::try_dispatch() {
                return result;
            }
            tokio::runtime::Runtime::new()?.block_on(run(bind, port))?;
        }
        Command::Install => {
            service::install()?;
            println!("embarch-core installed and started as a background service.");
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
            let port = dev_bench::detect()?;
            println!("{}", port.port_name);
            println!(
                "  detected_by: {}\n  serial: {:?}\n  product: {:?}\n  interface: {:?}",
                port.detected_by, port.serial_number, port.product, port.interface
            );
        }
    }

    Ok(())
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
