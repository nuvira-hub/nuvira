//! macOS: unattended access at the **sign-in screen** (after a restart without
//! automatic login, after logging out). A LaunchAgent of the system,
//! `/Library/LaunchAgents/io.github.nuvira-hub.nuvira.signin.plist`, limited to the
//! `LoginWindow` session: launchd runs it as root while the sign-in screen is shown.
//!
//! - It runs the program inside the app's bundle (`nuvira service signin-run`),
//!   which starts the agent with `--signin-screen`: the same signed app, so the
//!   Screen Recording and Accessibility permissions given to NuviraHub hold there
//!   too. macOS asks once more at the sign-in screen ("nuvira is requesting to
//!   bypass the system private window picker"): the technician answers it.
//! - The agent has the same ID as the person's one: the device key, the server's
//!   fingerprint and the permanent password's hash are copied into
//!   `/Library/Application Support/NuviraHub` (root only) when installing.
//! - Only one of the two agents may be registered (the server keeps the last
//!   connection with an ID): as soon as someone is signed in on the console the
//!   sign-in screen's agent ends, and launchd does not start it again
//!   (`KeepAlive` only after a failure) until the sign-in screen comes back.
//!
//! The lock screen of a signed-in person is another matter: it belongs to their
//! session, and no capture program sees it (see `nuvira_capture`).

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub const LABEL: &str = "io.github.nuvira-hub.nuvira.signin";
const BUNDLE_ID: &str = "io.github.nuvira-hub.nuvira";
/// Root's copy of the data the agent needs.
const SYSTEM_DIR: &str = "/Library/Application Support/NuviraHub";
const LOG: &str = "/Library/Logs/NuviraHub/signin.log";
/// Files of the person's data folder the agent of the sign-in screen needs.
const COPIED: [&str; 3] = ["device.key", "servers.txt", "service.env"];

pub fn plist_path() -> PathBuf {
    PathBuf::from("/Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

/// What `signin-run` starts the agent with.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Settings {
    server: String,
    server_key: Option<String>,
}

fn settings_path() -> PathBuf {
    Path::new(SYSTEM_DIR).join("signin.json")
}

pub fn installed() -> bool {
    plist_path().exists()
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The job's definition. Readable by everybody, as launchd wants it: it holds no
/// secret (the password's hash stays in the root-only folder).
fn definition(exe: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>{LABEL}</string>\n\
         \t<key>AssociatedBundleIdentifiers</key>\n\t<array>\n\t\t<string>{BUNDLE_ID}</string>\n\t</array>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t\t<string>service</string>\n\t\t<string>signin-run</string>\n\t</array>\n\
         \t<key>LimitLoadToSessionType</key>\n\t<string>LoginWindow</string>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{LOG}</string>\n\
         \t<key>StandardOutPath</key>\n\t<string>{LOG}</string>\n\
         </dict>\n\
         </plist>\n",
        exe = xml(&exe.display().to_string()),
    )
}

/// As root: copies the person's ID and password from `user_data` (their data
/// folder) and installs the job. It starts at the next sign-in screen.
pub fn install(exe: &Path, user_data: &Path, server: &str, server_key: Option<&str>) -> Result<()> {
    ensure_root()?;
    let dir = Path::new(SYSTEM_DIR);
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    for name in COPIED {
        let from = user_data.join(name);
        let to = dir.join(name);
        match std::fs::copy(&from, &to) {
            Ok(_) => std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?,
            // Without a remembered fingerprint the first connection's is taken.
            Err(e) if name == "servers.txt" && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| from.display().to_string()),
        }
    }
    let settings = Settings { server: server.to_string(), server_key: server_key.map(str::to_string) };
    std::fs::write(settings_path(), serde_json::to_string(&settings)?)?;
    std::fs::set_permissions(settings_path(), std::fs::Permissions::from_mode(0o600))?;
    if let Some(logs) = Path::new(LOG).parent() {
        std::fs::create_dir_all(logs)?;
    }
    std::fs::write(plist_path(), definition(exe))?;
    std::fs::set_permissions(plist_path(), std::fs::Permissions::from_mode(0o644))?;
    info!(plist = %plist_path().display(), "sign-in screen agent installed");
    Ok(())
}

