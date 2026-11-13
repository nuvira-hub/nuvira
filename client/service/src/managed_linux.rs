//! Linux: unattended access installed for the **whole computer** by an
//! administrator, for organisations that install software on many computers at
//! once (Ansible, Puppet, Salt, Landscape, a script over SSH):
//!
//! ```text
//! sudo nuvira service deploy [--server HOST] [--server-key FP] [--password-hash HASH | --password-file FILE] [--profile CODE]
//! sudo nuvira service deploy --remove
//! ```
//!
//! Unlike the person's own installation (module `user`) nobody has to sign in
//! first, and the ID belongs to the computer, not to a person:
//!
//! - the device key and the permanent password's hash are made by root in
//!   `/var/lib/nuvira` (root only), where the sign-in screen's agent reads them;
//! - the system service (`nuvira-signin.service`, module `signin`) follows who is
//!   at the computer (`seat0`'s active session): to a person's graphical session
//!   it hands a copy of the key, the server and the hash in
//!   `/run/nuvira-session-<uid>` (that person's only, gone at the next restart),
//!   and starts the agent in their systemd (`nuvira-agent.service`, from
//!   `/etc/systemd/user`); when another session becomes active it stops that agent
//!   and removes the copy, so only one agent at a time has the ID;
//! - every person's agent runs as that person, as with their own installation:
//!   it sees their screen, their files, and opens their terminal;
//! - the organisation's policies (`/etc/nuvira/policy.json`, module `policy`) win
//!   over the settings, also later: the service checks them every minute;
//! - `/etc/nuvira/managed.json` (readable by everyone) tells the app the
//!   installation is the organisation's: server fixed, no Install or Remove.
//!
//! What a person at the computer can still read is their own copy of the key and
//! of the hash while they are signed in (their agent runs as them), as with their
//! own installation.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::policy::Policy;
use crate::signin::{self, SYSTEM_DIR, SessionInfo, Settings};
use crate::unix_shared::write_private;

/// Present while NuviraHub is installed for the whole computer; what the app reads.
pub const STATE_FILE: &str = "/etc/nuvira/managed.json";
const USER_UNIT: &str = "/etc/systemd/user/nuvira-agent.service";
const AUTOSTART: &str = "/etc/xdg/autostart/nuvira-agent.desktop";
const UNIT: &str = "nuvira-agent.service";
const SESSION_PREFIX: &str = "nuvira-session-";
const HASH_VAR: &str = "NUVIRA_PASSWORD_HASH=";
/// `session-agent` without a copy (not the active session): systemd does not
/// start it again, the system service does when the session becomes active.
const NOT_HANDED_OVER: i32 = 75;

pub fn installed() -> bool {
    Path::new(STATE_FILE).exists()
}

/// What the app may know of the installation (no password).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct State {
    pub server: String,
    #[serde(default)]
    pub server_key: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

pub fn state() -> Option<State> {
    serde_json::from_str(&std::fs::read_to_string(STATE_FILE).ok()?).ok()
}

/// What the agent of a person's session gets (`agent.json` in their copy).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct SessionAgent {
    server: String,
    server_key: Option<String>,
    password_hash: String,
}

#[derive(Debug, Default)]
pub struct DeployOptions {
    pub server: Option<String>,
    pub server_key: Option<String>,
    /// In clear text (from `--password-file`): only its hash is kept.
    pub password: Option<String>,
    pub password_hash: Option<String>,
    pub profile: Option<String>,
}

fn service_env() -> PathBuf {
    Path::new(SYSTEM_DIR).join("service.env")
}

fn write_hash(hash: &str) -> Result<()> {
    write_private(&service_env(), &format!("{HASH_VAR}{hash}\n"))
}

/// A file readable by everyone, replaced in one step.
fn write_public(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("new");
    std::fs::write(&partial, text)?;
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&partial, path).with_context(|| path.display().to_string())
}

