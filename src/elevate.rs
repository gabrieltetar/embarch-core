//! Self-elevation for `embarch-core`'s privileged subcommands (`install`,
//! `uninstall`, `start`, `stop`, `update`) — `design.md` §3 decision 3,
//! reversing `embarch-umbrella/design.md` §3 decision 7's original "never
//! self-elevate, print the command" stance.
//!
//! Every OS's service-control call needs elevation (root/polkit on Linux,
//! Administrator on Windows — `service.rs`'s own decision 3 text). Rather
//! than fail and tell a human to re-run the same command in a shell they
//! open themselves, a privileged subcommand re-launches this same
//! already-running binary elevated, waits for it, and propagates its exit
//! code — one UAC/polkit/administrator prompt, scoped to exactly the one
//! subcommand being run, never a standing elevated process or helper.
//!
//! Re-launching the *same already-running binary* elevated adds no new
//! "trust an unseen artifact" step: the human already chose to run this
//! exact binary unprivileged, and elevation just reuses that same trust
//! decision instead of asking them to separately open an elevated shell and
//! retype/paste a command by hand.

use anyhow::{bail, Context, Result};

/// True when this process already holds the privileges its caller needs —
/// re-elevating would be pointless (and on Windows, `ShellExecuteW`'s
/// `"runas"` verb from an already-elevated process just re-prompts for no
/// reason).
#[cfg(windows)]
pub fn is_elevated() -> bool {
    windows::is_elevated()
}

