//! macOS input: mouse and keyboard as Quartz events (`CGEventPost`). macOS
//! accepts them only once the person allowed NuviraHub in System Settings →
//! Privacy & Security → Accessibility.
//!
//! Keys are sent by position (the W3C code of the viewer's physical key): the
//! character is the one of the Mac's own layout at that position. Two details
//! macOS does not do by itself for synthetic events:
//! - the modifiers held (Shift, Control, Option, Command, Caps Lock) must be in
//!   the flags of every key and mouse event, otherwise many apps see "a" instead
//!   of "A" or "è" instead of "é", and Cmd+click is a plain click;
//! - on ISO keyboards (Italian, German, French…) the key left of 1 and the one
//!   left of Z have each other's virtual codes.

use std::collections::HashSet;

use anyhow::Result;
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField};
use core_graphics::geometry::CGPoint;
use nuvira_proto::v1::input_event::Event;
use nuvira_proto::v1::{Button, InputEvent};
use tracing::{debug, info, warn};

use crate::{InputInjector, ScreenRect};

/// Units of one wheel notch in [`nuvira_proto::v1::MouseWheel`].
const NOTCH: i32 = 120;

pub struct QuartzInjector {
    /// Position and size of the streamed screen, in points.
    screen: ScreenRect,
    keys: HashSet<u16>,
    buttons: HashSet<u8>,
    position: CGPoint,
    wheel: (i32, i32),
    /// The Mac's keyboard is ISO (see the module's notes).
    iso: bool,
}

impl QuartzInjector {
    pub fn new(screen: ScreenRect) -> Result<Self> {
        report_permission();
        let iso = iso_keyboard();
        debug!(iso, "Mac keyboard type");
        Ok(Self {
            screen: points(screen),
            keys: HashSet::new(),
            buttons: HashSet::new(),
            position: CGPoint::new(0.0, 0.0),
            wheel: (0, 0),
            iso,
        })
    }

    /// Posts the event with the modifiers held.
    fn post(&self, event: Result<CGEvent, ()>) {
        match event {
            Ok(event) => {
                event.set_flags(modifier_flags(&self.keys));
                // Marked as ours: privacy mode lets only these through.
                event.set_integer_value_field(crate::privacy::USER_DATA, crate::privacy::INJECTED_TAG);
                event.post(CGEventTapLocation::HID);
            }
            Err(()) => debug!("Quartz event not created"),
        }
    }

    fn move_to(&mut self, x: f32, y: f32) {
        let s = self.screen;
        self.position = CGPoint::new(
            f64::from(s.left) + f64::from(x.clamp(0.0, 1.0)) * f64::from(s.width.saturating_sub(1)),
            f64::from(s.top) + f64::from(y.clamp(0.0, 1.0)) * f64::from(s.height.saturating_sub(1)),
        );
        // While a button is held, macOS wants "dragged" events, or drags do not work.
        let (kind, button) = if self.buttons.contains(&0) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons.contains(&1) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if !self.buttons.is_empty() {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        self.post(events::mouse(kind, self.position, button));
    }

    fn button(&mut self, button: Button, down: bool) {
        // Quartz button numbers: 0 left, 1 right, 2 middle, 3 back, 4 forward.
        let (number, kind, cg) = match (button, down) {
            (Button::Left, true) => (0, CGEventType::LeftMouseDown, CGMouseButton::Left),
            (Button::Left, false) => (0, CGEventType::LeftMouseUp, CGMouseButton::Left),
            (Button::Right, true) => (1, CGEventType::RightMouseDown, CGMouseButton::Right),
            (Button::Right, false) => (1, CGEventType::RightMouseUp, CGMouseButton::Right),
            (Button::Middle, d) => (2, if d { CGEventType::OtherMouseDown } else { CGEventType::OtherMouseUp }, CGMouseButton::Center),
            (Button::Back, d) => (3, if d { CGEventType::OtherMouseDown } else { CGEventType::OtherMouseUp }, CGMouseButton::Center),
            (Button::Forward, d) => (4, if d { CGEventType::OtherMouseDown } else { CGEventType::OtherMouseUp }, CGMouseButton::Center),
            (Button::Unspecified, _) => return,
        };
        if down {
            self.buttons.insert(number);
        } else if !self.buttons.remove(&number) {
            return;
        }
        let event = events::mouse(kind, self.position, cg);
        if let Ok(event) = &event {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, i64::from(number));
        }
        self.post(event);
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        self.wheel.0 += dx;
        self.wheel.1 += dy;
        let (steps_x, steps_y) = (self.wheel.0 / NOTCH, self.wheel.1 / NOTCH);
        self.wheel.0 -= steps_x * NOTCH;
        self.wheel.1 -= steps_y * NOTCH;
        if steps_x == 0 && steps_y == 0 {
            return;
        }
        // Positive is up and right in both NuviraHub's units and Quartz lines.
        self.post(events::scroll(steps_y, steps_x));
    }

