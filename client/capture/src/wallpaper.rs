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
//! - macOS: black windows at the wallpaper's level, below the desktop's icons,
//!   on every screen and every Space (`setDesktopImageURL` changes the picture
//!   of the Space in front only). AppKit wants a main thread, which the agent's
//!   is not: this same program is started again as a helper holding them
//!   (`--nuvira-wallpaper cover`, see [`helper_if_asked`]), as the black screens
//!   of privacy mode; it ends with the session, or with the agent.

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
    use std::io::{BufRead, Write};
    use std::process::{Child, Stdio};
    use std::sync::Mutex;

    use anyhow::{Context, Result, bail};

    /// What the state file keeps on macOS now: nothing to put back but the helper.
    const COVER: &str = "macos-cover\n";

    /// The helper holding the black windows (`--nuvira-wallpaper cover`).
    static COVER_HELPER: Mutex<Option<Child>> = Mutex::new(None);

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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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

    pub fn current() -> Result<String> {
        Ok(COVER.into())
    }

    /// Black windows over the wallpaper of every screen, on every Space, until
    /// [`restore`]. Changing the picture (`setDesktopImageURL`) hid it only on the
    /// Space in front: the others kept their photo.
    pub fn hide() -> Result<()> {
        let mut helper = COVER_HELPER.lock().map_err(|_| anyhow::anyhow!("wallpaper helper lock"))?;
        if helper.is_some() {
            return Ok(());
        }
        let exe = std::env::current_exe().context("program path")?;
        let mut child = std::process::Command::new(exe)
            .args([super::mac_helper::ARG, "cover"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("wallpaper helper")?;
        // "ready" once the windows are up (or nothing: it failed).
        let mut line = String::new();
        if let Some(out) = child.stdout.take() {
            let _ = std::io::BufReader::new(out).read_line(&mut line);
        }
        if line.trim() != "ready" {
            let _ = child.kill();
            let _ = child.wait();
            bail!("wallpaper helper did not start");
        }
        *helper = Some(child);
        Ok(())
    }

    pub fn restore(saved: &str) -> Result<()> {
        if saved == COVER {
            if let Some(mut child) = COVER_HELPER.lock().ok().and_then(|mut h| h.take()) {
                // Its standard input closed: it ends.
                drop(child.stdin.take());
                for _ in 0..30 {
                    if child.try_wait().ok().flatten().is_some() {
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
            // No helper (an agent that was killed): its windows went with it.
            return Ok(());
        }
        // Left by a version that changed the picture: the screens' own pictures.
        let Some(screens) = saved.strip_prefix("macos\n") else { bail!("not a macOS wallpaper") };
        helper("restore", screens).map(drop)
    }
}

/// macOS: the helper process, on its own main thread.
#[cfg(target_os = "macos")]
mod mac_helper {
    use std::cell::RefCell;
    use std::io::Read;

    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSScreen, NSWindow,
        NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
    };
    use objc2_foundation::{NSRect, NSString, NSURL};

    pub const ARG: &str = "--nuvira-wallpaper";

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        /// `CGWindowLevelForKey`.
        fn CGWindowLevelForKey(key: i32) -> i32;
    }
    /// `kCGDesktopWindowLevelKey`: the wallpaper's level, below the desktop's icons.
    const DESKTOP_LEVEL_KEY: i32 = 2;

    thread_local! {
        /// The black windows and the screens they cover (main thread only).
        static COVERS: RefCell<(Vec<NSRect>, Vec<Retained<NSWindow>>)> = const { RefCell::new((Vec::new(), Vec::new())) };
    }

    pub fn run(step: &str) -> ! {
        let Some(mtm) = MainThreadMarker::new() else {
            eprintln!("wallpaper helper: not on the main thread");
            std::process::exit(2);
        };
        if step == "cover" {
            cover(mtm);
        }
        let workspace = NSWorkspace::sharedWorkspace();
        let screens = NSScreen::screens(mtm);
        let ok = match step {
            // Pictures kept by a version that changed them (see imp::restore).
            "restore" => {
                let mut saved = String::new();
                let _ = std::io::stdin().read_to_string(&mut saved);
                saved
                    .lines()
                    .filter_map(|l| l.split_once('\t'))
                    .filter_map(|(i, url)| {
                        Some((i.parse::<usize>().ok()?, NSURL::URLWithString(&NSString::from_str(url))?))
                    })
                    .map(|(i, url)| {
                        let Some(screen) = screens.iter().nth(i) else { return false };
                        let options = objc2_foundation::NSDictionary::new();
                        // SAFETY: an empty dictionary of options.
                        unsafe {
                            workspace.setDesktopImageURL_forScreen_options_error(&url, &screen, &options)
                        }
                        .is_ok()
                    })
                    .fold(true, |a, b| a && b)
            }
            _ => false,
        };
        std::process::exit(if ok { 0 } else { 1 });
    }

    /// Black windows at the wallpaper's level on every screen and every Space,
    /// following the screens, until the agent closes the standard input.
    fn cover(mtm: MainThreadMarker) -> ! {
        let app = NSApplication::sharedApplication(mtm);
        // No Dock icon; a "prohibited" app's windows would not be shown at all.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        update(mtm);
        println!("ready");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        std::thread::spawn(|| {
            let mut sink = Vec::new();
            // Until the end of the input: the agent's restore, or the agent gone.
            let _ = std::io::stdin().read_to_end(&mut sink);
            std::process::exit(0);
        });
        // A screen added, removed or changed: covered again.
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                dispatch2::DispatchQueue::main().exec_async(|| {
                    // SAFETY: work given to the main queue runs on the main thread.
                    update(unsafe { MainThreadMarker::new_unchecked() });
                });
            }
        });
        app.run();
        std::process::exit(0);
    }

    fn update(mtm: MainThreadMarker) {
        let frames: Vec<NSRect> = NSScreen::screens(mtm).iter().map(|s| s.frame()).collect();
        COVERS.with_borrow_mut(|(covered, windows)| {
            if *covered == frames && !windows.is_empty() {
                return;
            }
            for window in windows.drain(..) {
                window.close();
            }
            *windows = frames.iter().map(|frame| black_window(mtm, *frame)).collect();
            *covered = frames;
        });
    }

    fn black_window(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSWindow> {
        // SAFETY: plain window creation on the main thread; released by `Retained`.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        // Just above the picture, below the desktop's icons and every window.
        window.setLevel(unsafe { CGWindowLevelForKey(DESKTOP_LEVEL_KEY) } as isize + 1);
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        window.setOpaque(true);
        window.setHasShadow(false);
        window.setIgnoresMouseEvents(true);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.orderFrontRegardless();
        window
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
