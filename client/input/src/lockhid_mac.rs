//! Keys for the macOS lock screen: while the person's screen is locked the
//! keyboard events go as HID reports to `nuvira service hid-run` (root), which
//! types them with a virtual HID keyboard (the lock screen drops synthetic events;
//! see nuvira_service::hid). The reports are those of Karabiner's driver: report
//! id 1, modifiers, reserved, 32 keys as HID usages (u16, little endian).

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

use tracing::{debug, info};

const SOCKET: &str = "/var/run/nuvira-hid.sock";
const REPORT_SIZE: usize = 67;

struct State {
    stream: Option<UnixStream>,
    modifiers: u8,
    keys: BTreeSet<u16>,
}

static STATE: Mutex<State> = Mutex::new(State { stream: None, modifiers: 0, keys: BTreeSet::new() });

/// The service that types into the lock screen is installed.
pub fn available() -> bool {
    std::path::Path::new(SOCKET).exists()
}

/// The person's screen is locked (`CGSSessionScreenIsLocked`).
pub fn screen_locked() -> bool {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    }
    // SAFETY: no arguments; the result is owned (+1) or NULL.
    let dict = unsafe { CGSessionCopyCurrentDictionary() };
    if dict.is_null() {
        return false;
    }
    // SAFETY: a dictionary just copied, released by the wrapper.
    let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_create_rule(dict) };
    dict.find(CFString::from_static_string("CGSSessionScreenIsLocked"))
        .and_then(|v| v.downcast::<CFBoolean>())
        .is_some_and(bool::from)
}

/// Bit of a modifier key in the report.
fn modifier_bit(code: &str) -> Option<u8> {
    Some(match code {
        "ControlLeft" => 0x01,
        "ShiftLeft" => 0x02,
        "AltLeft" => 0x04,
        "MetaLeft" => 0x08,
        "ControlRight" => 0x10,
        "ShiftRight" => 0x20,
        "AltRight" => 0x40,
        "MetaRight" => 0x80,
        _ => return None,
    })
}

/// HID usage (keyboard page) of the key at this position (W3C code).
pub fn hid_usage(code: &str) -> Option<u16> {
    if let Some(letter) = code.strip_prefix("Key")
        && letter.len() == 1
        && let Some(c) = letter.chars().next().filter(char::is_ascii_uppercase)
    {
        return Some(4 + (c as u16 - 'A' as u16));
    }
    if let Some(digit) = code.strip_prefix("Digit")
        && let Ok(d) = digit.parse::<u16>()
        && d <= 9
    {
        return Some(if d == 0 { 39 } else { 29 + d });
    }
    if let Some(n) = code.strip_prefix('F')
        && let Ok(n) = n.parse::<u16>()
        && (1..=12).contains(&n)
    {
        return Some(57 + n);
    }
    Some(match code {
        "Enter" => 40,
        "Escape" => 41,
        "Backspace" => 42,
        "Tab" => 43,
        "Space" => 44,
        "Minus" => 45,
        "Equal" => 46,
        "BracketLeft" => 47,
        "BracketRight" => 48,
        "Backslash" => 49,
        "Semicolon" => 51,
        "Quote" => 52,
        "Backquote" => 53,
        "Comma" => 54,
        "Period" => 55,
        "Slash" => 56,
        "CapsLock" => 57,
        "Home" => 74,
        "PageUp" => 75,
        "Delete" => 76,
        "End" => 77,
        "PageDown" => 78,
        "ArrowRight" => 79,
        "ArrowLeft" => 80,
        "ArrowDown" => 81,
        "ArrowUp" => 82,
        "NumpadEnter" => 88,
        "IntlBackslash" => 100,
        _ => return None,
    })
}

fn report(state: &State) -> [u8; REPORT_SIZE] {
    let mut out = [0u8; REPORT_SIZE];
    out[0] = 1;
    out[1] = state.modifiers;
    for (i, key) in state.keys.iter().take(32).enumerate() {
        out[3 + i * 2..5 + i * 2].copy_from_slice(&key.to_le_bytes());
    }
    out
}

fn send(state: &mut State) -> bool {
    if state.stream.is_none() {
        match UnixStream::connect(SOCKET) {
            Ok(s) => {
                info!("typing into the lock screen through the virtual HID keyboard");
                state.stream = Some(s);
            }
            Err(e) => {
                debug!("virtual HID keyboard service not reachable: {e}");
                return false;
            }
        }
    }
    let data = report(state);
    if let Some(s) = state.stream.as_mut()
        && s.write_all(&data).is_err()
    {
        state.stream = None;
        return false;
    }
    true
}

/// The screen just locked: the service is reached now, so that it opens the
/// virtual keyboard before the first key.
pub fn prepare() {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.stream.is_none() && available() {
        state.modifiers = 0;
        state.keys.clear();
        send(&mut state);
    }
}

/// A key of the viewer while the screen is locked. False when it could not be
/// typed this way (the service is missing: the caller posts it as usual).
pub fn key(code: &str, down: bool) -> bool {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(bit) = modifier_bit(code) {
        if down {
            state.modifiers |= bit;
        } else {
            state.modifiers &= !bit;
        }
    } else if let Some(usage) = hid_usage(code) {
        if down {
            state.keys.insert(usage);
        } else {
            state.keys.remove(&usage);
        }
    } else {
        debug!("key without a HID usage: {code}");
        return true;
    }
    send(&mut state)
}

/// Releases what is held (end of the session, screen unlocked).
pub fn release_all() {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.stream.is_none() || (state.modifiers == 0 && state.keys.is_empty()) {
        return;
    }
    state.modifiers = 0;
    state.keys.clear();
    send(&mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usages_of_the_keyboard_page() {
        assert_eq!(hid_usage("KeyA"), Some(4));
        assert_eq!(hid_usage("KeyZ"), Some(29));
        assert_eq!(hid_usage("Digit1"), Some(30));
        assert_eq!(hid_usage("Digit0"), Some(39));
        assert_eq!(hid_usage("Enter"), Some(40));
        assert_eq!(hid_usage("F12"), Some(69));
        assert_eq!(hid_usage("Keyboard"), None);
    }

    #[test]
    fn report_layout() {
        let state = State { stream: None, modifiers: 0x02, keys: BTreeSet::from([25u16]) };
        let r = report(&state);
        assert_eq!(&r[..5], &[1, 0x02, 0, 25, 0]);
        assert!(r[5..].iter().all(|&b| b == 0));
    }
}
