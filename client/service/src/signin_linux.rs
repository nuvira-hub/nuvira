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

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const UNIT: &str = "nuvira-signin.service";
const SYSTEM_DIR: &str = "/var/lib/nuvira";
/// Root's copy of the program, when the person's one is not root's.
const PROGRAM_COPY: &str = "/usr/local/lib/nuvira/nuvira";
/// Files of the person's data folder the greeter's agent needs.
const COPIED: [&str; 3] = ["device.key", "servers.txt", "service.env"];

fn unit_path() -> PathBuf {
    Path::new("/etc/systemd/system").join(UNIT)
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Settings {
    server: String,
    server_key: Option<String>,
}

fn settings_path() -> PathBuf {
    Path::new(SYSTEM_DIR).join("signin.json")
}

pub fn installed() -> bool {
    unit_path().exists()
}

fn ensure_root() -> Result<()> {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!("the sign-in screen's agent is installed by root");
    }
    Ok(())
}

/// A program only root can change: the packages' one, or a copy made now.
fn program_for_root(exe: &Path) -> Result<PathBuf> {
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
    Ok(copy.to_path_buf())
}

fn systemctl(args: &[&str]) -> Result<()> {
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
    for name in COPIED {
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
    let settings = Settings { server: server.to_string(), server_key: server_key.map(str::to_string) };
    std::fs::write(settings_path(), serde_json::to_string(&settings)?)?;
    std::fs::set_permissions(settings_path(), std::fs::Permissions::from_mode(0o600))?;
    let program = program_for_root(exe)?;
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
    ensure_root()?;
    if unit_path().exists() {
        let _ = systemctl(&["disable", "--now", UNIT]);
        std::fs::remove_file(unit_path()).context("removing the service")?;
        let _ = systemctl(&["daemon-reload"]);
    }
    for path in [Path::new(SYSTEM_DIR), Path::new(PROGRAM_COPY).parent().unwrap_or(Path::new(PROGRAM_COPY))] {
        match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| path.display().to_string());
            }
            _ => {}
        }
    }
    info!("sign-in screen agent removed");
    Ok(())
}

/// What logind says of a session (`loginctl show-session`).
#[derive(Debug, Default, Clone, PartialEq)]
struct SessionInfo {
    id: String,
    class: String,
    kind: String,
    display: String,
    uid: u32,
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

fn password_hash() -> Result<String> {
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
            std::fs::copy(&from, &to)?;
            std::os::unix::fs::chown(&to, Some(uid), Some(gid))?;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    std::os::unix::fs::chown(&dir, Some(uid), Some(gid))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn primary_gid(uid: u32) -> u32 {
    // SAFETY: getpwuid returns a pointer into static storage or NULL; read at once.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() { uid } else { (*pw).pw_gid }
    }
}

fn start_agent(exe: &Path, settings: &Settings, hash: &str, session: &SessionInfo) -> Result<Child> {
    let mut command = Command::new(exe);
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
    let settings: Settings =
        serde_json::from_str(&std::fs::read_to_string(settings_path()).context("signin.json")?)?;
    let hash = password_hash()?;
    // The greeter session the agent was started for.
    let mut agent: Option<(String, Child)> = None;
    let mut next_start = Instant::now();
    loop {
        let greeter = active_session().filter(|s| s.class == "greeter");
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
        {
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
