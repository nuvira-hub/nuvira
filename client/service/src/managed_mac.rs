//! macOS: unattended access installed for the **whole Mac** by an administrator
//! or an MDM (Jamf, Intune, Kandji, Mosyle): the `.pkg` package runs
//!
//! ```text
//! sudo /Applications/NuviraHub.app/Contents/MacOS/nuvira service deploy [--server HOST] [--server-key FP] [--password-hash HASH | --password-file FILE] [--profile CODE]
//! sudo /Applications/NuviraHub.app/Contents/MacOS/nuvira service deploy --remove
//! ```
//!
//! The counterpart of the Linux one (`managed_linux.rs`), with launchd:
//!
//! - the device key and the permanent password's hash are made by root in
//!   `/Library/Application Support/NuviraHub` (root only), where the sign-in
//!   screen's agent reads them: the ID is the Mac's;
//! - a LaunchDaemon, `io.github.nuvira-hub.nuvira.managed` (`nuvira service
//!   managed-run`, root), follows the person in front of the Mac (the console
//!   user, also with fast user switching): it hands them a copy of the key, the
//!   server and the hash in `/var/run/nuvira-session-<uid>` and starts the agent in
//!   their session; when another person comes to the front, it stops that agent
//!   and removes the copy, so only one agent at a time has the ID;
//! - the agent is a LaunchAgent of the system (`/Library/LaunchAgents`, label
//!   `io.github.nuvira-hub.nuvira.managed-agent`), run by launchd as each person
//!   in their desktop session: the same signed program of the app's bundle, so the
//!   Screen Recording and Accessibility permissions (system-wide on macOS) hold;
//! - the organisation's settings come from a configuration profile (managed
//!   preferences of `io.github.nuvira-hub.nuvira`, module `policy`), checked every
//!   minute;
//! - `/Library/Application Support/io.github.nuvira-hub.nuvira/managed.json`
//!   (readable by everyone) tells the app: server fixed, no Install or Remove.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::policy::Policy;
use crate::signin::{self, SYSTEM_DIR, Settings};
use crate::unix_shared::write_private;

/// Readable by everyone: the installation's state and the Pro customisation.
pub const PUBLIC_DIR: &str = "/Library/Application Support/io.github.nuvira-hub.nuvira";
pub const STATE_FILE: &str = "/Library/Application Support/io.github.nuvira-hub.nuvira/managed.json";
pub const DAEMON_LABEL: &str = "io.github.nuvira-hub.nuvira.managed";
pub const AGENT_LABEL: &str = "io.github.nuvira-hub.nuvira.managed-agent";
const BUNDLE_ID: &str = "io.github.nuvira-hub.nuvira";
const LOG: &str = "/Library/Logs/NuviraHub/managed.log";
const SESSION_PREFIX: &str = "nuvira-session-";
const HASH_VAR: &str = "NUVIRA_PASSWORD_HASH=";

fn daemon_plist() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{DAEMON_LABEL}.plist"))
}

fn agent_plist() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchAgents/{AGENT_LABEL}.plist"))
}

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
    /// `NUVIRA_MAC_KEYBOARD` for the agent (the profile's `Keyboard`).
    #[serde(default)]
    keyboard: Option<String>,
}

#[derive(Debug, Default)]
pub struct DeployOptions {
    pub server: Option<String>,
    pub server_key: Option<String>,
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

fn write_public(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    }
    let partial = path.with_extension("new");
    std::fs::write(&partial, text)?;
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&partial, path).with_context(|| path.display().to_string())
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("/bin/launchctl").args(args).output().context("launchctl")
}

