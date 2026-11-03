//! X11 input: mouse and keyboard through the XTest extension.

use std::collections::HashSet;

use anyhow::{Context, Result};
use nuvira_proto::v1::input_event::Event;
use nuvira_proto::v1::{Button, InputEvent};
use tracing::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::{InputInjector, ScreenRect, keys};

/// Units of one wheel notch in [`nuvira_proto::v1::MouseWheel`].
const NOTCH: i32 = 120;

/// X key codes are the Linux ones plus 8.
const KEYCODE_OFFSET: u16 = 8;

pub struct XTestInjector {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    keys: HashSet<u8>,
    buttons: HashSet<u8>,
    wheel: (i32, i32),
}

impl XTestInjector {
    pub fn new() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("X11 connection")?;
        conn.xtest_get_version(2, 2)?.reply().context("XTest extension not available")?;
        let root = &conn.setup().roots[screen];
        let (root, width, height) = (root.root, root.width_in_pixels, root.height_in_pixels);
        Ok(Self { conn, root, width, height, keys: HashSet::new(), buttons: HashSet::new(), wheel: (0, 0) })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        let sent = self.conn.xtest_fake_input(kind, detail, 0, self.root, x, y, 0);
        if let Err(e) = sent.map(|_| ()).and_then(|()| self.conn.flush()) {
            debug!("XTest: {e}");
        }
    }

    fn key(&mut self, code: &str, down: bool) {
        let Some(keycode) = keys::evdev_code(code).and_then(|c| u8::try_from(c + KEYCODE_OFFSET).ok()) else {
            debug!("key without an X code: {code}");
            return;
        };
        if down {
            self.keys.insert(keycode);
        } else if !self.keys.remove(&keycode) {
            return;
        }
        self.fake(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, keycode, 0, 0);
    }

    fn button(&mut self, button: Button, down: bool) {
        let code = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
            Button::Back => 8,
            Button::Forward => 9,
            Button::Unspecified => return,
        };
        if down {
            self.buttons.insert(code);
        } else if !self.buttons.remove(&code) {
            return;
        }
        self.fake(if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, code, 0, 0);
    }

    /// X has no wheel events: each notch is a click of buttons 4/5 (up/down)
    /// or 6/7 (left/right).
    fn wheel(&mut self, dx: i32, dy: i32) {
        self.wheel.0 += dx;
        self.wheel.1 += dy;
        let (steps_x, steps_y) = (self.wheel.0 / NOTCH, self.wheel.1 / NOTCH);
        self.wheel.0 -= steps_x * NOTCH;
        self.wheel.1 -= steps_y * NOTCH;
        // Positive is up, as Windows' wheel.
        let clicks = [(steps_y, 4, 5), (steps_x, 7, 6)];
        for (steps, positive, negative) in clicks {
            let button = if steps > 0 { positive } else { negative };
            for _ in 0..steps.unsigned_abs() {
                self.fake(BUTTON_PRESS_EVENT, button, 0, 0);
                self.fake(BUTTON_RELEASE_EVENT, button, 0, 0);
            }
        }
    }
}

impl InputInjector for XTestInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match &event.event {
            Some(Event::MouseMove(m)) => {
                let x = (m.x.clamp(0.0, 1.0) * f32::from(self.width.saturating_sub(1))).round() as i16;
                let y = (m.y.clamp(0.0, 1.0) * f32::from(self.height.saturating_sub(1))).round() as i16;
                self.fake(MOTION_NOTIFY_EVENT, 0, x, y);
            }
            Some(Event::MouseButton(b)) => self.button(b.button(), b.down),
            Some(Event::MouseWheel(w)) => self.wheel(w.dx, w.dy),
            Some(Event::Key(k)) => self.key(&k.code, k.down),
            Some(Event::SecureAttention(_)) => debug!("Ctrl+Alt+Del ignored on Linux"),
            None => {}
        }
        Ok(())
    }

    fn set_screen(&mut self, _screen: ScreenRect) {
        // The whole X screen is streamed.
    }

    fn release_all(&mut self) {
        for keycode in std::mem::take(&mut self.keys) {
            self.fake(KEY_RELEASE_EVENT, keycode, 0, 0);
        }
        for button in std::mem::take(&mut self.buttons) {
            self.fake(BUTTON_RELEASE_EVENT, button, 0, 0);
        }
        self.wheel = (0, 0);
    }
}

impl Drop for XTestInjector {
    fn drop(&mut self) {
        self.release_all();
    }
}
