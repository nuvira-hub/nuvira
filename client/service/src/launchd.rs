//! macOS: unattended access through a launchd **LaunchAgent** of the person's
//! user (`~/Library/LaunchAgents`), the counterpart of the Linux user service:
//! - launchd starts it at every login, in the desktop (Aqua) session, so the
//!   agent can read the screen and inject input, and starts it again if it ends;
//! - it runs the program inside the app's bundle, so macOS sees the same signed
//!   app: the Screen Recording and Accessibility permissions given to NuviraHub
//!   hold for it too (a copy of the program elsewhere would need them again);
//! - the permanent password is in `service.env` (mode 0600), as a hash, and is
//!   handed to the agent by the definition (mode 0600 as well).
//!
//! What it cannot do: reach the Mac before someone logs in, or while the screen
//! is locked. After a restart the Mac needs automatic login (not available with
//! FileVault on).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::info;

pub use crate::unix_shared::{InstallOptions, ServiceState, installed_version};
use crate::unix_shared::{env_path, home, password_hash, store_password, write_private};

/// Name of the job for launchd, after the app's bundle identifier.
pub const LABEL: &str = "io.github.nuvira-hub.nuvira.agent";
/// The app's bundle, shown by macOS as the owner of the background item.
const BUNDLE_ID: &str = "io.github.nuvira-hub.nuvira";

pub fn plist_path() -> Result<PathBuf> {
    Ok(home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn log_path() -> Result<PathBuf> {
    Ok(home()?.join("Library/Logs/NuviraHub/agent.log"))
}

/// The person's launchd domain (`gui/UID`).
fn domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn job() -> String {
    format!("{}/{LABEL}", domain())
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("/bin/launchctl").args(args).output().context("launchctl")
}

fn launchctl_ok(args: &[&str]) -> Result<()> {
    let out = launchctl(args)?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr);
        let text = if text.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { text };
        bail!("launchctl {} failed ({}): {}", args.join(" "), out.status, text.trim());
    }
    Ok(())
}

/// A program opened from the disk image, or moved by Gatekeeper to a random
/// read-only place ("App Translocation"), would be gone at the next login.
fn check_location(exe: &Path) -> Result<()> {
    let path = exe.to_string_lossy();
    if path.starts_with("/Volumes/") || path.contains("/AppTranslocation/") {
        bail!("@install.move_to_applications");
    }
    Ok(())
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The job's definition (property list).
fn definition(exe: &Path, opts: &InstallOptions, hash: &str, log: &Path) -> String {
    // Always with --server: empty is the public server (without it the agent
    // would wait for viewers on the local network only).
    let mut args = vec![exe.display().to_string(), "agent".into(), "--ipc".into(), "--server".into(), opts.server.clone()];
    if let Some(key) = &opts.server_key {
        args.push("--server-key".into());
        args.push(key.clone());
    }
    let args: String = args.iter().map(|a| format!("\t\t<string>{}</string>\n", xml(a))).collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>{LABEL}</string>\n\
         \t<key>AssociatedBundleIdentifiers</key>\n\t<array>\n\t\t<string>{BUNDLE_ID}</string>\n\t</array>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n{args}\t</array>\n\
         \t<key>EnvironmentVariables</key>\n\t<dict>\n\
         \t\t<key>NUVIRA_PASSWORD_HASH</key>\n\t\t<string>{hash}</string>\n\
         \t</dict>\n\
         \t<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>\n\
         \t<key>ProcessType</key>\n\t<string>Interactive</string>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>5</integer>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
         \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        hash = xml(hash),
        log = xml(&log.display().to_string()),
    )
}

/// Installs or updates the LaunchAgent and starts it. Returns the permanent
/// password when it was generated, to show it once.
pub fn install(opts: &InstallOptions) -> Result<Option<String>> {
    let exe = std::env::current_exe().context("program path")?;
    check_location(&exe)?;
    let generated = store_password(opts)?;
    let log = log_path()?;
    std::fs::create_dir_all(log.parent().context("log folder")?)?;
    let plist = plist_path()?;
    write_private(&plist, &definition(&exe, opts, &password_hash()?, &log))?;
    // A job already loaded keeps its old definition: out, then in again.
    let _ = launchctl(&["bootout", &job()]);
    bootstrap(&plist)?;
    info!(plist = %plist.display(), "LaunchAgent installed");
    Ok(generated)
}

