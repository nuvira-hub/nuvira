//! Linux: unattended access through a systemd **user** service, `nuvira-agent`.
//! It runs as the person's own user, not as root:
//! - at login an autostart entry imports the variables of the desktop session
//!   (display, D-Bus, Wayland) into systemd and starts the service, so the
//!   agent can read the screen and inject input;
//! - systemd starts it again if it ends;
//! - the permanent password is in `service.env` (mode 0600), as a hash.
//!
//! What it cannot do: reach the computer before someone logs in (the system's
//! part does, see `signin`). On GNOME with Wayland the lock screen works through
//! a GNOME Shell extension installed here (module `gnome`), from the next sign-in.
//!
//! On a computer where NuviraHub is installed for everyone by an administrator
//! (module `managed`) the unit is the system's (`/etc/systemd/user`): a person
//! cannot install or remove it, and their own earlier installation gives way.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

pub use crate::unix_shared::{InstallOptions, ServiceState, installed_version};
use crate::unix_shared::{env_path, home, store_password};

const UNIT: &str = "nuvira-agent.service";

fn unit_path() -> Result<PathBuf> {
    Ok(home()?.join(".config/systemd/user").join(UNIT))
}

fn autostart_path() -> Result<PathBuf> {
    Ok(home()?.join(".config/autostart/nuvira-agent.desktop"))
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

/// Installs or updates the user service. Returns the permanent password when it
/// was generated, to show it once.
pub fn install(opts: &InstallOptions) -> Result<Option<String>> {
    if crate::managed::installed() {
        bail!("@install.managed");
    }
    let exe = std::env::current_exe().context("program path")?;
    let generated = store_password(opts)?;

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
    if let Err(e) = crate::gnome::install_user() {
        warn!("GNOME extension for the lock screen not installed: {e:#}");
    }
    info!(unit = %unit_path.display(), "user service installed");
    Ok(generated)
}

/// Removes the user service; the ID and the address book stay.
pub fn uninstall() -> Result<()> {
    if crate::managed::installed() {
        bail!("@install.managed");
    }
    remove_own()?;
    forget_imported();
    if let Err(e) = crate::gnome::uninstall_user() {
        warn!("GNOME extension for the lock screen not removed: {e:#}");
    }
    info!("user service removed");
    Ok(())
}

/// The person's own unit, autostart entry and password.
fn remove_own() -> Result<()> {
    let _ = systemctl(&["stop", UNIT]);
    for path in [unit_path()?, autostart_path()?, env_path()] {
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| path.display().to_string())?;
        }
    }
    let _ = systemctl(&["daemon-reload"]);
    Ok(())
}

/// Wayland: asks GNOME's permission to share the screen, while the person is at
/// the computer, and keeps it for the service. GNOME ties a saved permission to
/// the program that asked for it, telling programs apart by their systemd unit:
/// asked from the app (a unit of the desktop) it is refused to the service. So
/// it is asked from a unit started by the user's systemd, as the service is:
/// `nuvira service portal` through `systemd-run`.
pub fn ask_screen_permission() -> Result<()> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("program path")?;
    let mut command = Command::new("systemd-run");
    command.args(["--user", "--wait", "--collect", "--quiet"]);
    for var in SESSION_VARS {
        if let Some(value) = std::env::var_os(var) {
            command.arg(format!("--setenv={var}={}", value.to_string_lossy()));
        }
    }
    let status = command.arg(exe).args(["service", "portal"]).status().context("systemd-run")?;
    if !status.success() {
        bail!("@agent.screen_denied");
    }
    Ok(())
}

/// `nuvira service portal`: opens the screen sharing once and closes it, so the
/// permission is saved (see [`ask_screen_permission`]).
pub fn portal() -> Result<()> {
    drop(nuvira_capture::open(None)?);
    Ok(())
}

/// macOS asks for some folders (see the macOS module); Linux does not.
pub fn ask_folder_access() {}

/// Hands the variables of the current desktop session to systemd and (re)starts
/// the agent: run at login by the autostart entry, and after installing.
pub fn start() -> Result<()> {
    // Installed for everyone since: the person's own unit would hide the system's.
    if crate::managed::installed() && unit_path()?.exists() {
        info!("unattended access is now installed for the whole computer: the person's own installation gives way");
        remove_own()?;
    }
    let present: Vec<&str> = SESSION_VARS.iter().copied().filter(|v| std::env::var_os(v).is_some()).collect();
    // Without names systemctl would import the whole environment.
    if !present.is_empty() {
        // The person's own installation notes what it adds, to take it away
        // when it is removed (the installation for everyone stays).
        let note = !crate::managed::installed();
        let before = if note { manager_environment() } else { None };
        let mut args = vec!["import-environment"];
        args.extend(&present);
        systemctl(&args)?;
        if let Some(before) = before {
            note_imported(&before, &present);
        }
    }
    systemctl(&["restart", UNIT])
}