/// `nuvira service deploy` (as root): installs or updates. What is not given is
/// kept from the installation in place (also the person's own one, whose ID the
/// computer takes over), or made the first time: the ID, and a permanent password,
/// returned to show it once. The policies win over everything.
pub fn deploy(opts: DeployOptions) -> Result<Option<String>> {
    signin::ensure_root()?;
    let given = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    // Also after `deploy --remove`, which keeps the data folder.
    let previous = signin::load_settings().ok();
    let dir = Path::new(SYSTEM_DIR);
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let key = dir.join("device.key");
    nuvira_transport::identity::Identity::load_or_create(&key).context("the computer's device key")?;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;

    // The organisation's policies win: with their password no other one is made
    // (a password printed by the package's script would not work).
    let policy = crate::policy::read();
    let mut generated = None;
    let hash = match (given(opts.password_hash), given(opts.password)) {
        (Some(hash), _) if nuvira_secret::is_hash(&hash) => Some(hash),
        (Some(_), _) => anyhow::bail!("--password-hash is not a hash made by `nuvira service password-hash`"),
        (None, Some(password)) if password.chars().count() < 8 => {
            anyhow::bail!("the password must have at least 8 characters")
        }
        (None, Some(password)) => Some(nuvira_secret::hash(&password)),
        (None, None) => None,
    };
    match hash {
        Some(hash) => write_hash(&hash)?,
        None if signin::password_hash().is_ok() || policy.password_hash.is_some() => {}
        None => {
            let password = crate::random_password();
            write_hash(&nuvira_secret::hash(&password))?;
            generated = Some(password);
        }
    }

    let previous_server = previous.as_ref().map(|p| p.server.clone());
    let server = given(opts.server).or(previous_server.clone()).unwrap_or_default();
    let server_key = given(opts.server_key).or_else(|| {
        // The remembered fingerprint belongs to the server it was seen on.
        previous.as_ref().filter(|p| p.server.eq_ignore_ascii_case(&server)).and_then(|p| p.server_key.clone())
    });
    let mut settings = Settings {
        server,
        server_key,
        user: None,
        managed: true,
        profile: given(opts.profile).or(previous.as_ref().and_then(|p| p.profile.clone())),
    };
    apply_policy(&mut settings, &policy)?;
    // Whoever installed unattended access for themselves: their agent has the same
    // ID and would take it from the computer's (from their next sign-in their own
    // installation gives way, see `user::start`).
    if let Some(user) = previous.as_ref().and_then(|p| p.user.as_deref()) {
        signin::stop_user_service(user);
    }

    let exe = std::env::current_exe().context("program path")?;
    let program = signin::program_for_root(&exe)?;
    write_public(
        Path::new(USER_UNIT),
        &format!(
            "[Unit]\n\
             Description=NuviraHub agent (unattended access, installed for the whole computer)\n\
             \n\
             [Service]\n\
             ExecStart=\"{}\" service session-agent\n\
             Restart=always\n\
             RestartSec=5\n\
             RestartPreventExitStatus={NOT_HANDED_OVER}\n",
            program.display()
        ),
    )?;
    // At sign-in, from the desktop session: hands its variables (display, D-Bus) to
    // systemd for the agent, as the person's own installation does.
    write_public(
        Path::new(AUTOSTART),
        &format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=NuviraHub unattended access\n\
             Exec=\"{}\" service start\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n",
            program.display()
        ),
    )?;
    if crate::cups::available() && !crate::cups::installed() {
        match crate::cups::install() {
            Ok(done) => info!("{done}"),
            Err(e) => warn!("NuviraHub printer not installed: {e:#}"),
        }
    }
    if let Err(e) = crate::gnome::install_system() {
        warn!("GNOME extension for the lock and sign-in screens not installed: {e:#}");
    }
    // Last: it starts again with everything in place, and hands the ID to whoever
    // is signed in.
    signin::install_service(&program, &settings)?;
    info!(server = settings.server, program = %program.display(), "installed for the whole computer");
    Ok(generated)
}

/// `nuvira service deploy --remove` (as root): the computer keeps its ID,
/// permanent password and server in `/var/lib/nuvira` for a later installation
/// (delete the folder to forget them); the policy file is the organisation's and
/// stays.
pub fn remove() -> Result<()> {
    signin::ensure_root()?;
    let mut errors = Vec::new();
    // First the service, which would hand the ID over again.
    if let Err(e) = signin::remove(true) {
        errors.push(format!("system service: {e:#}"));
    }
    for uid in leftovers() {
        release(uid);
    }
    for path in [Path::new(USER_UNIT), Path::new(AUTOSTART), Path::new(STATE_FILE), &crate::branding::installed_path()] {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => errors.push(format!("{}: {e}", path.display())),
            _ => {}
        }
    }
    // Only if empty: the policy file is the organisation's.
    let _ = std::fs::remove_dir("/etc/nuvira");
    if let Err(e) = crate::gnome::uninstall_system() {
        errors.push(format!("GNOME extension: {e:#}"));
    }
    if !crate::cups::packaged()
        && let Err(e) = crate::cups::uninstall()
    {
        errors.push(format!("printer: {e:#}"));
    }
    if !errors.is_empty() {
        anyhow::bail!("{}", errors.join("; "));
    }
    info!("installation for the whole computer removed");
    Ok(())
}

