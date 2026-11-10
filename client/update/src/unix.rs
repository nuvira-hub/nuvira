//! Installing an update on Linux and macOS (docs/19-updates.md).
//!
//! What gets replaced depends on how NuviraHub was installed ([`this_install`]):
//! - Linux, a program the user can replace (`install.sh` of the tar.gz in
//!   `~/.local/share/nuvira`, or a copy anywhere): the `nuvira` program, channel
//!   `linux-x64`, by the user's unattended agent or the app;
//! - Linux, from the .deb or .rpm (`/usr/bin/nuvira`, root's): the package, channel
//!   `linux-x64-deb` / `linux-x64-rpm`, by root's timer (`nuvira service
//!   update-package`), never by the user;
//! - macOS: the whole NuviraHub.app (program, CUPS backend, signature), channel
//!   `macos`, a zip made with `ditto`, when the user can write the app and the
//!   folder holding it (an administrator, for an app dragged into /Applications);
//! - macOS, installed for the whole Mac by the `.pkg` (an app of root's): by the
//!   root service of that installation (`nuvira service managed-run`), never by the
//!   user: moving a folder elsewhere needs write access to the folder itself.
//!
//! A replaced program is tried (`agent --version`) before it takes the old one's
//! place, and the old one comes back if anything fails. Programs already running
//! keep the old code: the unattended agent restarts by itself once it is not in a
//! session ([`Replaced`]), the open app shows "Restart".

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

/// How this copy of NuviraHub can be updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// Linux: the program, in a folder this user can write.
    Program(PathBuf),
    /// macOS: the app bundle, in a folder this user can write.
    Bundle(PathBuf),
    /// Linux: installed by a package, updated by root's timer.
    Package(PackageKind),
    /// macOS: the app bundle is root's (the `.pkg` for the whole Mac): the root
    /// service of that installation updates it.
    RootBundle(PathBuf),
    /// This user cannot replace it (e.g. /Applications for a standard user).
    ReadOnly(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Deb,
    Rpm,
}

impl PackageKind {
    /// Update channel of the package.
    pub fn channel(self) -> &'static str {
        match self {
            PackageKind::Deb => "linux-x64-deb",
            PackageKind::Rpm => "linux-x64-rpm",
        }
    }

    /// The package NuviraHub is installed from, if any.
    pub fn installed() -> Option<Self> {
        let ok = |program: &str, args: &[&str], expect: &str| {
            Command::new(program)
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(expect))
        };
        if ok("dpkg-query", &["-W", "-f=${Status}", "nuvira"], "install ok installed") {
            Some(PackageKind::Deb)
        } else if ok("rpm", &["-q", "nuvira"], "nuvira") {
            Some(PackageKind::Rpm)
        } else {
            None
        }
    }
}

impl Install {
    /// Channel of the file that updates this install, if this user can apply it.
    pub fn channel(&self) -> Option<&'static str> {
        match self {
            Install::Program(_) | Install::Bundle(_) => Some(crate::platform()),
            Install::Package(_) | Install::RootBundle(_) | Install::ReadOnly(_) => None,
        }
    }
}

/// How the program `exe` (this one: `std::env::current_exe()` at start-up) is installed.
pub fn this_install(exe: &Path) -> Install {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        match bundle_of(exe) {
            // The bundle itself too: it is moved aside into another folder, which
            // needs write access to it (an app of root's: "Permission denied",
            // Mac mini 2026-11-10).
            Some(bundle) if bundle.parent().is_some_and(writable) && writable(&bundle) => Install::Bundle(bundle),
            Some(bundle) if std::fs::metadata(&bundle).is_ok_and(|m| m.uid() == 0) => Install::RootBundle(bundle),
            Some(bundle) => Install::ReadOnly(bundle),
            None => Install::ReadOnly(exe.to_path_buf()),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if exe.starts_with("/usr/")
            && let Some(kind) = PackageKind::installed()
        {
            return Install::Package(kind);
        }
        match exe.parent() {
            Some(dir) if writable(dir) => Install::Program(exe.to_path_buf()),
            _ => Install::ReadOnly(exe.to_path_buf()),
        }
    }
}

/// `…/NuviraHub.app` for `…/NuviraHub.app/Contents/MacOS/nuvira`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension().is_some_and(|e| e == "app") && exe.parent()?.ends_with("Contents/MacOS"))
        .then(|| bundle.to_path_buf())
}

fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: a valid NUL-terminated path.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// The copy put aside by the last update, and the folder it was unpacked in.
fn leftovers(install: &Install) -> Vec<PathBuf> {
    match install {
        Install::Program(exe) => vec![aside_of(exe)],
        Install::Bundle(bundle) => bundle.parent().map(|p| vec![p.join(WORK_DIR)]).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Removes what the last update left (the old program is no longer running).
pub fn clean(install: &Install) {
    for path in leftovers(install) {
        let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
}

fn aside_of(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Folder beside the bundle where the new one is unpacked (same volume: renames).
const WORK_DIR: &str = ".NuviraHub-update";

/// Puts the downloaded, already verified file in place of this install.
pub fn apply(install: &Install, downloaded: &Path) -> Result<()> {
    match install {
        Install::Program(exe) => apply_program(exe, downloaded),
        Install::Bundle(bundle) => apply_bundle(bundle, downloaded),
        Install::Package(_) => bail!("installed from a package: root's timer updates it"),
        Install::RootBundle(_) => bail!("installed for the whole Mac: the NuviraHub service updates it"),
        Install::ReadOnly(path) => bail!("{} cannot be replaced by this user", path.display()),
    }
}

/// The new program must start (libraries, architecture) before replacing anything.
fn try_program(program: &Path) -> Result<()> {
    let output = Command::new(program)
        .args(["agent", "--version"])
        .output()
        .with_context(|| format!("starting {}", program.display()))?;
    if !output.status.success() {
        bail!("the new program does not start: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn apply_program(exe: &Path, downloaded: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = exe.parent().context("program folder")?;
    let new = dir.join(".nuvira.new");
    std::fs::copy(downloaded, &new).with_context(|| format!("copying into {}", dir.display()))?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    if let Err(e) = try_program(&new) {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    let aside = aside_of(exe);
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(exe, &aside).context("moving the old program aside")?;
    if let Err(e) = std::fs::rename(&new, exe) {
        let _ = std::fs::rename(&aside, exe);
        let _ = std::fs::remove_file(&new);
        return Err(e).context("putting the new program in place");
    }
    Ok(())
}

fn apply_bundle(bundle: &Path, downloaded: &Path) -> Result<()> {
    let parent = bundle.parent().context("folder of the app")?;
    let work = parent.join(WORK_DIR);
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let result = (|| {
        run("/usr/bin/ditto", &["-x", "-k"], &[downloaded, &work]).context("unpacking the update")?;
        let new = work.join(bundle.file_name().context("app name")?);
        if !new.join("Contents/MacOS/nuvira").exists() {
            bail!("the update holds no {}", bundle.file_name().unwrap_or_default().to_string_lossy());
        }
        run("/usr/bin/codesign", &["--verify", "--deep", "--strict"], &[&new])
            .context("signature of the new app")?;
        // Same signer and identifier: macOS keeps the Screen Recording and
        // Accessibility permissions only for the same designated requirement.
        if let Some(current) = requirement(bundle)
            && requirement(&new).as_deref() != Some(current.as_str())
        {
            bail!("the new app is not signed like this one");
        }
        try_program(&new.join("Contents/MacOS/nuvira"))?;
        // Unpacked by root (the service of a whole-Mac installation) the files keep
        // the owner of the archive: the app must stay root's, as the .pkg left it,
        // or the people's agents would take it for theirs.
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            run("/usr/sbin/chown", &["-R", "root:wheel"], &[&new]).context("owner of the new app")?;
        }
        let aside = work.join("previous.app");
        std::fs::rename(bundle, &aside).context("moving the old app aside")?;
        if let Err(e) = std::fs::rename(&new, bundle) {
            let _ = std::fs::rename(&aside, bundle);
            return Err(e).context("putting the new app in place");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&work);
    }
    result
}

fn run(program: &str, args: &[&str], paths: &[&Path]) -> Result<()> {
    let output =
        Command::new(program).args(args).args(paths).output().with_context(|| program.to_string())?;
    if !output.status.success() {
        bail!("{program}: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// The designated requirement of a signed bundle (`None` if unsigned).
fn requirement(bundle: &Path) -> Option<String> {
    let output = Command::new("/usr/bin/codesign").args(["-d", "-r-"]).arg(bundle).output().ok()?;
    let text =
        format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    text.lines().find_map(|l| l.strip_prefix("designated => ").map(str::to_string))
}

/// Installs a downloaded, verified package (root, Linux).
pub fn install_package(kind: PackageKind, file: &Path) -> Result<()> {
    let exists = |p: &str| Path::new(p).exists();
    let mut command = match kind {
        PackageKind::Deb if exists("/usr/bin/apt-get") => {
            let mut c = Command::new("/usr/bin/apt-get");
            c.args([
                "install",
                "-y",
                "-o",
                "Dpkg::Options::=--force-confdef",
                "-o",
                "Dpkg::Options::=--force-confold",
            ]);
            c
        }
        PackageKind::Deb => {
            let mut c = Command::new("dpkg");
            c.arg("-i");
            c
        }
        PackageKind::Rpm if exists("/usr/bin/dnf") => {
            let mut c = Command::new("/usr/bin/dnf");
            c.args(["install", "-y"]);
            c
        }
        PackageKind::Rpm if exists("/usr/bin/zypper") => {
            let mut c = Command::new("/usr/bin/zypper");
            c.args(["--non-interactive", "install", "--allow-unsigned-rpm"]);
            c
        }
        PackageKind::Rpm => {
            let mut c = Command::new("rpm");
            c.arg("-U");
            c
        }
    };
    let output = command
        .arg(file)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("LC_ALL", "C")
        .output()
        .context("package manager")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        let line = error.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default();
        bail!("installing {}: {}", file.display(), line.trim());
    }
    Ok(())
}

/// The program file was replaced since start-up (by an update of any kind).
pub struct Replaced {
    path: PathBuf,
    at_start: Option<(u64, u64, i64)>,
}

impl Replaced {
    pub fn watch(path: &Path) -> Self {
        Self { path: path.to_path_buf(), at_start: identity(path) }
    }

    pub fn happened(&self) -> bool {
        // Missing: in the middle of the swap.
        self.at_start.is_some() && identity(&self.path).is_some_and(|now| Some(now) != self.at_start)
    }
}

fn identity(path: &Path) -> Option<(u64, u64, i64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino(), m.mtime()))
}

const FIRST_CHECK: Duration = Duration::from_secs(120);
const INTERVAL: Duration = Duration::from_secs(6 * 3600);
const RETRY: Duration = Duration::from_secs(3600);
/// The root service of a whole-Mac installation checks every hour, as Linux root's timer.
const ROOT_INTERVAL: Duration = Duration::from_secs(3600);
/// Busy when the update was found: tried again soon.
const BUSY_RETRY: Duration = Duration::from_secs(300);

/// Exit code of an agent that ends to start again with its replaced program. Not 0:
/// the Mac's LaunchAgents of the whole-Mac installation and of the sign-in screen
/// restart only after an unsuccessful exit (`KeepAlive` → `SuccessfulExit` false),
/// so that their agent can end for good where it has nothing to do; with 0 a Mac
/// updated by its MDM stayed unreachable until the next sign-in. Not 75 either,
/// which the Linux session unit keeps for "not handed over" (no restart).
pub const RESTART_EXIT: i32 = 3;

/// The unattended agent (user service): checks its server 2 minutes after start
/// and every 6 hours, installs what this user can install, and ends the process
/// when its program was replaced and no session is in progress (`busy` is the
/// file the agent keeps during a session): systemd or launchd start it again with
/// the new program. `cache` holds the downloads.
pub fn spawn_agent_updates(base: String, busy: PathBuf, cache: PathBuf) {
    let Ok(exe) = std::env::current_exe() else { return };
    let spawned = std::thread::Builder::new().name("updates".into()).spawn(move || {
        let install = this_install(&exe);
        clean(&install);
        let replaced = Replaced::watch(&exe);
        match &install {
            Install::Package(kind) => info!("installed from the {kind:?} package: root's timer updates it"),
            Install::RootBundle(_) => info!("installed for the whole Mac: the NuviraHub service updates it"),
            Install::ReadOnly(path) => {
                warn!("automatic updates off: {} cannot be replaced by this user", path.display())
            }
            _ => {}
        }
        let mut next = Instant::now() + FIRST_CHECK;
        loop {
            std::thread::sleep(Duration::from_secs(15));
            if replaced.happened() && !busy.exists() {
                info!("the program was updated: restarting with the new one");
                std::process::exit(RESTART_EXIT);
            }
            let Some(channel) = install.channel() else { continue };
            if Instant::now() < next || replaced.happened() {
                continue;
            }
            next = Instant::now()
                + match check_and_apply(&base, &install, channel, &|| busy.exists(), &cache) {
                    Ok(wait) => wait,
                    Err(e) => {
                        warn!("update check failed: {e:#}");
                        RETRY
                    }
                };
        }
    });
    if let Err(e) = spawned {
        warn!("no automatic updates: {e}");
    }
}

fn check_and_apply(
    base: &str,
    install: &Install,
    channel: &str,
    busy: &dyn Fn() -> bool,
    cache: &Path,
) -> Result<Duration> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let Some(manifest) = rt.block_on(crate::check_channel(base, &crate::PUBLIC_KEYS, channel))? else {
        return Ok(INTERVAL);
    };
    info!("version {} available (this is {})", manifest.version, crate::CURRENT);
    std::fs::create_dir_all(cache)?;
    let file = cache.join(format!("nuvira-{}{}", manifest.version, crate::extension(channel)));
    if !file.exists() {
        rt.block_on(crate::download(base, &manifest, &file))?;
        info!("version {} downloaded and checked", manifest.version);
    }
    // Not in the middle of someone's work.
    if busy() {
        info!("session in progress: the update waits for it to end");
        return Ok(BUSY_RETRY);
    }
    info!("installing version {}", manifest.version);
    let result = apply(install, &file);
    let _ = std::fs::remove_file(&file);
    result.map(|()| {
        info!("version {} installed", manifest.version);
        INTERVAL
    })
}

/// macOS, installed for the whole Mac: the root service updates the app of root's
/// as the users' agents update their own, 2 minutes after it starts and then every
/// hour (as Linux root's timer). `busy` tells whether a session is in progress on
/// the Mac; `restarted` is called once the app was replaced (the service then ends
/// and launchd starts the new program; the agents restart by themselves).
pub fn spawn_root_bundle_updates(
    base: String,
    busy: Box<dyn Fn() -> bool + Send>,
    cache: PathBuf,
    restarted: Box<dyn Fn() + Send>,
) {
    let Ok(exe) = std::env::current_exe() else { return };
    let spawned = std::thread::Builder::new().name("updates".into()).spawn(move || {
        let install = this_install(&exe);
        let Install::Bundle(_) = &install else {
            warn!("automatic updates off: {install:?} cannot be replaced by the service");
            return;
        };
        clean(&install);
        let replaced = Replaced::watch(&exe);
        std::thread::sleep(FIRST_CHECK);
        loop {
            let wait = match check_and_apply(&base, &install, crate::platform(), &*busy, &cache) {
                Ok(wait) => wait.min(ROOT_INTERVAL),
                Err(e) => {
                    warn!("update check failed: {e:#}");
                    RETRY
                }
            };
            if replaced.happened() {
                info!("the app was updated: starting again with the new one");
                restarted();
                return;
            }
            std::thread::sleep(wait);
        }
    });
    if let Err(e) = spawned {
        warn!("no automatic updates: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_of_the_program() {
        assert_eq!(
            bundle_of(Path::new("/Applications/NuviraHub.app/Contents/MacOS/nuvira")),
            Some(PathBuf::from("/Applications/NuviraHub.app"))
        );
        assert_eq!(bundle_of(Path::new("/usr/bin/nuvira")), None);
        assert_eq!(bundle_of(Path::new("/tmp/x.app/Contents/Resources/nuvira")), None);
    }

    #[test]
    fn channels() {
        assert_eq!(PackageKind::Deb.channel(), "linux-x64-deb");
        assert_eq!(Install::Package(PackageKind::Rpm).channel(), None);
        assert_eq!(Install::Program("/x/nuvira".into()).channel(), Some(crate::platform()));
        assert_eq!(aside_of(Path::new("/x/nuvira")), PathBuf::from("/x/nuvira.old"));
    }

    /// A program replaced by a copy that starts; one that does not start is refused
    /// and the old one stays.
    #[test]
    fn program_replaced_only_if_it_starts() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("nuvira-apply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("nuvira");
        let script = |path: &Path, body: &str| {
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        script(&exe, "echo old");
        let replaced = Replaced::watch(&exe);
        let broken = dir.join("broken");
        script(&broken, "exit 3");
        assert!(apply(&Install::Program(exe.clone()), &broken).is_err());
        assert!(std::fs::read_to_string(&exe).unwrap().contains("old"));
        assert!(!replaced.happened());
        let good = dir.join("good");
        script(&good, "echo new");
        apply(&Install::Program(exe.clone()), &good).unwrap();
        assert!(std::fs::read_to_string(&exe).unwrap().contains("new"));
        assert!(aside_of(&exe).exists() && replaced.happened());
        clean(&Install::Program(exe.clone()));
        assert!(!aside_of(&exe).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
