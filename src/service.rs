use anyhow::{bail, Context, Result};
use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceManager, ServiceStartCtx,
    ServiceStopCtx, ServiceUninstallCtx,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::elevate;

const SERVICE_LABEL: &str = "com.embarch.core";

/// Detects the OS's native service manager: Windows Service (via sc.exe) on
/// Windows, systemd on Linux, launchd on macOS. Same call either way — this
/// is the one place OS-specific code lives, and it's the crate's job, not ours.
fn manager() -> Result<Box<dyn ServiceManager>> {
    <dyn ServiceManager>::native().context("could not detect a service manager for this OS")
}

/// Name of the record the elevated install writes into the data dir, listing
/// the SHA-256 of every binary a signed release installed (decision 157).
pub const RELEASE_RECORD_FILE: &str = "installed-release.json";

/// Linux and macOS: the account the service runs under (the elevated install
/// creates it, decision 157).
const SERVICE_USER: &str = "embarch";

/// Windows: the virtual account named after the service. `sc config obj=`
/// sets it after the create; the `sc` backend ignores `username`.
const WINDOWS_SERVICE_ACCOUNT: &str = "NT SERVICE\\com.embarch.core";

/// The account Core's service runs under, and how it is applied.
#[derive(Debug, PartialEq, Eq)]
enum Account {
    /// Passed as `ServiceInstallCtx.username`.
    User(&'static str),
    /// Applied with `sc config <service> obj= <account>` after the create.
    ScObj(&'static str),
}

fn account_for(windows: bool) -> Account {
    if windows {
        Account::ScObj(WINDOWS_SERVICE_ACCOUNT)
    } else {
        Account::User(SERVICE_USER)
    }
}

/// `sc config` arguments that put the service under its virtual account.
#[cfg_attr(not(windows), allow(dead_code))]
fn account_args(service: &str, account: &str) -> Vec<String> {
    vec!["config".into(), service.into(), "obj=".into(), account.into()]
}

/// Refuses unless the record lists `sha256`. `record` is the file's text, or
/// `None` when it does not exist.
fn check_release_record(record: Option<&str>, sha256: &str, path: &Path) -> Result<()> {
    let Some(text) = record else {
        bail!(
            "{} does not exist: `install --service` registers only a binary a signed release \
             installed. A source build runs as a development Core; see `embarch deploy-core`.",
            path.display()
        );
    };
    let value: serde_json::Value = serde_json::from_str(text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let listed = value["binary_sha256"]
        .as_array()
        .is_some_and(|a| a.iter().any(|h| h.as_str() == Some(sha256)));
    if !listed {
        bail!(
            "{} does not list this binary (sha256 {sha256}): `install --service` registers only \
             a binary a signed release installed. A source build runs as a development Core; \
             see `embarch deploy-core`.",
            path.display()
        );
    }
    Ok(())
}

/// The release record's `{suite_version, manifest_sha256}` when it lists `sha256`, else `None`.
fn release_of(record: Option<&str>, sha256: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(record?).ok()?;
    let listed = value["binary_sha256"].as_array()?.iter().any(|h| h.as_str() == Some(sha256));
    let (suite, manifest) = (value["suite_version"].as_str()?, value["manifest_sha256"].as_str()?);
    listed.then(|| serde_json::json!({ "suite_version": suite, "manifest_sha256": manifest }))
}

/// The release this binary was installed from, as the signed record's `observed.release` carries
/// it: `null` when `<data dir>/installed-release.json` is absent, unreadable or does not list
/// this binary (a development Core). Read once per process.
pub fn current_release() -> serde_json::Value {
    static RELEASE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    RELEASE
        .get_or_init(|| {
            let read = || -> Result<Option<serde_json::Value>> {
                let path = embarch_topology::local_machine_data_dir()?.join(RELEASE_RECORD_FILE);
                let sha = self_sha256(&std::env::current_exe()?)?;
                Ok(release_of(std::fs::read_to_string(path).ok().as_deref(), &sha))
            };
            read().ok().flatten().unwrap_or(serde_json::Value::Null)
        })
        .clone()
}

fn self_sha256(exe: &Path) -> Result<String> {
    let bytes = std::fs::read(exe)
        .with_context(|| format!("could not read {} to hash it", exe.display()))?;
    Ok(crate::publish::sha256_hex(&bytes))
}

/// Whether a registration's command text (`ImagePath`, a unit's `ExecStart`,
/// a launchd plist) starts Core with `--service`.
fn command_runs_service(text: &str) -> bool {
    text.replace("<string>", " ")
        .replace("</string>", " ")
        .split_whitespace()
        .any(|t| t.trim_matches('"') == "--service")
}

/// The registered command text, if the service is registered.
fn registered_command() -> Option<String> {
    #[cfg(windows)]
    {
        winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey(format!("SYSTEM\\CurrentControlSet\\Services\\{SERVICE_LABEL}"))
            .ok()?
            .get_value::<String, _>("ImagePath")
            .ok()
    }
    #[cfg(not(windows))]
    {
        [
            format!("/etc/systemd/system/{SERVICE_LABEL}.service"),
            format!("/Library/LaunchDaemons/{SERVICE_LABEL}.plist"),
        ]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
    }
}

/// What `install --service` registers on a service manager that honours
/// `username`: `run --service --bind <bind>` under Core's own account.
fn install_ctx(
    label: ServiceLabel,
    exe: PathBuf,
    bind: &str,
    environment: Option<Vec<(String, String)>>,
    account: &Account,
) -> ServiceInstallCtx {
    ServiceInstallCtx {
        label,
        program: exe,
        args: ["run", "--service", "--bind", bind].map(OsString::from).to_vec(),
        contents: None,
        username: match account {
            Account::User(u) => Some((*u).to_string()),
            Account::ScObj(_) => None,
        },
        working_directory: None,
        environment,
        autostart: true,
        restart_policy: RestartPolicy::OnFailure {
            delay_secs: Some(RESTART_DELAY_SECS),
            max_retries: Some(RESTART_RETRIES),
            reset_after_secs: Some(RESTART_RESET_SECS),
        },
    }
}

/// `install` without `--service`: registers nothing (decision 157).
pub fn install_development() {
    println!(
        "Nothing registered. A source build runs as a development Core \
         (`embarch-core run --store <dir>`); a service Core comes from a signed release. \
         To deploy this build for development, use `embarch deploy-core`."
    );
}

/// Register a signed release's embarch-core as a background OS service under
/// Core's own account (`run --service --bind`, `--bind` baked into the
/// registered command so it survives reboots) and start it. Refuses before
/// any service-manager call unless `installed-release.json` lists this
/// binary's hash.
pub fn install(bind: &str) -> Result<()> {
    let exe = std::env::current_exe().context("could not determine this executable's path")?;
    let record_path = embarch_topology::local_machine_data_dir()?.join(RELEASE_RECORD_FILE);
    check_release_record(
        std::fs::read_to_string(&record_path).ok().as_deref(),
        &self_sha256(&exe)?,
        &record_path,
    )?;

    elevate::ensure_elevated_or_fallback()?;

    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    let mgr = manager()?;
    let account = account_for(cfg!(windows));

    let explicit_token = std::env::var("EMBARCH_TOKEN").ok();
    if explicit_token.is_none() {
        eprintln!(
            "EMBARCH_TOKEN not set at install time — the installed service will use the \
             auto-generated machine-wide token file instead."
        );
    }
    #[cfg(windows)]
    if windows_service_registered(SERVICE_LABEL) {
        // Decision 106: a registered service is rewritten in place, not refused.
        let _ = mgr.stop(ServiceStopCtx { label: label.clone() });
        let kept = rewrite_windows_registration(SERVICE_LABEL, &exe, bind, explicit_token.as_deref())
            .context("failed to rewrite the registered embarch-core service in place")?;
        eprintln!(
            "embarch-core service already registered; rewrote its command line (--bind {bind}). \
             Environment kept: [{}]",
            kept.join(", ")
        );
        apply_windows_failure_actions(SERVICE_LABEL)?;
        apply_windows_account(SERVICE_LABEL, &account)?;
        mgr.start(ServiceStartCtx { label })
            .context("service reconfigured, but failed to start")?;
        return Ok(());
    }

    let environment = explicit_token
        .clone()
        .map(|token| vec![("EMBARCH_TOKEN".to_string(), token)]);

    mgr.install(install_ctx(label.clone(), exe, bind, environment, &account))
    .context("failed to install embarch-core as a service")?;

    #[cfg(windows)]
    {
        if let Some(token) = explicit_token {
            let existing = read_windows_environment(SERVICE_LABEL);
            let merged = merge_environment(existing, Some(&token));
            write_windows_environment(SERVICE_LABEL, &merged)
                .context("failed to set the installed service's Windows registry environment")?;
        }
        // `service_manager`'s sc backend ignores `restart_policy` (it warns so).
        apply_windows_failure_actions(SERVICE_LABEL)?;
        apply_windows_account(SERVICE_LABEL, &account)?;
    }

    mgr.start(ServiceStartCtx { label })
        .context("service installed, but failed to start")?;

    Ok(())
}

const RESTART_DELAY_SECS: u32 = 5;
const RESTART_RETRIES: u32 = 5;
const RESTART_RESET_SECS: u32 = 3600;

/// The service's command line as SCM stores it in `ImagePath`: the exe path
/// quoted (it may hold spaces), then `run --service --bind <bind>`.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_command_line(exe: &Path, bind: &str) -> String {
    format!("\"{}\" run --service --bind {}", exe.display(), bind)
}

/// Merges an existing `Environment` value with an explicit token: every
/// existing entry is kept (signal baud etc.), `EMBARCH_TOKEN` is replaced
/// when one is given, and absent otherwise stays untouched.
#[cfg_attr(not(windows), allow(dead_code))]
fn merge_environment(existing: Vec<String>, token: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = match token {
        Some(_) => existing.into_iter().filter(|e| !e.starts_with("EMBARCH_TOKEN=")).collect(),
        None => existing,
    };
    if let Some(t) = token {
        out.push(format!("EMBARCH_TOKEN={t}"));
    }
    out
}

/// Entries for display: names only, so a token never reaches the terminal.
#[cfg_attr(not(windows), allow(dead_code))]
fn environment_names(entries: &[String]) -> Vec<String> {
    entries.iter().map(|e| e.split('=').next().unwrap_or("").to_string()).collect()
}

/// `sc failure` arguments for the restart policy `install` has always claimed.
#[cfg_attr(not(windows), allow(dead_code))]
fn failure_args(service: &str) -> Vec<String> {
    let step = format!("restart/{}", RESTART_DELAY_SECS * 1000);
    let actions = vec![step; RESTART_RETRIES as usize].join("/");
    vec![
        "failure".into(),
        service.into(),
        "reset=".into(),
        RESTART_RESET_SECS.to_string(),
        "actions=".into(),
        actions,
    ]
}

#[cfg(windows)]
fn service_key(service_name: &str) -> Result<winreg::RegKey> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let hklm = winreg::RegKey::predef(HKEY_LOCAL_MACHINE);
    let (services, _) = hklm
        .create_subkey("SYSTEM\\CurrentControlSet\\Services")
        .context("failed to open HKLM\\SYSTEM\\CurrentControlSet\\Services")?;
    let (key, _) = services
        .create_subkey(service_name)
        .with_context(|| format!("failed to open the {service_name} service registry key"))?;
    Ok(key)
}

#[cfg(windows)]
fn windows_service_registered(service_name: &str) -> bool {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!("SYSTEM\\CurrentControlSet\\Services\\{service_name}"))
        .is_ok()
}

