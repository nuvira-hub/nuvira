//! Screen capture.
//!
//! Each system has its own implementation behind the [`Capturer`] trait:
//! - Windows: DXGI Desktop Duplication (`windows.rs`);
//! - macOS: ScreenCaptureKit (`macos.rs`);
//! - Linux on Wayland: the remote desktop portal and PipeWire (`linux.rs`);
//! - Linux on X11: `GetImage` of the root window (`x11.rs`).

use std::time::Duration;

mod all;
#[cfg(windows)]
pub mod desktop;
#[cfg(windows)]
mod gdi;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod synthetic;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

pub use all::{ALL_DISPLAYS, AllCapturer};
pub use synthetic::SyntheticCapturer;

/// A screen connected to the system.
#[derive(Debug, Clone)]
pub struct Display {
    pub index: usize,
    pub name: String,
    /// Position in the virtual desktop, in physical pixels.
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Captured image in 8-bit BGRA format.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (may be larger than `width * 4`).
    pub stride: usize,
    pub data: Vec<u8>,
    /// System clock in microseconds at capture time.
    pub captured_us: u64,
    /// GPU mode (Windows): the image is in this BGRA texture and `data` is empty.
    #[cfg(windows)]
    pub texture: Option<::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
}

pub trait Capturer: Send {
    fn display(&self) -> &Display;

    /// Waits up to `timeout` for a new image. `None` if the screen did not change.
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<&Frame>>;

    /// Last captured image (to encode it again when a keyframe is needed on a still screen).
    fn last_frame(&self) -> Option<&Frame>;

    /// Switches to GPU mode: frames stay in Direct3D 11 textures on this device,
    /// to be shared with the hardware encoder. `None` if the source does not support it.
    #[cfg(windows)]
    fn enable_gpu_output(&mut self) -> Option<::windows::Win32::Graphics::Direct3D11::ID3D11Device> {
        None
    }
}

/// List of the available screens.
pub fn displays() -> anyhow::Result<Vec<Display>> {
    #[cfg(windows)]
    return match windows::displays() {
        Ok(list) if !list.is_empty() && !force_gdi() => Ok(list),
        // Without DXGI (some Remote Desktop sessions), as GDI sees them.
        _ => Ok(gdi::displays()),
    };
    #[cfg(target_os = "linux")]
    return if wayland() { Ok(linux::displays()) } else { x11::displays() };
    #[cfg(target_os = "macos")]
    return macos::displays();
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    anyhow::bail!("screen capture is not available on this system yet");
}

/// Opens capture of the given screen (the primary one if `None`, all of them with
/// [`ALL_DISPLAYS`]).
pub fn open(index: Option<usize>) -> anyhow::Result<Box<dyn Capturer>> {
    #[cfg(windows)]
    {
        if index == Some(ALL_DISPLAYS) {
            let screens = displays()?;
            return Ok(Box::new(AllCapturer::new(|i| open_one(Some(i)), &screens)?));
        }
        open_one(index)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = index;
        if wayland() { linux::open() } else { x11::open() }
    }
    #[cfg(target_os = "macos")]
    return macos::open(index);
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = index;
        anyhow::bail!("screen capture is not available on this system yet");
    }
}

/// A Wayland session (capture through the portal); otherwise X11.
/// The windows of process `pid` are left out of the capture from now on (privacy
/// mode on macOS: the black screens are another process). Waits a little for
/// the running capture to apply it. Other systems: nothing to do.
pub fn hide_process(pid: u32) {
    #[cfg(target_os = "macos")]
    macos::hide_process(pid);
    #[cfg(not(target_os = "macos"))]
    let _ = pid;
}

/// The screen of the person's session is locked and its image cannot be captured
/// (macOS, GNOME): the capture sends a placeholder meanwhile.
pub fn screen_locked() -> bool {
    #[cfg(target_os = "macos")]
    return macos::screen_locked();
    #[cfg(target_os = "linux")]
    return linux::screen_locked();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    false
}

/// While [`screen_locked`], the keyboard and mouse still reach the lock screen.
/// Nowhere so far: macOS 27's lock screen drops every synthetic event (any API,
/// also as root or as a pre-login app), GNOME closes the input with the screen.
pub fn lock_screen_takes_input() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// `NUVIRA_CAPTURE=gdi`: GDI capture even where DXGI works (tests).
#[cfg(windows)]
fn force_gdi() -> bool {
    std::env::var("NUVIRA_CAPTURE").is_ok_and(|v| v.eq_ignore_ascii_case("gdi"))
}

/// DXGI Desktop Duplication, or GDI where it is not available (some Remote Desktop
/// sessions, virtual machines).
#[cfg(windows)]
fn open_one(index: Option<usize>) -> anyhow::Result<Box<dyn Capturer>> {
    if !force_gdi() {
        match windows::DxgiCapturer::new(index) {
            Ok(capturer) => return Ok(Box::new(capturer)),
            Err(e) => tracing::info!("DXGI capture not available ({e:#}): GDI capture"),
        }
    }
    let screens = gdi::displays();
    let display = match index {
        Some(i) => screens.into_iter().find(|d| d.index == i),
        None => screens.into_iter().find(|d| d.primary),
    }
    .ok_or_else(|| anyhow::anyhow!("screen not found"))?;
    Ok(Box::new(gdi::GdiCapturer::new(display)?))
}
