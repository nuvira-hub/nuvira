//! Wayland input: mouse and keyboard through the remote desktop portal, in the
//! same session that shares the screen (opened by the capture).

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use nuvira_portal::WheelAxis;
use nuvira_proto::v1::input_event::Event;
use nuvira_proto::v1::{Button, InputEvent};
use tracing::debug;

use crate::{InputInjector, ScreenRect, keys};

/// Units of one wheel notch in [`nuvira_proto::v1::MouseWheel`].
const NOTCH: i32 = 120;

pub struct PortalInjector {
    session: Arc<nuvira_portal::Session>,
    keys: HashSet<u16>,
    buttons: HashSet<i32>,
    /// Wheel movement smaller than a notch, kept for the next event.
    wheel: (i32, i32),
}

impl PortalInjector {
    pub fn new() -> Result<Self> {
        let session = nuvira_portal::current().context("no remote desktop session: the screen is not shared")?;
        Ok(Self { session, keys: HashSet::new(), buttons: HashSet::new(), wheel: (0, 0) })
    }

    fn key(&mut self, code: &str, down: bool) {
        let Some(keycode) = keys::evdev_code(code) else {
            debug!("key without a Linux code: {code}");
            return;
        };
        if down {
            self.keys.insert(keycode);
        } else if !self.keys.remove(&keycode) {
            return;
        }
        self.session.key(keycode.into(), down);
    }

    fn button(&mut self, button: Button, down: bool) {
        // BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA.
        let code = match button {
            Button::Left => 0x110,
            Button::Right => 0x111,
            Button::Middle => 0x112,
            Button::Back => 0x113,
            Button::Forward => 0x114,
            Button::Unspecified => return,
        };
        if down {
            self.buttons.insert(code);
        } else if !self.buttons.remove(&code) {
            return;
        }
        self.session.pointer_button(code, down);
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        self.wheel.0 += dx;
        self.wheel.1 += dy;
        let (steps_x, steps_y) = (self.wheel.0 / NOTCH, self.wheel.1 / NOTCH);
        self.wheel.0 -= steps_x * NOTCH;
        self.wheel.1 -= steps_y * NOTCH;
        // Windows' wheel is positive upwards, Linux's downwards.
        if steps_y != 0 {
            self.session.wheel(WheelAxis::Vertical, -steps_y);
        }
        if steps_x != 0 {
            self.session.wheel(WheelAxis::Horizontal, steps_x);
        }
    }
}

impl InputInjector for PortalInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match &event.event {
            Some(Event::MouseMove(m)) => {
                // Logical coordinates of the shared screen, as the portal wants them.
                let screen = self.session.screen();
                let x = f64::from(m.x.clamp(0.0, 1.0)) * f64::from(screen.width.saturating_sub(1));
                let y = f64::from(m.y.clamp(0.0, 1.0)) * f64::from(screen.height.saturating_sub(1));
                self.session.pointer_motion(x, y);
            }
            Some(Event::MouseButton(b)) => self.button(b.button(), b.down),
            Some(Event::MouseWheel(w)) => self.wheel(w.dx, w.dy),
            Some(Event::Key(k)) => self.key(&k.code, k.down),
            // The portal has no Ctrl+Alt+Del; it would mean nothing on Linux anyway.
            Some(Event::SecureAttention(_)) => debug!("Ctrl+Alt+Del ignored on Linux"),
            None => {}
        }
        Ok(())
    }

    fn set_screen(&mut self, _screen: ScreenRect) {
        // There is one shared screen, the portal's.
    }

    fn release_all(&mut self) {
        for keycode in std::mem::take(&mut self.keys) {
            self.session.key(keycode.into(), false);
        }
        for button in std::mem::take(&mut self.buttons) {
            self.session.pointer_button(button, false);
        }
        self.wheel = (0, 0);
    }
}

impl Drop for PortalInjector {
    fn drop(&mut self) {
        self.release_all();
    }
}
