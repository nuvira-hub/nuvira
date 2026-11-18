//! Shared by the screen tests of `nuvira-capture` (Phase 2 characterisation):
//! the env-var gate, a known pattern drawn on the real screen, and the check of a
//! captured frame against it.
//!
//! The pattern is a 4x4 grid of 16 distinct colours covering a rectangle (a whole
//! screen). A frame matches when, in every block, at least [`MIN_MATCH`] of the
//! sampled points (a 7x7 grid in the middle 60% of the block) are within the
//! tolerance of the expected colour: the pointer drawn into the picture, or a
//! stray notification, cannot make the check fail on their own.

#![allow(dead_code)]

use std::io::Write;

/// Tests that need a real desktop are gated: without `var=1` they print a line
/// starting with `NOT RUN` straight to the process's stderr (not captured by
/// libtest, so it is always in the log) and return. Such a run is NOT a pass:
/// the review's scripts count these lines as "not run".
pub fn gate(var: &str, needs: &str) -> bool {
    if std::env::var(var).as_deref() == Ok("1") {
        return true;
    }
    let _ = writeln!(std::io::stderr(), "NOT RUN (needs {needs}): set {var}=1 to run it");
    false
}

/// Several gates at once (all must be set).
pub fn gates(list: &[(&str, &str)]) -> bool {
    list.iter().fold(true, |all, (var, needs)| gate(var, needs) && all)
}

/// Prints a result line that the scripts keep (bypasses libtest's capture).
pub fn report(text: &str) {
    let _ = writeln!(std::io::stderr(), "RESULT {text}");
}

/// RGB colours of the pattern, far from each other and from black/white/grey.
pub const PALETTE: [[u8; 3]; 16] = [
    [230, 25, 75],
    [60, 180, 75],
    [255, 225, 25],
    [0, 130, 200],
    [245, 130, 48],
    [145, 30, 180],
    [70, 240, 240],
    [240, 50, 230],
    [210, 245, 60],
    [250, 190, 212],
    [0, 128, 128],
    [220, 190, 255],
    [170, 110, 40],
    [255, 250, 200],
    [128, 0, 0],
    [170, 255, 195],
];

pub const GRID: u32 = 4;
/// Share of the sampled points of a block that must match.
pub const MIN_MATCH: f64 = 0.8;

/// Colour of block (`col`, `row`) of the pattern shifted by `shift`.
pub fn colour(col: u32, row: u32, shift: usize) -> [u8; 3] {
    PALETTE[((row * GRID + col) as usize + shift) % PALETTE.len()]
}

/// The pixel rectangle of block (`col`, `row`) in an area `width`x`height`.
pub fn block(col: u32, row: u32, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let (x0, x1) = (col * width / GRID, (col + 1) * width / GRID);
    let (y0, y1) = (row * height / GRID, (row + 1) * height / GRID);
    (x0, y0, x1 - x0, y1 - y0)
}

/// How a frame compares with the pattern.
#[derive(Debug)]
pub struct Match {
    /// Worst block: share of its points that matched.
    pub worst: f64,
    /// Largest channel difference seen on a matching point.
    pub max_diff: u8,
    pub blocks_ok: u32,
}

impl Match {
    pub fn ok(&self) -> bool {
        self.blocks_ok == GRID * GRID
    }
}

