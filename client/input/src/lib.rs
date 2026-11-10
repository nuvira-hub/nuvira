//! Injection of the input received from the viewer.
//!
//! - Windows: SendInput (`windows.rs`). The secure desktop (UAC, sign-in) needs
//!   the system service (nuvira-service).
//! - macOS: CGEvent; Linux: RemoteDesktop portal / XTest (phase 4).

use nuvira_proto::v1::InputEvent;

pub mod keys;
pub mod privacy;
#[cfg(target_os = "linux")]
#[path = "local_input_linux.rs"]
pub mod local_input;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod keymap_wayland;
#[cfg(target_os = "linux")]
mod ibus_watch;
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

/// Most characters typed from one [`nuvira_proto::v1::Text`]: a longer one is cut.
pub const MAX_TEXT_CHARS: usize = 10_000;

/// A piece of a [`nuvira_proto::v1::Text`]: a character to type as it is, or the
/// key of one of its control characters (W3C code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    Char(char),
    Key(&'static str),
}

/// The pieces of a text: "\n" or "\r\n" is Enter, "\t" Tab, "\u{8}" Backspace;
/// other control characters are left out.
pub fn pieces(text: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut chars = text.chars().take(MAX_TEXT_CHARS).peekable();
    while let Some(c) = chars.next() {
        out.push(match c {
            '\r' => {
                chars.next_if_eq(&'\n');
                Piece::Key("Enter")
            }
            '\n' => Piece::Key("Enter"),
            '\t' => Piece::Key("Tab"),
            '\u{8}' => Piece::Key("Backspace"),
            c if c.is_control() => continue,
            c => Piece::Char(c),
        });
    }
    out
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

#[cfg(test)]
mod tests {
    use super::{Piece, pieces};

    #[test]
    fn text_pieces() {
        assert_eq!(
            pieces("è€\r\nA\t\u{8}\u{7}\n😀"),
            [
                Piece::Char('è'),
                Piece::Char('€'),
                Piece::Key("Enter"),
                Piece::Char('A'),
                Piece::Key("Tab"),
                Piece::Key("Backspace"),
                Piece::Key("Enter"),
                Piece::Char('😀'),
            ]
        );
    }
}
