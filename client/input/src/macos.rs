//! macOS input: mouse and keyboard as Quartz events (`CGEventPost`). macOS
//! accepts them only once the person allowed NuviraHub in System Settings →
//! Privacy & Security → Accessibility.

use std::collections::HashSet;

use anyhow::{Result, anyhow};
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use nuvira_proto::v1::input_event::Event;
use nuvira_proto::v1::{Button, InputEvent};
use tracing::debug;

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
}

// CGEventSource is made for every event: it is not Send.
impl QuartzInjector {
    pub fn new(screen: ScreenRect) -> Result<Self> {
        let _ = source()?;
        Ok(Self { screen: points(screen), keys: HashSet::new(), buttons: HashSet::new(), position: CGPoint::new(0.0, 0.0), wheel: (0, 0) })
    }

    fn post(event: Result<CGEvent, ()>) {
        match event {
            Ok(event) => event.post(CGEventTapLocation::HID),
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
        let Ok(src) = source() else { return };
        Self::post(CGEvent::new_mouse_event(src, kind, self.position, button));
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
        let Ok(src) = source() else { return };
        let event = CGEvent::new_mouse_event(src, kind, self.position, cg);
        if let Ok(event) = &event {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, i64::from(number));
        }
        Self::post(event);
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
        let Ok(src) = source() else { return };
        Self::post(CGEvent::new_scroll_event(src, ScrollEventUnit::LINE, 2, steps_y, steps_x, 0));
    }

    fn key(&mut self, code: &str, down: bool) {
        let Some(keycode) = mac_key_code(code) else {
            debug!("key without a macOS code: {code}");
            return;
        };
        if down {
            self.keys.insert(keycode);
        } else if !self.keys.remove(&keycode) {
            return;
        }
        let Ok(src) = source() else { return };
        Self::post(CGEvent::new_keyboard_event(src, keycode, down));
    }
}

fn source() -> Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|()| anyhow!("Quartz event source not available"))
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
        for keycode in std::mem::take(&mut self.keys) {
            if let Ok(src) = source() {
                Self::post(CGEvent::new_keyboard_event(src, keycode, false));
            }
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
    use super::mac_key_code;

    #[test]
    fn mac_codes() {
        assert_eq!(mac_key_code("KeyA"), Some(0x00));
        assert_eq!(mac_key_code("KeyC"), Some(0x08));
        assert_eq!(mac_key_code("MetaLeft"), Some(0x37));
        assert_eq!(mac_key_code("ArrowUp"), Some(0x7E));
        assert_eq!(mac_key_code("Sconosciuto"), None);
    }
}
