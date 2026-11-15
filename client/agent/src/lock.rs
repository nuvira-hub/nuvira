//! Locking the computer when a session ends (the computer's setting, the server's
//! rule `lock_on_end` or the technician's choice for one session): the person
//! signed in has to sign in again, as after Win+L.
//!
//! - Windows: `LockWorkStation` from the agent, which runs on the interactive
//!   desktop of the person's session (also when started by the service, as SYSTEM);
//!   if it fails, the session is disconnected (`WTSDisconnectSession`), which on
//!   the console shows the lock screen too.
//! - Linux: logind's `LockSession` on the person's graphical session
//!   (`loginctl lock-session`), which GNOME, KDE Plasma and Cinnamon obey.
//! - macOS: `SACLockScreenImmediate` of the login framework, as the menu bar's
//!   "Lock Screen" does.

use anyhow::Result;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use anyhow::{Context, bail};

/// Whether this agent can lock the computer (a person signed in to lock).
pub fn available() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        // The agent of the sign-in screen (root) has nobody to lock.
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() != 0 }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() != 0 }
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// Locks the computer now.
pub fn lock() -> Result<()> {
    imp::lock()
}

#[cfg(windows)]
mod imp {
    use anyhow::{Result, bail};
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTSDisconnectSession,
    };
    use windows::Win32::System::Shutdown::LockWorkStation;
    use windows::Win32::System::Threading::GetCurrentProcessId;

    pub fn lock() -> Result<()> {
        // SAFETY: no arguments; it only asks Windows to show the lock screen.
        let locked = unsafe { LockWorkStation() };
        if locked.is_ok() {
            return Ok(());
        }
        let first = locked.err().map(|e| e.to_string()).unwrap_or_default();
        let mut session = 0u32;
        // SAFETY: plain out parameter.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }?;
        // SAFETY: the session is this process's own.
        match unsafe { WTSDisconnectSession(Some(WTS_CURRENT_SERVER_HANDLE), session, false) } {
            Ok(()) => {
                tracing::info!("LockWorkStation failed ({first}): session {session} disconnected instead");
                Ok(())
            }
            Err(e) => bail!("LockWorkStation: {first}; WTSDisconnectSession: {e}"),
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn lock() -> Result<()> {
        // The person's graphical session: the agent itself runs as a systemd user
        // service, outside any session, so `loginctl lock-session` without an ID
        // would not know which one.
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let out = std::process::Command::new("loginctl")
            .args(["show-user", &uid.to_string(), "-p", "Display", "--value"])
            .output()
            .context("loginctl")?;
        let session = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if session.is_empty() {
            bail!("no graphical session for user {uid}");
        }
        let status = std::process::Command::new("loginctl").args(["lock-session", &session]).status()?;
        if !status.success() {
            bail!("loginctl lock-session {session}: {status}");
        }
        cinnamon_fallback(uid);
        Ok(())
    }

    /// Cinnamon locks through its screensaver daemon, which listens to logind; when
    /// that daemon is not running (it died) logind's request reaches nobody and the
    /// session stays open. Asking the daemon itself starts it (D-Bus activation)
    /// and locks. GNOME and KDE Plasma lock inside the shell, always running.
    fn cinnamon_fallback(uid: u32) {
        // On the command line: the process name is cut to 15 characters
        // ("cinnamon-screen").
        let running = |name: &str| {
            std::process::Command::new("pgrep")
                .args(["-u", &uid.to_string(), "-f", &format!("(^|/){name}( |$)")])
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if !running("cinnamon") || running("cinnamon-screensaver") {
            return;
        }
        match std::process::Command::new("cinnamon-screensaver-command").arg("--lock").status() {
            Ok(s) if s.success() => {
                tracing::info!("Cinnamon's screensaver was not running: started and locked")
            }
            Ok(s) => tracing::warn!("cinnamon-screensaver-command --lock: {s}"),
            Err(e) => tracing::warn!("cinnamon-screensaver-command: {e}"),
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    pub fn lock() -> Result<()> {
        const PATH: &[u8] = b"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login\0";
        // SAFETY: a system framework, loaded once and never unloaded; the symbol is
        // a function without arguments returning an int.
        unsafe {
            let lib = libc::dlopen(PATH.as_ptr().cast(), libc::RTLD_LAZY);
            if lib.is_null() {
                bail!("login framework not found");
            }
            let sym = libc::dlsym(lib, c"SACLockScreenImmediate".as_ptr());
            if sym.is_null() {
                bail!("SACLockScreenImmediate not found");
            }
            let f: extern "C" fn() -> i32 = std::mem::transmute(sym);
            let rc = f();
            if rc != 0 {
                bail!("SACLockScreenImmediate: {rc}");
            }
        }
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    pub fn lock() -> anyhow::Result<()> {
        anyhow::bail!("not available on this system")
    }
}
