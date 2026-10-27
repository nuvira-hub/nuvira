//! Injection of the input received from the viewer.
//!
//! - Windows: SendInput (`windows.rs`). The secure desktop (UAC, sign-in) needs
//!   the system service (nexodesk-service).
//! - macOS: CGEvent; Linux: RemoteDesktop portal / XTest (phase 4).

use nexodesk_proto::v1::InputEvent;

pub mod keys;
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
    #[cfg(not(windows))]
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
