//! Injection of the input received from the viewer.
//!
//! - Windows: SendInput (`windows.rs`). The secure desktop (UAC, sign-in) needs
//!   the system service (nuvira-service).
//! - macOS: CGEvent; Linux: RemoteDesktop portal / XTest (phase 4).

use nuvira_proto::v1::InputEvent;

pub mod keys;
pub mod privacy;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
/// Keys typed into the macOS lock screen by a virtual HID keyboard.
#[cfg(target_os = "macos")]
#[path = "lockhid_mac.rs"]
pub mod lockhid;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(windows)]
mod windows;

/// Area of the transmitted screen in the virtual desktop, in physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

pub trait InputInjector: Send {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;

    /// Releases keys and buttons still pressed (end of session, lost connection).
    fn release_all(&mut self);

    /// The transmitted screen changed: mouse coordinates refer to this one.
    fn set_screen(&mut self, screen: ScreenRect);
}

/// Opens input injection on the given screen.
pub fn open(screen: ScreenRect) -> anyhow::Result<Box<dyn InputInjector>> {
    #[cfg(windows)]
    return Ok(Box::new(windows::SendInputInjector::new(screen)));
    #[cfg(target_os = "linux")]
    {
        let _ = screen;
        if wayland_session() {
            Ok(Box::new(linux::PortalInjector::new()?))
        } else {
            Ok(Box::new(x11::XTestInjector::new()?))
        }
    }
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos::QuartzInjector::new(screen)?));
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = screen;
        anyhow::bail!("remote input control is not available on this system yet");
    }
}

/// Makes the process per-monitor DPI aware: coordinates in physical pixels,
/// consistent with the capture ones. Call it at start-up, before creating windows.
pub fn set_dpi_awareness() {
    #[cfg(windows)]
    windows::set_dpi_awareness();
}

/// A Wayland session (input through the portal); otherwise X11.
#[cfg(target_os = "linux")]
pub(crate) fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}
