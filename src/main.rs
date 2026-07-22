mod api;
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
