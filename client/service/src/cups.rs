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
    let ppd = PrivatePpd::write(&std::env::temp_dir())?;
    let uri = format!("{}:/", nuvira_cups::SCHEME);
    let output = lpadmin()
        .args(["-p", PRINTER, "-E", "-v", &uri, "-P"])
        .arg(ppd.path())
        .args(["-D", PRINTER, "-o", "printer-is-shared=false", "-o", "printer-error-policy=abort-job"])
        .output();
    drop(ppd);
    let output = output.context("lpadmin")?;
    if !output.status.success() {
        bail!("lpadmin: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok("NuviraHub printer installed (remote printing)")
}

/// The PPD for `lpadmin`, in a new folder of its own (mode 0700, random name) in
/// `parent`: root runs this, and on Linux `parent` is the shared /tmp, where a
/// predictable name could be a file or a link someone else put there first.
/// The folder is created, never reused, and the file inside it too; both are
/// removed on drop.
struct PrivatePpd {
    dir: PathBuf,
    file: PathBuf,
}

impl PrivatePpd {
    fn write(parent: &Path) -> Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        let mut tries = 0;
        let dir = loop {
            let mut random = [0u8; 12];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
                .map_err(|_| anyhow::anyhow!("random generator"))?;
            let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let dir = parent.join(format!("nuvira-printer-{name}"));
            // Not recursive: fails if anything (also a link) has that name.
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
                Err(e) => {
                    return Err(e).with_context(|| format!("folder for the PPD in {}", parent.display()));
                }
            }
        };
        let file = dir.join("nuvira.ppd");
        let ppd = Self { dir, file };
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&ppd.file)
            .context("writing the PPD")?;
        out.write_all(nuvira_cups::PPD.as_bytes()).context("writing the PPD")?;
        Ok(ppd)
    }

    fn path(&self) -> &Path {
        &self.file
    }
}

impl Drop for PrivatePpd {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.dir);
    }
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
    // macOS's copy (a clone) keeps the owner of the original, the user's.
    std::os::unix::fs::chown(&new, Some(0), Some(0)).context("backend owned by root")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// APP-4: what someone else left at the old predictable names in the
    /// shared folder is not written to nor followed; the PPD is in a new
    /// private folder, removed afterwards.
    #[test]
    fn ppd_written_in_a_private_folder() {
        let parent = std::env::temp_dir().join(format!("nuvira-ppd-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        // The old name, as a planted file and as a link to a file.
        let planted = parent.join(format!("nuvira-printer-{}.ppd", std::process::id()));
        std::fs::write(&planted, "planted").unwrap();
        let target = parent.join("target");
        std::fs::write(&target, "target").unwrap();
        let link = parent.join(format!("nuvira-printer-{}.ppd", std::process::id() + 1));
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let ppd = PrivatePpd::write(&parent).unwrap();
        let path = ppd.path().to_path_buf();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), nuvira_cups::PPD);
        let dir = path.parent().unwrap().to_path_buf();
        assert_eq!(dir.parent().unwrap(), parent);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // A second one gets another folder.
        let other = PrivatePpd::write(&parent).unwrap();
        assert_ne!(other.path().parent(), Some(dir.as_path()));
        drop(other);
        drop(ppd);
        assert!(!path.exists() && !dir.exists(), "removed after use");
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), "planted");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "target");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        // Only what the test made is left.
        let mut left: Vec<_> = std::fs::read_dir(&parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left.len(), 3, "{left:?}");
        std::fs::remove_dir_all(&parent).unwrap();
    }
}
