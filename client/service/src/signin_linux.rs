//! Linux: unattended access at the **sign-in screen** (the display manager's
//! greeter: LightDM, GDM…). A systemd system service, `nuvira-signin.service`,
//! runs `nuvira service signin-run` as root: it follows the active session of
//! `seat0` (logind) and, while it is a greeter, keeps an agent there:
//!
//! - X11 greeters (LightDM): the agent runs as root on the greeter's X display,
//!   with the X server's own authority file (`-auth` of its command line);
//! - Wayland greeters (GDM): the agent runs as the greeter's user, with its
//!   session bus, and reaches the screen through GNOME's own remote desktop API
//!   (there is no portal at the greeter).
//!
//! The agent has the same ID as the person's one: device key, the server's
//! fingerprint and the permanent password's hash are copied into
//! `/var/lib/nuvira` (root only) when installing. Only one of the two agents may
//! be registered (the server keeps the last connection with an ID): as soon as
//! the active session is a person's one, the greeter's agent ends.
//!
//! The program must not be one a user can replace (it runs as root): from the
//! packages it is `/usr/bin/nuvira`; otherwise a copy goes into
//! `/usr/local/lib/nuvira`.
//!
//! The same service blocks the local mouse and keyboard for privacy mode on
//! Wayland, when the person's agent asks (`nuvira_input::local_input`).
//!
//! Installed for the whole computer (`nuvira service deploy`, module `managed`)
//! the ID is the computer's own, kept here, and the service also hands it to the
//! agent of whoever is signed in at the computer, and follows the
//! organisation's policies (`/etc/nuvira/policy.json`).

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const UNIT: &str = "nuvira-signin.service";
pub(crate) const SYSTEM_DIR: &str = "/var/lib/nuvira";
/// Root's copy of the program, when the person's one is not root's.
const PROGRAM_COPY: &str = "/usr/local/lib/nuvira/nuvira";
/// Files of the person's data folder the greeter's agent needs.
const COPIED: [&str; 3] = ["device.key", "servers.txt", "service.env"];
/// SDDM on Wayland: the program the greeter's agent runs as, a link to root's
/// program under a path of its own. KWin offers its screen cast and fake input
/// protocols only to the executable named in [`KWIN_DESKTOP`]; a path of its own
/// keeps that authorisation away from `nuvira` itself (docs: nuvira-pro
/// docs/internal/36-sddm-wayland-greeter.md).
const KWIN_PROGRAM: &str = "/usr/local/lib/nuvira/nuvira-greeter";
const KWIN_DESKTOP: &str = "/usr/local/share/applications/nuvira-greeter.desktop";

fn unit_path() -> PathBuf {
    Path::new("/etc/systemd/system").join(UNIT)
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub(crate) struct Settings {
    pub server: String,
    pub server_key: Option<String>,
    /// Who installed unattended access: their user service is stopped while the
    /// sign-in screen is shown (their systemd can outlive the session, kept by an
    /// SSH login for example, and the two agents would take the ID from each other).
    #[serde(default)]
    pub user: Option<String>,
    /// Installed for the whole computer by an administrator (`nuvira service deploy`).
    #[serde(default)]
    pub managed: bool,
    /// Profile of a customised installer of the Pro server (managed installations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

fn settings_path() -> PathBuf {
    Path::new(SYSTEM_DIR).join("signin.json")
}

pub(crate) fn load_settings() -> Result<Settings> {
    Ok(serde_json::from_str(&std::fs::read_to_string(settings_path()).context("signin.json")?)?)
}

pub(crate) fn save_settings(settings: &Settings) -> Result<()> {
    crate::unix_shared::write_private(&settings_path(), &serde_json::to_string(settings)?)
}

pub fn installed() -> bool {
    unit_path().exists()
}

pub(crate) fn ensure_root() -> Result<()> {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!("this is done by root (sudo)");
    }
    Ok(())
}

/// A program only root can change: the packages' one, or a copy made now.
pub(crate) fn program_for_root(exe: &Path) -> Result<PathBuf> {
    let owned_by_root = |path: &Path| {
        use std::os::unix::fs::MetadataExt;
        let mut all = true;
        let mut current = Some(path);
        while let Some(p) = current {
            match std::fs::metadata(p) {
                Ok(m) if m.uid() == 0 && m.mode() & 0o022 == 0 => {}
                _ => all = false,
            }
            current = p.parent();
        }
        all
    };
    if owned_by_root(exe) {
        return Ok(exe.to_path_buf());
    }
    let copy = Path::new(PROGRAM_COPY);
    std::fs::create_dir_all(copy.parent().context("program folder")?)?;
    let partial = copy.with_extension("new");
    std::fs::copy(exe, &partial).with_context(|| exe.display().to_string())?;
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&partial, copy)?;
    label_as_program(copy);
    Ok(copy.to_path_buf())
}

