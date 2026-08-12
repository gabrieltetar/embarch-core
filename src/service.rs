use anyhow::{Context, Result};
use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceManager, ServiceStartCtx,
    ServiceStopCtx, ServiceUninstallCtx,
};
use std::ffi::OsString;

const SERVICE_LABEL: &str = "com.embarch.core";

/// Detects the OS's native service manager: Windows Service (via sc.exe) on
/// Windows, systemd on Linux, launchd on macOS. Same call either way — this
/// is the one place OS-specific code lives, and it's the crate's job, not ours.
fn manager() -> Result<Box<dyn ServiceManager>> {
    <dyn ServiceManager>::native().context("could not detect a service manager for this OS")
}

/// Register embarch-core as a background OS service (pointing back at this
/// same binary with the `run` subcommand) and start it.
pub fn install() -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    let mgr = manager()?;

    let exe = std::env::current_exe().context("could not determine this executable's path")?;

    let explicit_token = std::env::var("EMBARCH_TOKEN").ok();
    if explicit_token.is_none() {
        eprintln!(
            "EMBARCH_TOKEN not set at install time — the installed service will use the \
             auto-generated machine-wide token file instead."
        );
    }
    let environment = explicit_token
        .clone()
        .map(|token| vec![("EMBARCH_TOKEN".to_string(), token)]);

    mgr.install(ServiceInstallCtx {
        label: label.clone(),
        program: exe,
        args: vec![OsString::from("run")],
        contents: None,
        username: None,
        working_directory: None,
        environment,
        autostart: true,
        restart_policy: RestartPolicy::OnFailure {
            delay_secs: Some(5),
            max_retries: Some(5),
            reset_after_secs: Some(3600),
        },
    })
    .context("failed to install embarch-core as a service")?;

    #[cfg(windows)]
    if let Some(token) = explicit_token {
        set_windows_service_environment(SERVICE_LABEL, &token)
            .context("failed to set the installed service's Windows registry environment")?;
    }

    mgr.start(ServiceStartCtx { label })
        .context("service installed, but failed to start")?;

    Ok(())
}

/// Writes the `Environment` `REG_MULTI_SZ` value under this service's own
/// registry key (`HKLM\SYSTEM\CurrentControlSet\Services\<service_name>`),
/// since `sc.rs` (the Windows backend behind `ServiceManager`) never reads
/// `ServiceInstallCtx.environment` — unlike `systemd.rs`/`launchd.rs`, which
/// already honor that field on Linux/macOS. Only called when an explicit
/// `EMBARCH_TOKEN` was set at install time; an installed service with none
/// set just uses `token_store`'s auto-generated machine-wide file like any
/// other invocation.
#[cfg(windows)]
fn set_windows_service_environment(service_name: &str, token: &str) -> Result<()> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (services, _) = hklm
        .create_subkey("SYSTEM\\CurrentControlSet\\Services")
        .context("failed to open HKLM\\SYSTEM\\CurrentControlSet\\Services")?;
    let (service_key, _) = services
        .create_subkey(service_name)
        .with_context(|| format!("failed to open the {service_name} service registry key"))?;

    let entries = vec![format!("EMBARCH_TOKEN={token}")];
    service_key
        .set_value("Environment", &entries)
        .context("failed to write the Environment registry value")?;

    Ok(())
}

/// Start the already-installed background service.
///
/// Exists so `embarch-umbrella`'s `up` doesn't have to re-derive per-OS
/// service control (`sc` vs `systemctl` vs `launchctl`) in a second codebase
/// — the `service-manager` crate already encapsulates exactly that, and this
/// is the binary that already depends on it (embarch-umbrella/design.md §3
/// decisions 4/7).
///
/// Not idempotent in any guaranteed way: starting an already-running service
/// is an error on some backends and a no-op on others, and that difference is
/// the crate's to hide or not, not something to paper over here with a
/// racy is-it-running check.
pub fn start() -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    manager()?
        .start(ServiceStartCtx { label })
        .context("failed to start the embarch-core service (is it installed?)")?;
    Ok(())
}

