//! X11 input: mouse and keyboard through the XTest extension.

use std::collections::HashSet;

use anyhow::{Context, Result};
use nuvira_proto::v1::input_event::Event;
use nuvira_proto::v1::{Button, InputEvent};
use tracing::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, ConnectionExt as _, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::{InputInjector, Piece, ScreenRect, keys};

/// Units of one wheel notch in [`nuvira_proto::v1::MouseWheel`].
const NOTCH: i32 = 120;

/// X key codes are the Linux ones plus 8.
const KEYCODE_OFFSET: u16 = 8;

/// Most spare keys used in turn (see [`Spare`]).
const MAX_SPARES: usize = 16;

/// A key code borrowed to type characters of any layout: XTest sends only key
/// codes, so a character's key first gets its symbol. Several free key codes are
/// used in turn, and a character keeps its key while it has one: GTK reads the
/// new map some time after the change, and a key changed again (or put back)
/// right after being pressed lost its character ("P" missing in "Più"). They
/// are put back as they were when the session ends.
struct Spare {
    keycode: u8,
    original: Vec<u32>,
    keysym: Option<u32>,
    used: u64,
}

struct Spares {
    keys: Vec<Spare>,
    per_keycode: u8,
    clock: u64,
}

pub struct XTestInjector {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    keys: HashSet<u8>,
    buttons: HashSet<u8>,
    wheel: (i32, i32),
    spares: Option<Spares>,
}

impl XTestInjector {
    pub fn new() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("X11 connection")?;
        conn.xtest_get_version(2, 2)?.reply().context("XTest extension not available")?;
        let root = &conn.setup().roots[screen];
        let (root, width, height) = (root.root, root.width_in_pixels, root.height_in_pixels);
        let (keys, buttons) = (HashSet::new(), HashSet::new());
        Ok(Self { conn, root, width, height, keys, buttons, wheel: (0, 0), spares: None })
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

    /// Characters through the spare keys, control characters as their keys.
    fn text(&mut self, text: &str) {
        for piece in crate::pieces(text) {
            match piece {
                Piece::Key(code) => {
                    self.key(code, true);
                    self.key(code, false);
                }
                Piece::Char(c) => {
                    if let Err(e) = self.type_char(c) {
                        debug!("character not typed: {e:#}");
                    }
                }
            }
        }
    }

    fn type_char(&mut self, c: char) -> Result<()> {
        if self.spares.is_none() {
            self.spares = Some(self.find_spares()?);
        }
        let Some(spares) = self.spares.as_mut() else { return Ok(()) };
        let keysym = keys::keysym(c);
        spares.clock += 1;
        let clock = spares.clock;
        let slot = match spares.keys.iter().position(|k| k.keysym == Some(keysym)) {
            Some(i) => i,
            None => {
                // The key unused for the longest time gets the symbol.
                let i = (0..spares.keys.len()).min_by_key(|&i| spares.keys[i].used).unwrap_or(0);
                spares.keys[i].keysym = Some(keysym);
                let keycode = spares.keys[i].keycode;
                self.conn.change_keyboard_mapping(1, keycode, 2, &[keysym, keysym])?;
                // A round trip: the server applied the change, and sent the programs
                // its MappingNotify, before the key, which comes after it in their queue.
                self.conn.get_input_focus()?.reply()?;
                i
            }
        };
        let Some(spares) = self.spares.as_mut() else { return Ok(()) };
        spares.keys[slot].used = clock;
        let keycode = spares.keys[slot].keycode;
        self.fake(KEY_PRESS_EVENT, keycode, 0, 0);
        self.fake(KEY_RELEASE_EVENT, keycode, 0, 0);
        Ok(())
    }

    /// The highest key codes without symbols; failing that, the highest one.
    fn find_spares(&self) -> Result<Spares> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = usize::from(map.keysyms_per_keycode.max(1));
        let syms = |keycode: u8| {
            let start = usize::from(keycode - min) * per;
            map.keysyms.get(start..start + per).unwrap_or_default().to_vec()
        };
        let mut free: Vec<u8> =
            (min..=max).rev().filter(|&k| syms(k).iter().all(|&s| s == 0)).take(MAX_SPARES).collect();
        if free.is_empty() {
            free.push(max);
        }
        debug!(?free, "spare keys for text");
        let keys = free.into_iter().map(|k| Spare { keycode: k, original: syms(k), keysym: None, used: 0 }).collect();
        Ok(Spares { keys, per_keycode: per as u8, clock: 0 })
    }

    fn restore_spares(&mut self) {
        let Some(spares) = self.spares.take() else { return };
        for key in spares.keys.iter().filter(|k| k.keysym.is_some()) {
            let restored = self
                .conn
                .change_keyboard_mapping(1, key.keycode, spares.per_keycode, &key.original)
                .map_err(anyhow::Error::from)
                .and_then(|cookie| cookie.check().map_err(anyhow::Error::from));
            if let Err(e) = restored {
                debug!(keycode = key.keycode, "spare key not put back: {e}");
            }
        }
        let _ = self.conn.flush();
        debug!("spare keys put back");
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
            Some(Event::Text(t)) => self.text(&t.text),
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
        self.restore_spares();
    }
}

impl Drop for XTestInjector {
    fn drop(&mut self) {
        debug!("X11 input closed");
        self.release_all();
    }
}