    fn key(&mut self, code: &str, down: bool) {
        let Some(keycode) = mac_key_code(code) else {
            debug!("key without a macOS code: {code}");
            return;
        };
        let keycode = if self.iso { iso_swap(keycode) } else { keycode };
        if down {
            // A repeat of a held key (auto-repeat of the viewer) is sent again.
            self.keys.insert(keycode);
        } else if !self.keys.remove(&keycode) {
            return;
        }
        self.post_key(keycode, down);
    }

    /// A modifier key changes the flags (`FlagsChanged`, as a real keyboard does);
    /// the others are key down/up events.
    fn post_key(&self, keycode: u16, down: bool) {
        let event = events::keyboard(keycode, down);
        if let Ok(event) = &event
            && modifier_flag(keycode).is_some()
        {
            event.set_type(CGEventType::FlagsChanged);
        }
        self.post(event);
    }
}

/// Without the Accessibility permission macOS drops the events without a word: said
/// in the log once per process, and asked for (the system adds NuviraHub to the
/// list, or shows its window where there is someone to answer it).
fn report_permission() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[link(name = "ApplicationServices", kind = "framework")]
        unsafe extern "C" {
            fn AXIsProcessTrusted() -> bool;
        }
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGPreflightPostEventAccess() -> bool;
            fn CGRequestPostEventAccess() -> bool;
        }
        // SAFETY: plain queries, no arguments.
        let (trusted, post) = unsafe { (AXIsProcessTrusted(), CGPreflightPostEventAccess()) };
        // SAFETY: as above.
        let uid = unsafe { libc::geteuid() };
        if trusted && post {
            info!(uid, "Accessibility permission: input allowed");
        } else {
            // SAFETY: as above.
            let asked = unsafe { CGRequestPostEventAccess() };
            warn!(uid, trusted, post, asked, "no Accessibility permission: macOS drops the input");
        }
    });
}

/// Flag of a modifier key, by virtual code.
fn modifier_flag(keycode: u16) -> Option<CGEventFlags> {
    Some(match keycode {
        0x38 | 0x3C => CGEventFlags::CGEventFlagShift,
        0x3B | 0x3E => CGEventFlags::CGEventFlagControl,
        0x3A | 0x3D => CGEventFlags::CGEventFlagAlternate,
        0x37 | 0x36 => CGEventFlags::CGEventFlagCommand,
        0x39 => CGEventFlags::CGEventFlagAlphaShift,
        _ => return None,
    })
}

/// Flags of the modifier keys held.
fn modifier_flags(keys: &HashSet<u16>) -> CGEventFlags {
    keys.iter().filter_map(|k| modifier_flag(*k)).fold(CGEventFlags::CGEventFlagNull, |a, b| a | b)
}

/// ISO keyboards: the key left of 1 is `kVK_ISO_Section` and the one left of Z
/// is `kVK_ANSI_Grave`, the other way round from their names.
fn iso_swap(keycode: u16) -> u16 {
    match keycode {
        0x0A => 0x32,
        0x32 => 0x0A,
        other => other,
    }
}

/// The keyboard of this Mac is ISO (Carbon's `KBGetLayoutType`). A Mac without a
/// keyboard reports ANSI. `NUVIRA_MAC_KEYBOARD=iso|ansi` forces it.
fn iso_keyboard() -> bool {
    match std::env::var("NUVIRA_MAC_KEYBOARD").as_deref() {
        Ok("iso") => return true,
        Ok("ansi") => return false,
        _ => {}
    }
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn LMGetKbdType() -> u8;
        fn KBGetLayoutType(keyboard_type: i16) -> u32;
    }
    const KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");
    // SAFETY: plain queries of the keyboard type, no arguments to keep alive.
    unsafe { KBGetLayoutType(i16::from(LMGetKbdType())) == KEYBOARD_ISO }
}

