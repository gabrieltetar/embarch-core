mod api;
mod dev_bench;
mod hardware;
mod serial;
mod service;
mod token_store;

use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "embarch-core")]
#[command(about = "EmbArch Core — the OS-level service that talks to debug/flash hardware")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP service in the foreground. This is also what the
    /// installed background service invokes — `install` just registers
    /// this same command with the OS.
    Run {
        /// Bind address. Use 0.0.0.0 (the default) so this is reachable
        /// from WSL2 or a LAN, not just from processes on this same box.
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(long, default_value_t = 4884)]
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
    /// Print which serial port embarch-dev-bench is on, using the same
    /// SEGGER-VID detection `GET /dev-bench/port` serves — a human's way to
    /// check the bench is visible without an HTTP client or a running service.
    DetectDevBench,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Command::Run { bind, port } => run(bind, port).await?,
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

async fn run(bind: String, port: u16) -> anyhow::Result<()> {
    let token = token_store::resolve_token()?;

    let state = api::AppState {
        token,
        hw_lock: Arc::new(Mutex::new(())),
    };

    let app = api::build_router(state);

    let addr = format!("{bind}:{port}");
    tracing::info!("embarch-core listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
