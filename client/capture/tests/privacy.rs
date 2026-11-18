//! Phase 2 characterisation: privacy mode on Windows (black windows excluded
//! from capture, local input blocked, injected input let through).
//!
//! Blacks out the screen and blocks the local mouse and keyboard for a few
//! seconds: only with `NUVIRA_TEST_SCREEN=1` and `NUVIRA_TEST_INPUT=1` on a test
//! machine (PC-LABORATORIO), never where someone works. Without them: `NOT RUN`.
//!
//! Pinned (Windows 10 2004+): one top-most window per screen of class
//! `NuviraPrivacy`, with display affinity `WDA_EXCLUDEFROMCAPTURE`; DXGI and GDI
//! capture still see the desktop under it; a mouse move sent without the
//! NuviraHub tag is dropped, the injector's (tagged) one moves the pointer; after
//! `stop` the windows are gone and untagged input works again.
//!
//! Linux (X11 gamma + XInput, GNOME, KDE) and macOS start a child process that
//! is this same program (`overlay_if_asked` in `main`): a libtest binary cannot
//! be that child, so they are not covered here (see the Phase 2 report).

mod common;

#[cfg(windows)]
mod win {
    use std::time::Duration;

    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
        MOUSEINPUT, SendInput,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetCursorPos, GetSystemMetrics, GetWindowDisplayAffinity, SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursorPos, WDA_EXCLUDEFROMCAPTURE,
    };
    use windows::core::w;

    use super::common::{self, gates, report, wait_for_pattern};

    fn cursor() -> (i32, i32) {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) }.expect("GetCursorPos");
        (p.x, p.y)
    }

    /// A mouse move as any other program (or a real mouse) would send it: no tag.
    fn untagged_move(x: i32, y: i32) {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let dx = ((x - vx) as f64 * 65535.0 / (vw - 1).max(1) as f64).round() as i32;
        let dy = ((y - vy) as f64 * 65535.0 / (vh - 1).max(1) as f64).round() as i32;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
        std::thread::sleep(Duration::from_millis(150));
    }

    fn near(a: (i32, i32), b: (i32, i32)) -> bool {
        (a.0 - b.0).abs() <= 3 && (a.1 - b.1).abs() <= 3
    }

    fn privacy_window() -> Option<(isize, u32)> {
        let hwnd = unsafe { FindWindowW(w!("NuviraPrivacy"), None) }.ok()?;
        let mut affinity = 0u32;
        unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }.ok()?;
        Some((hwnd.0 as isize, affinity))
    }

    #[test]
    fn privacy_mode_hides_from_the_screen_not_from_capture_and_blocks_local_input() {
        if !gates(&[
            ("NUVIRA_TEST_SCREEN", "a real desktop session (the screen turns black)"),
            ("NUVIRA_TEST_INPUT", "permission to block this machine's mouse and keyboard for seconds"),
        ]) {
            return;
        }
        let _screen = common::screen_lock();
        nuvira_input::set_dpi_awareness();
        common::win::wake_display();
        assert!(nuvira_input::privacy::available(), "Windows 10 2004 or later");
        let start_pos = cursor();
        let (left, top, width, height) = common::win::primary_rect();
        let window = common::win::PatternWindow::show(left, top, width, height);
        let screen = nuvira_input::ScreenRect { left, top, width, height };
        let mut injector = nuvira_input::open(screen).expect("injector");
        let at = |fx: f32, fy: f32| {
            (left + (fx * (width - 1) as f32).round() as i32, top + (fy * (height - 1) as f32).round() as i32)
        };
        let tagged_move = |inj: &mut Box<dyn nuvira_input::InputInjector>, fx: f32, fy: f32| {
            let event = nuvira_proto::v1::InputEvent {
                event: Some(nuvira_proto::v1::input_event::Event::MouseMove(nuvira_proto::v1::MouseMove {
                    x: fx,
                    y: fy,
                })),
            };
            inj.inject(&event).expect("inject");
            std::thread::sleep(Duration::from_millis(150));
        };

        assert!(privacy_window().is_none(), "no privacy window before");
        // Untagged input works before privacy mode.
        untagged_move(at(0.3, 0.3).0, at(0.3, 0.3).1);
        assert!(near(cursor(), at(0.3, 0.3)), "untagged move before privacy: {:?}", cursor());

        let privacy =
            nuvira_input::privacy::start("NuviraHub test: privacy mode", &|_| {}).expect("privacy on");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (hwnd, affinity) = privacy_window().expect("privacy window");
            report(&format!("privacy window {hwnd:#x}, affinity {affinity:#x}"));
            assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0, "excluded from capture");

            // The capture still sees the desktop (the pattern), not black.
            let mut capturer = nuvira_capture::open(None).expect("capture");
            window.set_shift(3);
            let (m, _, frames) = wait_for_pattern(capturer.as_mut(), 3, 4, Duration::from_secs(8));
            report(&format!("DXGI under privacy: {m:?} after {frames} frames"));
            assert!(m.expect("frame").ok(), "DXGI sees the desktop under the privacy windows");
            drop(capturer);
            unsafe { std::env::set_var("NUVIRA_CAPTURE", "gdi") };
            let gdi = nuvira_capture::open(None).map(|mut c| {
                window.set_shift(7);
                wait_for_pattern(c.as_mut(), 7, 4, Duration::from_secs(8))
            });
            unsafe { std::env::remove_var("NUVIRA_CAPTURE") };
            let (m, _, _) = gdi.expect("GDI capture");
            report(&format!("GDI under privacy: {m:?}"));
            assert!(m.expect("frame").ok(), "GDI sees the desktop under the privacy windows");

            // Local (untagged) input is dropped; the injector's goes through.
            let before = cursor();
            untagged_move(at(0.7, 0.7).0, at(0.7, 0.7).1);
            let after = cursor();
            report(&format!("untagged move under privacy: {before:?} -> {after:?}"));
            assert!(near(after, before), "untagged move blocked");
            tagged_move(&mut injector, 0.6, 0.4);
            assert!(near(cursor(), at(0.6, 0.4)), "injected move let through: {:?}", cursor());
        }));
        privacy.stop();
        std::thread::sleep(Duration::from_millis(300));
        let gone = privacy_window().is_none();
        untagged_move(at(0.4, 0.6).0, at(0.4, 0.6).1);
        let works = near(cursor(), at(0.4, 0.6));
        let _ = unsafe { SetCursorPos(start_pos.0, start_pos.1) };
        drop(window);
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
        assert!(gone, "privacy windows gone after stop");
        assert!(works, "untagged input works again after stop");
    }
}

#[cfg(not(windows))]
#[test]
fn privacy_mode_windows_only_here() {
    let _ = std::io::Write::write_all(
        &mut std::io::stderr(),
        b"NOT RUN (needs Windows: privacy mode on Linux/macOS runs as a child process of the agent binary)\n",
    );
}
