//! The desktop wallpaper hidden during a session (the viewer asks for it): a plain
//! background compresses far better than a photo, which matters on slow links.
//! What was there is written to a state file first, and put back at the end of
//! the session or, after an agent that was killed, at the next start
//! ([`recover`]).
//!
//! - Windows: `SystemParametersInfo(SPI_SETDESKWALLPAPER)` with no picture,
//!   without writing the profile (after a sign-out the picture is back anyway).
//! - GNOME and Cinnamon: `picture-options` of their background schema set to
//!   `none` (the background colour shows).
//! - KDE Plasma: a Plasma script through D-Bus switches every desktop to the
//!   plain colour wallpaper (`org.kde.color`, black) and back to its own plugin.
//! - macOS: not yet (`setDesktopImageURL` must be called from the main thread of
//!   an AppKit app).

use std::path::Path;

use anyhow::Result;

/// Whether this system can hide the wallpaper.
pub fn available() -> bool {
    imp::available()
}

/// Hides the wallpaper, keeping in `state` what was there. Nothing if `state`
/// already exists (hidden by an earlier session that did not put it back).
pub fn hide(state: &Path) -> Result<()> {
    if state.exists() {
        return Ok(());
    }
    let saved = imp::current()?;
    if let Some(dir) = state.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(state, &saved)?;
    if let Err(e) = imp::hide() {
        let _ = std::fs::remove_file(state);
        return Err(e);
    }
    tracing::info!("wallpaper hidden for the session");
    Ok(())
}

/// Puts back the wallpaper kept in `state`, if any.
pub fn restore(state: &Path) {
    let Ok(saved) = std::fs::read_to_string(state) else { return };
    match imp::restore(&saved) {
        Ok(()) => {
            tracing::info!("wallpaper put back");
            let _ = std::fs::remove_file(state);
        }
        Err(e) => tracing::warn!("wallpaper not put back: {e:#}"),
    }
}

/// At the start of the agent: a wallpaper left hidden by an agent that was killed.
pub fn recover(state: &Path) {
    if state.exists() {
        tracing::info!("wallpaper left hidden by an earlier session: putting it back");
        restore(state);
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::{Result, bail};
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETDESKWALLPAPER, SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        SystemParametersInfoW,
    };

    pub fn available() -> bool {
        true
    }

    /// "windows\n<path of the picture>" (empty: there was none).
    pub fn current() -> Result<String> {
        let mut buf = [0u16; 1024];
        // SAFETY: the buffer is as long as said.
        unsafe {
            SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                buf.len() as u32,
                Some(buf.as_mut_ptr().cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(format!("windows\n{}", String::from_utf16_lossy(&buf[..len])))
    }

    fn set(path: &str) -> Result<()> {
        let mut wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated string; not written to the profile.
        unsafe {
            SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, Some(wide.as_mut_ptr().cast()), SPIF_SENDCHANGE)
        }?;
        Ok(())
    }

    pub fn hide() -> Result<()> {
        set("")
    }

    pub fn restore(saved: &str) -> Result<()> {
        let Some(path) = saved.strip_prefix("windows\n") else { bail!("not a Windows wallpaper") };
        set(path)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::process::Command;

    use anyhow::{Context, Result, bail};

    #[derive(Clone, Copy)]
    enum Desktop {
        Gnome,
        Cinnamon,
        Plasma,
    }

    const GNOME: &str = "org.gnome.desktop.background";
    const CINNAMON: &str = "org.cinnamon.desktop.background";

    fn running(name: &str) -> bool {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        Command::new("pgrep")
            .args(["-u", &uid.to_string(), "-x", name])
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn desktop() -> Option<Desktop> {
        if running("cinnamon") {
            Some(Desktop::Cinnamon)
        } else if running("gnome-shell") {
            Some(Desktop::Gnome)
        } else if running("plasmashell") {
            Some(Desktop::Plasma)
        } else {
            None
        }
    }

    pub fn available() -> bool {
        desktop().is_some()
    }

    fn gsettings(args: &[&str]) -> Result<String> {
        let out = Command::new("gsettings").args(args).output().context("gsettings")?;
        if !out.status.success() {
            bail!("gsettings {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Runs a Plasma script and returns what it printed.
    fn plasma(script: &str) -> Result<String> {
        let out = Command::new("busctl")
            .args([
                "--user",
                "call",
                "org.kde.plasmashell",
                "/PlasmaShell",
                "org.kde.PlasmaShell",
                "evaluateScript",
                "s",
            ])
            .arg(script)
            .output()
            .context("busctl")?;
        if !out.status.success() {
            bail!("Plasma script: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        // `s "text"`, with C escapes.
        let text = String::from_utf8_lossy(&out.stdout);
        let inner = text.trim().strip_prefix("s \"").and_then(|t| t.strip_suffix('"')).unwrap_or_default();
        Ok(inner.replace("\\n", "\n").replace("\\\"", "\"").trim().to_string())
    }

    pub fn current() -> Result<String> {
        match desktop().context("desktop without a known wallpaper")? {
            Desktop::Gnome => Ok(format!("gnome\n{}", gsettings(&["get", GNOME, "picture-options"])?)),
            Desktop::Cinnamon => {
                Ok(format!("cinnamon\n{}", gsettings(&["get", CINNAMON, "picture-options"])?))
            }
            Desktop::Plasma => {
                let plugins = plasma(
                    "var p=[];var d=desktops();for(var i=0;i<d.length;i++){p.push(d[i].wallpaperPlugin);}print(p.join(','));",
                )?;
                Ok(format!("plasma\n{plugins}"))
            }
        }
    }

    pub fn hide() -> Result<()> {
        match desktop().context("desktop without a known wallpaper")? {
            Desktop::Gnome => gsettings(&["set", GNOME, "picture-options", "none"]).map(drop),
            Desktop::Cinnamon => gsettings(&["set", CINNAMON, "picture-options", "none"]).map(drop),
            Desktop::Plasma => plasma(
                "var d=desktops();for(var i=0;i<d.length;i++){d[i].wallpaperPlugin='org.kde.color';\
                 d[i].currentConfigGroup=Array('Wallpaper','org.kde.color','General');d[i].writeConfig('Color','0,0,0');}",
            )
            .map(drop),
        }
    }

    pub fn restore(saved: &str) -> Result<()> {
        let (kind, value) = saved.split_once('\n').unwrap_or((saved, ""));
        // gsettings prints strings quoted ('zoom'), as `set` takes them.
        let value = value.trim();
        match kind {
            "gnome" => gsettings(&["set", GNOME, "picture-options", value]).map(drop),
            "cinnamon" => gsettings(&["set", CINNAMON, "picture-options", value]).map(drop),
            "plasma" => {
                let list: Vec<String> = value
                    .split(',')
                    .filter(|p| !p.is_empty())
                    .map(|p| format!("'{}'", p.replace(['\'', '\\'], "")))
                    .collect();
                plasma(&format!(
                    "var p=[{}];var d=desktops();for(var i=0;i<d.length&&i<p.length;i++){{d[i].wallpaperPlugin=p[i];}}",
                    list.join(",")
                ))
                .map(drop)
            }
            other => bail!("unknown wallpaper kind {other}"),
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use anyhow::{Result, bail};

    pub fn available() -> bool {
        false
    }

    pub fn current() -> Result<String> {
        bail!("not available on this system")
    }

    pub fn hide() -> Result<()> {
        bail!("not available on this system")
    }

    pub fn restore(_saved: &str) -> Result<()> {
        bail!("not available on this system")
    }
}