#[cfg(windows)]
fn read_windows_environment(service_name: &str) -> Vec<String> {
    service_key(service_name)
        .ok()
        .and_then(|k| k.get_value::<Vec<String>, _>("Environment").ok())
        .unwrap_or_default()
}

/// Rewrites `ImagePath` and merges `Environment`; returns the kept entry names.
#[cfg(windows)]
fn rewrite_windows_registration(
    service_name: &str,
    exe: &Path,
    bind: &str,
    token: Option<&str>,
) -> Result<Vec<String>> {
    let key = service_key(service_name)?;
    key.set_value("ImagePath", &windows_command_line(exe, bind))
        .context("failed to write the ImagePath registry value")?;
    let merged = merge_environment(read_windows_environment(service_name), token);
    if !merged.is_empty() {
        write_windows_environment(service_name, &merged)?;
    }
    Ok(environment_names(&merged))
}

#[cfg(windows)]
fn apply_windows_account(service_name: &str, account: &Account) -> Result<()> {
    let Account::ScObj(obj) = account else { return Ok(()) };
    let out = std::process::Command::new("sc.exe")
        .args(account_args(service_name, obj))
        .output()
        .context("could not run sc.exe config")?;
    if !out.status.success() {
        bail!("sc config obj= failed: {}", String::from_utf8_lossy(&out.stdout).trim());
    }
    Ok(())
}