/// Compares a BGRA picture (`stride` bytes a row) with the pattern drawn over
/// the whole of it, shifted by `shift`.
pub fn compare(width: u32, height: u32, stride: usize, data: &[u8], shift: usize, tolerance: u8) -> Match {
    let mut worst = 1.0f64;
    let mut max_diff = 0u8;
    let mut blocks_ok = 0;
    for row in 0..GRID {
        for col in 0..GRID {
            let (bx, by, bw, bh) = block(col, row, width, height);
            let want = colour(col, row, shift);
            let (mut hits, mut total) = (0u32, 0u32);
            for sy in 0..7u32 {
                for sx in 0..7u32 {
                    let x = bx + bw / 5 + sx * (bw * 3 / 5) / 6;
                    let y = by + bh / 5 + sy * (bh * 3 / 5) / 6;
                    let at = y as usize * stride + x as usize * 4;
                    let Some(px) = data.get(at..at + 4) else { continue };
                    total += 1;
                    let got = [px[2], px[1], px[0]];
                    let diff = (0..3).map(|i| got[i].abs_diff(want[i])).max().unwrap_or(255);
                    if diff <= tolerance {
                        hits += 1;
                        max_diff = max_diff.max(diff);
                    }
                }
            }
            let share = if total == 0 { 0.0 } else { f64::from(hits) / f64::from(total) };
            worst = worst.min(share);
            if share >= MIN_MATCH {
                blocks_ok += 1;
            }
        }
    }
    Match { worst, max_diff, blocks_ok }
}

/// The middle pixel of block (`col`, `row`) as RGB, for messages.
pub fn sample(width: u32, height: u32, stride: usize, data: &[u8], col: u32, row: u32) -> [u8; 3] {
    let (bx, by, bw, bh) = block(col, row, width, height);
    let at = (by + bh / 2) as usize * stride + (bx + bw / 2) as usize * 4;
    data.get(at..at + 4).map_or([0; 3], |p| [p[2], p[1], p[0]])
}

/// Waits up to `timeout` for a frame of `capturer` that matches the pattern
/// with `shift`; returns the last comparison and the frame size.
pub fn wait_for_pattern(
    capturer: &mut dyn nuvira_capture::Capturer,
    shift: usize,
    tolerance: u8,
    timeout: std::time::Duration,
) -> (Option<Match>, (u32, u32), u32) {
    let start = std::time::Instant::now();
    let mut last = None;
    let mut size = (0, 0);
    let mut frames = 0;
    while start.elapsed() < timeout {
        let frame = match capturer.next_frame(std::time::Duration::from_millis(100)) {
            Ok(Some(f)) => Some(f),
            Ok(None) => capturer.last_frame(),
            Err(e) => panic!("capture failed: {e:#}"),
        };
        let Some(f) = frame else { continue };
        if f.data.is_empty() {
            continue;
        }
        frames += 1;
        size = (f.width, f.height);
        let m = compare(f.width, f.height, f.stride, &f.data, shift, tolerance);
        let ok = m.ok();
        last = Some(m);
        if ok {
            break;
        }
    }
    (last, size, frames)
}

