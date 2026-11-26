//! Keys towards the remote computer: W3C names of winit keys, the Cmd/Ctrl swap
//! on a Mac, the AltGr fix from Windows and the characters of `--type-keys`.

use std::time::{Duration, Instant};

/// `--type-keys`: the W3C code of a test character's key on a US layout and the
/// modifier held with it (Shift for capitals, Ctrl for the control characters of
/// Ctrl+A, C and V).
pub(crate) fn test_key_code(c: char) -> Option<(String, Option<&'static str>)> {
    Some(match c {
        'a'..='z' => (format!("Key{}", c.to_ascii_uppercase()), None),
        'A'..='Z' => (format!("Key{c}"), Some("ShiftLeft")),
        '0'..='9' => (format!("Digit{c}"), None),
        ' ' => ("Space".into(), None),
        '\n' => ("Enter".into(), None),
        '\u{8}' => ("Backspace".into(), None),
        '\u{1}' => ("KeyA".into(), Some("ControlLeft")),
        '\u{3}' => ("KeyC".into(), Some("ControlLeft")),
        '\u{16}' => ("KeyV".into(), Some("ControlLeft")),
        _ => return None,
    })
}

/// W3C name (`KeyboardEvent.code`) of a winit key. The names are the same except
/// for the Windows/Cmd key, which winit calls "Super". From a Mac to a Windows or
/// Linux computer Cmd and Ctrl are swapped ([`mac_swap`]); from a Mac to a Mac
/// they stay what they are.
pub(crate) fn w3c_code(code: winit::keyboard::KeyCode, remote_mac: bool) -> String {
    let name = match format!("{code:?}").as_str() {
        "SuperLeft" => "MetaLeft".into(),
        "SuperRight" => "MetaRight".into(),
        other => other.to_string(),
    };
    if cfg!(target_os = "macos") && !remote_mac { mac_swap(name) } else { name }
}

/// From a Windows viewer towards a Mac or Linux, AltGr goes through [`AltGrFix`].
pub(crate) fn altgr_fix_needed(remote_windows: bool) -> bool {
    cfg!(windows) && !remote_windows
}

/// AltGr from Windows towards a Mac or Linux. Windows reports AltGr as a left Ctrl
/// followed at once by the right Alt. A Mac would get Ctrl+Option, which types
/// nothing, where Option alone types what AltGr types on Windows (`[`, `{`, `@`, `#` on
/// Italian layouts); Linux would get Ctrl with AltGr, which programs take as a shortcut
/// (Ctrl+@, Ctrl+[) and type nothing. winit drops that Ctrl itself when the right Alt
/// is already queued behind it, but not when something slows the keys down between
/// the two (a low-level keyboard hook, as PowerToys, AutoHotkey and KVM tools install:
/// with one of 5 ms per key, `AltGr+ò` typed nothing on Linux). When the right Alt
/// goes down within a few milliseconds of the left Ctrl, the Ctrl is let go on the
/// remote computer and its own release is dropped. Towards Windows nothing changes:
/// the right Alt is AltGr again there.
#[derive(Default)]
pub(crate) struct AltGrFix {
    ctrl_down: Option<Instant>,
    swallow_ctrl_up: bool,
}

impl AltGrFix {
    /// Windows sends the two together: well under this.
    const WINDOW: Duration = Duration::from_millis(30);

    /// The keys to send for this one.
    pub(crate) fn filter(&mut self, code: &str, down: bool, now: Instant) -> Vec<(String, bool)> {
        match (code, down) {
            ("ControlLeft", true) => {
                // Repeats of a held Ctrl keep the first time.
                if self.ctrl_down.is_none() && !self.swallow_ctrl_up {
                    self.ctrl_down = Some(now);
                }
                if self.swallow_ctrl_up {
                    return Vec::new();
                }
            }
            ("ControlLeft", false) => {
                self.ctrl_down = None;
                if std::mem::take(&mut self.swallow_ctrl_up) {
                    return Vec::new();
                }
            }
            ("AltRight", true) => {
                if let Some(at) = self.ctrl_down.take()
                    && now.duration_since(at) <= Self::WINDOW
                {
                    self.swallow_ctrl_up = true;
                    return vec![("ControlLeft".into(), false), ("AltRight".into(), true)];
                }
            }
            _ => self.ctrl_down = None,
        }
        vec![(code.to_string(), down)]
    }
}