#[cfg(windows)]
fn apply_windows_failure_actions(service_name: &str) -> Result<()> {
    let out = std::process::Command::new("sc.exe")
        .args(failure_args(service_name))
        .output()
        .context("could not run sc.exe failure")?;
    if !out.status.success() {
        bail!(
            "sc failure failed: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

/// Writes the `Environment` `REG_MULTI_SZ` value under this service's own
/// registry key, since `sc.rs` (the Windows backend behind `ServiceManager`)
/// never reads `ServiceInstallCtx.environment`. Callers pass the merged list.
#[cfg(windows)]
fn write_windows_environment(service_name: &str, entries: &[String]) -> Result<()> {
    service_key(service_name)?
        .set_value("Environment", &entries.to_vec())
        .context("failed to write the Environment registry value")?;
    Ok(())
}

/// Start the already-installed background service.
///
/// Exists so `embarch-umbrella`'s `up` doesn't have to re-derive per-OS
/// service control (`sc` vs `systemctl` vs `launchctl`) in a second codebase
/// — the `service-manager` crate already encapsulates exactly that, and this
/// is the binary that already depends on it (`embarch-umbrella`
/// decisions 4/7).
///
/// Not idempotent in any guaranteed way: starting an already-running service
/// is an error on some backends and a no-op on others, and that difference is
/// the crate's to hide or not, not something to paper over here with a
/// racy is-it-running check.
pub fn start() -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    control(|m| m.start(ServiceStartCtx { label: label.clone() }))
        .context("failed to start the embarch-core service (is it installed?)")
}

/// Stop the running background service, leaving it installed.
pub fn stop() -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    control(|m| m.stop(ServiceStopCtx { label: label.clone() }))
        .context("failed to stop the embarch-core service (is it installed and running?)")
}

/// Runs a start or stop against the service manager as this user first
/// (decision 163). Only a refusal for lack of rights falls through to the
/// elevation prompt, which then behaves as it always did; any other failure is
/// returned as it is.
fn control(op: impl Fn(&dyn ServiceManager) -> std::io::Result<()>) -> Result<()> {
    let mgr = manager()?;
    let Err(e) = op(mgr.as_ref()) else {
        return Ok(());
    };
    if elevate::is_elevated() || !is_rights_refusal(&e, cfg!(windows)) {
        return Err(e.into());
    }
    tracing::debug!("service control refused without elevation ({e}); elevating");
    elevate::ensure_elevated_or_fallback()?;
    op(mgr.as_ref()).map_err(Into::into)
}

/// Whether a `service-manager` start/stop error is a refusal for lack of
/// rights. The crate wraps a failed `sc.exe` or `systemctl` as an
/// `io::Error` of kind `Other` whose text is `Command failed with exit code
/// <n>: <stderr>`, so the text is all there is to read.
fn is_rights_refusal(err: &std::io::Error, windows: bool) -> bool {
    let text = err.to_string().to_lowercase();
    if windows {
        // `sc.exe` exits 5 (ERROR_ACCESS_DENIED) and prints "Access is denied."
        text.contains("exit code 5:") || text.contains("access is denied")
    } else {
        // systemctl without a polkit grant: "Interactive authentication
        // required." or "Access denied"; a permission error on the bus socket.
        ["interactive authentication required", "access denied", "permission denied"]
            .iter()
            .any(|m| text.contains(m))
    }
}

/// Stop and remove the background service.
pub fn uninstall() -> Result<()> {
    elevate::ensure_elevated_or_fallback()?;

    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    let mgr = manager()?;

    // Best-effort stop; don't fail uninstall just because it wasn't running.
    let _ = mgr.stop(ServiceStopCtx { label: label.clone() });

    mgr.uninstall(ServiceUninstallCtx { label })
        .context("failed to uninstall embarch-core service")?;

    Ok(())
}

/// Replace the installed service's binary with `new_exe` and restart it —
/// the motivating case for self-elevation (decision 3):
/// updating an already-installed Core previously had no supported path at
/// all, only manual stop/copy/start surgery in a hand-opened elevated
/// shell.
///
/// **Must be run via the currently-installed copy itself** — like
/// `install` (which registers `std::env::current_exe()` as the service
/// body), this operates on `current_exe()`, not a separately-discovered
/// service path. Windows won't allow overwriting a running process's own
/// image file in place, even after the *service* instance is stopped —
/// this `update` invocation is itself a second, distinct running instance
/// of that same file — so the current binary is renamed out of the way
/// first (renaming a running exe is allowed on Windows) before the new one
/// is copied into its place, the standard self-update idiom. Works
/// identically, if not strictly necessary, on Unix.
///
/// Rolls back to the previous binary — and tries to get the service
/// running again on it — if the new one fails to start.
///
/// The renamed-aside backup from *this* run is deliberately not removed on
/// success: the process running this code is itself still executing from
/// that exact file (this `update` invocation was launched from the
/// currently-installed exe, same as `install`), so Windows won't allow
/// deleting it yet. Cleaned up at the start of the *next* `update` call
/// instead, by which point this process is long gone — confirmed live: a
/// `.bak` file really does linger after a successful update until then.
pub fn update(new_exe: &Path) -> Result<()> {
    elevate::ensure_elevated_or_fallback()?;

    if !new_exe.is_file() {
        bail!("new binary not found: {}", new_exe.display());
    }
    if registered_command().is_some_and(|c| command_runs_service(&c)) {
        bail!(
            "the registered service runs `--service`: a signed release replaces it through \
             the elevated install, not `update`"
        );
    }

    let label: ServiceLabel = SERVICE_LABEL.parse().context("invalid service label")?;
    let current_exe = std::env::current_exe()
        .context("could not determine the installed binary's own path")?;

    // Best-effort stop — it might already be stopped, or `update` might be
    // run before the service was ever started.
    let _ = manager()?.stop(ServiceStopCtx { label: label.clone() });

    let backup = backup_path(&current_exe);

    // Clean up a *previous* run's backup, if one is still sitting here — it
    // couldn't be removed at the end of that run (see the comment below:
    // the process doing the removing is itself still executing from that
    // exact file), but by now that process is long gone, so it's safe to
    // remove here instead. Best-effort: a real error here shouldn't block
    // installing the actually-requested new binary.
    let _ = std::fs::remove_file(&backup);

    std::fs::rename(&current_exe, &backup).with_context(|| {
        format!(
            "failed to move the current binary from {} to {} ahead of replacing it",
            current_exe.display(),
            backup.display()
        )
    })?;

    if let Err(e) = std::fs::copy(new_exe, &current_exe) {
        let _ = std::fs::rename(&backup, &current_exe); // best-effort rollback
        return Err(e).with_context(|| {
            format!(
                "failed to install {} as {}; rolled back",
                new_exe.display(),
                current_exe.display()
            )
        });
    }

    match manager()?.start(ServiceStartCtx { label: label.clone() }) {
        Ok(()) => {
            // Not removing `backup` here — see the doc comment above: this
            // process is still executing from it right now, so the removal
            // would just fail. Next `update` call cleans it up instead.
            Ok(())
        }
        Err(e) => {
            // The new binary is bad or incompatible — roll back and try to
            // get the service running again on the known-good previous one.
            let _ = std::fs::remove_file(&current_exe);
            let _ = std::fs::rename(&backup, &current_exe);
            let _ = manager()?.start(ServiceStartCtx { label });
            Err(e).context("new binary failed to start as the service; rolled back to the previous one")
        }
    }
}

fn backup_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    exe.with_file_name(name)
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
/// this machine for the first time (decision 3).
#[cfg(windows)]
pub mod windows {
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher, Error as WsError};

    use super::SERVICE_LABEL;

    define_windows_service!(ffi_service_main, service_main);

    /// Stashes `try_dispatch`'s `bind`/`port` for `run_service` to read.
    /// Needed only because `define_windows_service!` fixes `service_main`'s
    /// signature (`Vec<OsString>`, `service_main`'s own `_arguments` is SCM's
    /// copy of them, not exposed in a form worth re-parsing) — a `OnceLock`
    /// set immediately before `service_dispatcher::start` is the standard way
    /// around that, not a real global-mutable-state concern (single-writer,
    /// before the one reader can possibly run).
    static BIND_PORT: OnceLock<(String, u16)> = OnceLock::new();

    /// `Some(result)` when this process really was launched by SCM: blocks
    /// for the service's whole lifetime, then returns how it exited. `None`
    /// when it wasn't — a human running `embarch-core.exe run` directly at a
    /// console, same as on any other OS — which is
    /// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` (raw OS error 1063) from
    /// `StartServiceCtrlDispatcherW`, the one failure this treats as "not
    /// SCM" rather than a real error to report.
    ///
    /// `bind`/`port` are `Command::Run`'s own already-parsed values — the
    /// same ones a non-Windows `run()` call gets — not re-derived here.
    /// **Real bug this fixed, 2026-08-24:** `run_service` used to hardcode
    /// `"0.0.0.0"`/`DEFAULT_PORT` directly, silently discarding whatever
    /// `--bind`/`--port` the service was actually registered with (this
    /// crate's own `service::install`, and by extension every topology
    /// `embarch-umbrella setup` might pass) — the live Windows Core has been
    /// binding `0.0.0.0` unconditionally the whole time, decision 6's
    /// loopback-default amendment notwithstanding.
    pub fn try_dispatch(bind: String, port: u16) -> Option<anyhow::Result<()>> {
        let _ = BIND_PORT.set((bind, port)); // can't fail: single call site, before the dispatcher's own thread starts
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

        // `try_dispatch` stashed `Command::Run`'s own already-parsed
        // `bind`/`port` in `BIND_PORT` immediately before starting this
        // dispatcher — falling back to the same defaults `main.rs` itself
        // uses is only a safety net for a call path that shouldn't exist
        // (`service_main` only ever runs after `try_dispatch` set it).
        let (bind, port) = BIND_PORT
            .get()
            .cloned()
            .unwrap_or_else(|| (crate::DEFAULT_BIND.to_string(), crate::DEFAULT_PORT));

        // A fresh runtime, not a nested one: `try_dispatch` is called from
        // plain (non-async) `main`, before any Tokio runtime exists, so
        // there's nothing to nest inside. `crate::build_runtime` (not a bare
        // `Runtime::new()`) — see its own doc comment: this exact path is
        // where the real, `--release`, production `STATUS_STACK_OVERFLOW`
        // crash happened.
        let result = crate::build_runtime()?.block_on(crate::serve(bind, port, async {
            let _ = shutdown_rx.await;
        }));

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

#[cfg(test)]
mod tests {
    #[test]
    fn release_of_gives_the_files_two_values_only_when_it_lists_the_binary() {
        let rec = r#"{"suite_version":"1.2.3","manifest_sha256":"ab","binary_sha256":["aa","bb"]}"#;
        assert_eq!(
            super::release_of(Some(rec), "bb"),
            Some(serde_json::json!({ "suite_version": "1.2.3", "manifest_sha256": "ab" }))
        );
        assert_eq!(super::release_of(Some(rec), "cc"), None);
        assert_eq!(super::release_of(None, "aa"), None);
        assert_eq!(super::release_of(Some("not json"), "aa"), None);
        assert_eq!(super::release_of(Some(r#"{"binary_sha256":["aa"]}"#), "aa"), None);
    }

    use super::*;

    fn failed(code: i32, msg: &str) -> std::io::Error {
        std::io::Error::other(format!("Command failed with exit code {code}: {msg}"))
    }

    #[test]
    fn rights_refusal_windows() {
        let denied = failed(5, "[SC] OpenService FAILED 5:\n\nAccess is denied.");
        assert!(is_rights_refusal(&denied, true));
        assert!(is_rights_refusal(&failed(5, ""), true));
        // 1060 (not installed) and 1056 (already running) are not rights.
        assert!(!is_rights_refusal(&failed(1060, "OpenService FAILED 1060"), true));
        assert!(!is_rights_refusal(&failed(1056, "already running"), true));
    }

    #[test]
    fn rights_refusal_systemd() {
        let polkit = failed(1, "Failed to stop com.embarch.core.service: Interactive authentication required.");
        assert!(is_rights_refusal(&polkit, false));
        assert!(is_rights_refusal(&failed(1, "Failed to start x.service: Access denied"), false));
        assert!(is_rights_refusal(&failed(1, "Permission denied"), false));
        assert!(!is_rights_refusal(&failed(5, "Unit com.embarch.core.service not loaded."), false));
        assert!(!is_rights_refusal(&failed(1, "Job failed. See journalctl"), false));
        // A Windows-shaped refusal is not read as one on Linux.
        assert!(!is_rights_refusal(&failed(5, "Access is denied."), false));
    }

    #[test]
    fn command_line_quotes_the_exe() {
        let c = windows_command_line(Path::new("C:\\Program Files\\e.exe"), "0.0.0.0");
        assert_eq!(c, "\"C:\\Program Files\\e.exe\" run --service --bind 0.0.0.0");
    }

    #[test]
    fn merge_keeps_existing_and_replaces_token() {
        let ex = vec!["EMBARCH_SIGNAL_BAUD=460800".to_string(), "EMBARCH_TOKEN=old".to_string()];
        let m = merge_environment(ex.clone(), Some("new"));
        assert_eq!(m, vec!["EMBARCH_SIGNAL_BAUD=460800", "EMBARCH_TOKEN=new"]);
        assert_eq!(merge_environment(ex.clone(), None), ex);
        assert_eq!(environment_names(&m), vec!["EMBARCH_SIGNAL_BAUD", "EMBARCH_TOKEN"]);
    }

    #[test]
    fn failure_args_are_five_restarts() {
        let a = failure_args("x");
        assert_eq!(a[3], "3600");
        assert_eq!(a[5].matches("restart/5000").count(), 5);
    }

    const SHA: &str = "ab";

    #[test]
    fn install_refuses_without_a_record_or_a_listed_hash() {
        let p = Path::new("/var/lib/embarch/installed-release.json");
        let e = check_release_record(None, SHA, p).unwrap_err().to_string();
        assert!(e.contains("does not exist") && e.contains("embarch deploy-core"), "{e}");
        let other = r#"{"binary_sha256":["cd"]}"#;
        let e = check_release_record(Some(other), SHA, p).unwrap_err().to_string();
        assert!(e.contains("does not list this binary"), "{e}");
        assert!(check_release_record(Some("not json"), SHA, p).is_err());
        assert!(check_release_record(Some(r#"{}"#), SHA, p).is_err());
    }

    #[test]
    fn install_accepts_a_listed_hash() {
        let p = Path::new("r.json");
        let rec = r#"{"binary_sha256":["cd","ab"]}"#;
        check_release_record(Some(rec), SHA, p).unwrap();
    }

    #[test]
    fn registration_runs_service_under_cores_own_account() {
        let label: ServiceLabel = SERVICE_LABEL.parse().unwrap();
        let linux = install_ctx(label.clone(), "/x/e".into(), "0.0.0.0", None, &account_for(false));
        assert_eq!(linux.args, ["run", "--service", "--bind", "0.0.0.0"].map(OsString::from));
        assert_eq!(linux.username.as_deref(), Some("embarch"));
        let win = install_ctx(label, "C:\\e.exe".into(), "127.0.0.1", None, &account_for(true));
        assert_eq!(win.args, ["run", "--service", "--bind", "127.0.0.1"].map(OsString::from));
        assert_eq!(win.username, None);
        assert_eq!(account_for(true), Account::ScObj("NT SERVICE\\com.embarch.core"));
        assert_eq!(
            account_args(SERVICE_LABEL, WINDOWS_SERVICE_ACCOUNT),
            ["config", "com.embarch.core", "obj=", "NT SERVICE\\com.embarch.core"]
        );
    }

    #[test]
    fn update_recognises_a_service_registration() {
        assert!(command_runs_service("\"C:\\e.exe\" run --service --bind 127.0.0.1"));
        assert!(command_runs_service("ExecStart=/usr/bin/e run --service --bind x"));
        assert!(command_runs_service("<string>--service</string>"));
        assert!(!command_runs_service("\"C:\\e.exe\" run --bind 127.0.0.1"));
        assert!(!command_runs_service("ExecStart=/usr/bin/e run --bind x --store /a/--service-dir"));
    }
}