/// `bootstrap` right after a `bootout` can fail while launchd is still taking the
/// old job down: tried again for a few seconds.
fn bootstrap(domain: &str, plist: &Path) -> Result<()> {
    let plist = plist.display().to_string();
    let mut last = String::new();
    for _ in 0..10 {
        let out = launchctl(&["bootstrap", domain, &plist])?;
        if out.status.success() {
            return Ok(());
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("launchctl bootstrap {domain} {plist}: {last}")
}

/// The program must be the one inside the app's bundle in /Applications (the
/// permissions are the bundle's, and only an administrator can replace it there).
fn check_program(exe: &Path) -> Result<()> {
    let path = exe.to_string_lossy();
    if !path.starts_with("/Applications/") || path.contains("/AppTranslocation/") {
        bail!("run the program inside /Applications/NuviraHub.app (the .pkg puts it there)");
    }
    Ok(())
}

fn daemon_definition(exe: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{DAEMON_LABEL}</string>\n\
         \t<key>AssociatedBundleIdentifiers</key>\n\t<array>\n\t\t<string>{BUNDLE_ID}</string>\n\t</array>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t\t<string>service</string>\n\t\t<string>managed-run</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>5</integer>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{LOG}</string>\n\
         \t<key>StandardOutPath</key>\n\t<string>{LOG}</string>\n\
         </dict>\n</plist>\n",
        exe = xml(&exe.display().to_string()),
    )
}

/// The agent of every person's desktop session. Not started again after a clean
/// exit: `session-agent` ends at once in a session without the copy (not the one
/// in front), and the daemon starts it when it is.
fn agent_definition(exe: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{AGENT_LABEL}</string>\n\
         \t<key>AssociatedBundleIdentifiers</key>\n\t<array>\n\t\t<string>{BUNDLE_ID}</string>\n\t</array>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t\t<string>service</string>\n\t\t<string>session-agent</string>\n\t</array>\n\
         \t<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>\n\
         \t<key>ProcessType</key>\n\t<string>Interactive</string>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>5</integer>\n\
         </dict>\n</plist>\n",
        exe = xml(&exe.display().to_string()),
    )
}

/// `nuvira service deploy` (as root; the `.pkg` runs it): installs or updates.
/// What is not given is kept (also from the person's own installation, whose ID
/// the Mac takes over), or made the first time; the policies win over everything.
pub fn deploy(opts: DeployOptions) -> Result<Option<String>> {
    signin::ensure_root()?;
    let exe = std::env::current_exe().context("program path")?;
    check_program(&exe)?;
    let given = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let previous = signin::load_settings().ok();
    let dir = Path::new(SYSTEM_DIR);
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let key = dir.join("device.key");
    nuvira_transport::identity::Identity::load_or_create(&key).context("the Mac's device key")?;
    std::os::unix::fs::chown(&key, Some(0), Some(0))?;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;

    let mut generated = None;
    let hash = match (given(opts.password_hash), given(opts.password)) {
        (Some(hash), _) if nuvira_secret::is_hash(&hash) => Some(hash),
        (Some(_), _) => bail!("--password-hash is not a hash made by `nuvira service password-hash`"),
        (None, Some(password)) if password.chars().count() < 8 => bail!("the password must have at least 8 characters"),
        (None, Some(password)) => Some(nuvira_secret::hash(&password)),
        (None, None) => None,
    };
    match hash {
        Some(hash) => write_hash(&hash)?,
        None if signin::password_hash().is_ok() => {}
        None => {
            let password = crate::random_password();
            write_hash(&nuvira_secret::hash(&password))?;
            generated = Some(password);
        }
    }

    let server = given(opts.server).or(previous.as_ref().map(|p| p.server.clone())).unwrap_or_default();
    let server_key = given(opts.server_key).or_else(|| {
        previous.as_ref().filter(|p| p.server.eq_ignore_ascii_case(&server)).and_then(|p| p.server_key.clone())
    });
    let mut settings = Settings {
        server,
        server_key,
        managed: true,
        profile: given(opts.profile).or(previous.as_ref().and_then(|p| p.profile.clone())),
    };
    apply_policy(&mut settings, &crate::policy::read())?;

    if let Some(logs) = Path::new(LOG).parent() {
        std::fs::create_dir_all(logs)?;
    }
    signin::install_job(&exe, &settings)?;
    if crate::cups::available() && !crate::cups::installed() {
        match crate::cups::install() {
            Ok(done) => info!("{done}"),
            Err(e) => warn!("NuviraHub printer not installed: {e:#}"),
        }
    }
    if let Err(e) = crate::hid::install(&exe) {
        warn!("virtual keyboard for the lock screen not installed: {e:#}");
    }
    write_public(&agent_plist(), &agent_definition(&exe))?;
    write_public(&daemon_plist(), &daemon_definition(&exe))?;
    // The daemon last: it hands the ID to whoever is in front of the Mac.
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
    bootstrap("system", &daemon_plist())?;
    info!(server = settings.server, "installed for the whole Mac");
    Ok(generated)
}

