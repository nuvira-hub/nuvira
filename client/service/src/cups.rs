//! The "NuviraHub" printer on Linux and macOS (remote printing,
//! docs/29-remote-printing.md): a CUPS queue with our PPD and our backend
//! (`nuvira_cups`). What is printed on it reaches the agent of a session as PDF.
//!
//! Installing it needs root: `nuvira service printer [--remove]` does it (the .deb
//! and .rpm packages run it); the app runs it through pkexec (Linux) or the macOS
//! administrator prompt while installing unattended access ([`run_as_admin`]).

use anyhow::{Context, Result, bail};
use nuvira_cups::PRINTER;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The backend program, shipped beside `nuvira` (in the .app bundle, the tar.gz) or
/// in [`PACKAGED_BACKEND`] (.deb and .rpm), copied into CUPS's backend folder.
const BACKEND_PROGRAM: &str = "nuvira-cups";
#[cfg(target_os = "linux")]
const PACKAGED_BACKEND: &str = "/usr/lib/nuvira/nuvira-cups";

/// CUPS is there (its commands at least).
pub fn available() -> bool {
    Command::new("lpstat").arg("-r").env("LC_ALL", "C").output().is_ok()
}

/// The printer is installed: backend and queue.
pub fn installed() -> bool {
    nuvira_cups::backend_path().exists()
        && Command::new("lpstat").args(["-v", PRINTER]).env("LC_ALL", "C").output().is_ok_and(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains(&format!("{}:", nuvira_cups::SCHEME))
        })
}

/// Installed by a .deb or .rpm package, which also removes it.
pub fn packaged() -> bool {
    #[cfg(target_os = "linux")]
    return Path::new(PACKAGED_BACKEND).exists()
        && std::env::current_exe().is_ok_and(|exe| exe.starts_with("/usr/"));
    #[cfg(not(target_os = "linux"))]
    false
}

fn ensure_root() -> Result<()> {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!("the NuviraHub printer is installed by root: sudo nuvira service printer");
    }
    Ok(())
}

/// `lpadmin` is in /usr/sbin, not always in the PATH.
fn lpadmin() -> Command {
    let path = ["/usr/sbin/lpadmin", "/usr/bin/lpadmin"].into_iter().find(|p| Path::new(p).exists());
    let mut command = Command::new(path.unwrap_or("lpadmin"));
    command.env("LC_ALL", "C");
    command
}

fn shipped_backend() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok()?.with_file_name(BACKEND_PROGRAM);
    #[cfg(target_os = "linux")]
    let candidates = [beside, PathBuf::from(PACKAGED_BACKEND)];
    #[cfg(not(target_os = "linux"))]
    let candidates = [beside];
    candidates.into_iter().find(|p| p.is_file())
}

/// Adds the queue, or updates backend and PPD of the one there (root).
pub fn install() -> Result<&'static str> {
    ensure_root()?;
    if !available() {
        bail!("CUPS is not installed: no remote printing");
    }
    let backend = nuvira_cups::backend_path();
    match shipped_backend() {
        Some(shipped) => copy_backend(&shipped, &backend)?,
        None if backend.exists() => {}
        None => bail!("{BACKEND_PROGRAM} not found beside this program"),
    }
    let ppd = std::env::temp_dir().join(format!("nuvira-printer-{}.ppd", std::process::id()));
    std::fs::write(&ppd, nuvira_cups::PPD).context("writing the PPD")?;
    let uri = format!("{}:/", nuvira_cups::SCHEME);
    let output = lpadmin()
        .args(["-p", PRINTER, "-E", "-v", &uri, "-P"])
        .arg(&ppd)
        .args(["-D", PRINTER, "-o", "printer-is-shared=false", "-o", "printer-error-policy=abort-job"])
        .output();
    let _ = std::fs::remove_file(&ppd);
    let output = output.context("lpadmin")?;
    if !output.status.success() {
        bail!("lpadmin: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok("NuviraHub printer installed (remote printing)")
}

/// CUPS runs a backend as root only if nobody else can execute or change it.
fn copy_backend(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = to.parent().context("backend folder")?;
    if !dir.is_dir() {
        bail!("CUPS's backend folder {} is missing", dir.display());
    }
    let new = dir.join(format!(".{}.new", nuvira_cups::SCHEME));
    std::fs::copy(from, &new)
        .with_context(|| format!("copying {} into {}", from.display(), dir.display()))?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o700))?;
    std::fs::rename(&new, to).with_context(|| format!("installing {}", to.display()))?;
    // SELinux: the label of the backend folder's programs, not the copy's.
    #[cfg(target_os = "linux")]
    let _ = Command::new("restorecon").arg(to).output();
    // Copied out of an app downloaded from the internet.
    #[cfg(target_os = "macos")]
    let _ = Command::new("/usr/bin/xattr").args(["-d", "com.apple.quarantine"]).arg(to).output();
    Ok(())
}

/// Removes queue and backend (nothing to do if they are not there).
pub fn uninstall() -> Result<()> {
    ensure_root()?;
    if available() {
        let output = lpadmin().args(["-x", PRINTER]).output().context("lpadmin")?;
        let error = String::from_utf8_lossy(&output.stderr);
        // Not there: fine.
        if !output.status.success() && !error.contains("does not exist") && !error.contains("not found") {
            bail!("lpadmin: {}", error.trim());
        }
    }
    match std::fs::remove_file(nuvira_cups::backend_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).context("removing the backend"),
        _ => Ok(()),
    }
}

/// `nuvira service printer [--remove]` of this program as root, after the system's
/// administrator prompt. `Ok(false)`: the person did not give the password.
pub fn run_as_admin(remove: bool) -> Result<bool> {
    let exe = std::env::current_exe().context("program path")?;
    let mut args = vec!["service", "printer"];
    if remove {
        args.push("--remove");
    }
    #[cfg(target_os = "linux")]
    {
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
            "do shell script (quoted form of item 1 of argv) & \" \" & (item 2 of argv) with administrator privileges",
            "end run",
        ];
        let mut command = Command::new("/usr/bin/osascript");
        for line in script {
            command.args(["-e", line]);
        }
        let output = command.arg(&exe).arg(args.join(" ")).output().context("osascript")?;
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
