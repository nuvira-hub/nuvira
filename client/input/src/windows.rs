//! Iniezione dell'input con SendInput.

use std::collections::HashSet;

use anyhow::Result;
use nexo_proto::v1::{Button, InputEvent, input_event::Event};
use tracing::{debug, warn};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_NUMLOCK, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use crate::{InputInjector, ScreenRect, keys};

pub fn set_dpi_awareness() {
    if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
        // Già impostata (es. dal manifest) oppure sistema troppo vecchio.
        debug!("SetProcessDpiAwarenessContext: {e}");
    }
}

/// Tasto premuto, da rilasciare a fine sessione.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Held {
    Key(String),
    Button(i32),
}

pub struct SendInputInjector {
    screen: ScreenRect,
    held: HashSet<Held>,
}

impl SendInputInjector {
    pub fn new(screen: ScreenRect) -> Self {
        Self { screen, held: HashSet::new() }
    }

    fn mouse(&self, flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) {
        send(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        });
    }

    fn move_to(&self, x: f32, y: f32) {
        let s = self.screen;
        let px = s.left as f64 + (x.clamp(0.0, 1.0) as f64) * (s.width.saturating_sub(1)) as f64;
        let py = s.top as f64 + (y.clamp(0.0, 1.0) as f64) * (s.height.saturating_sub(1)) as f64;
        // Coordinate assolute normalizzate 0..65535 sull'intero desktop virtuale.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            )
        };
        let ax = ((px - vx as f64) * 65535.0 / (vw - 1).max(1) as f64).round() as i32;
        let ay = ((py - vy as f64) * 65535.0 / (vh - 1).max(1) as f64).round() as i32;
        self.mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, ax, ay, 0);
    }

    fn button(&mut self, button: Button, down: bool) {
        let (flags, data) = match (button, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
            (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
            (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
            (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
            (Button::Unspecified, _) => return,
        };
        self.track(Held::Button(button as i32), down);
        self.mouse(flags, 0, 0, data);
    }

    fn key(&mut self, code: &str, down: bool) {
        let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
        let input = if let Some(sc) = keys::scan_code(code) {
            let ext = if sc.extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
            keyboard(VIRTUAL_KEY(0), sc.code, KEYEVENTF_SCANCODE | ext | up)
        } else {
            // Tasti senza un codice di scansione semplice: si usa il tasto virtuale.
            let vk = match code {
                "Pause" => VK_PAUSE,
                "NumLock" => VK_NUMLOCK,
                _ => {
                    warn!(code, "tasto sconosciuto ignorato");
                    return;
                }
            };
            let ext = if vk == VK_NUMLOCK { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
            keyboard(vk, 0, ext | up)
        };
        self.track(Held::Key(code.to_string()), down);
        send(input);
    }

    fn track(&mut self, held: Held, down: bool) {
        if down {
            self.held.insert(held);
        } else {
            self.held.remove(&held);
        }
    }
}

fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(input: INPUT) {
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent == 0 {
        // Succede se la finestra in primo piano ha privilegi più alti (UIPI) o sul desktop sicuro.
        debug!("SendInput rifiutato: {}", windows::core::Error::from_thread());
    }
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match &event.event {
            Some(Event::MouseMove(m)) => self.move_to(m.x, m.y),
            Some(Event::MouseButton(b)) => self.button(b.button(), b.down),
            Some(Event::MouseWheel(w)) => {
                if w.dy != 0 {
                    self.mouse(MOUSEEVENTF_WHEEL, 0, 0, w.dy);
                }
                if w.dx != 0 {
                    self.mouse(MOUSEEVENTF_HWHEEL, 0, 0, w.dx);
                }
            }
            Some(Event::Key(k)) => self.key(&k.code, k.down),
            None => {}
        }
        Ok(())
    }

    fn release_all(&mut self) {
        for held in std::mem::take(&mut self.held) {
            match held {
                Held::Key(code) => self.key(&code, false),
                Held::Button(b) => self.button(Button::try_from(b).unwrap_or_default(), false),
            }
        }
    }
}

impl Drop for SendInputInjector {
    fn drop(&mut self) {
        self.release_all();
    }
}
