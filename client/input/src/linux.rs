//! Wayland input: mouse and keyboard through the remote desktop portal, in the
//! same session that shares the screen (opened by the capture).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use nuvira_portal::WheelAxis;
use nuvira_proto::v1::input_event::Event;
use nuvira_proto::v1::{Button, InputEvent};
use tracing::debug;

use crate::ibus_watch::Event as Ibus;
use crate::{InputInjector, Piece, ScreenRect, keys};

/// Units of one wheel notch in [`nuvira_proto::v1::MouseWheel`].
const NOTCH: i32 = 120;

/// Most time IBus may take to show or commit an entry before the next character
/// goes anyway (it takes a few milliseconds).
const IBUS_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

pub struct PortalInjector {
    session: Arc<nuvira_portal::Session>,
    keys: HashSet<u16>,
    buttons: HashSet<i32>,
    /// Wheel movement smaller than a notch, kept for the next event.
    wheel: (i32, i32),
    /// Characters of the compositor's layout (keymap_wayland.rs), read at the
    /// first text: `None` inside if it could not be read.
    layout: Option<Option<HashMap<char, u32>>>,
    /// IBus's signals (ibus_watch.rs), opened at the first character the layout
    /// lacks: `None` inside without IBus.
    ibus: Option<Option<crate::ibus_watch::IbusWatch>>,
}

impl PortalInjector {
    pub fn new() -> Result<Self> {
        let session = nuvira_portal::current().context("no remote desktop session: the screen is not shared")?;
        Ok(Self { session, keys: HashSet::new(), buttons: HashSet::new(), wheel: (0, 0), layout: None, ibus: None })
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

    /// Characters as keysyms, control characters as their keys. The compositor
    /// types a keysym only if its layout has it: the others go through the
    /// input method's Unicode entry where there is IBus (Ctrl+Shift+U, the hex
    /// code, space), and are left out elsewhere.
    fn text(&mut self, text: &str) {
        let layout = self.layout.get_or_insert_with(|| match crate::keymap_wayland::layout_chars() {
            Ok(chars) => {
                debug!(chars = chars.len(), "characters of the compositor's layout");
                Some(chars)
            }
            Err(e) => {
                debug!("compositor's layout not read: {e:#}");
                None
            }
        });
        let layout = layout.clone();
        for piece in crate::pieces(text) {
            match piece {
                Piece::Key(code) => {
                    self.key(code, true);
                    self.key(code, false);
                }
                Piece::Char(c) => match layout.as_ref().map(|l| l.get(&c).copied()) {
                    // The layout makes it, or the layout is not known: as a keysym.
                    Some(Some(keysym)) => self.tap_keysym(keysym),
                    None => self.tap_keysym(keys::keysym(c)),
                    Some(None) => self.unicode_entry(c),
                },
            }
        }
    }

    fn tap_keysym(&self, keysym: u32) {
        self.session.keysym(keysym as i32, true);
        self.session.keysym(keysym as i32, false);
    }

    /// IBus's Unicode entry: Ctrl+Shift+U, the code in hex, space; each step
    /// once IBus shows the last one (ibus_watch.rs). Without IBus the character
    /// is left out: the keys would reach the application as they are.
    fn unicode_entry(&mut self, c: char) {
        let ibus = self.ibus.get_or_insert_with(|| {
            if !ibus() {
                return None;
            }
            crate::ibus_watch::IbusWatch::open()
                .inspect_err(|e| debug!("IBus not reachable: {e:#}"))
                .ok()
        });
        let Some(watch) = ibus.as_mut() else {
            debug!("character not in the layout, left out (no IBus)");
            return;
        };
        watch.drain();
        let session = &self.session;
        let key = |code: u16, down: bool| session.key(code.into(), down);
        // Ctrl+Shift+U by position (evdev codes), released at once.
        let (ctrl, shift, u) = (29, 42, 22);
        key(ctrl, true);
        key(shift, true);
        key(u, true);
        key(u, false);
        key(shift, false);
        key(ctrl, false);
        if !watch.wait(IBUS_LIMIT, |e| matches!(e, Ibus::Preedit(t) if t.starts_with('u'))) {
            debug!("IBus did not open the Unicode entry: {c:?} left out");
            return;
        }
        let tap = |keysym: u32| {
            session.keysym(keysym as i32, true);
            session.keysym(keysym as i32, false);
        };
        for digit in format!("{:x}", u32::from(c)).chars() {
            tap(keys::keysym(digit));
        }
        tap(keys::keysym(' '));
        let want = c.to_string();
        if !watch.wait(IBUS_LIMIT, |e| matches!(e, Ibus::Commit(t) if *t == want)) {
            debug!("IBus did not commit {c:?}");
        }
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

/// IBus is the session's input method (GNOME always has it): its Unicode entry works.
fn ibus() -> bool {
    let env = |name| std::env::var(name).unwrap_or_default().to_ascii_lowercase();
    env("XMODIFIERS").contains("ibus") || env("XDG_CURRENT_DESKTOP").contains("gnome")
}

impl InputInjector for PortalInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        // The capture opens a new session when the system closed the old one
        // (screen locked and unlocked): the input follows it.
        if !self.session.is_open()
            && let Some(session) = nuvira_portal::current()
        {
            self.session = session;
            self.keys.clear();
            self.buttons.clear();
        }
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
            Some(Event::Text(t)) => self.text(&t.text),
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
