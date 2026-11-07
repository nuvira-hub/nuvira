//! Privacy mode: the screen of the controlled PC turns black and its own mouse and
//! keyboard are blocked, while the technician keeps working on the real image.
//!
//! Windows (10 version 2004 or later): a black window on every screen, always on
//! top, that lets clicks through and is excluded from capture
//! (`WDA_EXCLUDEFROMCAPTURE`: screen capture does not see it); low-level hooks drop
//! the mouse and keyboard events that do not come from our own injection
//! ([`INJECTED_TAG`]). Ctrl+Alt+Del cannot be blocked: the person at the PC can
//! always lock it. The secure desktop (UAC, lock screen) is not covered.
//! macOS: see `privacy_mac.rs` (black windows in a child process left out of
//! ScreenCaptureKit, event tap for the local input).
//! Linux: not available yet.

use anyhow::Result;

/// `dwExtraInfo` of the events injected by NuviraHub: the only ones privacy mode lets through.
#[cfg(windows)]
pub(crate) const INJECTED_TAG: usize = 0x4e55_5652; // "NUVR"

#[cfg(target_os = "macos")]
#[path = "privacy_mac.rs"]
mod mac;
#[cfg(target_os = "macos")]
pub(crate) use mac::{INJECTED_TAG, USER_DATA};

/// Privacy mode is on as long as this value lives.
pub struct Privacy {
    #[cfg(windows)]
    inner: imp::Overlay,
    #[cfg(target_os = "macos")]
    inner: mac::Overlay,
}

/// The system can turn on privacy mode.
pub fn available() -> bool {
    #[cfg(windows)]
    return imp::available();
    #[cfg(target_os = "macos")]
    return mac::available();
    #[cfg(not(any(windows, target_os = "macos")))]
    false
}

/// Turns privacy mode on; `message` is shown on the black screen to the person at the PC.
/// `hide_from_capture` gets the process whose windows the screen capture must leave
/// out (macOS: the black screens are another process), before they turn black.
pub fn start(message: &str, hide_from_capture: &dyn Fn(u32)) -> Result<Privacy> {
    #[cfg(windows)]
    let privacy = {
        let _ = hide_from_capture;
        Privacy { inner: imp::Overlay::start(message)? }
    };
    #[cfg(target_os = "macos")]
    let privacy = Privacy { inner: mac::Overlay::start(message, hide_from_capture)? };
    #[cfg(not(any(windows, target_os = "macos")))]
    let privacy = {
        let _ = (message, hide_from_capture);
        anyhow::bail!("@agent.privacy_unsupported")
    };
    #[allow(unreachable_code)]
    Ok(privacy)
}

/// macOS: runs the black screens when the program was started as their child
/// process (and never returns then). Call it first thing in `main`.
pub fn overlay_if_asked() {
    #[cfg(target_os = "macos")]
    mac::overlay_if_asked();
}

impl Privacy {
    /// Ends privacy mode (as dropping it does).
    pub fn stop(self) {}
}

#[cfg(any(windows, target_os = "macos"))]
impl Drop for Privacy {
    fn drop(&mut self) {
        self.inner.stop();
    }
}