/// The policies over the settings, saved; `managed.json` brought up to date.
pub(crate) fn apply_policy(settings: &mut Settings, policy: &Policy) -> Result<()> {
    let before = settings.clone();
    if let Some(server) = &policy.server
        && !server.eq_ignore_ascii_case(&settings.server)
    {
        settings.server = server.clone();
        // The remembered fingerprint was the old server's.
        settings.server_key = None;
    }
    if policy.server_key.is_some() {
        settings.server_key = policy.server_key.clone();
    }
    if policy.profile.is_some() {
        settings.profile = policy.profile.clone();
    }
    if *settings != before {
        info!(server = settings.server, "settings from the organisation's policies applied");
        signin::save_settings(settings)?;
    }
    if let Some(hash) = &policy.password_hash
        && signin::password_hash().ok().as_ref() != Some(hash)
    {
        info!("permanent password from the organisation's policies applied");
        write_hash(hash)?;
    }
    let state = State {
        server: settings.server.clone(),
        server_key: settings.server_key.clone(),
        profile: settings.profile.clone(),
    };
    if self::state().as_ref() != Some(&state) {
        write_public(Path::new(STATE_FILE), &serde_json::to_string_pretty(&state)?)?;
    }
    Ok(())
}

/// The customisation of the Pro profile, kept in `/etc/nuvira/branding.json`.
pub(crate) fn spawn_profile(settings: &Settings) {
    static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    crate::profile::spawn(
        &STOP,
        crate::profile::Target {
            server: settings.server.clone(),
            server_key: settings.server_key.clone(),
            server_ws: None,
            transport: "auto".into(),
            profile: settings.profile.clone(),
            data_dir: SYSTEM_DIR.into(),
        },
    );
}

fn session_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/{SESSION_PREFIX}{uid}"))
}

/// The sessions that have a copy (`/run/nuvira-session-<uid>`).
fn leftovers() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/run") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix(SESSION_PREFIX)?.parse().ok())
        .collect()
}

/// The system service's side: the ID goes to the agent of the person at the
/// computer, one at a time.
pub(crate) struct Handover {
    /// Whose session has the copy.
    current: Option<u32>,
    /// Copies left by the previous run of the service (started again by an update
    /// of the package, or by changed policies), dealt with at the first look.
    leftovers: Vec<u32>,
    retry_at: Instant,
}

impl Handover {
    pub fn new() -> Self {
        Self { current: None, leftovers: leftovers(), retry_at: Instant::now() }
    }

    /// The active session of `seat0` (or none): a person's graphical session gets
    /// the ID, any other loses it.
    pub fn follow(&mut self, active: Option<&SessionInfo>, settings: &Settings, hash: &str) {
        let wanted = active
            .filter(|s| s.class == "user" && matches!(s.kind.as_str(), "x11" | "wayland"))
            .map(|s| s.uid);
        // After a restart of the service: the active session's copy, if still the
        // same, is kept with its agent (a session in progress goes on); the others go.
        for uid in std::mem::take(&mut self.leftovers) {
            if Some(uid) == wanted && self.current.is_none() && up_to_date(uid, settings, hash) {
                info!(uid, "the agent of the person signed in keeps the computer's ID");
                self.current = Some(uid);
            } else {
                release(uid);
            }
        }
        if wanted == self.current {
            return;
        }
        if let Some(old) = self.current.take() {
            info!(uid = old, "the person's session is no longer the active one: its agent ends");
            release(old);
        }
        let Some(uid) = wanted else { return };
        if Instant::now() < self.retry_at {
            return;
        }
        match hand_over(uid, settings, hash) {
            Ok(()) => {
                info!(uid, "the computer's ID handed to the agent of the person signed in");
                self.current = Some(uid);
            }
            Err(e) => {
                warn!(uid, "agent of the person signed in not started: {e:#}");
                release(uid);
                self.retry_at = Instant::now() + Duration::from_secs(10);
            }
        }
    }
}

fn session_agent_for(settings: &Settings, hash: &str) -> SessionAgent {
    SessionAgent { server: settings.server.clone(), server_key: settings.server_key.clone(), password_hash: hash.to_string() }
}

