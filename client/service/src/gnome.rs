//! GNOME on Wayland: the lock screen and GDM's sign-in screen.
//!
//! GNOME Shell inhibits remote access whenever its session mode does not allow
//! screencasts: at the lock it closes every remote desktop session and refuses
//! new ones ("Session creation inhibited") until someone unlocks at the computer,
//! and GDM's greeter does the same. There is no setting for it; a GNOME Shell
//! extension, `remote-access@nuvirahub.dev` (`client/service/gnome/extension.js`),
//! makes the inhibition do nothing while unattended access is installed:
//!
//! - the user's part installs it in the person's extensions and enables it there:
//!   lock screen (from the next sign-in: GNOME loads extensions when the session
//!   starts);
//! - the system's part (root) installs it for everyone and enables it for GDM's
//!   greeter through dconf's `gdm` database: sign-in screen.
//!
//! Removing unattended access removes it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use tracing::{info, warn};

const UUID: &str = "remote-access@nuvirahub.dev";
const EXTENSION_JS: &str = include_str!("../gnome/extension.js");
/// The first GNOME Shell with ES module extensions, which the extension is.
const FIRST_SHELL: u32 = 45;
const SYSTEM_EXTENSIONS: &str = "/usr/share/gnome-shell/extensions";
const GDM_DCONF_DIR: &str = "/etc/dconf/db/gdm.d";
const GDM_DCONF_FILE: &str = "90-nuvira-remote-access";
const DCONF_PROFILES: [&str; 2] = ["/etc/dconf/profile/gdm", "/usr/share/dconf/profile/gdm"];

/// GNOME Shell's major version, when it is installed.
fn shell_major() -> Option<u32> {
    let out = Command::new("gnome-shell").arg("--version").output().ok()?;
    parse_shell_version(&String::from_utf8_lossy(&out.stdout))
}

fn parse_shell_version(text: &str) -> Option<u32> {
    text.split_whitespace().find_map(|word| word.split('.').next()?.parse().ok())
}

/// `metadata.json`: GNOME Shell refuses extensions that do not list its version,
/// so the list goes from the first one with this kind of extension to the one
/// installed and the next (written again at every installation).
fn metadata(major: u32) -> String {
    let versions: Vec<String> =
        (FIRST_SHELL..=major.max(FIRST_SHELL) + 1).map(|v| format!("\"{v}\"")).collect();
    format!(
        "{{\n  \"uuid\": \"{UUID}\",\n  \"name\": \"NuviraHub unattended access\",\n  \
         \"description\": \"Keeps NuviraHub's remote control working on the lock screen and at the sign-in screen. \
         Installed with NuviraHub's unattended access, removed with it.\",\n  \
         \"url\": \"https://nuvirahub.dev\",\n  \
         \"session-modes\": [\"user\", \"unlock-dialog\", \"gdm\"],\n  \
         \"shell-version\": [{}]\n}}\n",
        versions.join(", ")
    )
}

fn write_extension(dir: &Path, major: u32) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    std::fs::write(dir.join("extension.js"), EXTENSION_JS)?;
    std::fs::write(dir.join("metadata.json"), metadata(major))?;
    Ok(())
}

fn remove_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| dir.display().to_string())
        }
        _ => Ok(()),
    }
}

/// The strings of a GVariant string array as `gsettings get` prints it
/// (`['a', 'b']`, `@as []`).
fn parse_strv(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\'' && c != '"' {
            continue;
        }
        let mut item = String::new();
        while let Some(d) = chars.next() {
            match d {
                '\\' => item.extend(chars.next()),
                d if d == c => break,
                d => item.push(d),
            }
        }
        items.push(item);
    }
    items
}

fn format_strv(items: &[String]) -> String {
    let quoted: Vec<String> =
        items.iter().map(|i| format!("'{}'", i.replace('\\', "\\\\").replace('\'', "\\'"))).collect();
    format!("[{}]", quoted.join(", "))
}

