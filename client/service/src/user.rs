//! Linux: unattended access through a systemd **user** service, `nuvira-agent`.
//! It runs as the person's own user, not as root:
//! - at login an autostart entry imports the variables of the desktop session
//!   (display, D-Bus, Wayland) into systemd and starts the service, so the
//!   agent can read the screen and inject input;
//! - systemd starts it again if it ends;
//! - the permanent password is in `service.env` (mode 0600), as a hash.
//!
//! What it cannot do: reach the computer before someone logs in. After a restart
//! the computer needs automatic login; on GNOME with Wayland the screen must not
//! lock either, since GNOME stops sharing it while locked.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::info;

const UNIT: &str = "nuvira-agent.service";

/// The same states as the Windows service, for the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    Running,
    Changing,
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME not set")
}

fn unit_path() -> Result<PathBuf> {
    Ok(home()?.join(".config/systemd/user").join(UNIT))
}

fn autostart_path() -> Result<PathBuf> {
    Ok(home()?.join(".config/autostart/nuvira-agent.desktop"))
}

fn env_path() -> PathBuf {
    nuvira_transport::identity::data_dir().join("service.env")
}

/// The variables of the desktop session the agent needs.
const SESSION_VARS: [&str; 7] = [
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl").arg("--user").args(args).status().context("systemctl")?;
    if !status.success() {
        bail!("systemctl --user {} failed ({status})", args.join(" "));
    }
    Ok(())
}

pub struct InstallOptions {
    pub server: String,
    pub server_key: Option<String>,
    /// `None`: the current one is kept, or one is generated the first time.
    pub password: Option<String>,
}

/// Installs or updates the user service. Returns the permanent password when it
/// was generated, to show it once.
pub fn install(opts: &InstallOptions) -> Result<Option<String>> {
    let exe = std::env::current_exe().context("program path")?;
    let store = |password: &str| {
        write_private(&env_path(), &format!("NUVIRA_PASSWORD_HASH={}\n", nuvira_secret::hash(password)))
    };
    let generated = match (&opts.password, env_path().exists()) {
        (Some(password), _) => {
            store(password)?;
            None
        }
        (None, true) => None,
        (None, false) => {
            let password = crate::random_password();
            store(&password)?;
            Some(password)
        }
    };

    // Always with --server: empty is the public server (without it the agent
    // would wait for viewers on the local network only).
    let mut exec = format!("\"{}\" agent --ipc --server \"{}\"", exe.display(), opts.server);
    if let Some(key) = &opts.server_key {
        exec += &format!(" --server-key {key}");
    }
    let unit = format!(
        "[Unit]\n\
         Description=NuviraHub agent (unattended access)\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         EnvironmentFile={env}\n\
         Restart=always\n\
         RestartSec=5\n",
        env = env_path().display(),
    );
    let unit_path = unit_path()?;
    std::fs::create_dir_all(unit_path.parent().context("unit folder")?)?;
    std::fs::write(&unit_path, unit)?;

    let autostart = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=NuviraHub unattended access\n\
         Exec=\"{}\" service start\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        exe.display()
    );
    let autostart_path = autostart_path()?;
    std::fs::create_dir_all(autostart_path.parent().context("autostart folder")?)?;
    std::fs::write(&autostart_path, autostart)?;

    systemctl(&["daemon-reload"])?;
    start()?;
    info!(unit = %unit_path.display(), "user service installed");
    Ok(generated)
}

/// Removes the user service; the ID and the address book stay.
pub fn uninstall() -> Result<()> {
    let _ = systemctl(&["stop", UNIT]);
    for path in [unit_path()?, autostart_path()?, env_path()] {
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| path.display().to_string())?;
        }
    }
    let _ = systemctl(&["daemon-reload"]);
    info!("user service removed");
    Ok(())
}

/// Hands the variables of the current desktop session to systemd and (re)starts
/// the agent: run at login by the autostart entry, and after installing.
pub fn start() -> Result<()> {
    let present: Vec<&str> = SESSION_VARS.iter().copied().filter(|v| std::env::var_os(v).is_some()).collect();
    let mut args = vec!["import-environment"];
    args.extend(&present);
    systemctl(&args)?;
    systemctl(&["restart", UNIT])
}

pub fn service_state() -> Result<ServiceState> {
    if !unit_path()?.exists() {
        return Ok(ServiceState::NotInstalled);
    }
    let out = Command::new("systemctl").args(["--user", "is-active", UNIT]).output().context("systemctl")?;
    Ok(match String::from_utf8_lossy(&out.stdout).trim() {
        "active" => ServiceState::Running,
        "activating" | "deactivating" | "reloading" => ServiceState::Changing,
        _ => ServiceState::Stopped,
    })
}

/// The service runs this same program: there is never an older installed one.
pub fn installed_version() -> Option<String> {
    None
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file =
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}
