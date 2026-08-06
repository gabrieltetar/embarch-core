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