/// SELinux (Fedora): a file under /usr/local/lib is a library (`lib_t`), and
/// systemd would run it as `init_t`, which may not talk to the greeter's D-Bus
/// (the screen at GDM). As a program (`bin_t`) it runs as
/// `unconfined_service_t`, as the packages' /usr/bin/nuvira does.
fn label_as_program(path: &Path) {
    if !Path::new("/sys/fs/selinux/enforce").exists() {
        return;
    }
    // Also kept by a relabelling of the whole system, when semanage is there.
    let rule = Command::new("semanage")
        .args(["fcontext", "-a", "-t", "bin_t"])
        .arg(path)
        .output()
        .is_ok_and(|o| o.status.success() || String::from_utf8_lossy(&o.stderr).contains("already defined"));
    let labelled = if rule {
        Command::new("restorecon").arg(path).status().is_ok_and(|s| s.success())
    } else {
        Command::new("chcon").args(["-t", "bin_t"]).arg(path).status().is_ok_and(|s| s.success())
    };
    if !labelled {
        warn!(path = %path.display(), "SELinux label bin_t not set: the screen at GDM may be refused");
    }
}

pub(crate) fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl").args(args).status().context("systemctl")?;
    if !status.success() {
        bail!("systemctl {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// As root: copies the person's ID and password from `user_data` (their data
/// folder), installs the service and starts it.
pub fn install(exe: &Path, user_data: &Path, server: &str, server_key: Option<&str>) -> Result<()> {
    ensure_root()?;
    let dir = Path::new(SYSTEM_DIR);
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    crate::system::take_key(user_data, &dir.join("device.key"))?;
    for name in COPIED.into_iter().filter(|n| *n != "device.key") {
        let from = user_data.join(name);
        let to = dir.join(name);
        match std::fs::copy(&from, &to) {
            Ok(_) => {
                std::os::unix::fs::chown(&to, Some(0), Some(0))?;
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?;
            }
            Err(e) if name == "servers.txt" && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| from.display().to_string()),
        }
    }
    let user = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(user_data).ok().and_then(|m| user_name(m.uid()))
    };
    let settings =
        Settings { server: server.to_string(), server_key: server_key.map(str::to_string), user, ..Default::default() };
    install_service(&program_for_root(exe)?, &settings)
}

/// As root: saves the settings, installs the service with `program` (one only root
/// can change) and starts it again.
pub(crate) fn install_service(program: &Path, settings: &Settings) -> Result<()> {
    ensure_root()?;
    save_settings(settings)?;
    let unit = format!(
        "[Unit]\n\
         Description=NuviraHub at the sign-in screen (unattended access)\n\
         After=display-manager.service systemd-logind.service\n\
         \n\
         [Service]\n\
         ExecStart=\"{}\" service signin-run\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=graphical.target\n",
        program.display()
    );
    std::fs::write(unit_path(), unit)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT])?;
    systemctl(&["restart", UNIT])?;
    info!(unit = %unit_path().display(), program = %program.display(), "sign-in screen agent installed");
    Ok(())
}

/// As root: removes the service, root's copy of the data and of the program.
pub fn uninstall() -> Result<()> {
    remove(false)
}