/// `nuvira service deploy --remove` (as root): the Mac keeps its ID, password and
/// server in `/Library/Application Support/NuviraHub` for a later installation
/// (delete the folder to forget them).
pub fn remove() -> Result<()> {
    signin::ensure_root()?;
    let mut errors = Vec::new();
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
    for uid in leftovers() {
        release(uid);
    }
    // The agent's job in every session it was loaded in.
    for uid in gui_sessions() {
        let _ = launchctl(&["bootout", &format!("gui/{uid}/{AGENT_LABEL}")]);
    }
    if let Err(e) = signin::remove(true) {
        errors.push(format!("sign-in screen: {e:#}"));
    }
    for path in [daemon_plist(), agent_plist(), PathBuf::from(STATE_FILE), crate::branding::installed_path()] {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => errors.push(format!("{}: {e}", path.display())),
            _ => {}
        }
    }
    let _ = std::fs::remove_dir(PUBLIC_DIR);
    if let Err(e) = crate::hid::uninstall() {
        errors.push(format!("lock screen keyboard: {e:#}"));
    }
    if let Err(e) = crate::cups::uninstall() {
        errors.push(format!("printer: {e:#}"));
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    info!("installation for the whole Mac removed");
    Ok(())
}

/// The policies over the settings, saved; `managed.json` brought up to date.
pub(crate) fn apply_policy(settings: &mut Settings, policy: &Policy) -> Result<()> {
    let before = settings.clone();
    if let Some(server) = &policy.server
        && !server.eq_ignore_ascii_case(&settings.server)
    {
        settings.server = server.clone();
        settings.server_key = None;
    }
    if policy.server_key.is_some() {
        settings.server_key = policy.server_key.clone();
    }
    if policy.profile.is_some() {
        settings.profile = policy.profile.clone();
    }
    if *settings != before {
        info!(server = settings.server, "settings from the organisation's profile applied");
        signin::save_settings(settings)?;
    }
    if let Some(hash) = &policy.password_hash
        && signin::password_hash().ok().as_ref() != Some(hash)
    {
        info!("permanent password from the organisation's profile applied");
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

fn session_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/var/run/{SESSION_PREFIX}{uid}"))
}

fn leftovers() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/var/run") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix(SESSION_PREFIX)?.parse().ok())
        .collect()
}

/// The people with a desktop session (launchd's `gui/<uid>` domains).
fn gui_sessions() -> Vec<u32> {
    let Ok(out) = Command::new("/usr/bin/who").output() else { return Vec::new() };
    let mut uids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.split_whitespace().nth(1) == Some("console"))
        .filter_map(|l| uid_of(l.split_whitespace().next()?))
        .collect();
    uids.sort_unstable();
    uids.dedup();
    uids
}

fn uid_of(user: &str) -> Option<u32> {
    let name = std::ffi::CString::new(user).ok()?;
    // SAFETY: getpwnam returns a pointer into static storage or NULL; read at once.
    unsafe {
        let pw = libc::getpwnam(name.as_ptr());
        if pw.is_null() { None } else { Some((*pw).pw_uid) }
    }
}

fn primary_gid(uid: u32) -> u32 {
    // SAFETY: as in `uid_of`.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() { 20 } else { (*pw).pw_gid }
    }
}

fn session_agent_for(settings: &Settings, hash: &str, keyboard: Option<&str>) -> SessionAgent {
    SessionAgent {
        server: settings.server.clone(),
        server_key: settings.server_key.clone(),
        password_hash: hash.to_string(),
        keyboard: keyboard.map(str::to_string),
    }
}

