//! Restart of the controlled computer, asked for by the viewer (`Restart`).
//!
//! Offered only with unattended access, so that the agent comes back by itself:
//! - Windows: the service starts the agent again after the boot. `shutdown /r /f`
//!   closes the open programs without asking (unsaved work is lost: the viewer
//!   warns before asking), a few seconds after the answer has left;
//! - Linux: `systemctl reboot` through logind, as the signed-in user; the computer
//!   comes back only with automatic login (see the service's notes);
//! - macOS: the "restart" Apple event to loginwindow (`kAERestart`, what the
//!   Apple menu does without its dialog), which needs no administrator. The open
//!   apps are asked to quit: one with unsaved work can stop the restart. The Mac
//!   comes back only with automatic login, then the LaunchAgent starts the agent.

use anyhow::Result;

/// The system can be restarted from here.
pub fn available() -> bool {
    cfg!(any(windows, target_os = "linux", target_os = "macos"))
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

/// The event starts closing the apps at once, this agent among them: it is sent
/// two seconds later from a thread, after the answer has reached the viewer.
/// A refusal is only logged; the viewer then finds the agent still online and
/// connects again.
#[cfg(target_os = "macos")]
pub fn restart() -> Result<()> {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match std::process::Command::new("/usr/bin/osascript")
            .args(["-e", "tell application \"loginwindow\" to «event aevtrest»"])
            .output()
        {
            Ok(output) => {
                if let Err(e) = check(output) {
                    tracing::warn!("restart refused by macOS: {e:#}");
                }
            }
            Err(e) => tracing::warn!("osascript not started: {e}"),
        }
    });
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn restart() -> Result<()> {
    anyhow::bail!("@agent.restart_unavailable")
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn check(output: std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let text = if text.trim().is_empty() { String::from_utf8_lossy(&output.stdout) } else { text };
    anyhow::bail!("{} ({})", text.trim(), output.status)
}