/// As root: removes the service and root's copy of the program; with
/// `keep_identity` the data folder stays, with the computer's ID, password and
/// server (a managed installation removed and installed again keeps them, as
/// Windows keeps `%ProgramData%\nuvira`).
pub(crate) fn remove(keep_identity: bool) -> Result<()> {
    ensure_root()?;
    if unit_path().exists() {
        let _ = systemctl(&["disable", "--now", UNIT]);
        std::fs::remove_file(unit_path()).context("removing the service")?;
        let _ = systemctl(&["daemon-reload"]);
    }
    let system_dir = (!keep_identity).then_some(Path::new(SYSTEM_DIR));
    let program_dir = Path::new(PROGRAM_COPY).parent().unwrap_or(Path::new(PROGRAM_COPY));
    for path in system_dir.into_iter().chain([program_dir]) {
        match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| path.display().to_string());
            }
            _ => {}
        }
    }
    if Path::new("/sys/fs/selinux/enforce").exists() {
        let _ = Command::new("semanage").args(["fcontext", "-d", PROGRAM_COPY]).output();
    }
    let _ = std::fs::remove_file(KWIN_DESKTOP);
    info!("sign-in screen agent removed");
    Ok(())
}

/// What logind says of a session (`loginctl show-session`).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct SessionInfo {
    pub id: String,
    pub class: String,
    pub kind: String,
    pub display: String,
    pub uid: u32,
}

fn key_values(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter_map(|l| l.split_once('='))
}

