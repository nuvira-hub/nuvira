//! Input injection with SendInput.

use std::collections::HashSet;

use anyhow::Result;
use nexodesk_proto::v1::{Button, InputEvent, input_event::Event};
use tracing::{debug, info, warn};
use windows::core::HSTRING;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_NUMLOCK, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1,
    XBUTTON2,
};

use crate::{InputInjector, ScreenRect, keys};

pub fn set_dpi_awareness() {
    if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
        // Already set (e.g. by the manifest) or system too old.
        debug!("SetProcessDpiAwarenessContext: {e}");
    }
}

/// Pressed key, to be released at the end of the session.
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
        // Absolute coordinates normalised to 0..65535 over the whole virtual desktop.
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
            // Keys without a simple scan code: the virtual key is used.
            let vk = match code {
                "Pause" => VK_PAUSE,
                "NumLock" => VK_NUMLOCK,
                _ => {
                    warn!(code, "unknown key ignored");
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

/// Event created by nexodesk-service: signalling it asks the service for Ctrl+Alt+Del.
/// With the default security of an object created by SYSTEM, only SYSTEM can signal it.
/// The same name is in nexodesk-service (`scm.rs`).
const SAS_EVENT: &str = r"Global\NexoDeskSAS";

/// Ctrl+Alt+Del. Windows accepts it only from a service (`SendSAS`, with the
/// SoftwareSASGeneration policy set by `nexodesk-service install`): the agent asks
/// the service for it. Without the service it is not available.
fn secure_attention() {
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(SAS_EVENT)) };
    match event {
        Ok(event) => {
            info!("Ctrl+Alt+Del requested by the viewer");
            let _ = unsafe { SetEvent(event) };
            let _ = unsafe { CloseHandle(event) };
        }
        Err(e) => warn!("Ctrl+Alt+Del not available: nexodesk-service is needed ({e})"),
    }
}

fn send(input: INPUT) {
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent == 0 {
        // Happens when the foreground window has higher privileges (UIPI) or on the secure desktop.
        debug!("SendInput refused: {}", windows::core::Error::from_thread());
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
            Some(Event::SecureAttention(_)) => secure_attention(),
            None => {}
        }
        Ok(())
    }

    fn set_screen(&mut self, screen: ScreenRect) {
        self.screen = screen;
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