/// As root: removes the job and root's copy of the data.
pub fn uninstall() -> Result<()> {
    ensure_root()?;
    match std::fs::remove_file(plist_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).context("removing the job"),
        _ => {}
    }
    match std::fs::remove_dir_all(SYSTEM_DIR) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).context(SYSTEM_DIR),
        _ => {}
    }
    info!("sign-in screen agent removed");
    Ok(())
}

fn ensure_root() -> Result<()> {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!("the sign-in screen's agent is installed by root");
    }
    Ok(())
}

/// Someone is signed in on the console (its owner is not root).
fn someone_signed_in() -> bool {
    std::fs::metadata("/dev/console").is_ok_and(|m| m.uid() != 0)
}

/// The permanent password's hash, from root's copy of `service.env`.
fn password_hash() -> Result<String> {
    let text = std::fs::read_to_string(Path::new(SYSTEM_DIR).join("service.env")).context("service.env")?;
    text.lines()
        .find_map(|l| l.strip_prefix("NUVIRA_PASSWORD_HASH="))
        .map(|h| h.trim().to_string())
        .context("no password in service.env")
}

fn start_agent(exe: &Path, settings: &Settings, hash: &str) -> Result<Child> {
    let mut command = Command::new(exe);
    command.args(["agent", "--signin-screen", "--server", &settings.server]);
    if let Some(key) = &settings.server_key {
        command.args(["--server-key", key]);
    }
    command.env("NUVIRA_PASSWORD_HASH", hash).env("NUVIRA_DATA_DIR", SYSTEM_DIR);
    command.spawn().context("starting the agent")
}

/// `nuvira service signin-run`, started by launchd at the sign-in screen: keeps the
/// agent running until someone signs in, then ends (exit 0: not started again).
pub fn run() -> Result<()> {
    if ensure_root().is_err() {
        bail!("signin-run is started by launchd as root");
    }
    // Into the job's log, with the agent's own lines.
    let _ = tracing_subscriber::fmt().with_ansi(false).try_init();
    let exe = std::env::current_exe().context("program path")?;
    let settings: Settings =
        serde_json::from_str(&std::fs::read_to_string(settings_path()).context("signin.json")?)?;
    let hash = password_hash()?;
    let mut agent: Option<Child> = None;
    let mut next_start = Instant::now();
    loop {
        if someone_signed_in() {
            if let Some(mut child) = agent.take() {
                info!("someone signed in: the sign-in screen's agent ends");
                // SAFETY: a child of ours, still to be waited for.
                unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
            return Ok(());
        }
        if let Some(child) = &mut agent
            && let Ok(Some(status)) = child.try_wait()
        {
            // At start-up the network may not be there yet: tried again.
            warn!(%status, "the sign-in screen's agent ended: started again in 5 s");
            agent = None;
            next_start = Instant::now() + Duration::from_secs(5);
        }
        if agent.is_none() && Instant::now() >= next_start {
            match start_agent(&exe, &settings, &hash) {
                Ok(child) => {
                    info!(pid = child.id(), "sign-in screen's agent started");
                    agent = Some(child);
                }
                Err(e) => {
                    warn!("{e:#}");
                    next_start = Instant::now() + Duration::from_secs(10);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_runs_only_at_the_sign_in_screen() {
        let text = definition(Path::new("/Applications/NuviraHub.app/Contents/MacOS/nuvira"));
        assert!(text.contains("<string>LoginWindow</string>"));
        assert!(text.contains("<string>signin-run</string>"));
        assert!(text.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(!text.contains("PASSWORD"));
    }
}