/// A pattern window on Windows: a top-most borderless window over a rectangle of
/// the virtual desktop (physical pixels), painted with GDI from its own thread.
#[cfg(windows)]
pub mod win {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, HGDIOBJ,
        MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, PAINTSTRUCT, RDW_INVALIDATE, RDW_UPDATENOW,
        RedrawWindow,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
        HWND_TOPMOST, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, SW_SHOW, SWP_SHOWWINDOW,
        SetWindowPos, ShowWindow, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_PAINT,
        WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };
    use windows::core::w;

    static SHIFT: AtomicUsize = AtomicUsize::new(0);

    pub struct PatternWindow {
        hwnd: isize,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    /// The primary monitor's rectangle in physical pixels (call
    /// `nuvira_input::set_dpi_awareness` first).
    pub fn primary_rect() -> (i32, i32, u32, u32) {
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        unsafe {
            let monitor = MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY);
            let _ = GetMonitorInfoW(monitor, &mut info);
        }
        let r = info.rcMonitor;
        (r.left, r.top, (r.right - r.left) as u32, (r.bottom - r.top) as u32)
    }

    /// Turns the monitor on if its power saving turned it off: with the
    /// display off DWM composes nothing and DXGI duplication gives no frame
    /// (GDI still reads the windows). A one-pixel mouse move there and back
    /// counts as user input, as at a real session's start.
    pub fn wake_display() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
        };
        let step = |dx: i32| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe {
            SendInput(&[step(1), step(-1)], size_of::<INPUT>() as i32);
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    impl PatternWindow {
        pub fn show(left: i32, top: i32, width: u32, height: u32) -> Self {
            SHIFT.store(0, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel::<isize>();
            let thread = std::thread::spawn(move || unsafe {
                let instance = GetModuleHandleW(None).expect("module");
                let class = WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(proc_),
                    hInstance: instance.into(),
                    lpszClassName: w!("NuviraTestPattern"),
                    ..Default::default()
                };
                RegisterClassExW(&class);
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                    w!("NuviraTestPattern"),
                    w!("NuviraHub test pattern"),
                    WS_POPUP,
                    left,
                    top,
                    width as i32,
                    height as i32,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
                .expect("pattern window");
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    left,
                    top,
                    width as i32,
                    height as i32,
                    SWP_SHOWWINDOW,
                );
                let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW);
                tx.send(hwnd.0 as isize).expect("send");
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            });
            let hwnd = rx.recv().expect("pattern window created");
            // Let DWM compose it.
            std::thread::sleep(std::time::Duration::from_millis(800));
            Self { hwnd, thread: Some(thread) }
        }

        fn hwnd(&self) -> HWND {
            HWND(self.hwnd as *mut _)
        }

        /// Repaints with the pattern shifted by `shift`.
        pub fn set_shift(&self, shift: usize) {
            SHIFT.store(shift, Ordering::SeqCst);
            unsafe {
                let _ = RedrawWindow(Some(self.hwnd()), None, None, RDW_INVALIDATE | RDW_UPDATENOW);
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    impl Drop for PatternWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
                let mut client = RECT::default();
                let _ = unsafe { GetClientRect(hwnd, &mut client) };
                let (w, h) = ((client.right - client.left) as u32, (client.bottom - client.top) as u32);
                let shift = SHIFT.load(Ordering::SeqCst);
                for row in 0..super::GRID {
                    for col in 0..super::GRID {
                        let (x, y, bw, bh) = super::block(col, row, w, h);
                        let [r, g, b] = super::colour(col, row, shift);
                        let rect = RECT {
                            left: x as i32,
                            top: y as i32,
                            right: (x + bw) as i32,
                            bottom: (y + bh) as i32,
                        };
                        unsafe {
                            let brush = CreateSolidBrush(COLORREF(
                                u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16,
                            ));
                            FillRect(hdc, &rect, brush);
                            let _ = DeleteObject(HGDIOBJ(brush.0));
                        }
                    }
                }
                let _ = unsafe { EndPaint(hwnd, &ps) };
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_CLOSE => {
                let _ = unsafe { DestroyWindow(hwnd) };
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }
}

/// A pattern window on X11 (a real X server, or Xwayland under GNOME): an
/// override-redirect window over a rectangle of the root window.
#[cfg(target_os = "linux")]
pub mod x11 {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{
        ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Rectangle, WindowClass,
    };
    use x11rb::rust_connection::RustConnection;

    pub struct PatternWindow {
        conn: Arc<RustConnection>,
        window: u32,
        gc: u32,
        size: (u16, u16),
        shift: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    /// Size of the X root window.
    pub fn root_size() -> (u16, u16) {
        let (conn, screen) = x11rb::connect(None).expect("X11 connection (DISPLAY)");
        let root = &conn.setup().roots[screen];
        (root.width_in_pixels, root.height_in_pixels)
    }

    fn paint(conn: &RustConnection, window: u32, gc: u32, (w, h): (u16, u16), shift: usize) {
        for row in 0..super::GRID {
            for col in 0..super::GRID {
                let (x, y, bw, bh) = super::block(col, row, u32::from(w), u32::from(h));
                let [r, g, b] = super::colour(col, row, shift);
                let pixel = u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b);
                let _ = conn.change_gc(gc, &ChangeGCAux::new().foreground(pixel));
                let rect = Rectangle { x: x as i16, y: y as i16, width: bw as u16, height: bh as u16 };
                let _ = conn.poly_fill_rectangle(window, gc, &[rect]);
            }
        }
        let _ = conn.flush();
    }

    /// GNOME starts Xwayland at the first X client and stops it again a while
    /// after the last one: a connection is kept for the life of the test
    /// process, and the first one waits for Xwayland to be in place.
    static KEEP: std::sync::OnceLock<Option<RustConnection>> = std::sync::OnceLock::new();

    fn keep_x_server() {
        KEEP.get_or_init(|| {
            let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
            let running = std::process::Command::new("pgrep")
                .args(["-x", "Xwayland"])
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            let conn = x11rb::connect(None).ok().map(|(c, _)| c);
            if wayland && !running {
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            conn
        });
    }

    impl PatternWindow {
        pub fn show(left: i16, top: i16, width: u16, height: u16) -> Self {
            keep_x_server();
            let (conn, screen) = x11rb::connect(None).expect("X11 connection (DISPLAY)");
            let conn = Arc::new(conn);
            let root = conn.setup().roots[screen].clone();
            let window = conn.generate_id().expect("id");
            conn.create_window(
                root.root_depth,
                window,
                root.root,
                left,
                top,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                root.root_visual,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .background_pixel(root.black_pixel)
                    .event_mask(EventMask::EXPOSURE),
            )
            .expect("create window");
            let gc = conn.generate_id().expect("id");
            conn.create_gc(gc, window, &CreateGCAux::new()).expect("gc");
            conn.map_window(window).expect("map");
            let _ = conn.configure_window(
                window,
                &x11rb::protocol::xproto::ConfigureWindowAux::new()
                    .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
            );
            conn.flush().expect("flush");
            let shift = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let size = (width, height);
            paint(&conn, window, gc, size, 0);
            let thread = {
                let (conn, shift, stop) = (conn.clone(), shift.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        match conn.poll_for_event() {
                            Ok(Some(Event::Expose(_))) => {
                                paint(&conn, window, gc, size, shift.load(Ordering::SeqCst))
                            }
                            Ok(Some(_)) => {}
                            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                            Err(_) => break,
                        }
                    }
                })
            };
            std::thread::sleep(std::time::Duration::from_millis(800));
            paint(&conn, window, gc, size, 0);
            Self { conn, window, gc, size, shift, stop, thread: Some(thread) }
        }

        pub fn set_shift(&self, shift: usize) {
            self.shift.store(shift, Ordering::SeqCst);
            paint(&self.conn, self.window, self.gc, self.size, shift);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    impl Drop for PatternWindow {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            let _ = self.conn.destroy_window(self.window);
            let _ = self.conn.flush();
        }
    }
}

/// The screens, as the agent would list them. On Wayland the list comes from
/// the open remote desktop session (empty without one, by design: listing
/// would ask): one is opened and held here first, through GNOME's own API
/// unless `NUVIRA_TEST_PORTAL=1` (then the portal, with its dialog).
pub fn displays() -> Vec<nuvira_capture::Display> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && nuvira_portal::current().is_none() {
        if std::env::var_os("NUVIRA_TEST_PORTAL").is_none() {
            nuvira_portal::set_direct(true);
        }
        let token = std::env::temp_dir().join(format!("nuvira-capture-token-{}", std::process::id()));
        let session = nuvira_portal::session(token, true).expect("remote desktop session");
        *HELD.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
    }
    nuvira_capture::displays().expect("displays")
}

/// The screens changed (a resolution): the held session goes, a new one is
/// opened by the next [`displays`].
pub fn refresh_displays() {
    #[cfg(target_os = "linux")]
    {
        nuvira_portal::forget();
        *HELD.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(target_os = "linux")]
static HELD: std::sync::Mutex<Option<std::sync::Arc<nuvira_portal::Session>>> = std::sync::Mutex::new(None);

/// Serialises the screen tests of one binary: they share the real screen.
pub static SCREEN: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn screen_lock() -> std::sync::MutexGuard<'static, ()> {
    SCREEN.lock().unwrap_or_else(|e| e.into_inner())
}