/// The copy of `uid` is the one this run would hand over.
fn up_to_date(uid: u32, settings: &Settings, hash: &str) -> bool {
    let dir = session_dir(uid);
    let agent = std::fs::read_to_string(dir.join("agent.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<SessionAgent>(&t).ok());
    let key = |path: &Path| nuvira_transport::identity::public_key_of(path);
    agent == Some(session_agent_for(settings, hash))
        // A copy written before 0.21 (plain) is handed again, as a handed-over one,
        // before the agent protects it for itself and root can no longer compare it.
        && nuvira_transport::keystore::protection_of(&dir.join("device.key"))
            == Some(nuvira_transport::keystore::Protection::Handed)
        && key(&dir.join("device.key")).is_some()
        && key(&dir.join("device.key")) == key(&Path::new(SYSTEM_DIR).join("device.key"))
}

fn hand_over(uid: u32, settings: &Settings, hash: &str) -> Result<()> {
    let gid = signin::primary_gid(uid);
    let dir = session_dir(uid);
    let _ = std::fs::remove_dir_all(&dir);
    // Under /run, which only root can write: nobody can put a link there first.
    std::fs::create_dir(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let key = dir.join("device.key");
    nuvira_transport::identity::hand_over(&Path::new(SYSTEM_DIR).join("device.key"), &key).context("device.key")?;
    let agent = session_agent_for(settings, hash);
    write_private(&dir.join("agent.json"), &serde_json::to_string(&agent)?)?;
    for path in [&key, &dir.join("agent.json"), &dir] {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    }
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
    // `service start` as the person, in a unit of their systemd (as the agent):
    // gives way to their own installation, if any, and (re)starts the agent. Their
    // systemd reads the unit from /etc/systemd/user.
    let exe = std::env::current_exe().context("program path")?.display().to_string();
    signin::as_user(uid, "systemctl", &["--user", "daemon-reload"])?;
    signin::as_user(uid, "systemd-run", &["--user", "--collect", "--quiet", "--wait", &exe, "service", "start"])
}

/// Stops the agent of `uid`'s session and removes their copy.
fn release(uid: u32) {
    let _ = signin::as_user(uid, "systemctl", &["--user", "stop", UNIT]);
    let _ = std::fs::remove_dir_all(session_dir(uid));
}

/// `nuvira service session-agent`: the unit `nuvira-agent.service` of a person's
/// systemd, with the copy handed over by the system service. Becomes the agent.
pub fn session_agent() -> Result<()> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = session_dir(uid);
    let Ok(text) = std::fs::read_to_string(dir.join("agent.json")) else {
        eprintln!("this session does not have the computer's ID (not the active session): the system service starts the agent when it is");
        std::process::exit(NOT_HANDED_OVER);
    };
    let agent: SessionAgent = serde_json::from_str(&text)?;
    if let Err(e) = crate::gnome::enable_user() {
        warn!("GNOME extension for the lock screen not enabled: {e:#}");
    }
    let exe = std::env::current_exe().context("program path")?;
    let mut command = Command::new(exe);
    command.args(["agent", "--ipc", "--server", &agent.server]);
    if let Some(key) = &agent.server_key {
        command.args(["--server-key", key]);
    }
    command.arg("--identity").arg(dir.join("device.key")).env("NUVIRA_PASSWORD_HASH", &agent.password_hash);
    Err(command.exec()).context("starting the agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings { server: "a.example.com".into(), server_key: Some("aa".into()), managed: true, ..Default::default() }
    }

    #[test]
    fn a_new_server_from_the_policy_forgets_the_fingerprint() {
        let mut s = settings();
        let before = s.clone();
        // Without root the saving fails: only what changed in memory is checked.
        let policy = Policy { server: Some("b.example.com".into()), ..Default::default() };
        let _ = apply_policy(&mut s, &policy);
        assert_eq!(s.server, "b.example.com");
        assert_eq!(s.server_key, None);
        let mut s = before.clone();
        let policy = Policy { server: Some("A.example.com".into()), ..Default::default() };
        let _ = apply_policy(&mut s, &policy);
        assert_eq!(s, before);
    }

    #[test]
    fn state_without_the_password() {
        let state = State { server: "a".into(), server_key: None, profile: Some("p".into()) };
        let text = serde_json::to_string(&state).unwrap();
        assert!(!text.contains("hash"));
        assert_eq!(serde_json::from_str::<State>(&text).unwrap(), state);
    }
}