fn up_to_date(uid: u32, settings: &Settings, hash: &str, keyboard: Option<&str>) -> bool {
    let dir = session_dir(uid);
    let agent = std::fs::read_to_string(dir.join("agent.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<SessionAgent>(&t).ok());
    let key = |path: &Path| nuvira_transport::identity::public_key_of(path);
    agent == Some(session_agent_for(settings, hash, keyboard))
        // A copy written before 0.21 (plain) is handed again, as a handed-over one,
        // before the agent protects it for itself and root can no longer compare it.
        && nuvira_transport::keystore::protection_of(&dir.join("device.key"))
            == Some(nuvira_transport::keystore::Protection::Handed)
        && key(&dir.join("device.key")).is_some()
        && key(&dir.join("device.key")) == key(&Path::new(SYSTEM_DIR).join("device.key"))
}

fn hand_over(uid: u32, settings: &Settings, hash: &str, keyboard: Option<&str>) -> Result<()> {
    let gid = primary_gid(uid);
    let dir = session_dir(uid);
    let _ = std::fs::remove_dir_all(&dir);
    // Under /var/run, which only root can write: nobody can put a link there first.
    std::fs::create_dir(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let key = dir.join("device.key");
    nuvira_transport::identity::hand_over(&Path::new(SYSTEM_DIR).join("device.key"), &key).context("device.key")?;
    write_private(&dir.join("agent.json"), &serde_json::to_string(&session_agent_for(settings, hash, keyboard))?)?;
    for path in [&key, &dir.join("agent.json"), &dir] {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    }
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
    // Loaded at the person's sign-in when the job was already there; otherwise
    // (installed while they were signed in) loaded now.
    let job = format!("gui/{uid}/{AGENT_LABEL}");
    if launchctl(&["print", &job])?.status.success() {
        let out = launchctl(&["kickstart", "-k", &job])?;
        if !out.status.success() {
            bail!("launchctl kickstart {job}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    } else {
        bootstrap(&format!("gui/{uid}"), &agent_plist())?;
    }
    Ok(())
}

/// Stops the agent of `uid`'s session and removes their copy (first the copy:
/// launchd starts the agent again after the signal, and without it it ends).
fn release(uid: u32) {
    let _ = std::fs::remove_dir_all(session_dir(uid));
    let _ = launchctl(&["kill", "SIGTERM", &format!("gui/{uid}/{AGENT_LABEL}")]);
}

/// `nuvira service managed-run`, the LaunchDaemon: the organisation's profile,
/// the Pro customisation and the ID for the person in front of the Mac.
pub fn run() -> Result<()> {
    signin::ensure_root().context("managed-run is started by launchd as root")?;
    let _ = tracing_subscriber::fmt().with_ansi(false).try_init();
    let mut settings = signin::load_settings()?;
    let policy = crate::policy::read();
    apply_policy(&mut settings, &policy)?;
    spawn_profile(&settings);
    spawn_updates(&settings);
    let hash = signin::password_hash()?;
    let keyboard = policy.keyboard.as_deref();
    let mut leftovers = leftovers();
    let mut current: Option<u32> = None;
    let mut retry_at = Instant::now();
    let mut next_policy_check = Instant::now() + Duration::from_secs(60);
    loop {
        let wanted = signin::console_uid();
        for uid in std::mem::take(&mut leftovers) {
            if Some(uid) == wanted && current.is_none() && up_to_date(uid, &settings, &hash, keyboard) {
                info!(uid, "the agent of the person in front keeps the Mac's ID");
                current = Some(uid);
            } else {
                release(uid);
            }
        }
        if wanted != current {
            if let Some(old) = current.take() {
                info!(uid = old, "the person is no longer in front of the Mac: their agent ends");
                release(old);
            }
            if let Some(uid) = wanted
                && Instant::now() >= retry_at
            {
                match hand_over(uid, &settings, &hash, keyboard) {
                    Ok(()) => {
                        info!(uid, "the Mac's ID handed to the agent of the person in front");
                        current = Some(uid);
                    }
                    Err(e) => {
                        warn!(uid, "agent of the person in front not started: {e:#}");
                        release(uid);
                        retry_at = Instant::now() + Duration::from_secs(10);
                    }
                }
            }
        }
        if Instant::now() >= next_policy_check {
            next_policy_check = Instant::now() + Duration::from_secs(60);
            if crate::policy::read() != policy {
                // Started again by launchd within 5 s, with the new settings.
                info!("the organisation's profile changed: starting again with it");
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Downloads of the root service's updates (root only).
const UPDATE_CACHE: &str = "/Library/Caches/NuviraHub/update";

/// The app installed by the `.pkg` is root's: the people's agents cannot replace
/// it, so this service does (nuvira_update::unix::spawn_root_bundle_updates), from
/// the installation's server, never during someone's session.
fn spawn_updates(settings: &Settings) {
    let server = nuvira_transport::signal::server_or_public(&settings.server).to_string();
    let base = nuvira_update::base_url(&server, None, nuvira_update::DEFAULT_PORT);
    // The agent of the person in front keeps `busy` in their data folder during a session.
    let busy = Box::new(|| {
        std::fs::read_dir("/Users").into_iter().flatten().flatten().any(|home| {
            home.path().join("Library/Application Support/nuvira/busy").exists()
        })
    });
    let restarted = Box::new(|| {
        // The virtual keyboard's daemon runs the same program: started again too.
        let _ = launchctl(&["kickstart", "-k", &format!("system/{}", crate::hid::LABEL)]);
        // launchd (KeepAlive) starts this service again with the new program, which
        // hands the agent back to the person in front; their agent restarts by itself.
        std::process::exit(nuvira_update::unix::RESTART_EXIT);
    });
    let cache = PathBuf::from(UPDATE_CACHE);
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    nuvira_update::unix::spawn_root_bundle_updates(base, busy, cache, restarted);
}

fn spawn_profile(settings: &Settings) {
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

/// `nuvira service session-agent`, the system's LaunchAgent in a person's desktop
/// session: becomes the agent with the copy the daemon handed over, or ends at once
/// (exit 0, not started again) without it.
pub fn session_agent() -> Result<()> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = session_dir(uid);
    let Ok(text) = std::fs::read_to_string(dir.join("agent.json")) else {
        eprintln!("this session does not have the Mac's ID (not the one in front): the daemon starts the agent when it is");
        return Ok(());
    };
    let agent: SessionAgent = serde_json::from_str(&text)?;
    // The person's own installation gives way (same ID, it would take it back).
    if crate::user::plist_path().is_ok_and(|p| p.exists()) {
        info!("unattended access is now installed for the whole Mac: the person's own installation gives way");
        let _ = Command::new("/bin/launchctl").args(["bootout", &format!("gui/{uid}/{}", crate::user::LABEL)]).output();
        if let Ok(plist) = crate::user::plist_path() {
            let _ = std::fs::remove_file(plist);
        }
        let _ = std::fs::remove_file(crate::unix_shared::env_path());
    }
    crate::hid::activate_quietly();
    let exe = std::env::current_exe().context("program path")?;
    let mut command = Command::new(exe);
    command.args(["agent", "--ipc", "--server", &agent.server]);
    if let Some(key) = &agent.server_key {
        command.args(["--server-key", key]);
    }
    command.arg("--identity").arg(dir.join("device.key")).env("NUVIRA_PASSWORD_HASH", &agent.password_hash);
    if let Some(keyboard) = &agent.keyboard {
        command.env("NUVIRA_MAC_KEYBOARD", keyboard);
    }
    // Into the person's log, as with their own installation.
    if let Some(home) = std::env::var_os("HOME") {
        let log = Path::new(&home).join("Library/Logs/NuviraHub/agent.log");
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&log)
            && let Ok(copy) = file.try_clone()
        {
            command.stdout(file).stderr(copy);
        }
    }
    Err(command.exec()).context("starting the agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_job_is_not_started_again_after_a_clean_exit() {
        let text = agent_definition(Path::new("/Applications/NuviraHub.app/Contents/MacOS/nuvira"));
        assert!(text.contains("<string>session-agent</string>"));
        assert!(text.contains("<string>Aqua</string>"));
        assert!(text.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(!text.contains("PASSWORD"));
        let daemon = daemon_definition(Path::new("/Applications/NuviraHub.app/Contents/MacOS/nuvira"));
        assert!(daemon.contains("<string>managed-run</string>"));
        assert!(!daemon.contains("LimitLoadToSessionType"));
    }

    #[test]
    fn program_outside_applications_refused() {
        assert!(check_program(Path::new("/Volumes/NuviraHub/NuviraHub.app/Contents/MacOS/nuvira")).is_err());
        assert!(check_program(Path::new("/Applications/NuviraHub.app/Contents/MacOS/nuvira")).is_ok());
    }
}
