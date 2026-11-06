//! Restart of the controlled computer, asked for by the viewer (`Restart`).
//!
//! Offered only with unattended access, so that the agent comes back by itself:
//! - Windows: the service starts the agent again after the boot. `shutdown /r /f`
//!   closes the open programs without asking (unsaved work is lost: the viewer
//!   warns before asking), a few seconds after the answer has left;
//! - Linux: `systemctl reboot` through logind, as the signed-in user; the computer
//!   comes back only with automatic login (see the service's notes).
//!
//! macOS has no unattended access yet: there it is not offered.

use anyhow::Result;

/// The system can be restarted from here.
pub fn available() -> bool {
    cfg!(any(windows, target_os = "linux"))
}

/// Asks the system to restart; `Err` with the reason when it refused.
#[cfg(windows)]
pub fn restart() -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let exe = std::path::Path::new(&system).join("System32").join("shutdown.exe");
    // A few seconds: the answer reaches the viewer before the network goes down.
    // "p:4:1" = planned, application maintenance (event log).
    let output = std::process::Command::new(exe)
        .args(["/r", "/f", "/t", "3", "/d", "p:4:1", "/c", "NuviraHub: remote restart"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    check(output)
}

#[cfg(target_os = "linux")]
pub fn restart() -> Result<()> {
    let output = std::process::Command::new("systemctl").args(["reboot", "--no-ask-password"]).output()?;
    check(output)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn restart() -> Result<()> {
    anyhow::bail!("@agent.restart_unavailable")
}

#[cfg(any(windows, target_os = "linux"))]
fn check(output: std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let text = if text.trim().is_empty() { String::from_utf8_lossy(&output.stdout) } else { text };
    anyhow::bail!("{} ({})", text.trim(), output.status)
}