/// Quartz events made without an event source (`NULL`). With a `HIDSystemState`
/// source, made for every event, creating the event never returned in the sign-in
/// screen's session (macOS 27: SkyLight's `CGSEventSourceForID` waiting on a lock),
/// and the agent's input stopped there. The modifier flags are set by
/// [`QuartzInjector::post`] anyway.
mod events {
    use core_graphics::event::{CGEvent, CGEventType, CGMouseButton};
    use core_graphics::geometry::CGPoint;
    use foreign_types::ForeignType;
    use std::ffi::c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> *mut c_void;
        fn CGEventCreateMouseEvent(source: *const c_void, kind: u32, at: CGPoint, button: u32) -> *mut c_void;
        fn CGEventCreateScrollWheelEvent2(
            source: *const c_void,
            units: u32,
            count: u32,
            wheel1: i32,
            wheel2: i32,
            wheel3: i32,
        ) -> *mut c_void;
    }

    fn wrap(event: *mut c_void) -> Result<CGEvent, ()> {
        if event.is_null() {
            return Err(());
        }
        // SAFETY: a CGEventRef just created (+1), owned by the wrapper from now on.
        Ok(unsafe { CGEvent::from_ptr(event.cast()) })
    }

    pub fn keyboard(key: u16, down: bool) -> Result<CGEvent, ()> {
        // SAFETY: a NULL source is allowed; plain values.
        wrap(unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), key, down) })
    }

    pub fn mouse(kind: CGEventType, at: CGPoint, button: CGMouseButton) -> Result<CGEvent, ()> {
        // SAFETY: as above.
        wrap(unsafe { CGEventCreateMouseEvent(std::ptr::null(), kind as u32, at, button as u32) })
    }

    /// Lines (`kCGScrollEventUnitLine`), vertical then horizontal.
    pub fn scroll(vertical: i32, horizontal: i32) -> Result<CGEvent, ()> {
        // SAFETY: as above.
        wrap(unsafe { CGEventCreateScrollWheelEvent2(std::ptr::null(), 1, 2, vertical, horizontal, 0) })
    }
}

/// The capture gives the screen in pixels; Quartz events are in points (half
/// as many on a Retina screen).
fn points(screen: ScreenRect) -> ScreenRect {
    let main = CGDisplay::main();
    let pixels = main.pixels_wide().max(1) as f64;
    let scale = (pixels / main.bounds().size.width.max(1.0)).max(1.0);
    ScreenRect {
        left: (f64::from(screen.left) / scale) as i32,
        top: (f64::from(screen.top) / scale) as i32,
        width: (f64::from(screen.width) / scale) as u32,
        height: (f64::from(screen.height) / scale) as u32,
    }
}

impl InputInjector for QuartzInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match &event.event {
            Some(Event::MouseMove(m)) => self.move_to(m.x, m.y),
            Some(Event::MouseButton(b)) => self.button(b.button(), b.down),
            Some(Event::MouseWheel(w)) => self.wheel(w.dx, w.dy),
            Some(Event::Key(k)) => self.key(&k.code, k.down),
            Some(Event::SecureAttention(_)) => debug!("Ctrl+Alt+Del ignored on macOS"),
            None => {}
        }
        Ok(())
    }

    fn set_screen(&mut self, screen: ScreenRect) {
        self.screen = points(screen);
    }

    fn release_all(&mut self) {
        // The other keys first, then the modifiers, each with the flags still held.
        let mut keys: Vec<u16> = self.keys.iter().copied().collect();
        keys.sort_by_key(|k| modifier_flag(*k).is_some());
        for keycode in keys {
            self.keys.remove(&keycode);
            self.post_key(keycode, false);
        }
        for button in [Button::Left, Button::Right, Button::Middle, Button::Back, Button::Forward] {
            self.button(button, false);
        }
        self.wheel = (0, 0);
    }
}

