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
//! - macOS: `NSWorkspace.setDesktopImageURL` with a black picture for every
//!   screen, and the screens' own pictures back. It must be called from a main
//!   thread, which the agent's is not: this same program is started again as a
//!   helper for each step (`--nuvira-wallpaper get|hide|restore`, see
//!   [`helper_if_asked`]), as the black screens of privacy mode.

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

/// macOS: when this program was started as the wallpaper's helper, does the step
/// and exits. Call it first thing in `main`. Other systems: nothing.
pub fn helper_if_asked() {
    #[cfg(target_os = "macos")]
    {
        let mut args = std::env::args();
        if args.nth(1).as_deref() == Some(mac_helper::ARG) {
            mac_helper::run(&args.next().unwrap_or_default());
        }
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

#[cfg(target_os = "macos")]
mod imp {
    use std::io::Write;

    use anyhow::{Context, Result, bail};

    pub fn available() -> bool {
        // The sign-in screen's agent (root) has no person's desktop.
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() != 0 }
    }

    /// Runs this program as the helper (`--nuvira-wallpaper <step>`), with `input`
    /// on its standard input; what it printed.
    fn helper(step: &str, input: &str) -> Result<String> {
        let exe = std::env::current_exe().context("program path")?;
        let mut child = std::process::Command::new(exe)
            .args([super::mac_helper::ARG, step])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("wallpaper helper")?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes());
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("wallpaper helper ({step}): {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// "macos" and a line "screen<TAB>URL" for each screen.
    pub fn current() -> Result<String> {
        Ok(format!("macos\n{}", helper("get", "")?))
    }

    pub fn hide() -> Result<()> {
        helper("hide", "").map(drop)
    }

    pub fn restore(saved: &str) -> Result<()> {
        let Some(screens) = saved.strip_prefix("macos\n") else { bail!("not a macOS wallpaper") };
        helper("restore", screens).map(drop)
    }
}

/// macOS: the helper process, on its own main thread.
#[cfg(target_os = "macos")]
mod mac_helper {
    use std::io::Read;

    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSScreen, NSWorkspace};
    use objc2_foundation::{NSDictionary, NSString, NSURL};

    pub const ARG: &str = "--nuvira-wallpaper";

    /// A 1x1 black PNG, scaled over each screen.
    const BLACK_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0, 144,
        119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 218, 99, 96, 96, 96, 0, 0, 0, 4, 0, 1, 200, 234, 235, 249,
        0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    pub fn run(step: &str) -> ! {
        let Some(mtm) = MainThreadMarker::new() else {
            eprintln!("wallpaper helper: not on the main thread");
            std::process::exit(2);
        };
        let workspace = NSWorkspace::sharedWorkspace();
        let screens = NSScreen::screens(mtm);
        let set = |index: usize, url: &NSURL| -> bool {
            let Some(screen) = screens.iter().nth(index) else { return false };
            let options = NSDictionary::new();
            // SAFETY: an empty dictionary of options.
            match unsafe { workspace.setDesktopImageURL_forScreen_options_error(url, &screen, &options) } {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("screen {index}: {}", e.localizedDescription());
                    false
                }
            }
        };
        let ok = match step {
            "get" => {
                for (i, screen) in screens.iter().enumerate() {
                    if let Some(text) = workspace.desktopImageURLForScreen(&screen).and_then(|u| u.absoluteString()) {
                        println!("{i}\t{text}");
                    }
                }
                true
            }
            "hide" => {
                let path = std::env::temp_dir().join("nuvira-black.png");
                let _ = std::fs::write(&path, BLACK_PNG);
                let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
                (0..screens.count()).map(|i| set(i, &url)).fold(true, |a, b| a && b)
            }
            "restore" => {
                let mut saved = String::new();
                let _ = std::io::stdin().read_to_string(&mut saved);
                saved
                    .lines()
                    .filter_map(|l| l.split_once('\t'))
                    .filter_map(|(i, url)| {
                        Some((i.parse::<usize>().ok()?, NSURL::URLWithString(&NSString::from_str(url))?))
                    })
                    .map(|(i, url)| set(i, &url))
                    .fold(true, |a, b| a && b)
            }
            _ => false,
        };
        std::process::exit(if ok { 0 } else { 1 });
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
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