/// The variables the agent's `import-environment` put into the person's
/// systemd (the manager's environment, which every user unit gets): one
/// `NAME=value` per line. Only those the desktop had not set already to the
/// same value, so that removing them later does not take away the desktop's own.
fn imported_path() -> PathBuf {
    nuvira_transport::identity::data_dir().join("imported-environment")
}

/// The environment of the person's systemd manager.
fn manager_environment() -> Option<Vec<(String, String)>> {
    let out = Command::new("systemctl").args(["--user", "show-environment"]).output().ok()?;
    out.status.success().then(|| parse_environment(&String::from_utf8_lossy(&out.stdout)))
}

/// `NAME=value` lines (values of `show-environment` with special characters
/// come quoted: compared as printed, which is what a later comparison sees too).
fn parse_environment(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// After an import: notes the variables that were not in the manager with the
/// same value before, plus those already noted (still ours), with the value now.
fn note_imported(before: &[(String, String)], present: &[&str]) {
    let Some(after) = manager_environment() else { return };
    let noted = std::fs::read_to_string(imported_path()).map(|t| parse_environment(&t)).unwrap_or_default();
    let ours = imported_names(before, &after, &noted, present);
    let text: String = ours.iter().map(|(name, value)| format!("{name}={value}\n")).collect();
    let path = imported_path();
    let saved = if text.is_empty() {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    } else {
        crate::unix_shared::write_private(&path, &text)
    };
    if let Err(e) = saved {
        warn!("variables given to systemd not noted ({}): {e:#}", path.display());
    }
}

/// Which of `present` are the agent's own in the manager after the import.
fn imported_names(
    before: &[(String, String)],
    after: &[(String, String)],
    noted: &[(String, String)],
    present: &[&str],
) -> Vec<(String, String)> {
    let get =
        |list: &[(String, String)], name: &str| list.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    present
        .iter()
        .filter_map(|name| {
            let now = get(after, name)?;
            let ours = get(before, name).as_ref() != Some(&now) || get(noted, name).is_some();
            ours.then(|| (name.to_string(), now))
        })
        .collect()
}

/// At uninstall: takes out of the person's systemd the variables this
/// installation put there and that still have the value it gave them (P8).
fn forget_imported() {
    let path = imported_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let noted = parse_environment(&text);
    let current = manager_environment().unwrap_or_default();
    let names: Vec<&str> = noted
        .iter()
        .filter(|(name, value)| current.iter().any(|(n, v)| n == name && v == value))
        .map(|(name, _)| name.as_str())
        .collect();
    if !names.is_empty() {
        let mut args = vec!["unset-environment"];
        args.extend(&names);
        if let Err(e) = systemctl(&args) {
            warn!("variables given to systemd not taken away: {e:#}");
            return;
        }
        info!(names = names.join(" "), "variables of the desktop session taken out of systemd");
    }
    let _ = std::fs::remove_file(&path);
}

pub fn service_state() -> Result<ServiceState> {
    if !crate::managed::installed() && !unit_path()?.exists() {
        return Ok(ServiceState::NotInstalled);
    }
    let out = Command::new("systemctl").args(["--user", "is-active", UNIT]).output().context("systemctl")?;
    Ok(match String::from_utf8_lossy(&out.stdout).trim() {
        "active" => ServiceState::Running,
        "activating" | "deactivating" | "reloading" => ServiceState::Changing,
        _ => ServiceState::Stopped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(text: &str) -> Vec<(String, String)> {
        parse_environment(text)
    }

    /// P8: only what the import added or changed is the agent's; the desktop's
    /// own values (already there, identical) are not.
    #[test]
    fn imported_variables_are_only_the_added_ones() {
        let before = env("XDG_RUNTIME_DIR=/run/user/1000\nDISPLAY=:1\nPATH=/usr/bin\n");
        let after =
            env("XDG_RUNTIME_DIR=/run/user/1000\nDISPLAY=:0\nWAYLAND_DISPLAY=wayland-0\nPATH=/usr/bin\n");
        let present = ["XDG_RUNTIME_DIR", "DISPLAY", "WAYLAND_DISPLAY"];
        assert_eq!(
            imported_names(&before, &after, &[], &present),
            env("DISPLAY=:0\nWAYLAND_DISPLAY=wayland-0\n")
        );
        // Noted at an earlier login: still ours even if the same value now.
        let noted = env("XDG_RUNTIME_DIR=/run/user/1000\n");
        assert_eq!(imported_names(&after, &after, &noted, &present), env("XDG_RUNTIME_DIR=/run/user/1000\n"));
        // Not in the manager after the import (refused): nothing.
        assert_eq!(imported_names(&before, &[], &noted, &present), vec![]);
        assert_eq!(env("A=b=c\nnot a line\n"), vec![("A".to_string(), "b=c".to_string())]);
    }
}
