//! Adapting the resolution of a screen to the viewer's window during a session.
//!
//! A screen takes only its own modes, so the viewer's area becomes the mode that
//! fills it best (see [`choose`]). The original mode is put back
//! when the session asks, when it ends, and at the next start of the agent if it
//! ended without doing it:
//!
//! - Windows: `ChangeDisplaySettingsEx` without `CDS_UPDATEREGISTRY`, so the mode
//!   saved in the registry stays the person's; recovery puts the registry mode back.
//! - macOS: a configuration "for this program only" (`kCGConfigureForAppOnly`):
//!   macOS itself restores the screen when the agent ends, however it ends.
//! - Linux: XRandR on X11, with one screen turned on; the original size is written
//!   down for recovery. Not on Wayland.
//!
//! One session at a time changes a screen: another one is refused until it is back.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::Display;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// Modes below this are left out when the screen has larger ones (320x200 and the
/// like: nobody works at them, even in a small window).
const MIN: Size = Size { width: 800, height: 600 };

/// A mode may be this much larger than the area, the viewer shrinking it a little.
/// A real monitor has few modes, and the one just larger often fills the window
/// far better than the largest that fits: 1280x768 for a 1280x720 area, not 800x600
/// (tried on PC-LABORATORIO and Mint, 2026-11-09).
const MAX_SHRINK: f64 = 0.85;

/// The mode for an area of `target` pixels: the one that fills it best once fitted
/// in it, among those it shrinks by at most [`MAX_SHRINK`]; a shrunk mode counts a
/// little less (blurrier text), so an exact fit wins at equal size, then the shape
/// closest to the area's. When every mode is larger than that, the smallest.
/// `None` without modes.
pub fn choose(modes: &[Size], target: Size) -> Option<Size> {
    let large: Vec<Size> =
        modes.iter().copied().filter(|m| m.width >= MIN.width && m.height >= MIN.height).collect();
    let modes = if large.is_empty() { modes } else { &large };
    let area = |m: &Size| m.width as u64 * m.height as u64;
    let scale = |m: &Size| {
        (target.width as f64 / m.width.max(1) as f64).min(target.height as f64 / m.height.max(1) as f64)
    };
    // Pixels shown, counted less when shrunk: (w*h*s^2) * s.
    let score = |m: &Size| area(m) as f64 * scale(m).min(1.0).powi(3);
    let shape = target.width.max(1) as f64 / target.height.max(1) as f64;
    let off_shape = |m: &Size| ((m.width as f64 / m.height.max(1) as f64) / shape).ln().abs();
    modes
        .iter()
        .filter(|m| scale(m) >= MAX_SHRINK)
        .max_by(|a, b| score(a).total_cmp(&score(b)).then(off_shape(b).total_cmp(&off_shape(a))))
        .or_else(|| modes.iter().min_by_key(|m| area(m)))
        .copied()
}

/// Whether this system can change the resolution of its screens.
pub fn supported() -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    return true;
    #[cfg(target_os = "linux")]
    return !crate::wayland();
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    false
}

/// A screen changed by a session.
struct Changed {
    owner: u64,
    key: String,
    original: platform::Original,
}

static CHANGED: Mutex<Vec<Changed>> = Mutex::new(Vec::new());
/// Where the changed screens are written down, for [`recover`].
static STATE_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// At the start of the agent: `file` keeps the screens changed by sessions, and the
/// ones a previous run left changed (it ended without putting them back) are restored.
pub fn recover(file: &Path) {
    *STATE_FILE.lock().expect("state file") = Some(file.to_path_buf());
    let Ok(text) = std::fs::read_to_string(file) else { return };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match platform::recover(line) {
            Ok(()) => info!("screen resolution put back after an interrupted session: {line}"),
            Err(e) => warn!("screen resolution not put back ({line}): {e:#}"),
        }
    }
    let _ = std::fs::remove_file(file);
}

fn write_state(changed: &[Changed]) {
    let Some(file) = STATE_FILE.lock().expect("state file").clone() else { return };
    let lines: String = changed.iter().map(|c| format!("{}\n", c.original.record())).collect();
    let result = match lines.is_empty() {
        true => std::fs::remove_file(&file).or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        }),
        false => std::fs::write(&file, lines),
    };
    if let Err(e) = result {
        warn!("{}: {e}", file.display());
    }
}