impl Drop for QuartzInjector {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// macOS virtual key code (`kVK_*` of Carbon's Events.h) of the W3C key.
pub fn mac_key_code(code: &str) -> Option<u16> {
    Some(match code {
        "KeyA" => 0x00,
        "KeyS" => 0x01,
        "KeyD" => 0x02,
        "KeyF" => 0x03,
        "KeyH" => 0x04,
        "KeyG" => 0x05,
        "KeyZ" => 0x06,
        "KeyX" => 0x07,
        "KeyC" => 0x08,
        "KeyV" => 0x09,
        "IntlBackslash" => 0x0A,
        "KeyB" => 0x0B,
        "KeyQ" => 0x0C,
        "KeyW" => 0x0D,
        "KeyE" => 0x0E,
        "KeyR" => 0x0F,
        "KeyY" => 0x10,
        "KeyT" => 0x11,
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit6" => 0x16,
        "Digit5" => 0x17,
        "Equal" => 0x18,
        "Digit9" => 0x19,
        "Digit7" => 0x1A,
        "Minus" => 0x1B,
        "Digit8" => 0x1C,
        "Digit0" => 0x1D,
        "BracketRight" => 0x1E,
        "KeyO" => 0x1F,
        "KeyU" => 0x20,
        "BracketLeft" => 0x21,
        "KeyI" => 0x22,
        "KeyP" => 0x23,
        "Enter" => 0x24,
        "KeyL" => 0x25,
        "KeyJ" => 0x26,
        "Quote" => 0x27,
        "KeyK" => 0x28,
        "Semicolon" => 0x29,
        "Backslash" => 0x2A,
        "Comma" => 0x2B,
        "Slash" => 0x2C,
        "KeyN" => 0x2D,
        "KeyM" => 0x2E,
        "Period" => 0x2F,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backquote" => 0x32,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "MetaRight" => 0x36,
        "MetaLeft" => 0x37,
        "ShiftLeft" => 0x38,
        "CapsLock" => 0x39,
        "AltLeft" => 0x3A,
        "ControlLeft" => 0x3B,
        "ShiftRight" => 0x3C,
        "AltRight" => 0x3D,
        "ControlRight" => 0x3E,
        "NumpadDecimal" => 0x41,
        "NumpadMultiply" => 0x43,
        "NumpadAdd" => 0x45,
        "NumLock" => 0x47,
        "NumpadDivide" => 0x4B,
        "NumpadEnter" => 0x4C,
        "NumpadSubtract" => 0x4E,
        "NumpadEqual" => 0x51,
        "Numpad0" => 0x52,
        "Numpad1" => 0x53,
        "Numpad2" => 0x54,
        "Numpad3" => 0x55,
        "Numpad4" => 0x56,
        "Numpad5" => 0x57,
        "Numpad6" => 0x58,
        "Numpad7" => 0x59,
        "Numpad8" => 0x5B,
        "Numpad9" => 0x5C,
        "IntlYen" => 0x5D,
        "IntlRo" => 0x5E,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F3" => 0x63,
        "F8" => 0x64,
        "F9" => 0x65,
        "F11" => 0x67,
        "F13" | "PrintScreen" => 0x69,
        "F14" => 0x6B,
        "F10" => 0x6D,
        "ContextMenu" => 0x6E,
        "F12" => 0x6F,
        "F15" => 0x71,
        "Insert" => 0x72,
        "Home" => 0x73,
        "PageUp" => 0x74,
        "Delete" => 0x75,
        "F4" => 0x76,
        "End" => 0x77,
        "F2" => 0x78,
        "PageDown" => 0x79,
        "F1" => 0x7A,
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "ArrowDown" => 0x7D,
        "ArrowUp" => 0x7E,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_codes() {
        assert_eq!(mac_key_code("KeyA"), Some(0x00));
        assert_eq!(mac_key_code("KeyC"), Some(0x08));
        assert_eq!(mac_key_code("MetaLeft"), Some(0x37));
        assert_eq!(mac_key_code("ArrowUp"), Some(0x7E));
        assert_eq!(mac_key_code("Sconosciuto"), None);
    }

    #[test]
    fn flags_of_held_modifiers() {
        let keys: HashSet<u16> = [0x38, 0x37, 0x00].into();
        assert_eq!(modifier_flags(&keys), CGEventFlags::CGEventFlagShift | CGEventFlags::CGEventFlagCommand);
        assert_eq!(modifier_flags(&HashSet::new()), CGEventFlags::CGEventFlagNull);
    }

    #[test]
    fn iso_keys_swapped() {
        assert_eq!(iso_swap(mac_key_code("Backquote").unwrap()), 0x0A);
        assert_eq!(iso_swap(mac_key_code("IntlBackslash").unwrap()), 0x32);
        assert_eq!(iso_swap(0x00), 0x00);
    }
}
