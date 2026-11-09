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

/// `other_users` knows who else is signed in (Hello.restart_users). Not on macOS:
/// loginwindow does not restart with other people signed in anyway.
pub fn knows_users() -> bool {
    cfg!(any(windows, target_os = "linux"))
}

/// The other people signed in on this computer, whose sessions a restart closes
/// too: user names, without this session's own, each once.
#[cfg(windows)]
pub fn other_users() -> Vec<String> {
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSQuerySessionInformationW, WTSUserName,
    };
    use windows::core::PWSTR;

    let user_of = |id: u32| -> Option<String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        unsafe { WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), id, WTSUserName, &mut buffer, &mut bytes) }
            .ok()?;
        // SAFETY: a NUL-terminated UTF-16 string until WTSFreeMemory.
        let name = unsafe { buffer.to_string() }.unwrap_or_default();
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        (!name.is_empty()).then_some(name)
    };
    let mut own = 0u32;
    let _ = unsafe { ProcessIdToSessionId(std::process::id(), &mut own) };
    let own_user = user_of(own);
    let mut list: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    if unsafe { WTSEnumerateSessionsW(Some(WTS_CURRENT_SERVER_HANDLE), 0, 1, &mut list, &mut count) }.is_err() {
        return Vec::new();
    }
    // SAFETY: WTSEnumerateSessionsW returned `count` entries, freed below.
    let ids: Vec<u32> = unsafe { std::slice::from_raw_parts(list, count as usize) }.iter().map(|s| s.SessionId).collect();
    unsafe { WTSFreeMemory(list.cast()) };
    // Active and disconnected sessions alike (fast user switching leaves the others
    // disconnected): `shutdown /r /f` closes them all.
    let mut names: Vec<String> =
        ids.into_iter().filter(|&id| id != own).filter_map(user_of).filter(|n| Some(n) != own_user.as_ref()).collect();
    names.sort_unstable_by_key(|n| n.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

#[cfg(target_os = "linux")]
pub fn other_users() -> Vec<String> {
    let own = std::env::var("USER").unwrap_or_default();
    let Ok(out) = std::process::Command::new("loginctl").args(["list-sessions", "--no-legend"]).output() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for id in String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.split_whitespace().next()) {
        // People's sessions only: not the greeters' nor the user managers.
        let Ok(info) =
            std::process::Command::new("loginctl").args(["show-session", id, "-p", "Name", "-p", "Class"]).output()
        else {
            continue;
        };
        let info = String::from_utf8_lossy(&info.stdout);
        let value = |key: &str| info.lines().find_map(|l| l.strip_prefix(key)).unwrap_or("").to_string();
        let (name, class) = (value("Name="), value("Class="));
        if class == "user" && !name.is_empty() && name != own && !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort();
    names
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn other_users() -> Vec<String> {
    Vec::new()
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