#[cfg(windows)]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::sync::mpsc;

    use anyhow::{Result, anyhow, bail};
    use tracing::{debug, info, warn};
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
        DT_CALCRECT, DT_CENTER, DT_NOPREFIX, DT_WORDBREAK, DeleteObject, DrawTextW, EndPaint,
        EnumDisplayMonitors, FW_NORMAL, FillRect, GetMonitorInfoW, HDC, HFONT, HGDIOBJ, HMONITOR,
        MONITORINFO, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
        GetMessageW, GetWindowDisplayAffinity, HC_ACTION, HTTRANSPARENT, HWND_TOPMOST, KBDLLHOOKSTRUCT,
        LWA_ALPHA, MA_NOACTIVATE, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, RegisterClassExW,
        SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetLayeredWindowAttributes, SetTimer,
        SetWindowDisplayAffinity, SetWindowPos, SetWindowsHookExW, ShowWindow, TranslateMessage,
        UnhookWindowsHookEx, WDA_EXCLUDEFROMCAPTURE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_CLOSE,
        WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT, WM_QUIT, WM_TIMER,
        WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
        WS_POPUP,
    };
    use windows::core::{BOOL, w};

    use super::INJECTED_TAG;

    /// First build with `WDA_EXCLUDEFROMCAPTURE` (Windows 10 version 2004): before
    /// it the flag acts as `WDA_MONITOR` and the technician would see black.
    const MIN_BUILD: u32 = 19041;
    /// The screens changed: the windows are created again.
    const WM_REBUILD: u32 = WM_APP + 1;
    /// How often the windows go back on top of those that became topmost after them.
    const TOPMOST_MS: u32 = 500;

    pub fn available() -> bool {
        let mut info =
            OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
        unsafe { RtlGetVersion(&mut info) }.is_ok() && info.dwBuildNumber >= MIN_BUILD
    }

    pub struct Overlay {
        thread_id: u32,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Overlay {
        pub fn start(message: &str) -> Result<Self> {
            if !available() {
                bail!("@agent.privacy_unsupported");
            }
            let message: Vec<u16> = message.encode_utf16().collect();
            let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
            let thread =
                std::thread::Builder::new().name("privacy".into()).spawn(move || run(message, ready_tx))?;
            match ready_rx.recv() {
                Ok(Ok(thread_id)) => {
                    info!("privacy mode on");
                    Ok(Self { thread_id, thread: Some(thread) })
                }
                Ok(Err(e)) => {
                    let _ = thread.join();
                    Err(anyhow!("{e}"))
                }
                Err(_) => {
                    let _ = thread.join();
                    bail!("privacy thread ended");
                }
            }
        }

        pub fn stop(&mut self) {
            let Some(thread) = self.thread.take() else { return };
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            let _ = thread.join();
            info!("privacy mode off");
        }
    }

    thread_local! {
        /// Text shown on the black screens (UTF-16).
        static MESSAGE: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
        static FONT: Cell<Option<HFONT>> = const { Cell::new(None) };
        /// A rebuild is already queued (every window receives WM_DISPLAYCHANGE).
        static REBUILD_QUEUED: Cell<bool> = const { Cell::new(false) };
    }

    /// The privacy thread: windows, hooks and their message loop, until WM_QUIT.
    fn run(message: Vec<u16>, ready: mpsc::Sender<Result<u32, String>>) {
        MESSAGE.with(|m| *m.borrow_mut() = message);
        let thread_id = unsafe { GetCurrentThreadId() };
        let mut windows = match create_windows() {
            Ok(windows) => windows,
            Err(e) => {
                warn!("privacy mode: {e:#}");
                let _ = ready.send(Err(format!("{e:#}")));
                return;
            }
        };
        let hmod = unsafe { GetModuleHandleW(None) }.ok().map(Into::into);
        let hooks = unsafe {
            (
                SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), hmod, 0),
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), hmod, 0),
            )
        };
        let (mouse, keyboard) = match hooks {
            (Ok(m), Ok(k)) => (m, k),
            (m, k) => {
                warn!(
                    "privacy mode: input hooks not installed ({:?}, {:?})",
                    m.as_ref().err(),
                    k.as_ref().err()
                );
                for hook in [m, k].into_iter().flatten() {
                    let _ = unsafe { UnhookWindowsHookEx(hook) };
                }
                destroy_windows(&mut windows);
                let _ = ready.send(Err("@agent.privacy_failed".into()));
                return;
            }
        };
        let _ = ready.send(Ok(thread_id));

        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if msg.hwnd.is_invalid() && msg.message == WM_REBUILD {
                REBUILD_QUEUED.set(false);
                destroy_windows(&mut windows);
                match create_windows() {
                    Ok(new) => windows = new,
                    // Without windows the input stays blocked: the session ends it.
                    Err(e) => warn!("privacy mode: windows not created again: {e:#}"),
                }
                continue;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = unsafe { UnhookWindowsHookEx(mouse) };
        let _ = unsafe { UnhookWindowsHookEx(keyboard) };
        destroy_windows(&mut windows);
        if let Some(font) = FONT.take() {
            let _ = unsafe { DeleteObject(HGDIOBJ(font.0)) };
        }
    }

    /// One black window for each screen, checked to be excluded from capture.
    fn create_windows() -> Result<Vec<HWND>> {
        let instance = unsafe { GetModuleHandleW(None) }?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("NuviraPrivacy"),
            ..Default::default()
        };
        // Fails harmlessly when the class is already registered (a second start).
        unsafe { RegisterClassExW(&class) };

        let mut windows = Vec::new();
        for rect in monitors() {
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    w!("NuviraPrivacy"),
                    w!("NuviraHub"),
                    WS_POPUP,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
            };
            let hwnd = match hwnd {
                Ok(hwnd) => hwnd,
                Err(e) => {
                    destroy_windows(&mut windows);
                    bail!("window not created: {e}");
                }
            };
            windows.push(hwnd);
            let excluded = unsafe {
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
                SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).and_then(|()| {
                    let mut affinity = 0u32;
                    GetWindowDisplayAffinity(hwnd, &mut affinity).map(|()| affinity)
                })
            };
            // Shown only once it is certain the capture will not see it: otherwise the
            // technician would get black too.
            if !matches!(excluded, Ok(a) if a == WDA_EXCLUDEFROMCAPTURE.0) {
                warn!("privacy mode: window not excluded from capture ({excluded:?})");
                destroy_windows(&mut windows);
                bail!("@agent.privacy_unsupported");
            }
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                SetTimer(Some(hwnd), 1, TOPMOST_MS, None);
            }
        }
        if windows.is_empty() {
            bail!("no screens");
        }
        debug!(screens = windows.len(), "privacy windows");
        Ok(windows)
    }

    fn destroy_windows(windows: &mut Vec<HWND>) {
        for hwnd in windows.drain(..) {
            let _ = unsafe { DestroyWindow(hwnd) };
        }
    }

    /// Rectangles of the screens, in physical pixels (the process is DPI aware).
    fn monitors() -> Vec<RECT> {
        unsafe extern "system" fn each(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
            let rects = unsafe { &mut *(data.0 as *mut Vec<RECT>) };
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
                rects.push(info.rcMonitor);
            }
            BOOL(1)
        }
        let mut rects = Vec::<RECT>::new();
        unsafe {
            let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut rects as *mut _ as isize));
        }
        rects
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_TIMER => {
                let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
                let _ = unsafe { SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags) };
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                if !REBUILD_QUEUED.replace(true) {
                    let _ =
                        unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_REBUILD, WPARAM(0), LPARAM(0)) };
                }
                LRESULT(0)
            }
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            // Only the end of privacy mode closes the windows.
            WM_CLOSE => LRESULT(0),
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// Black, with the message in grey in the middle.
    fn paint(hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut client) };
        unsafe {
            let black = CreateSolidBrush(COLORREF(0));
            FillRect(hdc, &client, black);
            let _ = DeleteObject(HGDIOBJ(black.0));
        }
        let font = FONT.get().unwrap_or_else(|| {
            let height = (client.bottom - client.top) / 40;
            let font = unsafe {
                CreateFontW(
                    height.max(16),
                    0,
                    0,
                    0,
                    FW_NORMAL.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    0,
                    w!("Segoe UI"),
                )
            };
            FONT.set(Some(font));
            font
        });
        let mut text = MESSAGE.with(|m| m.borrow().clone());
        if !text.is_empty() {
            unsafe {
                let old = SelectObject(hdc, HGDIOBJ(font.0));
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, COLORREF(0x00a0_a0a0));
                // A column two thirds of the screen wide; DT_CALCRECT gives its height.
                let width = (client.right - client.left) * 2 / 3;
                let mut rect = RECT { left: 0, top: 0, right: width, bottom: 0 };
                let format = DT_CENTER | DT_WORDBREAK | DT_NOPREFIX;
                DrawTextW(hdc, &mut text, &mut rect, format | DT_CALCRECT);
                let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
                let left = client.left + (client.right - client.left - w) / 2;
                let top = client.top + (client.bottom - client.top - h) / 2;
                let mut rect = RECT { left, top, right: left + w, bottom: top + h };
                DrawTextW(hdc, &mut text, &mut rect, format);
                SelectObject(hdc, old);
            }
        }
        let _ = unsafe { EndPaint(hwnd, &ps) };
    }

    /// Lets through only the events injected by NuviraHub.
    unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if event.dwExtraInfo != INJECTED_TAG {
                return LRESULT(1);
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if event.dwExtraInfo != INJECTED_TAG {
                return LRESULT(1);
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}