/// `bootstrap` right after a `bootout` can fail while launchd is still taking the
/// old job down ("Input/output error"): tried again for a few seconds.
fn bootstrap(plist: &Path) -> Result<()> {
    let plist = plist.display().to_string();
    let mut last = None;
    for _ in 0..10 {
        match launchctl_ok(&["bootstrap", &domain(), &plist]) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(last.expect("tried"))
}

/// Removes the LaunchAgent; the ID and the address book stay.
pub fn uninstall() -> Result<()> {
    let _ = launchctl(&["bootout", &job()]);
    for path in [plist_path()?, env_path()] {
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| path.display().to_string())?;
        }
    }
    info!("LaunchAgent removed");
    Ok(())
}

/// (Re)starts the agent: loads the job if it is not loaded (removed from the
/// login items and allowed again, for example), otherwise restarts it.
pub fn start() -> Result<()> {
    if launchctl(&["print", &job()])?.status.success() {
        launchctl_ok(&["kickstart", "-k", &job()])
    } else {
        bootstrap(&plist_path()?)
    }
}

/// The Screen Recording permission is asked by the app at start; the agent
/// started by launchd uses the same one. Without it the agent would run with
/// nothing to show: the person is told now, while at the Mac.
pub fn ask_screen_permission() -> Result<()> {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    // SAFETY: plain queries of the permission, no arguments.
    if unsafe { CGPreflightScreenCaptureAccess() } {
        return Ok(());
    }
    if unsafe { CGRequestScreenCaptureAccess() } {
        return Ok(());
    }
    // macOS 27 adds NuviraHub to the list without showing any window: the person is
    // taken to the page where the switch is.
    let _ = Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .status();
    bail!("@install.screen_permission_mac")
}

/// Desktop, Documents and Downloads have permissions of their own on macOS, asked
/// the first time a program opens them: opened now, while the person is at the Mac,
/// so that the file manager of an unattended session finds them allowed (the
/// permission is the app's, the agent is the same app).
pub fn ask_folder_access() {
    let Ok(home) = home() else { return };
    for folder in ["Desktop", "Documents", "Downloads"] {
        // Waits for the person's answer; refused, the folder stays closed to sessions.
        let allowed = std::fs::read_dir(home.join(folder)).is_ok();
        info!(folder, allowed, "access to the folder");
    }
}

pub fn service_state() -> Result<ServiceState> {
    if !plist_path()?.exists() {
        return Ok(ServiceState::NotInstalled);
    }
    let out = launchctl(&["print", &job()])?;
    if !out.status.success() {
        // Not loaded: switched off in the login items, or removed by hand.
        return Ok(ServiceState::Stopped);
    }
    Ok(state_of(&String::from_utf8_lossy(&out.stdout)))
}

/// The `state = …` line of `launchctl print`.
fn state_of(print: &str) -> ServiceState {
    let state = print.lines().find_map(|l| l.trim().strip_prefix("state = ")).unwrap_or("").trim();
    match state {
        "running" => ServiceState::Running,
        // Started again by KeepAlive after ThrottleInterval.
        "spawn scheduled" | "spawning" | "xpcproxy" => ServiceState::Changing,
        _ => ServiceState::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_escapes_and_has_the_job() {
        let opts = InstallOptions { server: "a&b".into(), server_key: Some("ff".into()), password: None };
        let text = definition(Path::new("/Applications/NuviraHub.app/Contents/MacOS/nuvira"), &opts, "h<1>", Path::new("/tmp/l"));
        assert!(text.contains("<string>a&amp;b</string>"));
        assert!(text.contains("<string>--server-key</string>\n\t\t<string>ff</string>"));
        assert!(text.contains("<string>h&lt;1&gt;</string>"));
        assert!(text.contains(&format!("<string>{LABEL}</string>")));
    }

    #[test]
    fn location_from_the_disk_image_refused() {
        assert!(check_location(Path::new("/Volumes/NuviraHub/NuviraHub.app/Contents/MacOS/nuvira")).is_err());
        assert!(check_location(Path::new("/private/var/folders/x/AppTranslocation/y/d/NuviraHub.app/Contents/MacOS/nuvira")).is_err());
        assert!(check_location(Path::new("/Applications/NuviraHub.app/Contents/MacOS/nuvira")).is_ok());
    }

    #[test]
    fn states_from_launchctl() {
        assert_eq!(state_of("gui/501/x = {\n\tactive count = 1\n\tstate = running\n"), ServiceState::Running);
        assert_eq!(state_of("\tstate = spawn scheduled\n"), ServiceState::Changing);
        assert_eq!(state_of("\tstate = not running\n"), ServiceState::Stopped);
    }
}