/// On a Mac, Cmd does what Ctrl does on Windows and Linux (Cmd+C, Cmd+V, Cmd+Z):
/// towards the remote computer the two are swapped. Option is already Alt.
fn mac_swap(name: String) -> String {
    match name.as_str() {
        "MetaLeft" => "ControlLeft".into(),
        "MetaRight" => "ControlRight".into(),
        "ControlLeft" => "MetaLeft".into(),
        "ControlRight" => "MetaRight".into(),
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn super_becomes_meta() {
        assert_eq!(w3c_code(winit::keyboard::KeyCode::SuperLeft, false), "MetaLeft");
        assert_eq!(w3c_code(winit::keyboard::KeyCode::KeyA, false), "KeyA");
    }

    #[test]
    fn mac_to_mac_keeps_cmd() {
        assert_eq!(w3c_code(winit::keyboard::KeyCode::SuperLeft, true), "MetaLeft");
        assert_eq!(w3c_code(winit::keyboard::KeyCode::ControlLeft, true), "ControlLeft");
    }

    #[test]
    fn altgr_fix_from_windows_towards_mac_and_linux() {
        // Mac and Linux agents (VWR-10: Linux got Ctrl with AltGr and typed nothing).
        for os in ["macos", "linux"] {
            assert!(!net::remote_is_windows(os), "{os}");
            assert_eq!(altgr_fix_needed(net::remote_is_windows(os)), cfg!(windows), "{os}");
        }
        // Windows, and older agents that send no system (all Windows): left alone.
        for os in ["windows", ""] {
            assert!(net::remote_is_windows(os), "{os:?}");
            assert!(!altgr_fix_needed(net::remote_is_windows(os)), "{os:?}");
        }
    }

    #[test]
    fn altgr_towards_mac_is_option() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let mut f = AltGrFix::default();
        let k = |c: &str, d| vec![(c.to_string(), d)];
        // AltGr: Ctrl then right Alt together → Ctrl let go, its release dropped.
        assert_eq!(f.filter("ControlLeft", true, ms(0)), k("ControlLeft", true));
        assert_eq!(
            f.filter("AltRight", true, ms(1)),
            vec![("ControlLeft".to_string(), false), ("AltRight".to_string(), true)]
        );
        assert!(f.filter("ControlLeft", true, ms(500)).is_empty(), "repeat while AltGr is held");
        assert_eq!(f.filter("BracketLeft", true, ms(600)), k("BracketLeft", true));
        assert_eq!(f.filter("AltRight", false, ms(700)), k("AltRight", false));
        assert!(f.filter("ControlLeft", false, ms(700)).is_empty());
        // A real Ctrl, then the right Alt later: both go through.
        assert_eq!(f.filter("ControlLeft", true, ms(1000)), k("ControlLeft", true));
        assert_eq!(f.filter("AltRight", true, ms(1300)), k("AltRight", true));
        assert_eq!(f.filter("ControlLeft", false, ms(1400)), k("ControlLeft", false));
        // Ctrl+C is untouched.
        assert_eq!(f.filter("ControlLeft", true, ms(2000)), k("ControlLeft", true));
        assert_eq!(f.filter("KeyC", true, ms(2001)), k("KeyC", true));
        assert_eq!(f.filter("ControlLeft", false, ms(2100)), k("ControlLeft", false));
    }

    #[test]
    fn mac_cmd_is_ctrl() {
        assert_eq!(mac_swap("MetaLeft".into()), "ControlLeft");
        assert_eq!(mac_swap("ControlRight".into()), "MetaRight");
        assert_eq!(mac_swap("KeyC".into()), "KeyC");
    }
}
