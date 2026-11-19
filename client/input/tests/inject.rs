//! Phase 2 characterisation: input injection read back by a test window.
//!
//! Moves the real pointer and types into a window of its own: only with
//! `NUVIRA_TEST_INPUT=1` in a desktop session of a test machine
//! (PC-LABORATORIO through a scheduled task; Mint/Kubuntu/Fedora VMs; MSI-UBUNTU).
//! Never on the NUC. Without it: `NOT RUN (needs ...)`, which is not a pass.
//! No digit is ever typed (PC-LABORATORIO's lock screen takes digits as a PIN),
//! and the keys go out only after the test window is checked to have the focus.
//!
//! Pinned (v0.28.0):
//! - Windows (SendInput): a move lands on `left + x * (width - 1)` of the screen
//!   rectangle, clamped; buttons, wheel (delta = the event's dy/dx as they are),
//!   keys by scan code (letters follow the layout, Shift applies, arrows are
//!   extended), text by `KEYEVENTF_UNICODE` (surrogate pairs as two WM_CHARs,
//!   "\r\n" one Enter, "\t" Tab), `release_all` lets go of what is held, a key
//!   released without being pressed is still sent up.
//! - X11 (XTest): the same moves; wheel notches as buttons 4/5 (up/down) and
//!   6/7; keys by evdev code + 8; characters through spare key codes whose
//!   symbol is the character; a key released without being pressed sends nothing.
//! - Wayland GNOME (Mutter's remote desktop through `nuvira_portal`, direct
//!   mode): moves land on the screen, keys reach the focused window.
//! - Wayland KDE: only through the portal (`NUVIRA_TEST_PORTAL=1`, a person or a
//!   helper approves the dialog: MT-P2-05); characters arrive as themselves,
//!   their Shift/AltGr level in the event's state (Phase 2 read only the key's
//!   first column and took that for a loss: K42, a test bug).

#![cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code, unused_imports))]

use std::io::Write;
use std::time::Duration;

use nuvira_proto::v1::input_event::Event;
use nuvira_proto::v1::{Button, InputEvent, Key, MouseButton, MouseMove, MouseWheel, Text};

fn gate() -> bool {
    if std::env::var("NUVIRA_TEST_INPUT").as_deref() == Ok("1") {
        return true;
    }
    let _ = writeln!(
        std::io::stderr(),
        "NOT RUN (needs a desktop session whose mouse and keyboard the test may take): set NUVIRA_TEST_INPUT=1 to run it"
    );
    false
}

fn report(text: &str) {
    let _ = writeln!(std::io::stderr(), "RESULT {text}");
}

fn mv(x: f32, y: f32) -> InputEvent {
    InputEvent { event: Some(Event::MouseMove(MouseMove { x, y })) }
}
fn btn(button: Button, down: bool) -> InputEvent {
    InputEvent { event: Some(Event::MouseButton(MouseButton { button: button as i32, down })) }
}
fn key(code: &str, down: bool) -> InputEvent {
    InputEvent { event: Some(Event::Key(Key { code: code.into(), down })) }
}
fn wheel(dx: i32, dy: i32) -> InputEvent {
    InputEvent { event: Some(Event::MouseWheel(MouseWheel { dx, dy })) }
}
fn text(t: &str) -> InputEvent {
    InputEvent { event: Some(Event::Text(Text { text: t.into() })) }
}

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn settle() {
    std::thread::sleep(Duration::from_millis(250));
}