/// Stop the running background service, leaving it installed.
pub fn stop() -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    manager()?
        .stop(ServiceStopCtx { label })
        .context("failed to stop the embarch-core service (is it installed and running?)")?;
    Ok(())
}

/// Stop and remove the background service.
pub fn uninstall() -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    let mgr = manager()?;

    // Best-effort stop; don't fail uninstall just because it wasn't running.
    let _ = mgr.stop(ServiceStopCtx { label: label.clone() });

    mgr.uninstall(ServiceUninstallCtx { label })
        .context("failed to uninstall embarch-core service")?;

    Ok(())
}

/// Windows-only: the actual Service Control Manager handshake. `install()`
/// above registers `run` as the service's command, same as on Linux/macOS —
/// but unlike systemd (`Type=simple`) or launchd, a Windows service *must*
/// call back into `StartServiceCtrlDispatcherW` promptly after launch and
/// report `SERVICE_RUNNING`, or SCM kills the start attempt after a 30-second
/// timeout (Win32 error 1053, System log event 7009/7000). `run()` never did
/// that — it just started the HTTP server like a plain console program,
/// which worked fine when a human ran it directly and failed every time SCM
/// launched it. Caught for real installing Core as an actual boot service on
/// this machine for the first time (embarch-umbrella/milestone-6.md §3.8).
#[cfg(windows)]
pub mod windows {
    use std::ffi::OsString;
    use std::sync::Mutex;
    use std::time::Duration;

    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher, Error as WsError};

    use super::SERVICE_LABEL;

    define_windows_service!(ffi_service_main, service_main);

    /// `Some(result)` when this process really was launched by SCM: blocks
    /// for the service's whole lifetime, then returns how it exited. `None`
    /// when it wasn't — a human running `embarch-core.exe run` directly at a
    /// console, same as on any other OS — which is
    /// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` (raw OS error 1063) from
    /// `StartServiceCtrlDispatcherW`, the one failure this treats as "not
    /// SCM" rather than a real error to report.
    pub fn try_dispatch() -> Option<anyhow::Result<()>> {
        match service_dispatcher::start(SERVICE_LABEL, ffi_service_main) {
            Ok(()) => Some(Ok(())),
            Err(WsError::Winapi(e)) if e.raw_os_error() == Some(1063) => None,
            Err(e) => Some(Err(e.into())),
        }
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_service() {
            tracing::error!("embarch-core windows service exited with an error: {e:#}");
        }
    }

    fn service_status(state: ServiceState, exit_code: ServiceExitCode) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if matches!(state, ServiceState::Running) {
                ServiceControlAccept::STOP
            } else {
                ServiceControlAccept::empty()
            },
            exit_code,
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        }
    }

    fn run_service() -> anyhow::Result<()> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown_tx = Mutex::new(Some(shutdown_tx));

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop => {
                    if let Some(tx) = shutdown_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_LABEL, event_handler)?;

        // The handshake SCM's 30-second start timeout is actually waiting
        // for — everything before this line has to stay fast.
        status_handle.set_service_status(service_status(ServiceState::Running, ServiceExitCode::Win32(0)))?;

        // A fresh runtime, not a nested one: `try_dispatch` is called from
        // plain (non-async) `main`, before any Tokio runtime exists, so
        // there's nothing to nest inside.
        let result = tokio::runtime::Runtime::new()?.block_on(crate::serve(
            "0.0.0.0".to_string(),
            crate::DEFAULT_PORT,
            async {
                let _ = shutdown_rx.await;
            },
        ));

        status_handle.set_service_status(service_status(
            ServiceState::Stopped,
            match &result {
                Ok(()) => ServiceExitCode::Win32(0),
                Err(_) => ServiceExitCode::ServiceSpecific(1),
            },
        ))?;

        result
    }
}