/// Changes `display` to the mode that best fits `target`, for session `owner`; the
/// first change remembers the mode to put back. Returns the screen's new size.
pub fn fit(owner: u64, display: &Display, target: Size) -> Result<Size> {
    if !supported() {
        bail!("@agent.resolution_unavailable");
    }
    let screen = platform::Screen::of(display)?;
    let mut changed = CHANGED.lock().expect("changed screens");
    if changed.iter().any(|c| c.key == screen.key() && c.owner != owner) {
        bail!("@agent.resolution_busy");
    }
    let modes = screen.modes()?;
    let Some(mode) = choose(&modes, target) else { bail!("@agent.resolution_unavailable") };
    let current = screen.current()?;
    let index = display.index;
    if mode == current {
        info!("screen {index} already at {current}, the best mode for a {target} area");
        return Ok(current);
    }
    if !changed.iter().any(|c| c.key == screen.key()) {
        changed.push(Changed { owner, key: screen.key(), original: screen.original()? });
        write_state(&changed);
    }
    info!("screen {index} from {current} to {mode} (viewer area {target})");
    if let Err(e) = screen.set(mode) {
        // Nothing changed: nothing to put back.
        if screen.current().ok() == Some(current) {
            let first_change =
                changed.iter().position(|c| c.key == screen.key() && c.original.size() == current);
            if let Some(i) = first_change {
                changed.remove(i);
                write_state(&changed);
            }
        }
        return Err(e).context("@agent.resolution_failed");
    }
    Ok(mode)
}

/// The modes of `display` and its current one (tests, logs).
pub fn modes(display: &Display) -> Result<(Vec<Size>, Size)> {
    let screen = platform::Screen::of(display)?;
    Ok((screen.modes()?, screen.current()?))
}

/// Whether session `owner` has changed `display`.
pub fn adapted(owner: u64, display: &Display) -> bool {
    let Ok(screen) = platform::Screen::of(display) else { return false };
    CHANGED.lock().expect("changed screens").iter().any(|c| c.owner == owner && c.key == screen.key())
}

/// Puts back the screens session `owner` changed: only `display`, or all of them.
/// True if some screen was put back.
pub fn restore(owner: u64, display: Option<&Display>) -> bool {
    let key = display.and_then(|d| platform::Screen::of(d).ok()).map(|s| s.key());
    if display.is_some() && key.is_none() {
        return false;
    }
    let mut changed = CHANGED.lock().expect("changed screens");
    let mut restored = false;
    changed.retain(|c| {
        if c.owner != owner || key.as_ref().is_some_and(|k| *k != c.key) {
            return true;
        }
        match c.original.put_back() {
            Ok(()) => info!("screen resolution put back to {}", c.original.size()),
            Err(e) => warn!("screen resolution not put back to {}: {e:#}", c.original.size()),
        }
        restored = true;
        false
    });
    if restored {
        write_state(&changed);
    }
    restored
}

#[cfg(windows)]
mod platform {
    use anyhow::{Result, bail};
    use windows::Win32::Graphics::Gdi::{
        CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL,
        DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
        ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsExW,
    };
    use windows::core::PCWSTR;

    use super::Size;
    use crate::Display;

    fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(Some(0)).collect()
    }

    fn mode(name: &[u16], index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
        let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
        // SAFETY: NUL-terminated name, a DEVMODEW with its size set.
        let ok = unsafe {
            EnumDisplaySettingsExW(PCWSTR(name.as_ptr()), index, &mut dm, ENUM_DISPLAY_SETTINGS_FLAGS(0))
        };
        ok.as_bool().then_some(dm)
    }

    fn change(name: &[u16], dm: Option<&DEVMODEW>) -> Result<()> {
        // Not CDS_UPDATEREGISTRY: the registry keeps the person's own mode.
        // SAFETY: NUL-terminated name; the DEVMODEW, if any, lives for the call.
        let result = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(name.as_ptr()),
                dm.map(|d| d as *const _),
                None,
                CDS_TYPE(0),
                None,
            )
        };
        if result != DISP_CHANGE_SUCCESSFUL {
            bail!("ChangeDisplaySettingsEx: {}", result.0);
        }
        Ok(())
    }

    fn size(dm: &DEVMODEW) -> Size {
        Size { width: dm.dmPelsWidth, height: dm.dmPelsHeight }
    }

    pub struct Screen {
        name: String,
        wide: Vec<u16>,
    }

    impl Screen {
        pub fn of(display: &Display) -> Result<Self> {
            // GDI capture names screens "screen N": no device to change.
            if !display.name.starts_with(r"\\.\") {
                bail!("@agent.resolution_unavailable");
            }
            Ok(Self { name: display.name.clone(), wide: wide(&display.name) })
        }

        pub fn key(&self) -> String {
            self.name.clone()
        }

        fn current_mode(&self) -> Result<DEVMODEW> {
            match mode(&self.wide, ENUM_CURRENT_SETTINGS) {
                Some(dm) => Ok(dm),
                None => bail!("the mode of {} cannot be read", self.name),
            }
        }

        pub fn current(&self) -> Result<Size> {
            Ok(size(&self.current_mode()?))
        }

        /// Modes of the current orientation, colour depth as now.
        fn all(&self) -> Result<Vec<DEVMODEW>> {
            let current = self.current_mode()?;
            let mut list = Vec::new();
            for i in 0.. {
                let Some(dm) = mode(&self.wide, ENUM_DISPLAY_SETTINGS_MODE(i)) else { break };
                if dm.dmBitsPerPel == current.dmBitsPerPel && dm.dmPelsWidth > 0 && dm.dmPelsHeight > 0 {
                    list.push(dm);
                }
            }
            Ok(list)
        }

        pub fn modes(&self) -> Result<Vec<Size>> {
            let mut sizes: Vec<Size> = self.all()?.iter().map(size).collect();
            sizes.sort_by_key(|s| (s.width, s.height));
            sizes.dedup();
            Ok(sizes)
        }

        pub fn set(&self, to: Size) -> Result<()> {
            let current = self.current_mode()?;
            // The same refresh rate as now when the size has it, otherwise the highest.
            let candidates: Vec<DEVMODEW> = self.all()?.into_iter().filter(|dm| size(dm) == to).collect();
            let Some(mut dm) = candidates
                .iter()
                .find(|dm| dm.dmDisplayFrequency == current.dmDisplayFrequency)
                .or_else(|| candidates.iter().max_by_key(|dm| dm.dmDisplayFrequency))
                .copied()
            else {
                bail!("no mode {to} on {}", self.name);
            };
            dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL | DM_DISPLAYFREQUENCY;
            change(&self.wide, Some(&dm))
        }

        pub fn original(&self) -> Result<Original> {
            Ok(Original { name: self.name.clone(), mode: self.current_mode()? })
        }
    }

    pub struct Original {
        name: String,
        mode: DEVMODEW,
    }

    impl Original {
        pub fn size(&self) -> Size {
            size(&self.mode)
        }

        pub fn record(&self) -> String {
            self.name.clone()
        }

        pub fn put_back(&self) -> Result<()> {
            let mut dm = self.mode;
            dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL | DM_DISPLAYFREQUENCY;
            change(&wide(&self.name), Some(&dm))
        }
    }

    /// The mode in the registry: the person's own, never changed by a session.
    pub fn recover(record: &str) -> Result<()> {
        change(&wide(record.trim()), None)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Result, anyhow, bail};
    use core_graphics::display::{CGConfigureOption, CGDisplay, CGDisplayMode};

    use super::Size;
    use crate::Display;

    /// kDisplayModeValidFlag
    const VALID: u32 = 0x1;

    pub struct Screen {
        id: u32,
    }

    fn pixels(mode: &CGDisplayMode) -> Size {
        Size { width: mode.pixel_width() as u32, height: mode.pixel_height() as u32 }
    }

    /// Pixels per point: Retina modes stay Retina.
    fn scale(mode: &CGDisplayMode) -> u64 {
        mode.pixel_width() / mode.width().max(1)
    }

    impl Screen {
        pub fn of(display: &Display) -> Result<Self> {
            Ok(Self { id: crate::macos::display_id(display.index)? })
        }

        pub fn key(&self) -> String {
            self.id.to_string()
        }

        fn current_mode(&self) -> Result<CGDisplayMode> {
            CGDisplay::new(self.id)
                .display_mode()
                .ok_or_else(|| anyhow!("the mode of screen {} cannot be read", self.id))
        }

        pub fn current(&self) -> Result<Size> {
            Ok(pixels(&self.current_mode()?))
        }

        /// Usable modes with the same pixels per point as now.
        fn all(&self) -> Result<Vec<CGDisplayMode>> {
            let current = self.current_mode()?;
            let modes = CGDisplayMode::all_display_modes(self.id, std::ptr::null())
                .ok_or_else(|| anyhow!("the modes of screen {} cannot be read", self.id))?;
            Ok(modes
                .into_iter()
                .filter(|m| m.io_flags() & VALID != 0 && scale(m) == scale(&current))
                .collect())
        }

        pub fn modes(&self) -> Result<Vec<Size>> {
            let mut sizes: Vec<Size> = self.all()?.iter().map(pixels).collect();
            sizes.sort_by_key(|s| (s.width, s.height));
            sizes.dedup();
            Ok(sizes)
        }

        pub fn set(&self, to: Size) -> Result<()> {
            let current = self.current_mode()?;
            let candidates: Vec<CGDisplayMode> =
                self.all()?.into_iter().filter(|m| pixels(m) == to).collect();
            let Some(mode) = candidates
                .iter()
                .find(|m| m.refresh_rate() == current.refresh_rate())
                .or_else(|| candidates.iter().max_by(|a, b| a.refresh_rate().total_cmp(&b.refresh_rate())))
            else {
                bail!("no mode {to} on screen {}", self.id);
            };
            apply(self.id, mode)
        }

        pub fn original(&self) -> Result<Original> {
            Ok(Original { id: self.id, mode: self.current_mode()? })
        }
    }

    /// For this program only: macOS puts the screen back when the agent ends.
    fn apply(id: u32, mode: &CGDisplayMode) -> Result<()> {
        let display = CGDisplay::new(id);
        let config =
            display.begin_configuration().map_err(|e| anyhow!("CGBeginDisplayConfiguration: {e}"))?;
        if let Err(e) = display.configure_display_with_display_mode(&config, mode) {
            let _ = display.cancel_configuration(&config);
            bail!("CGConfigureDisplayWithDisplayMode: {e}");
        }
        display
            .complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
            .map_err(|e| anyhow!("CGCompleteDisplayConfiguration: {e}"))
    }

    pub struct Original {
        id: u32,
        mode: CGDisplayMode,
    }

    // SAFETY: an immutable CoreFoundation object, retained.
    unsafe impl Send for Original {}

    impl Original {
        pub fn size(&self) -> Size {
            pixels(&self.mode)
        }

        pub fn record(&self) -> String {
            String::new()
        }

        pub fn put_back(&self) -> Result<()> {
            apply(self.id, &self.mode)
        }
    }

    /// Nothing to do: macOS restored the screens when the previous agent ended.
    pub fn recover(_record: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use anyhow::{Context, Result, bail};
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    use super::Size;
    use crate::Display;

    /// The one screen turned on, through XRandR.
    struct Output {
        conn: RustConnection,
        root: u32,
        name: String,
        crtc: randr::Crtc,
        config_timestamp: u32,
        /// The output's modes: id, size, refresh in mHz.
        modes: Vec<(randr::Mode, Size, u64)>,
    }

    fn open(name: Option<&str>) -> Result<Output> {
        let (conn, screen) = x11rb::connect(None).context("X11 connection")?;
        let root = conn.setup().roots[screen].root;
        conn.randr_query_version(1, 3)?.reply().context("@agent.resolution_unavailable")?;
        let res = conn.randr_get_screen_resources_current(root)?.reply()?;
        let mut on = Vec::new();
        for &output in &res.outputs {
            let info = conn.randr_get_output_info(output, res.config_timestamp)?.reply()?;
            if info.crtc != 0 {
                on.push((output, info));
            }
        }
        let (output, info) = match name {
            Some(name) => {
                on.into_iter().find(|(_, i)| i.name == name.as_bytes()).context("the screen is gone")?
            }
            None if on.len() == 1 => on.remove(0),
            None if on.is_empty() => bail!("@agent.resolution_unavailable"),
            None => bail!("@agent.resolution_several_screens"),
        };
        let modes = info
            .modes
            .iter()
            .filter_map(|id| res.modes.iter().find(|m| m.id == *id))
            .map(|m| {
                let total = m.htotal as u64 * m.vtotal as u64;
                let refresh = if total == 0 { 0 } else { m.dot_clock as u64 * 1000 / total };
                (m.id, Size { width: m.width.into(), height: m.height.into() }, refresh)
            })
            .collect();
        Ok(Output {
            name: String::from_utf8_lossy(&info.name).into_owned(),
            crtc: info.crtc,
            config_timestamp: res.config_timestamp,

            modes,
            root,
            conn,
        })
    }

    impl Output {
        fn current(&self) -> Result<(randr::GetCrtcInfoReply, Size)> {
            let crtc = self.conn.randr_get_crtc_info(self.crtc, self.config_timestamp)?.reply()?;
            let size = Size { width: crtc.width.into(), height: crtc.height.into() };
            Ok((crtc, size))
        }

        fn set(&self, to: Size) -> Result<()> {
            let (crtc, _) = self.current()?;
            if crtc.rotation != randr::Rotation::ROTATE0 {
                bail!("@agent.resolution_unavailable");
            }
            let current_refresh =
                self.modes.iter().find(|(id, ..)| *id == crtc.mode).map(|(.., r)| *r).unwrap_or(0);
            let candidates: Vec<_> = self.modes.iter().filter(|(_, s, _)| *s == to).collect();
            let Some((mode, ..)) =
                candidates.iter().min_by_key(|(.., r)| r.abs_diff(current_refresh)).copied()
            else {
                bail!("no mode {to} on {}", self.name);
            };
            let screen = self.conn.setup().roots.iter().find(|s| s.root == self.root).context("X screen")?;
            let (sw, sh) = (screen.width_in_pixels as u32, screen.height_in_pixels as u32);
            let (mmw, mmh) = (screen.width_in_millimeters as u32, screen.height_in_millimeters as u32);
            // The screen must hold the CRTC at every step (as xrandr does): grown first,
            // shrunk after.
            let (nw, nh) = (crtc.x as u32 + to.width, crtc.y as u32 + to.height);
            let (bw, bh) = (sw.max(nw), sh.max(nh));
            let mm =
                |px: u32, of_px: u32, of_mm: u32| (px as u64 * of_mm as u64 / of_px.max(1) as u64) as u32;
            let resize = |w: u32, h: u32| -> Result<()> {
                self.conn
                    .randr_set_screen_size(self.root, w as u16, h as u16, mm(w, sw, mmw), mm(h, sh, mmh))?
                    .check()
                    .context("RRSetScreenSize")
            };
            self.conn.grab_server()?;
            let result = (|| {
                if (bw, bh) != (sw, sh) {
                    resize(bw, bh)?;
                }
                let reply = self
                    .conn
                    .randr_set_crtc_config(
                        self.crtc,
                        x11rb::CURRENT_TIME,
                        self.config_timestamp,
                        crtc.x,
                        crtc.y,
                        *mode,
                        crtc.rotation,
                        &crtc.outputs,
                    )?
                    .reply()?;
                if reply.status != randr::SetConfig::SUCCESS {
                    bail!("RRSetCrtcConfig: {:?}", reply.status);
                }
                if (nw, nh) != (bw, bh) {
                    resize(nw, nh)?;
                }
                Ok(())
            })();
            let _ = self.conn.ungrab_server();
            let _ = self.conn.flush();
            result
        }
    }

    pub struct Screen {
        output: Output,
    }

    impl Screen {
        pub fn of(_display: &Display) -> Result<Self> {
            // One display on X11: the whole screen, with one output turned on.
            Ok(Self { output: open(None)? })
        }

        pub fn key(&self) -> String {
            self.output.name.clone()
        }

        pub fn current(&self) -> Result<Size> {
            Ok(self.output.current()?.1)
        }

        pub fn modes(&self) -> Result<Vec<Size>> {
            let mut sizes: Vec<Size> = self.output.modes.iter().map(|(_, s, _)| *s).collect();
            sizes.sort_by_key(|s| (s.width, s.height));
            sizes.dedup();
            Ok(sizes)
        }

        pub fn set(&self, to: Size) -> Result<()> {
            self.output.set(to)
        }

        pub fn original(&self) -> Result<Original> {
            Ok(Original { name: self.output.name.clone(), size: self.current()? })
        }
    }

    pub struct Original {
        name: String,
        size: Size,
    }

    impl Original {
        pub fn size(&self) -> Size {
            self.size
        }

        /// Output name and size: "Virtual-1 1920 1080".
        pub fn record(&self) -> String {
            format!("{} {} {}", self.name, self.size.width, self.size.height)
        }

        pub fn put_back(&self) -> Result<()> {
            open(Some(&self.name))?.set(self.size)
        }
    }

    pub fn recover(record: &str) -> Result<()> {
        let mut parts = record.rsplitn(3, ' ');
        let (Some(h), Some(w), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("unreadable line");
        };
        let size = Size { width: w.parse()?, height: h.parse()? };
        let output = open(Some(name))?;
        if output.current()?.1 != size {
            output.set(size)?;
        }
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod platform {
    use anyhow::{Result, bail};

    use super::Size;
    use crate::Display;

    pub struct Screen;

    impl Screen {
        pub fn of(_display: &Display) -> Result<Self> {
            bail!("@agent.resolution_unavailable")
        }
        pub fn key(&self) -> String {
            String::new()
        }
        pub fn current(&self) -> Result<Size> {
            bail!("unsupported")
        }
        pub fn modes(&self) -> Result<Vec<Size>> {
            bail!("unsupported")
        }
        pub fn set(&self, _to: Size) -> Result<()> {
            bail!("unsupported")
        }
        pub fn original(&self) -> Result<Original> {
            bail!("unsupported")
        }
    }

    pub struct Original;

    impl Original {
        pub fn size(&self) -> Size {
            Size { width: 0, height: 0 }
        }
        pub fn record(&self) -> String {
            String::new()
        }
        pub fn put_back(&self) -> Result<()> {
            Ok(())
        }
    }

    pub fn recover(_record: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(width: u32, height: u32) -> Size {
        Size { width, height }
    }

    const MODES: [Size; 7] = [
        Size { width: 800, height: 600 },
        Size { width: 1024, height: 768 },
        Size { width: 1280, height: 720 },
        Size { width: 1280, height: 1024 },
        Size { width: 1366, height: 768 },
        Size { width: 1600, height: 900 },
        Size { width: 1920, height: 1080 },
    ];

    #[test]
    fn fills_the_area_best() {
        assert_eq!(choose(&MODES, s(1920, 1080)), Some(s(1920, 1080)));
        assert_eq!(choose(&MODES, s(2560, 1400)), Some(s(1920, 1080)));
        // A maximised window on a 1080p laptop: its own mode, shrunk by 6%.
        assert_eq!(choose(&MODES, s(1918, 1017)), Some(s(1920, 1080)));
        assert_eq!(choose(&MODES, s(1300, 1050)), Some(s(1280, 1024)));
        // Mint's virtual screen, a 1280x720 window: just larger rather than 800x600.
        let mint = [s(800, 600), s(1024, 768), s(1280, 768), s(1280, 800), s(1920, 1080)];
        assert_eq!(choose(&mint, s(1280, 720)), Some(s(1280, 768)));
        // PC-LABORATORIO's monitor, a 1100x728 window.
        let lab = [s(800, 600), s(1024, 768), s(1152, 864), s(1280, 720), s(1280, 1024)];
        assert_eq!(choose(&lab, s(1100, 728)), Some(s(1024, 768)));
    }

    #[test]
    fn exact_fit_wins() {
        assert_eq!(choose(&[s(1366, 768), s(1280, 720)], s(1280, 720)), Some(s(1280, 720)));
    }

    #[test]
    fn closest_shape_at_equal_size() {
        let modes = [s(1280, 800), s(1600, 640), s(1024, 1000)];
        // All the same area: the 16:10 window takes the 16:10 mode.
        assert_eq!(choose(&modes, s(1700, 1100)), Some(s(1280, 800)));
    }

    #[test]
    fn smallest_when_none_fits() {
        assert_eq!(choose(&MODES, s(500, 400)), Some(s(800, 600)));
        let tiny = [s(320, 200), s(640, 480), s(800, 600), s(1024, 768)];
        assert_eq!(choose(&tiny, s(700, 500)), Some(s(800, 600)));
        assert_eq!(choose(&tiny[..2], s(500, 400)), Some(s(320, 200)));
        assert_eq!(choose(&[], s(500, 400)), None);
    }
}