#[cfg(windows)]
mod win {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState, VK_CAPITAL, VK_SHIFT};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetForegroundWindow,
        GetMessageW, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, SW_SHOW, SetCursorPos,
        SetForegroundWindow, ShowWindow, TranslateMessage, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_KEYDOWN,
        WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEWHEEL,
        WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_EX_TOPMOST, WS_POPUP,
        WS_VISIBLE,
    };
    use windows::core::w;

    use super::*;

    /// What the test window received.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Got {
        Char(u16),
        KeyDown(u16, bool),
        KeyUp(u16),
        Button(&'static str, bool),
        Wheel(i16),
        HWheel(i16),
    }

    static GOT: Mutex<Vec<Got>> = Mutex::new(Vec::new());

    fn take() -> Vec<Got> {
        std::mem::take(&mut *GOT.lock().unwrap())
    }

    unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let hi = ((wparam.0 >> 16) & 0xffff) as u16;
        let got = match msg {
            WM_CHAR => Some(Got::Char(wparam.0 as u16)),
            WM_KEYDOWN => Some(Got::KeyDown(wparam.0 as u16, (lparam.0 >> 24) & 1 == 1)),
            WM_KEYUP => Some(Got::KeyUp(wparam.0 as u16)),
            WM_LBUTTONDOWN => Some(Got::Button("left", true)),
            WM_LBUTTONUP => Some(Got::Button("left", false)),
            WM_RBUTTONDOWN => Some(Got::Button("right", true)),
            WM_RBUTTONUP => Some(Got::Button("right", false)),
            WM_MBUTTONDOWN => Some(Got::Button("middle", true)),
            WM_MBUTTONUP => Some(Got::Button("middle", false)),
            WM_XBUTTONDOWN => Some(Got::Button(if hi == 1 { "back" } else { "forward" }, true)),
            WM_XBUTTONUP => Some(Got::Button(if hi == 1 { "back" } else { "forward" }, false)),
            WM_MOUSEWHEEL => Some(Got::Wheel(hi as i16)),
            WM_MOUSEHWHEEL => Some(Got::HWheel(hi as i16)),
            WM_CLOSE => {
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                return LRESULT(0);
            }
            _ => None,
        };
        if let Some(g) = got {
            GOT.lock().unwrap().push(g);
            if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) {
                return LRESULT(1);
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    pub struct TestWindow {
        hwnd: isize,
        thread: Option<std::thread::JoinHandle<()>>,
        pub rect: (i32, i32, u32, u32),
    }

    impl TestWindow {
        pub fn show(rect: (i32, i32, u32, u32)) -> Self {
            let (tx, rx) = mpsc::channel::<isize>();
            let thread = std::thread::spawn(move || unsafe {
                let instance = GetModuleHandleW(None).expect("module");
                let class = WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(proc_),
                    hInstance: instance.into(),
                    lpszClassName: w!("NuviraTestInput"),
                    ..Default::default()
                };
                RegisterClassExW(&class);
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST,
                    w!("NuviraTestInput"),
                    w!("NuviraHub input test"),
                    WS_POPUP | WS_VISIBLE,
                    rect.0,
                    rect.1,
                    rect.2 as i32,
                    rect.3 as i32,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
                .expect("test window");
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
                tx.send(hwnd.0 as isize).unwrap();
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            });
            let hwnd = rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(500));
            Self { hwnd, thread: Some(thread), rect }
        }

        pub fn focused(&self) -> bool {
            unsafe { GetForegroundWindow() }.0 as isize == self.hwnd
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn cursor() -> (i32, i32) {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) }.expect("GetCursorPos");
        (p.x, p.y)
    }

    fn primary() -> nuvira_input::ScreenRect {
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
        };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        unsafe {
            let _ =
                GetMonitorInfoW(MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY), &mut info);
        }
        let r = info.rcMonitor;
        nuvira_input::ScreenRect {
            left: r.left,
            top: r.top,
            width: (r.right - r.left) as u32,
            height: (r.bottom - r.top) as u32,
        }
    }

    /// Where a move to (x, y) must land, as `SendInputInjector::move_to` computes it.
    fn expected(s: nuvira_input::ScreenRect, x: f32, y: f32) -> (i32, i32) {
        let px = s.left as f64 + (x.clamp(0.0, 1.0) as f64) * (s.width.saturating_sub(1)) as f64;
        let py = s.top as f64 + (y.clamp(0.0, 1.0) as f64) * (s.height.saturating_sub(1)) as f64;
        (px.round() as i32, py.round() as i32)
    }

    /// PC-LABORATORIO has a USB mouse jiggler: a stray move may land between
    /// the injection and the read, so a move is tried up to three times.
    fn move_and_check(
        inj: &mut Box<dyn nuvira_input::InputInjector>,
        s: nuvira_input::ScreenRect,
        x: f32,
        y: f32,
    ) {
        let want = expected(s, x, y);
        let mut got = (0, 0);
        for _ in 0..3 {
            inj.inject(&mv(x, y)).unwrap();
            std::thread::sleep(Duration::from_millis(80));
            got = cursor();
            if (got.0 - want.0).abs() <= 1 && (got.1 - want.1).abs() <= 1 {
                return;
            }
        }
        panic!("move to ({x}, {y}) of {s:?}: pointer at {got:?}, expected {want:?}");
    }

    /// Clicks into the window so that it has the focus; false if it does not.
    fn focus(
        inj: &mut Box<dyn nuvira_input::InputInjector>,
        window: &TestWindow,
        s: nuvira_input::ScreenRect,
    ) -> bool {
        let (wx, wy, ww, wh) = window.rect;
        let fx = (wx + ww as i32 / 2 - s.left) as f32 / (s.width - 1) as f32;
        let fy = (wy + wh as i32 / 2 - s.top) as f32 / (s.height - 1) as f32;
        for _ in 0..3 {
            inj.inject(&mv(fx, fy)).unwrap();
            inj.inject(&btn(Button::Left, true)).unwrap();
            inj.inject(&btn(Button::Left, false)).unwrap();
            settle();
            if window.focused() {
                return true;
            }
        }
        false
    }

    struct Setup {
        screen: nuvira_input::ScreenRect,
        window: TestWindow,
        inj: Box<dyn nuvira_input::InputInjector>,
        start: (i32, i32),
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            self.inj.release_all();
            let _ = unsafe { SetCursorPos(self.start.0, self.start.1) };
        }
    }

    fn setup() -> Setup {
        nuvira_input::set_dpi_awareness();
        let screen = primary();
        let rect = (
            screen.left + screen.width as i32 / 4,
            screen.top + screen.height as i32 / 4,
            screen.width / 2,
            screen.height / 2,
        );
        let window = TestWindow::show(rect);
        let inj = nuvira_input::open(screen).expect("injector");
        let s = Setup { screen, window, inj, start: cursor() };
        take();
        s
    }

    #[test]
    fn mouse_moves_land_on_the_screen_rectangle() {
        if !gate() {
            return;
        }
        let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = setup();
        let screen = s.screen;
        for (x, y) in [(0.0, 0.0), (1.0, 1.0), (0.5, 0.5), (0.25, 0.75), (-0.5, 2.0)] {
            move_and_check(&mut s.inj, screen, x, y);
        }
        // A sub-rectangle (one screen of several): coordinates relative to it.
        let half = nuvira_input::ScreenRect {
            left: screen.left,
            top: screen.top,
            width: screen.width / 2,
            height: screen.height / 2,
        };
        s.inj.set_screen(half);
        move_and_check(&mut s.inj, half, 1.0, 1.0);
        move_and_check(&mut s.inj, half, 0.5, 0.5);
        report(&format!("moves ok on {screen:?} and {half:?}"));
    }

    #[test]
    fn buttons_and_wheel_reach_the_window() {
        if !gate() {
            return;
        }
        let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = setup();
        assert!(focus(&mut s.inj, &s.window, s.screen), "test window has the focus after a click");
        take();
        for b in [Button::Left, Button::Right, Button::Middle, Button::Back, Button::Forward] {
            s.inj.inject(&btn(b, true)).unwrap();
            s.inj.inject(&btn(b, false)).unwrap();
        }
        s.inj.inject(&btn(Button::Unspecified, true)).unwrap();
        // One at a time: Windows merges wheel messages still in the queue.
        for (dx, dy) in [(0, 120), (0, -240), (120, 0), (-60, 0), (0, 30)] {
            s.inj.inject(&wheel(dx, dy)).unwrap();
            settle();
        }
        settle();
        let got = take();
        report(&format!("buttons and wheel: {got:?}"));
        let want = vec![
            Got::Button("left", true),
            Got::Button("left", false),
            Got::Button("right", true),
            Got::Button("right", false),
            Got::Button("middle", true),
            Got::Button("middle", false),
            Got::Button("back", true),
            Got::Button("back", false),
            Got::Button("forward", true),
            Got::Button("forward", false),
            // The wheel's units go to Windows as they are (no accumulation into notches).
            Got::Wheel(120),
            Got::Wheel(-240),
            Got::HWheel(120),
            Got::HWheel(-60),
            Got::Wheel(30),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn keys_and_text_reach_the_focused_window() {
        if !gate() {
            return;
        }
        let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = setup();
        assert!(focus(&mut s.inj, &s.window, s.screen), "test window has the focus after a click");
        let caps = unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 == 1;
        take();
        let chars = |got: &[Got]| -> String {
            let units: Vec<u16> =
                got.iter().filter_map(|g| if let Got::Char(c) = g { Some(*c) } else { None }).collect();
            String::from_utf16_lossy(&units)
        };
        // Keys (checked again that the focus is still ours before each group).
        assert!(s.window.focused());
        for code in ["KeyA", "KeyQ"] {
            s.inj.inject(&key(code, true)).unwrap();
            s.inj.inject(&key(code, false)).unwrap();
        }
        s.inj.inject(&key("ShiftLeft", true)).unwrap();
        s.inj.inject(&key("KeyB", true)).unwrap();
        s.inj.inject(&key("KeyB", false)).unwrap();
        s.inj.inject(&key("ShiftLeft", false)).unwrap();
        for code in ["Enter", "Backspace", "Tab", "Space"] {
            s.inj.inject(&key(code, true)).unwrap();
            s.inj.inject(&key(code, false)).unwrap();
        }
        // Unknown codes are ignored.
        s.inj.inject(&key("NoSuchKey", true)).unwrap();
        s.inj.inject(&key("NoSuchKey", false)).unwrap();
        settle();
        let got = take();
        report(&format!("keys: {got:?}"));
        let letters = if caps { "AQb" } else { "aqB" };
        assert_eq!(chars(&got), format!("{letters}\r\u{8}\t "), "characters of the keys");
        // Letters by scan code: virtual keys A, Q, B.
        assert!(got.contains(&Got::KeyDown(b'A' as u16, false)));
        assert!(got.contains(&Got::KeyDown(b'Q' as u16, false)));

        // Extended keys.
        assert!(s.window.focused());
        s.inj.inject(&key("ArrowLeft", true)).unwrap();
        s.inj.inject(&key("ArrowLeft", false)).unwrap();
        s.inj.inject(&key("Delete", true)).unwrap();
        s.inj.inject(&key("Delete", false)).unwrap();
        settle();
        let got = take();
        report(&format!("extended keys: {got:?}"));
        assert!(got.contains(&Got::KeyDown(0x25, true)), "ArrowLeft is VK_LEFT, extended");
        assert!(got.contains(&Got::KeyDown(0x2E, true)), "Delete is VK_DELETE, extended");

        // Text: any character whatever the layout; Enter, Tab; other controls left out.
        assert!(s.window.focused());
        s.inj.inject(&text("àè€ñ😀\tZ\r\nx\u{7}y")).unwrap();
        settle();
        let got = take();
        report(&format!("text: {got:?}"));
        assert_eq!(chars(&got), "àè€ñ😀\tZ\rxy");
        let units: Vec<u16> =
            got.iter().filter_map(|g| if let Got::Char(c) = g { Some(*c) } else { None }).collect();
        assert!(units.contains(&0xD83D) && units.contains(&0xDE00), "the emoji as a surrogate pair");

        // A key released without being pressed is still sent (Windows).
        assert!(s.window.focused());
        s.inj.inject(&key("KeyZ", false)).unwrap();
        settle();
        let got = take();
        report(&format!("release without press: {got:?}"));
        assert_eq!(got, vec![Got::KeyUp(b'Z' as u16)]);
    }

    #[test]
    fn release_all_lets_go_of_held_keys_and_buttons() {
        if !gate() {
            return;
        }
        let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = setup();
        assert!(focus(&mut s.inj, &s.window, s.screen), "test window has the focus after a click");
        take();
        s.inj.inject(&key("ShiftLeft", true)).unwrap();
        s.inj.inject(&btn(Button::Left, true)).unwrap();
        settle();
        let held = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } as u16 & 0x8000 != 0;
        s.inj.release_all();
        settle();
        let released = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } as u16 & 0x8000 == 0;
        let got = take();
        report(&format!("release_all: shift held {held}, released {released}, {got:?}"));
        assert!(held && released, "Shift held, then released by release_all");
        assert!(got.contains(&Got::Button("left", false)), "left button released");
        // Dropping the injector releases too.
        s.inj.inject(&key("ShiftLeft", true)).unwrap();
        settle();
        let inj = std::mem::replace(&mut s.inj, nuvira_input::open(s.screen).unwrap());
        drop(inj);
        settle();
        assert!(unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } as u16 & 0x8000 == 0, "released on drop");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Arc;

    use x11rb::connection::Connection;
    use x11rb::protocol::Event as XEvent;
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass};
    use x11rb::rust_connection::RustConnection;

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    enum Got {
        /// Key code, the keysym of its first column and the keysym of the level
        /// the event's modifier state selects, both at the time it came.
        Key(u8, u32, u32, bool),
        Button(u8, bool),
    }

    struct TestWindow {
        conn: RustConnection,
        window: u32,
        root: u32,
        /// Where the window is on the root (the window manager may have moved it).
        rect: (i32, i32, u32, u32),
    }

    impl TestWindow {
        fn show(rect: (i16, i16, u16, u16)) -> Self {
            let (conn, screen) = x11rb::connect(None).expect("X11 connection (DISPLAY)");
            let root = conn.setup().roots[screen].clone();
            let window = conn.generate_id().unwrap();
            let mask = EventMask::KEY_PRESS
                | EventMask::KEY_RELEASE
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::STRUCTURE_NOTIFY
                | EventMask::FOCUS_CHANGE;
            conn.create_window(
                root.root_depth,
                window,
                root.root,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                0,
                WindowClass::INPUT_OUTPUT,
                root.root_visual,
                &CreateWindowAux::new().background_pixel(0x0040_8040).event_mask(mask),
            )
            .unwrap();
            conn.map_window(window).unwrap();
            conn.flush().unwrap();
            std::thread::sleep(Duration::from_millis(1000));
            let _ = conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
            conn.flush().unwrap();
            let geo = conn.get_geometry(window).unwrap().reply().unwrap();
            let at = conn.translate_coordinates(window, root.root, 0, 0).unwrap().reply().unwrap();
            let rect =
                (i32::from(at.dst_x), i32::from(at.dst_y), u32::from(geo.width), u32::from(geo.height));
            let w = Self { conn, window, root: root.root, rect };
            w.drain();
            w
        }

        /// The key's first column, and the column its state selects: Shift the
        /// second, Mod5 (ISO_Level3_Shift, AltGr) the fifth and sixth, as Xlib's
        /// lookup does with the core map of an XKB server. KWin types a character
        /// of the layout as its key with the level in the state, without
        /// pressing Shift or AltGr.
        fn keysyms(&self, keycode: u8, state: u16) -> (u32, u32) {
            let Some(map) = self.conn.get_keyboard_mapping(keycode, 1).ok().and_then(|c| c.reply().ok())
            else {
                return (0, 0);
            };
            let col = |i: usize| map.keysyms.get(i).copied().filter(|s| *s != 0);
            let base = col(0).unwrap_or(0);
            let shift = state & 0x1 != 0;
            let level3 = state & 0x80 != 0;
            let level = match (shift, level3) {
                (false, false) => Some(base),
                (true, false) => col(1),
                (false, true) => col(4),
                (true, true) => col(5).or(col(4)),
            };
            (base, level.unwrap_or(base))
        }

        fn drain(&self) -> Vec<Got> {
            let mut got = Vec::new();
            while let Ok(Some(e)) = self.conn.poll_for_event() {
                match e {
                    XEvent::KeyPress(k) => {
                        let (base, level) = self.keysyms(k.detail, u16::from(k.state));
                        got.push(Got::Key(k.detail, base, level, true));
                    }
                    XEvent::KeyRelease(k) => {
                        let (base, level) = self.keysyms(k.detail, u16::from(k.state));
                        got.push(Got::Key(k.detail, base, level, false));
                    }
                    XEvent::ButtonPress(b) => got.push(Got::Button(b.detail, true)),
                    XEvent::ButtonRelease(b) => got.push(Got::Button(b.detail, false)),
                    _ => {}
                }
            }
            got
        }

        /// Reads events for a while: each key's symbol is looked up when it comes,
        /// before a later character takes the spare key code again.
        fn collect(&self, wait: Duration) -> Vec<Got> {
            let start = std::time::Instant::now();
            let mut got = Vec::new();
            while start.elapsed() < wait {
                got.extend(self.drain());
                std::thread::sleep(Duration::from_millis(5));
            }
            got
        }

        fn pointer(&self) -> (i32, i32) {
            let p = self.conn.query_pointer(self.root).unwrap().reply().unwrap();
            (i32::from(p.root_x), i32::from(p.root_y))
        }

        /// The focus is on the window or on one of its ancestors (a reparenting
        /// window manager may focus its frame): with the pointer inside the
        /// window, X delivers the keys to it either way.
        fn focused(&self) -> bool {
            let Some(focus) = self.conn.get_input_focus().ok().and_then(|c| c.reply().ok()).map(|f| f.focus)
            else {
                return false;
            };
            let mut w = self.window;
            for _ in 0..8 {
                if w == focus {
                    return true;
                }
                match self.conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
                    Some(t) if t.parent != 0 && t.parent != self.root => w = t.parent,
                    _ => return false,
                }
            }
            false
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = self.conn.destroy_window(self.window);
            let _ = self.conn.flush();
        }
    }

    fn wayland() -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
    }

    struct Setup {
        screen: nuvira_input::ScreenRect,
        window: TestWindow,
        inj: Box<dyn nuvira_input::InputInjector>,
        _session: Option<Arc<nuvira_portal::Session>>,
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            self.inj.release_all();
        }
    }

    fn setup() -> Setup {
        let _ =
            tracing_subscriber::fmt().with_env_filter("nuvira_input=debug,nuvira_portal=debug").try_init();
        let (session, screen) = if wayland() {
            // GNOME's own remote desktop API, as unattended access uses (no dialog).
            // With NUVIRA_TEST_PORTAL=1 the portal and its dialog instead, as the
            // screen tests do: the only way on KDE, where the direct API is not
            // offered and a session without a dialog is refused.
            let portal = std::env::var_os("NUVIRA_TEST_PORTAL").is_some();
            if !portal {
                nuvira_portal::set_direct(true);
            }
            let token = std::env::temp_dir().join(format!("nuvira-input-token-{}", std::process::id()));
            let session = nuvira_portal::session(token, portal).expect("remote desktop session");
            let s = session.screen();
            let rect =
                nuvira_input::ScreenRect { left: s.left, top: s.top, width: s.width, height: s.height };
            (Some(session), rect)
        } else {
            let (conn, n) = x11rb::connect(None).expect("X11");
            let r = &conn.setup().roots[n];
            (
                None,
                nuvira_input::ScreenRect {
                    left: 0,
                    top: 0,
                    width: r.width_in_pixels.into(),
                    height: r.height_in_pixels.into(),
                },
            )
        };
        report(&format!("screen {screen:?}, wayland {}", wayland()));
        let window = TestWindow::show((
            (screen.width / 4) as i16,
            (screen.height / 4) as i16,
            (screen.width / 2) as u16,
            (screen.height / 2) as u16,
        ));
        report(&format!("test window at {:?}", window.rect));
        let inj = nuvira_input::open(screen).expect("injector");
        Setup { screen, window, inj, _session: session }
    }

    fn expected(s: nuvira_input::ScreenRect, x: f32, y: f32) -> (i32, i32) {
        let px = s.left + (x.clamp(0.0, 1.0) * s.width.saturating_sub(1) as f32).round() as i32;
        let py = s.top + (y.clamp(0.0, 1.0) * s.height.saturating_sub(1) as f32).round() as i32;
        (px, py)
    }

    fn click_into(s: &mut Setup) {
        // A window manager (Cinnamon) may hand the focus back to another window
        // right after mapping ours, or move it: tried a few times, where it is now.
        for _ in 0..5 {
            let c = &s.window.conn;
            let at = c.translate_coordinates(s.window.window, s.window.root, 0, 0).unwrap().reply().unwrap();
            let (wx, wy) = (i32::from(at.dst_x), i32::from(at.dst_y));
            let (ww, wh) = (s.window.rect.2, s.window.rect.3);
            let fx = (wx + ww as i32 / 2 - s.screen.left) as f32 / (s.screen.width - 1) as f32;
            let fy = (wy + wh as i32 / 2 - s.screen.top) as f32 / (s.screen.height - 1) as f32;
            s.inj.inject(&mv(fx, fy)).unwrap();
            settle();
            s.inj.inject(&btn(Button::Left, true)).unwrap();
            s.inj.inject(&btn(Button::Left, false)).unwrap();
            settle();
            if wayland() {
                return;
            }
            let _ = s.window.conn.set_input_focus(InputFocus::PARENT, s.window.window, x11rb::CURRENT_TIME);
            let _ = s.window.conn.flush();
            settle();
            if s.window.focused() {
                return;
            }
        }
    }

    #[test]
    fn mouse_moves_land_on_the_screen() {
        if !gate() {
            return;
        }
        let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = setup();
        let screen = s.screen;
        // Xwayland knows the pointer only over its own windows: inside the test window there.
        let points: &[(f32, f32)] = if wayland() {
            &[(0.5, 0.5), (0.3, 0.3), (0.7, 0.6)]
        } else {
            &[(0.0, 0.0), (1.0, 1.0), (0.5, 0.5), (0.25, 0.75), (-1.0, 3.0)]
        };
        for &(x, y) in points {
            s.inj.inject(&mv(x, y)).unwrap();
            std::thread::sleep(Duration::from_millis(150));
            let got = s.window.pointer();
            let want = expected(screen, x, y);
            report(&format!("move ({x}, {y}) -> {got:?}, expected {want:?}"));
            assert!(
                (got.0 - want.0).abs() <= 1 && (got.1 - want.1).abs() <= 1,
                "move ({x}, {y}): {got:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn buttons_wheel_keys_and_text_reach_the_window() {
        if !gate() {
            return;
        }
        let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = setup();
        click_into(&mut s);
        let focused = s.window.focused();
        report(&format!("focused after click: {focused}"));
        if !focused && !wayland() {
            let c = &s.window.conn;
            let focus = c.get_input_focus().unwrap().reply().unwrap().focus;
            let class = c
                .get_property(
                    false,
                    focus,
                    x11rb::protocol::xproto::AtomEnum::WM_CLASS,
                    x11rb::protocol::xproto::AtomEnum::STRING,
                    0,
                    64,
                )
                .ok()
                .and_then(|r| r.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.value).replace(' ', " "));
            let pointer = c.query_pointer(s.window.root).unwrap().reply().unwrap();
            report(&format!(
                "focus on {focus:#x} ({class:?}), ours {:#x}; pointer at {},{} child {:#x}",
                s.window.window, pointer.root_x, pointer.root_y, pointer.child
            ));
        }
        assert!(focused || wayland(), "X11: the test window has the focus");
        s.window.drain();

        for b in [Button::Left, Button::Right, Button::Middle, Button::Back, Button::Forward] {
            s.inj.inject(&btn(b, true)).unwrap();
            s.inj.inject(&btn(b, false)).unwrap();
        }
        // Notches: 120 up, 240 down, 120 right, then 60+60 left (kept until a notch).
        for (dx, dy) in [(0, 120), (0, -240), (120, 0), (-60, 0), (-60, 0)] {
            s.inj.inject(&wheel(dx, dy)).unwrap();
        }
        let got = s.window.collect(Duration::from_millis(500));
        report(&format!("buttons and wheel: {got:?}"));
        let buttons: Vec<(u8, bool)> = got
            .iter()
            .filter_map(|g| if let Got::Button(b, d) = g { Some((*b, *d)) } else { None })
            .collect();
        let mut want = vec![
            (1, true),
            (1, false),
            (3, true),
            (3, false),
            (2, true),
            (2, false),
            (8, true),
            (8, false),
            (9, true),
            (9, false),
        ];
        // Wheel: up = 4, down = 5 (two notches), right = 7, left = 6.
        want.extend([
            (4, true),
            (4, false),
            (5, true),
            (5, false),
            (5, true),
            (5, false),
            (7, true),
            (7, false),
            (6, true),
            (6, false),
        ]);
        let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.contains("KDE"));
        if wayland() && kde {
            // KWin's portal: every notch reaches Xwayland as one click (before Plasma
            // 6.9 a notch is sent as 10 smooth units, Xwayland's click; Phase 2 saw
            // 1 click for the 5 notches at most: VWR-3).
            report(&format!("wayland buttons seen {buttons:?}"));
            assert_eq!(buttons, want, "buttons and wheel through KWin's portal");
        } else if wayland() {
            // Through Mutter the wheel arrives as Xwayland's own scroll buttons; pin the order seen.
            report(&format!("wayland buttons seen {buttons:?}"));
            assert_eq!(&buttons[..10], &want[..10], "buttons through Mutter");
        } else {
            assert_eq!(buttons, want, "XTest buttons and wheel");
        }

        // Keys by evdev code + 8 (KeyA = 30 -> X keycode 38).
        s.window.drain();
        for code in ["KeyA", "KeyQ"] {
            s.inj.inject(&key(code, true)).unwrap();
            s.inj.inject(&key(code, false)).unwrap();
        }
        // A release without a press sends nothing.
        s.inj.inject(&key("KeyZ", false)).unwrap();
        s.inj.inject(&key("NoSuchKey", true)).unwrap();
        let got = s.window.collect(Duration::from_millis(500));
        report(&format!("keys: {got:?}"));
        let presses: Vec<u8> = got
            .iter()
            .filter_map(|g| if let Got::Key(k, _, _, true) = g { Some(*k) } else { None })
            .collect();
        assert_eq!(presses, vec![38, 24], "KeyA and KeyQ by evdev code + 8");
        let releases: Vec<u8> = got
            .iter()
            .filter_map(|g| if let Got::Key(k, _, _, false) = g { Some(*k) } else { None })
            .collect();
        assert_eq!(releases, vec![38, 24], "no release for KeyZ, never pressed");

        // Text: each character a key whose symbol is the character.
        s.window.drain();
        let typed = "àè€ñZ";
        s.inj.inject(&text(typed)).unwrap();
        let got = s.window.collect(Duration::from_millis(800));
        report(&format!("text: {got:?}"));
        let syms: Vec<u32> = got
            .iter()
            .filter_map(|g| if let Got::Key(_, s, _, true) = g { Some(*s) } else { None })
            .collect();
        let levels: Vec<u32> = got
            .iter()
            .filter_map(|g| if let Got::Key(_, _, l, true) = g { Some(*l) } else { None })
            .collect();
        let want: Vec<u32> = typed.chars().map(nuvira_input::keys::keysym).collect();
        report(&format!("text keysyms {syms:x?}, with the state {levels:x?}, expected {want:x?}"));
        if wayland() && kde {
            // KWin's portal (NotifyKeyboardKeysym) types a character as the key
            // that carries it, its Shift or AltGr level in the modifier state of
            // the event (no modifier press): with the state the symbols are the
            // characters (the first column alone read à è e n z, Phase 2 K42).
            // The layout's own keysyms are compared as characters: Latin-1 and
            // EuroSign (0x20ac) are their code points, a Unicode keysym is
            // 0x01000000 + the code point.
            let char_of = |s: u32| if s >= 0x0100_0000 { s - 0x0100_0000 } else { s };
            let got: Vec<u32> = levels.iter().copied().map(char_of).collect();
            let want: Vec<u32> = want.iter().copied().map(char_of).collect();
            assert_eq!(got, want, "characters typed through KWin, read with the event's state");
        } else if wayland() {
            // Mutter types the compositor's layout keys: a character of the
            // layout is its key, with Shift (0xffe1) or AltGr (ISO_Level3_Shift,
            // 0xfe03) held around it when its level needs one; the first column
            // read back is then the key's base symbol. One non-modifier press per
            // character, each base-level character as its own symbol.
            let modifier = |s: u32| (0xffe1..=0xffee).contains(&s) || (0xfe01..=0xfe13).contains(&s);
            let base: Vec<u32> = syms.iter().copied().filter(|s| !modifier(*s)).collect();
            assert_eq!(base.len(), want.len(), "one key press per character through Mutter: {syms:x?}");
            for (got, (c, want)) in base.iter().zip(typed.chars().zip(&want)) {
                let lower = nuvira_input::keys::keysym(c.to_lowercase().next().unwrap_or(c));
                assert!(got == want || *got == lower || syms.iter().any(|s| modifier(*s)), "{c:?}: {got:x}");
            }
        } else {
            assert_eq!(syms, want, "XTest: spare keys carry the characters' symbols");
        }
    }

    /// X11: the spare key codes borrowed for text are put back by release_all.
    #[test]
    fn x11_spare_keys_put_back_after_text() {
        if !gate() {
            return;
        }
        if wayland() {
            let _ = writeln!(std::io::stderr(), "NOT RUN (needs an X11 session: spare keys are XTest's)");
            return;
        }
        let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = setup();
        let (conn, _) = x11rb::connect(None).unwrap();
        let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let map =
            |c: &RustConnection| c.get_keyboard_mapping(min, max - min + 1).unwrap().reply().unwrap().keysyms;
        let before = map(&conn);
        s.inj.inject(&text("€ß")).unwrap();
        settle();
        let during = map(&conn);
        s.inj.release_all();
        settle();
        let after = map(&conn);
        report(&format!("keyboard map changed during text: {}", before != during));
        assert_ne!(before, during, "characters typed through borrowed key codes");
        assert_eq!(before, after, "borrowed key codes put back by release_all");
    }
}

/// macOS (Quartz events): no test window is written for macOS yet; injection
/// there also needs the Accessibility permission. A manual entry until a Mac.
#[cfg(target_os = "macos")]
#[test]
fn macos_injection_not_covered_here() {
    let _ = writeln!(
        std::io::stderr(),
        "NOT RUN (needs a macOS test window and the Accessibility permission: not written)"
    );
}