fn gsettings_strv(key: &str, envs: &[(&str, &str)]) -> Result<Vec<String>> {
    let out = Command::new("gsettings")
        .envs(envs.iter().copied())
        .args(["get", "org.gnome.shell", key])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("gsettings get {key}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(parse_strv(&String::from_utf8_lossy(&out.stdout)))
}

fn gsettings_set_strv(key: &str, items: &[String]) -> Result<()> {
    let status =
        Command::new("gsettings").args(["set", "org.gnome.shell", key, &format_strv(items)]).status()?;
    if !status.success() {
        anyhow::bail!("gsettings set {key} failed ({status})");
    }
    Ok(())
}

fn user_dir() -> Result<PathBuf> {
    Ok(crate::unix_shared::home()?.join(".local/share/gnome-shell/extensions").join(UUID))
}

/// As the person: installs and enables the extension in their GNOME session
/// (nothing without GNOME Shell). It works from the next sign-in.
pub fn install_user() -> Result<()> {
    let Some(major) = shell_major().filter(|&m| m >= FIRST_SHELL) else {
        return Ok(());
    };
    write_extension(&user_dir()?, major)?;
    let mut enabled = gsettings_strv("enabled-extensions", &[])?;
    if !enabled.iter().any(|e| e == UUID) {
        enabled.push(UUID.to_string());
        gsettings_set_strv("enabled-extensions", &enabled)?;
    }
    let disabled = gsettings_strv("disabled-extensions", &[])?;
    if disabled.iter().any(|e| e == UUID) {
        let kept: Vec<String> = disabled.into_iter().filter(|e| e != UUID).collect();
        gsettings_set_strv("disabled-extensions", &kept)?;
    }
    info!(gnome = major, "GNOME extension for the lock screen installed (from the next sign-in)");
    Ok(())
}

/// As the person: disables and removes it.
pub fn uninstall_user() -> Result<()> {
    let dir = user_dir()?;
    if Command::new("gsettings").arg("--version").output().is_ok() {
        match gsettings_strv("enabled-extensions", &[]) {
            Ok(enabled) if enabled.iter().any(|e| e == UUID) => {
                let kept: Vec<String> = enabled.into_iter().filter(|e| e != UUID).collect();
                gsettings_set_strv("enabled-extensions", &kept)?;
            }
            Ok(_) => {}
            Err(e) => warn!("GNOME extensions not read: {e:#}"),
        }
    }
    remove_dir(&dir)
}

/// Where GDM's greeter takes its settings from.
#[derive(Clone, Copy)]
enum GreeterDb {
    /// dconf's `gdm` system database (`system-db:gdm` in the profile: Fedora,
    /// upstream GDM), rebuilt with `dconf update`.
    Gdm,
    /// Debian and Ubuntu: the keyfiles of `/usr/share/gdm/dconf`, compiled by
    /// gdm3's `generate-config` into `/var/lib/gdm3/greeter-dconf-defaults` at
    /// every start of GDM.
    Debian,
}

const DEBIAN_DCONF_DIR: &str = "/usr/share/gdm/dconf";
const DEBIAN_GENERATE: &str = "/usr/share/gdm/generate-config";
/// After the distribution's `90-debian-settings`: the later keyfile wins.
const DEBIAN_DCONF_FILE: &str = "95-nuvira-remote-access";

impl GreeterDb {
    fn find() -> Option<Self> {
        let gdm_db = DCONF_PROFILES.iter().any(|p| {
            std::fs::read_to_string(p).is_ok_and(|t| t.lines().any(|l| l.trim() == "system-db:gdm"))
        });
        if gdm_db {
            Some(Self::Gdm)
        } else if Path::new(DEBIAN_DCONF_DIR).is_dir() && Path::new(DEBIAN_GENERATE).exists() {
            Some(Self::Debian)
        } else {
            None
        }
    }

    fn path(self) -> PathBuf {
        match self {
            Self::Gdm => Path::new(GDM_DCONF_DIR).join(GDM_DCONF_FILE),
            Self::Debian => Path::new(DEBIAN_DCONF_DIR).join(DEBIAN_DCONF_FILE),
        }
    }

    fn rebuild(self) -> Result<()> {
        let mut command = match self {
            Self::Gdm => {
                let mut c = Command::new("dconf");
                c.arg("update");
                c
            }
            Self::Debian => Command::new(DEBIAN_GENERATE),
        };
        let status = command.status().with_context(|| format!("{command:?}"))?;
        if !status.success() {
            anyhow::bail!("{command:?} failed ({status})");
        }
        Ok(())
    }

    /// Removes our keyfile, if there.
    fn remove(self) -> Result<()> {
        let path = self.path();
        if path.exists() {
            std::fs::remove_file(&path)?;
            self.rebuild()?;
        }
        Ok(())
    }
}

/// As root: installs the extension for everyone and enables it at GDM's
/// sign-in screen (nothing without GNOME Shell or GDM).
pub fn install_system() -> Result<()> {
    let Some(major) = shell_major().filter(|&m| m >= FIRST_SHELL) else {
        return Ok(());
    };
    write_extension(&Path::new(SYSTEM_EXTENSIONS).join(UUID), major)?;
    let Some(db) = GreeterDb::find() else {
        info!("GNOME extension installed; GDM's settings not found");
        return Ok(());
    };
    // The greeter's list without ours (the distribution may set one), plus ours.
    db.remove()?;
    // Without the user-db of whoever runs this (the profile starts with it).
    let mut enabled = gsettings_strv(
        "enabled-extensions",
        &[("DCONF_PROFILE", "gdm"), ("XDG_CONFIG_HOME", "/nonexistent")],
    )
    .unwrap_or_else(|e| {
        warn!("enabled extensions of the sign-in screen not read: {e:#}");
        Vec::new()
    });
    enabled.retain(|e| e != UUID);
    enabled.push(UUID.to_string());
    let path = db.path();
    std::fs::create_dir_all(path.parent().context("dconf folder")?)?;
    std::fs::write(
        &path,
        format!(
            "# NuviraHub unattended access: remote control at the sign-in screen.\n\
             [org/gnome/shell]\nenabled-extensions={}\n",
            format_strv(&enabled)
        ),
    )?;
    db.rebuild()?;
    info!(
        gnome = major,
        settings = %path.display(),
        "GNOME extension installed and enabled at the sign-in screen (from GDM's next start)"
    );
    Ok(())
}

/// As root: removes it.
pub fn uninstall_system() -> Result<()> {
    for db in [GreeterDb::Gdm, GreeterDb::Debian] {
        if db.path().exists() {
            db.remove()?;
        }
    }
    remove_dir(&Path::new(SYSTEM_EXTENSIONS).join(UUID))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_version() {
        assert_eq!(parse_shell_version("GNOME Shell 50.1\n"), Some(50));
        assert_eq!(parse_shell_version("GNOME Shell 46.rc"), Some(46));
        assert_eq!(parse_shell_version(""), None);
    }

    #[test]
    fn metadata_lists_the_versions() {
        let m = metadata(50);
        assert!(
            m.contains("\"shell-version\": [\"45\", \"46\", \"47\", \"48\", \"49\", \"50\", \"51\"]"),
            "{m}"
        );
        assert!(m.contains("\"gdm\""));
    }

    #[test]
    fn strv_round_trip() {
        assert_eq!(parse_strv("@as []\n"), Vec::<String>::new());
        let items = parse_strv("['background-logo@fedorahosted.org', 'a\\'b']\n");
        assert_eq!(items, vec!["background-logo@fedorahosted.org".to_string(), "a'b".to_string()]);
        assert_eq!(parse_strv(&format_strv(&items)), items);
    }
}
