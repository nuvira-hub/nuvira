//! The system's part of unattended access on Linux and macOS, installed as root
//! after a single administrator prompt: the NuviraHub printer (remote printing)
//! and the sign-in screen's agent. Without the password the user's
//! part works the same.
//!
//! ```text
//! nuvira service system --user-data DIR --server HOST [--server-key FP]   (as root)
//! nuvira service system --remove                                          (as root)
//! ```

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::cups;

/// Something of the system's part is installed.
pub fn installed() -> bool {
    crate::signin::installed() || (cups::installed() && !cups::packaged())
}

/// Something of the system's part is missing or must be brought up to date: always,
/// since the sign-in screen's agent keeps a copy of the password and of the server.
pub fn wanted() -> bool {
    true
}

/// As root: installs what is missing. Each part is tried even if the other fails.
pub fn install(user_data: &Path, server: &str, server_key: Option<&str>) -> Result<()> {
    let mut errors = Vec::new();
    if cups::available() && !cups::installed() {
        match cups::install() {
            Ok(done) => info!("{done}"),
            Err(e) => errors.push(format!("printer: {e:#}")),
        }
    }
    let exe = std::env::current_exe().context("program path")?;
    if let Err(e) = crate::signin::install(&exe, user_data, server, server_key) {
        errors.push(format!("sign-in screen: {e:#}"));
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = crate::gnome::install_system() {
        errors.push(format!("GNOME sign-in screen: {e:#}"));
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::hid::install(&exe) {
        errors.push(format!("lock screen keyboard: {e:#}"));
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(())
}

/// The copy of the person's device key the app leaves for root, next to their key,
/// just for the administrator prompt: root cannot open the person's own (their
/// login keychain or Secret Service).
const HANDED_KEY: &str = "device.key.handed";

/// As the person, before the prompt: their key, handed over for root.
fn hand_key_to_root(user_data: &Path) -> Option<std::path::PathBuf> {
    let handed = user_data.join(HANDED_KEY);
    match nuvira_transport::identity::hand_over(&user_data.join("device.key"), &handed) {
        Ok(()) => Some(handed),
        Err(e) => {
            warn!("device key not handed over for the sign-in screen: {e:#}");
            None
        }
    }
}

/// As root: the person's key (the copy they handed over, or their key file if it
/// is a plain one) into `to`, protected for root. The handed copy is removed.
pub(crate) fn take_key(user_data: &Path, to: &Path) -> Result<()> {
    let handed = user_data.join(HANDED_KEY);
    let from = if handed.exists() { handed.clone() } else { user_data.join("device.key") };
    let result = nuvira_transport::identity::import(&from, to);
    let _ = std::fs::remove_file(&handed);
    let protection = result.with_context(|| format!("the person's device key ({})", from.display()))?;
    std::os::unix::fs::chown(to, Some(0), Some(0))?;
    std::fs::set_permissions(to, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    info!("device key for the sign-in screen: {}", protection.name());
    Ok(())
}

/// As root: removes the system's part (the packages keep their printer).
pub fn uninstall() -> Result<()> {
    let mut errors = Vec::new();
    if !cups::packaged()
        && let Err(e) = cups::uninstall()
    {
        errors.push(format!("printer: {e:#}"));
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = crate::gnome::uninstall_system() {
        errors.push(format!("GNOME sign-in screen: {e:#}"));
    }
    // The keyboard first: the sign-in screen's removal deletes the data folder.
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::hid::uninstall() {
        errors.push(format!("lock screen keyboard: {e:#}"));
    }
    if let Err(e) = crate::signin::uninstall() {
        errors.push(format!("sign-in screen: {e:#}"));
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(())
}

/// A word for the shell, in single quotes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The command line of `nuvira service system` for this person's settings.
fn arguments(remove: bool, server: &str, server_key: Option<&str>) -> Vec<String> {
    let mut args = vec!["service".to_string(), "system".into()];
    if remove {
        args.push("--remove".into());
        return args;
    }
    args.push("--user-data".into());
    args.push(nuvira_transport::identity::data_dir().display().to_string());
    args.push("--server".into());
    args.push(server.to_string());
    if let Some(key) = server_key {
        args.push("--server-key".into());
        args.push(key.to_string());
    }
    args
}

/// `nuvira service system` of this program as root, after the system's
/// administrator prompt: pkexec shows the text of its own policy, macOS `reason`
/// (macOS 27 only its own text). `Ok(false)`: the person did not give the password.
pub fn run_as_admin(remove: bool, reason: &str, server: &str, server_key: Option<&str>) -> Result<bool> {
    let handed = if remove { None } else { hand_key_to_root(&nuvira_transport::identity::data_dir()) };
    let result = elevated(remove, reason, server, server_key);
    if let Some(handed) = handed {
        let _ = std::fs::remove_file(handed);
    }
    result
}

fn elevated(remove: bool, reason: &str, server: &str, server_key: Option<&str>) -> Result<bool> {
    let exe = std::env::current_exe().context("program path")?;
    let args = arguments(remove, server, server_key);
    #[cfg(target_os = "linux")]
    {
        let _ = reason;
        let output = Command::new("pkexec").arg(&exe).args(&args).output().context("pkexec")?;
        match output.status.code() {
            Some(0) => Ok(true),
            // Dismissed, or not authorized.
            Some(126 | 127) => Ok(false),
            _ => bail!("{}", last_line(&output)),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let script = [
            "on run argv",
            "do shell script (quoted form of item 1 of argv) & \" \" & (item 2 of argv) with prompt (item 3 of argv) with administrator privileges",
            "end run",
        ];
        let mut command = Command::new("/usr/bin/osascript");
        for line in script {
            command.args(["-e", line]);
        }
        let line: Vec<String> = args.iter().map(|a| quoted(a)).collect();
        let output = command.arg(&exe).arg(line.join(" ")).arg(reason).output().context("osascript")?;
        if output.status.success() {
            return Ok(true);
        }
        // "User canceled. (-128)"
        if String::from_utf8_lossy(&output.stderr).contains("-128") {
            return Ok(false);
        }
        bail!("{}", last_line(&output))
    }
}

fn last_line(output: &std::process::Output) -> String {
    let error = String::from_utf8_lossy(&output.stderr);
    let line = error.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    line.strip_prefix("error: ").unwrap_or(line).to_string()
}

/// `nuvira service system ...` (as root).
pub fn command(args: &[String]) -> Result<()> {
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "--remove") {
        return uninstall().map(|()| println!("system part of unattended access removed"));
    }
    let user_data = value("--user-data").context("--user-data is needed")?;
    let server = value("--server").unwrap_or_default();
    let result = install(Path::new(&user_data), &server, value("--server-key").as_deref());
    if let Err(e) = &result {
        warn!("{e:#}");
    }
    result.map(|()| println!("system part of unattended access installed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_quoted_for_the_shell() {
        assert_eq!(quoted(""), "''");
        assert_eq!(quoted("a b"), "'a b'");
        assert_eq!(quoted("it's"), "'it'\\''s'");
    }

    #[test]
    fn removal_needs_no_settings() {
        assert_eq!(arguments(true, "x", Some("k")), ["service", "system", "--remove"]);
        let args = arguments(false, "", None);
        assert_eq!(args[..3], ["service", "system", "--user-data"]);
        assert_eq!(args[4..], ["--server", ""]);
    }
}