fn loginctl(args: &[&str]) -> Option<String> {
    let out = Command::new("loginctl").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The active session of `seat0`, if any.
fn active_session() -> Option<SessionInfo> {
    let seat = loginctl(&["show-seat", "seat0", "-p", "ActiveSession"])?;
    let id = key_values(&seat).find(|(k, _)| *k == "ActiveSession")?.1.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let text = loginctl(&["show-session", &id, "-p", "Class", "-p", "Type", "-p", "Display", "-p", "User"])?;
    Some(parse_session(id, &text))
}

fn parse_session(id: String, text: &str) -> SessionInfo {
    let mut info = SessionInfo { id, ..Default::default() };
    for (k, v) in key_values(text) {
        match k {
            "Class" => info.class = v.trim().into(),
            "Type" => info.kind = v.trim().into(),
            "Display" => info.display = v.trim().into(),
            "User" => info.uid = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    info
}

/// The `-auth` file of the X server of `display` (":0"), from its command line.
fn x_authority(display: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        if let Some(found) = x_auth_of(&args, display) {
            return Some(found);
        }
    }
    None
}

fn x_auth_of(args: &[&[u8]], display: &str) -> Option<PathBuf> {
    let name = Path::new(std::str::from_utf8(args.first()?).ok()?).file_name()?.to_str()?;
    if !matches!(name, "Xorg" | "X" | "Xorg.bin") {
        return None;
    }
    if !args.iter().any(|a| *a == display.as_bytes()) {
        return None;
    }
    let i = args.iter().position(|a| *a == b"-auth")?;
    Some(PathBuf::from(std::str::from_utf8(args.get(i + 1)?).ok()?))
}

pub(crate) fn password_hash() -> Result<String> {
    let text = std::fs::read_to_string(Path::new(SYSTEM_DIR).join("service.env")).context("service.env")?;
    text.lines()
        .find_map(|l| l.strip_prefix("NUVIRA_PASSWORD_HASH="))
        .map(|h| h.trim().to_string())
        .context("no password in service.env")
}

/// A data folder the greeter's user can read (the Wayland agent runs as that
/// user): a copy in `/run`, which is not kept across restarts.
fn greeter_data(uid: u32, gid: u32) -> Result<PathBuf> {
    let dir = PathBuf::from(format!("/run/nuvira-greeter-{uid}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    for name in COPIED {
        let from = Path::new(SYSTEM_DIR).join(name);
        if from.exists() {
            let to = dir.join(name);
            if name == "device.key" {
                nuvira_transport::identity::hand_over(&from, &to)?;
            } else {
                std::fs::copy(&from, &to)?;
            }
            std::os::unix::fs::chown(&to, Some(uid), Some(gid))?;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    std::os::unix::fs::chown(&dir, Some(uid), Some(gid))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub(crate) fn user_name(uid: u32) -> Option<String> {
    // SAFETY: as in `primary_gid`.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_name.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned())
    }
}

/// Stops the user service of `user`, if their systemd is running.
pub(crate) fn stop_user_service(user: &str) {
    let Some(uid) = uid_of(user) else { return };
    if as_user(uid, "systemctl", &["--user", "stop", "nuvira-agent.service"]).is_ok() {
        info!(user, "the user's agent stopped while the sign-in screen is shown");
    }
}

fn uid_of(user: &str) -> Option<u32> {
    let name = std::ffi::CString::new(user).ok()?;
    // SAFETY: as in `primary_gid`.
    unsafe {
        let pw = libc::getpwnam(name.as_ptr());
        if pw.is_null() { None } else { Some((*pw).pw_uid) }
    }
}

/// Runs `program` as `uid`, talking to that person's systemd and session bus (in
/// their runtime folder): `systemctl --user --machine=<user>@.host` needs
/// systemd-machined's bridge, which Fedora 44 refused ("Transport endpoint is
/// not connected"). Fails when their systemd is not running.
pub(crate) fn as_user(uid: u32, program: &str, args: &[&str]) -> Result<()> {
    let runtime = format!("/run/user/{uid}");
    if !Path::new(&runtime).join("systemd/private").exists() {
        bail!("the person's systemd is not running");
    }
    let home = home_of(uid).unwrap_or_else(|| "/".into());
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={runtime}/bus"))
        .uid(uid)
        .gid(primary_gid(uid))
        .output()
        .with_context(|| program.to_string())?;
    if !output.status.success() {
        bail!("{program} {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn home_of(uid: u32) -> Option<String> {
    // SAFETY: as in `primary_gid`.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned())
    }
}

pub(crate) fn primary_gid(uid: u32) -> u32 {
    // SAFETY: getpwuid returns a pointer into static storage or NULL; read at once.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() { uid } else { (*pw).pw_gid }
    }
}

/// The greeter's compositor is KWin (SDDM on Wayland): the pid of a `kwin_wayland`
/// of its user.
fn kwin_greeter(uid: u32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir("/proc").into_iter().flatten().flatten().find_map(|entry| {
        let dir = entry.path();
        let pid: u32 = dir.file_name()?.to_str()?.parse().ok()?;
        (std::fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.trim() == "kwin_wayland")
            && std::fs::metadata(&dir).is_ok_and(|m| m.uid() == uid))
        .then_some(pid)
    })
}

/// The greeter's compositor is up: on Wayland its socket exists, and a
/// `kwin_wayland_wrapper` (which makes the socket before starting KWin) has
/// started `kwin_wayland`. SDDM starts its compositor together with the greeter
/// session; the agent started before it went through the plain program, which
/// KWin does not authorise (seen on Kubuntu 26.04 after restarting SDDM).
fn greeter_compositor_ready(session: &SessionInfo) -> bool {
    use std::os::unix::fs::MetadataExt;
    if session.kind != "wayland" {
        return true;
    }
    if !Path::new(&format!("/run/user/{}/wayland-0", session.uid)).exists() {
        return false;
    }
    // `comm` is cut at 15 characters.
    let wrapper = std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|entry| {
        let dir = entry.path();
        std::fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.trim() == "kwin_wayland_wr")
            && std::fs::metadata(&dir).is_ok_and(|m| m.uid() == session.uid)
    });
    !wrapper || kwin_greeter(session.uid).is_some()
}

/// KWin's authorisation for the greeter's agent: the program under its own path
/// (a hard link to root's program, a copy across file systems, renewed when the
/// program changes) and the `.desktop` file that names it.
fn prepare_kwin_program(exe: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let program = Path::new(KWIN_PROGRAM);
    let source = std::fs::metadata(exe).with_context(|| exe.display().to_string())?;
    let current = std::fs::metadata(program).ok();
    let same = current.as_ref().is_some_and(|m| m.dev() == source.dev() && m.ino() == source.ino())
        || current.as_ref().is_some_and(|m| m.len() == source.len() && m.mtime() == source.mtime());
    if !same {
        std::fs::create_dir_all(program.parent().context("program folder")?)?;
        let partial = program.with_extension("new");
        let _ = std::fs::remove_file(&partial);
        if std::fs::hard_link(exe, &partial).is_err() {
            std::fs::copy(exe, &partial).with_context(|| exe.display().to_string())?;
            std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&partial, program)?;
        label_as_program(program);
    }
    // `NoDisplay` keeps it out of the menus; KWin still takes it. Not `OnlyShowIn`
    // or `NotShowIn=KDE`: KWin then ignores the entry (tried on Plasma 6.6).
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=NuviraHub at the sign-in screen\nExec={KWIN_PROGRAM}\n\
         NoDisplay=true\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input\n"
    );
    if std::fs::read_to_string(KWIN_DESKTOP).ok().as_deref() != Some(desktop.as_str()) {
        std::fs::create_dir_all(Path::new(KWIN_DESKTOP).parent().context("applications folder")?)?;
        std::fs::write(KWIN_DESKTOP, desktop).context(KWIN_DESKTOP)?;
    }
    Ok(program.to_path_buf())
}

/// KWin finds the `.desktop` file in the KDE service cache of its user, which at
/// the greeter has no applications: `kbuildsycoca6` looks for the menu of the
/// applications, which on Plasma systems needs `XDG_MENU_PREFIX=plasma-` (set by a
/// Plasma session, not by SDDM). The cache's file name depends on the language,
/// so it is built with KWin's own environment (`LANG`, the XDG folders). Built
/// again at every start of the greeter's agent: a few seconds, and an update of
/// packages may have rebuilt it without our entry. KWin reads the new cache a few
/// seconds later: the agent's KWin session tries again meanwhile (`nuvira_portal`).
fn rebuild_kde_cache(uid: u32, gid: u32, kwin: u32) {
    const TAKEN: [&str; 12] = [
        "LANG",
        "LANGUAGE",
        "LC_ALL",
        "LC_MESSAGES",
        "HOME",
        "XDG_RUNTIME_DIR",
        "XDG_DATA_DIRS",
        "XDG_CONFIG_DIRS",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
    ];
    let environ = std::fs::read(format!("/proc/{kwin}/environ")).unwrap_or_default();
    let mut command = Command::new("kbuildsycoca6");
    command
        .arg("--noincremental")
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", home_of(uid).unwrap_or_else(|| "/run".into()))
        .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .uid(uid)
        .gid(gid);
    for pair in environ.split(|b| *b == 0) {
        let pair = String::from_utf8_lossy(pair);
        if let Some((key, value)) = pair.split_once('=')
            && TAKEN.contains(&key)
        {
            command.env(key, value);
        }
    }
    if Path::new("/etc/xdg/menus/plasma-applications.menu").exists() {
        command.env("XDG_MENU_PREFIX", "plasma-");
    }
    match command.status() {
        Ok(status) if status.success() => info!("KDE service cache of the sign-in screen rebuilt"),
        Ok(status) => warn!(%status, "kbuildsycoca6 failed: KWin may not authorise the sign-in screen's agent"),
        Err(e) => warn!("kbuildsycoca6: {e}: KWin may not authorise the sign-in screen's agent"),
    }
}

fn start_agent(exe: &Path, settings: &Settings, hash: &str, session: &SessionInfo) -> Result<Child> {
    // SDDM on Wayland: the agent runs as the authorised program (see KWIN_PROGRAM).
    let kwin_program = match (session.kind == "wayland").then(|| kwin_greeter(session.uid)).flatten() {
        Some(kwin) => match prepare_kwin_program(exe) {
            Ok(program) => {
                rebuild_kde_cache(session.uid, primary_gid(session.uid), kwin);
                Some(program)
            }
            Err(e) => {
                warn!("KWin's sign-in screen not prepared: {e:#}");
                None
            }
        },
        None => None,
    };
    let mut command = Command::new(kwin_program.as_deref().unwrap_or(exe));
    command.args(["agent", "--signin-screen", "--server", &settings.server]);
    if let Some(key) = &settings.server_key {
        command.args(["--server-key", key]);
    }
    command.env_clear().env("PATH", "/usr/local/bin:/usr/bin:/bin").env("NUVIRA_PASSWORD_HASH", hash);
    if session.kind == "wayland" {
        let (uid, gid) = (session.uid, primary_gid(session.uid));
        let runtime = format!("/run/user/{uid}");
        command
            .env("NUVIRA_DATA_DIR", greeter_data(uid, gid)?)
            .env("HOME", "/run")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("WAYLAND_DISPLAY", "wayland-0")
            .env("XDG_SESSION_TYPE", "wayland")
            .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={runtime}/bus"))
            .uid(uid)
            .gid(gid);
    } else {
        let display = if session.display.is_empty() { ":0".to_string() } else { session.display.clone() };
        let authority = x_authority(&display).context("X server of the sign-in screen not found")?;
        command
            .env("NUVIRA_DATA_DIR", SYSTEM_DIR)
            .env("HOME", "/root")
            .env("DISPLAY", &display)
            .env("XAUTHORITY", authority);
    }
    command.spawn().context("starting the agent")
}

fn stop(agent: &mut Option<(String, Child)>) {
    if let Some((_, mut child)) = agent.take() {
        // SAFETY: a child of ours, still to be waited for.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// `nuvira service signin-run`, the system service: an agent at the greeter while
/// it is the active session of `seat0`, none otherwise.
pub fn run() -> Result<()> {
    ensure_root().context("signin-run is started by systemd as root")?;
    let _ = tracing_subscriber::fmt().with_ansi(false).try_init();
    let exe = std::env::current_exe().context("program path")?;
    let mut settings = load_settings()?;
    // Managed: the organisation's policies, checked again every minute below.
    let policy = crate::policy::read();
    if settings.managed {
        crate::managed::apply_policy(&mut settings, &policy)?;
        crate::managed::spawn_profile(&settings);
    }
    let hash = password_hash()?;
    let mut handover = settings.managed.then(crate::managed::Handover::new);
    let mut next_policy_check = Instant::now() + Duration::from_secs(60);
    // Privacy mode on Wayland: the person's agent asks root to hold their local input.
    std::thread::Builder::new().name("input-block".into()).spawn(|| {
        let allowed = |uid: u32| active_session().is_some_and(|s| s.uid == uid && s.class == "user");
        if let Err(e) = nuvira_input::local_input::serve(allowed) {
            warn!("local input blocking not available: {e:#}");
        }
    })?;
    // The greeter session the agent was started for.
    let mut agent: Option<(String, Child)> = None;
    let mut next_start = Instant::now();
    loop {
        let active = active_session();
        if let Some(handover) = &mut handover {
            handover.follow(active.as_ref(), &settings, &hash);
        }
        if settings.managed && Instant::now() >= next_policy_check {
            next_policy_check = Instant::now() + Duration::from_secs(60);
            if crate::policy::read() != policy {
                // Started again by systemd within 5 s, with the new settings.
                info!("the organisation's policies changed: starting again with them");
                stop(&mut agent);
                return Ok(());
            }
        }
        let greeter = active.filter(|s| s.class == "greeter");
        match (&greeter, &mut agent) {
            (None, Some(_)) => {
                info!("the sign-in screen is gone: its agent ends");
                stop(&mut agent);
            }
            (Some(session), Some((id, _))) if *id != session.id => {
                info!(session = session.id, "another sign-in screen: its agent starts again");
                stop(&mut agent);
            }
            (Some(_), Some((_, child))) => {
                if let Ok(Some(status)) = child.try_wait() {
                    warn!(%status, "the sign-in screen's agent ended: started again in 5 s");
                    agent = None;
                    next_start = Instant::now() + Duration::from_secs(5);
                }
            }
            _ => {}
        }
        if let Some(session) = &greeter
            && agent.is_none()
            && Instant::now() >= next_start
            && greeter_compositor_ready(session)
        {
            if let Some(user) = &settings.user {
                stop_user_service(user);
            }
            match start_agent(&exe, &settings, &hash, session) {
                Ok(child) => {
                    info!(pid = child.id(), session = session.id, kind = session.kind, "sign-in screen's agent started");
                    agent = Some((session.id.clone(), child));
                }
                Err(e) => {
                    warn!("{e:#}");
                    next_start = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_from_loginctl() {
        let s = parse_session("c2".into(), "Class=greeter\nType=x11\nDisplay=:0\nUser=117\n");
        assert_eq!(s, SessionInfo { id: "c2".into(), class: "greeter".into(), kind: "x11".into(), display: ":0".into(), uid: 117 });
    }

    #[test]
    fn authority_from_the_x_command_line() {
        let args: Vec<&[u8]> = vec![b"/usr/lib/xorg/Xorg", b"-core", b":0", b"-seat", b"seat0", b"-auth", b"/var/run/lightdm/root/:0"];
        assert_eq!(x_auth_of(&args, ":0"), Some(PathBuf::from("/var/run/lightdm/root/:0")));
        assert_eq!(x_auth_of(&args, ":1"), None);
        let other: Vec<&[u8]> = vec![b"/usr/bin/bash", b":0", b"-auth", b"x"];
        assert_eq!(x_auth_of(&other, ":0"), None);
    }
}