#[cfg(unix)]
pub fn is_elevated() -> bool {
    // SAFETY: geteuid() takes no arguments, has no preconditions, and
    // cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// A graphical session is available to show a prompt through — an X11/
/// Wayland Linux desktop, Windows (UAC is available in any interactive
/// session), or macOS. `false` on Linux with neither `DISPLAY` nor
/// `WAYLAND_DISPLAY` set means there's no desktop to show `pkexec`'s dialog
/// in, though a plain terminal (`tty_available`) may still let `sudo`
/// prompt.
#[cfg(target_os = "linux")]
fn gui_available() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

#[cfg(windows)]
fn gui_available() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn gui_available() -> bool {
    // Reasoned, not validated against a real machine — embarch-umbrella's
    // design.md §10 already flags macOS as reasoned-only for this whole
    // suite (no Mac to test on). A genuinely headless Mac (rare) would want
    // this to detect no WindowServer session and fall through to `sudo`;
    // not implemented since there's nothing to verify it against yet.
    true
}

/// A real terminal is attached to stdin — `sudo` can prompt interactively
/// here even with no GUI at all. Irrelevant on Windows: UAC never needs a
/// console TTY the way `sudo` does, so `tty_on_this_platform` below just
/// returns `false` there without calling this.
#[cfg(unix)]
fn tty_available() -> bool {
    // SAFETY: isatty is safe to call with any fd number, valid or not.
    unsafe { libc::isatty(0) != 0 }
}

/// Outcome of attempting to relaunch elevated.
enum Elevated {
    /// Ran to completion; here's its exit code.
    Ran(i32),
    /// No GUI and no TTY to prompt through at all (CI, a script, a headless
    /// remote session) — nothing to click through, so there's no point
    /// trying.
    NoPromptAvailable,
}

/// Re-launch this exact invocation (same subcommand, same args) elevated,
/// wait for it, and exit this process with the elevated child's exit code.
/// A no-op (returns `Ok(())` immediately) if already elevated.
///
/// If elevation genuinely can't be triggered — no GUI, no TTY — prints the
/// exact command to run in an elevated shell instead and exits nonzero,
/// falling back to decision 7's original behavior for exactly the case it
/// still applies to: nobody there to answer a prompt.
pub fn ensure_elevated_or_fallback() -> Result<()> {
    if is_elevated() {
        return Ok(());
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

    match relaunch_elevated(&args)? {
        Elevated::Ran(code) => std::process::exit(code),
        Elevated::NoPromptAvailable => {
            let exe = std::env::current_exe().context("could not determine this executable's path")?;
            eprintln!(
                "This needs elevated privileges (Administrator/root) and there's no GUI or \
                 terminal here to prompt through. Run this yourself in an elevated shell:\n  \
                 {} {}",
                exe.display(),
                args.join(" ")
            );
            std::process::exit(1);
        }
    }
}

fn relaunch_elevated(args: &[String]) -> Result<Elevated> {
    if !gui_available() && !tty_on_this_platform() {
        return Ok(Elevated::NoPromptAvailable);
    }

    #[cfg(windows)]
    {
        windows::relaunch(args).map(Elevated::Ran)
    }

    #[cfg(target_os = "linux")]
    {
        linux::relaunch(args).map(Elevated::Ran)
    }

    #[cfg(target_os = "macos")]
    {
        macos::relaunch(args).map(Elevated::Ran)
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = args;
        Ok(Elevated::NoPromptAvailable)
    }
}

fn tty_on_this_platform() -> bool {
    #[cfg(unix)]
    {
        tty_available()
    }
    #[cfg(windows)]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::process::Command;

    /// Prefers `pkexec` (polkit's GUI prompt, matching the same polkit
    /// authentication a systemd *system* unit already demands per decision
    /// 3's own text) when a desktop is present and `pkexec` is on `PATH`;
    /// falls back to `sudo` (interactive terminal prompt) otherwise.
    ///
    /// Declined-prompt detection: `pkexec` reliably returns `126` when the
    /// user dismisses or is denied the polkit dialog (distinct from `127`,
    /// "failed to execute the command"). `sudo` has no equivalent reserved
    /// exit code for "auth failed" versus "the program itself returned 1" —
    /// its exit code is propagated either way rather than guessed at.
    pub fn relaunch(args: &[String]) -> Result<i32> {
        let exe = std::env::current_exe().context("could not determine this executable's path")?;
        let use_pkexec = super::gui_available() && on_path("pkexec");

        let mut cmd = if use_pkexec {
            Command::new("pkexec")
        } else {
            let mut c = Command::new("sudo");
            c.arg("-k"); // force a fresh prompt rather than reusing a cached sudo timestamp
            c
        };
        cmd.arg(&exe).args(args);

        let status = cmd.status().with_context(|| {
            format!(
                "failed to launch {} to elevate",
                if use_pkexec { "pkexec" } else { "sudo" }
            )
        })?;

        match status.code() {
            Some(126) if use_pkexec => bail!("elevation declined (polkit prompt dismissed or denied)"),
            Some(code) => Ok(code),
            None => bail!("elevated process was killed by a signal"),
        }
    }

    fn on_path(bin: &str) -> bool {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
            .unwrap_or(false)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::process::Command;

    /// `osascript ... with administrator privileges` is launchd's GUI
    /// admin-password prompt, the closest macOS equivalent to UAC/polkit.
    /// Reasoned, not validated — no Mac in this suite's development to
    /// confirm the exit-code-on-cancel assumption against.
    pub fn relaunch(args: &[String]) -> Result<i32> {
        let exe = std::env::current_exe().context("could not determine this executable's path")?;

        let mut shell_cmd = shell_quote(&exe.to_string_lossy());
        for a in args {
            shell_cmd.push(' ');
            shell_cmd.push_str(&shell_quote(a));
        }
        let script = format!(
            "do shell script {} with administrator privileges",
            applescript_quote(&shell_cmd)
        );

        let status = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .status()
            .context("failed to launch osascript to elevate")?;

        match status.code() {
            // osascript's documented behavior for a cancelled administrator
            // prompt is a nonzero exit with "User canceled." on stderr;
            // exit code 1 is the common case but not guaranteed distinct
            // from the elevated command's own exit(1) — best-effort only.
            Some(1) => bail!("elevation declined or the elevated command failed (administrator prompt cancelled?)"),
            Some(code) => Ok(code),
            None => bail!("elevated process was killed by a signal"),
        }
    }

    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    fn applescript_quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    /// `ERROR_CANCELLED` — `ShellExecuteExW`'s `GetLastError()` when the
    /// user clicks "No" on the UAC consent dialog.
    const ERROR_CANCELLED: u32 = 1223;

    pub fn is_elevated() -> bool {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }

            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut TOKEN_ELEVATION as *mut _,
                size,
                &mut size,
            );
            CloseHandle(token);

            ok != 0 && elevation.TokenIsElevated != 0
        }
    }

    /// Re-launches this exe with the `"runas"` verb (triggers the UAC
    /// consent dialog once), waits for it, and returns its exit code. Runs
    /// in a separate console window — an inherent limitation of `runas`,
    /// unlike the Linux/macOS paths, which inherit this process's own
    /// stdio.
    pub fn relaunch(args: &[String]) -> Result<i32> {
        let exe = std::env::current_exe().context("could not determine this executable's path")?;
        let exe_wide = to_wide(exe.as_os_str());
        let params = quote_args(args);
        let params_wide = to_wide(OsStr::new(&params));
        let verb_wide = to_wide(OsStr::new("runas"));

        let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb_wide.as_ptr();
        sei.lpFile = exe_wide.as_ptr();
        sei.lpParameters = params_wide.as_ptr();
        sei.nShow = SW_SHOWNORMAL;

        let ok = unsafe { ShellExecuteExW(&mut sei) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_CANCELLED {
                bail!("elevation declined (UAC prompt cancelled)");
            }
            bail!("ShellExecuteExW failed to relaunch elevated (error {err})");
        }

        let handle = sei.hProcess;
        unsafe {
            WaitForSingleObject(handle, INFINITE);
            let mut exit_code: u32 = 0;
            GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            Ok(exit_code as i32)
        }
    }

    fn to_wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// `lpParameters` is one flat string, not an argv array — quote any arg
    /// containing whitespace or a quote. Sufficient for this binary's own
    /// subcommands/paths; not a general command-line-quoting library.
    fn quote_args(args: &[String]) -> String {
        args.iter()
            .map(|a| {
                if a.chars().any(|c| c.is_whitespace() || c == '"') {
                    format!("\"{}\"", a.replace('"', "\\\""))
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}
