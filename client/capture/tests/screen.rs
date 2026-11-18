//! Phase 2 characterisation: real screen capture against a known pattern.
//!
//! Needs a real desktop: run with `NUVIRA_TEST_SCREEN=1` in the person's
//! session (PC-LABORATORIO through a scheduled task, MSI-UBUNTU, the Linux VMs).
//! Without it every test prints `NOT RUN (needs ...)` and returns: that is not a
//! pass. Never on the NUC (it takes the user's screen).
//!
//! What is pinned (current behaviour, v0.28.0):
//! - the default capture (DXGI on Windows, X11 or PipeWire on Linux) returns the
//!   pixels on the screen, at the screen's size, and a change of the screen comes
//!   as a new frame;
//! - Windows: GDI capture (`NUVIRA_CAPTURE=gdi`) and the GPU texture output
//!   (`enable_gpu_output`) give the same picture; "all screens" on a machine
//!   with one screen is refused ("only one screen");
//! - Linux: "all screens" with one screen is that screen (index 0); on Wayland the
//!   DMA-BUF path gives the same picture when linear buffers are asked for.

#![cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code, unused_imports))]

mod common;

use std::time::Duration;

use common::{gate, report, screen_lock, wait_for_pattern};

const TIMEOUT: Duration = Duration::from_secs(8);

fn screen_gate() -> bool {
    gate("NUVIRA_TEST_SCREEN", "a real desktop session whose screen the test may cover")
}

/// Exact on Windows (BGRA copied as it is).
#[cfg(windows)]
const TOLERANCE: u8 = 4;
/// PipeWire / X11 on a desktop that may dither or convert colours.
#[cfg(not(windows))]
const TOLERANCE: u8 = 12;

#[cfg(target_os = "linux")]
fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// The pattern window over `display` (Linux: logical coordinates of the root).
#[cfg(windows)]
fn pattern_over(d: &nuvira_capture::Display) -> common::win::PatternWindow {
    common::win::PatternWindow::show(d.left, d.top, d.width, d.height)
}

#[cfg(target_os = "linux")]
fn pattern_over(d: &nuvira_capture::Display) -> common::x11::PatternWindow {
    common::x11::PatternWindow::show(d.left as i16, d.top as i16, d.width as u16, d.height as u16)
}

fn primary() -> nuvira_capture::Display {
    let list = common::displays();
    report(&format!("displays: {list:?}"));
    list.iter().find(|d| d.primary).or(list.first()).cloned().expect("at least one display")
}

#[cfg(any(windows, target_os = "linux"))]
fn setup() {
    let _ = tracing_subscriber::fmt().with_env_filter("nuvira_capture=debug,nuvira_portal=debug").try_init();
    #[cfg(windows)]
    {
        nuvira_input::set_dpi_awareness();
        common::win::wake_display();
    }
    // Unattended access's own API on GNOME (no dialog); the portal elsewhere.
    #[cfg(target_os = "linux")]
    if std::env::var_os("NUVIRA_TEST_PORTAL").is_none() {
        nuvira_portal::set_direct(true);
    }
}

/// Pattern, then the same pattern shifted: both must be captured.
#[cfg(any(windows, target_os = "linux"))]
fn pattern_and_change(capturer: &mut dyn nuvira_capture::Capturer, set_shift: &dyn Fn(usize), what: &str) {
    let (m, size, frames) = wait_for_pattern(capturer, 0, TOLERANCE, TIMEOUT);
    report(&format!("{what}: first pattern {m:?} size {size:?} after {frames} frames"));
    let m = m.expect("no frame captured");
    if !m.ok()
        && let Some(f) = capturer.last_frame()
    {
        let seen: Vec<[u8; 3]> =
            (0..16).map(|i| common::sample(f.width, f.height, f.stride, &f.data, i % 4, i / 4)).collect();
        report(&format!("{what}: block centres seen {seen:?}"));
    }
    assert!(m.ok(), "{what}: the captured frame is not the pattern: {m:?}");
    set_shift(5);
    let (m, _, frames) = wait_for_pattern(capturer, 5, TOLERANCE, TIMEOUT);
    report(&format!("{what}: changed pattern {m:?} after {frames} frames"));
    assert!(m.expect("no frame").ok(), "{what}: the change of the screen was not captured");
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn default_capture_sees_the_pattern_and_its_change() {
    if !screen_gate() {
        return;
    }
    let _screen = screen_lock();
    setup();
    let display = primary();
    let window = pattern_over(&display);
    let mut capturer = nuvira_capture::open(None).expect("open capture");
    let d = capturer.display().clone();
    report(&format!("capturing {d:?}"));
    assert!(d.primary || d.index == 0, "open(None) is the primary screen");
    pattern_and_change(capturer.as_mut(), &|s| window.set_shift(s), "default capture");
    let f = capturer.last_frame().expect("last frame");
    // Windows and X11: the frame is the screen's size in pixels. On Wayland the
    // display is in logical pixels and the frame in real ones (same at 100%).
    assert_eq!((f.width, f.height), (display.width, display.height), "frame size is the screen's");
    assert!(f.stride >= f.width as usize * 4);
    // A last frame is kept for keyframes on a still screen.
    assert!(capturer.last_frame().is_some());
}

#[cfg(windows)]
#[test]
fn gdi_capture_sees_the_same_pattern() {
    if !screen_gate() {
        return;
    }
    let _screen = screen_lock();
    setup();
    let display = primary();
    // SAFETY: the tests of this binary that read the variable hold the screen lock.
    unsafe { std::env::set_var("NUVIRA_CAPTURE", "gdi") };
    let gdi_displays = nuvira_capture::displays().expect("GDI displays");
    let result = std::panic::catch_unwind(|| {
        let window = pattern_over(&display);
        let mut capturer = nuvira_capture::open(None).expect("open GDI capture");
        report(&format!("GDI capturing {:?}", capturer.display()));
        pattern_and_change(capturer.as_mut(), &|s| window.set_shift(s), "GDI capture");
        let f = capturer.last_frame().expect("frame");
        assert_eq!((f.width, f.height), (display.width, display.height));
    });
    unsafe { std::env::remove_var("NUVIRA_CAPTURE") };
    let dxgi_displays = common::displays();
    report(&format!("GDI displays {gdi_displays:?}"));
    // The same screens, in the same places, whichever API lists them.
    assert_eq!(gdi_displays.len(), dxgi_displays.len());
    for (g, d) in gdi_displays.iter().zip(&dxgi_displays) {
        assert_eq!(
            (g.left, g.top, g.width, g.height, g.primary),
            (d.left, d.top, d.width, d.height, d.primary)
        );
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[cfg(windows)]
fn read_texture(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
) -> (u32, u32, usize, Vec<u8>) {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING,
    };
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging)).expect("staging texture");
        let staging = staging.expect("staging");
        let context = device.GetImmediateContext().expect("context");
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).expect("map");
        let pitch = mapped.RowPitch as usize;
        let data =
            std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * desc.Height as usize).to_vec();
        context.Unmap(&staging, 0);
        (desc.Width, desc.Height, pitch, data)
    }
}

#[cfg(windows)]
#[test]
fn gpu_output_texture_holds_the_pattern() {
    if !screen_gate() {
        return;
    }
    let _screen = screen_lock();
    setup();
    let display = primary();
    let window = pattern_over(&display);
    let mut capturer = nuvira_capture::open(None).expect("open capture");
    let device = capturer.enable_gpu_output().expect("DXGI capture gives a GPU device");
    let start = std::time::Instant::now();
    let mut last = None;
    let mut shift = 0;
    let mut seen_both = false;
    while start.elapsed() < Duration::from_secs(16) {
        let Some(f) = capturer.next_frame(Duration::from_millis(100)).expect("capture") else {
            if last.is_none() {
                // Nothing changed since the duplication started: change the screen.
                window.set_shift(shift);
            }
            continue;
        };
        assert!(f.data.is_empty(), "GPU mode: no picture in RAM");
        let texture = f.texture.clone().expect("GPU mode: a texture");
        let (w, h, pitch, data) = read_texture(&device, &texture);
        let m = common::compare(f.width.min(w), f.height.min(h), pitch, &data, shift, TOLERANCE);
        if m.ok() {
            report(&format!(
                "GPU texture {w}x{h} (frame {}x{}) matches shift {shift}: {m:?}",
                f.width, f.height
            ));
            assert_eq!((f.width, f.height), (display.width, display.height));
            if shift == 5 {
                seen_both = true;
                break;
            }
            shift = 5;
            window.set_shift(5);
        }
        last = Some(m);
    }
    assert!(seen_both, "GPU texture never held the pattern and its change: last {last:?}");
}

#[cfg(windows)]
#[test]
fn all_screens_with_one_screen_is_refused_on_windows() {
    if !screen_gate() {
        return;
    }
    let _screen = screen_lock();
    setup();
    let list = common::displays();
    let result = nuvira_capture::open(Some(nuvira_capture::ALL_DISPLAYS));
    if list.len() < 2 {
        let err = result.err().map(|e| format!("{e:#}"));
        report(&format!("all screens with {} screen: {err:?}", list.len()));
        assert!(err.as_deref().is_some_and(|e| e.contains("only one screen")), "refused with one screen");
        return;
    }
    // Two or more screens: one pattern over the bounding box of the virtual desktop.
    let left = list.iter().map(|d| d.left).min().unwrap();
    let top = list.iter().map(|d| d.top).min().unwrap();
    let right = list.iter().map(|d| d.left + d.width as i32).max().unwrap();
    let bottom = list.iter().map(|d| d.top + d.height as i32).max().unwrap();
    let window = common::win::PatternWindow::show(left, top, (right - left) as u32, (bottom - top) as u32);
    let mut capturer = result.expect("all screens");
    let d = capturer.display().clone();
    report(&format!("all screens: {d:?}"));
    assert_eq!(d.index, nuvira_capture::ALL_DISPLAYS);
    assert_eq!((d.left, d.top), (left, top));
    pattern_and_change(capturer.as_mut(), &|s| window.set_shift(s), "all screens");
}

#[cfg(target_os = "linux")]
#[test]
fn all_screens_with_one_screen_is_that_screen_on_linux() {
    if !screen_gate() {
        return;
    }
    let _screen = screen_lock();
    setup();
    let list = common::displays();
    if list.len() > 1 {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"NOT RUN (needs one screen exactly; multi-monitor Linux is a manual entry)\n",
        );
        return;
    }
    let display = primary();
    let window = pattern_over(&display);
    let mut capturer = nuvira_capture::open(Some(nuvira_capture::ALL_DISPLAYS)).expect("all screens");
    let d = capturer.display().clone();
    report(&format!("all screens on Linux: {d:?}"));
    // One screen (X11: one RandR monitor or none; Wayland: one shared screen):
    // "all screens" is that screen, index 0, named "Screen" (X11 names it "all
    // screens" with index ALL_DISPLAYS only with two monitors or more).
    assert_eq!((d.index, d.name.as_str(), d.primary), (0, "Screen", true));
    assert_eq!((d.width, d.height), (display.width, display.height));
    pattern_and_change(capturer.as_mut(), &|s| window.set_shift(s), "all screens");
}

/// Wayland: with linear DMA-BUFs asked for (as a VA-API encoder that cannot
/// import tiled ones would), the frames come as DMA-BUFs and read back as the
/// pattern. X11 has no DMA-BUF path.
#[cfg(target_os = "linux")]
#[test]
fn wayland_dmabuf_frames_hold_the_pattern() {
    if !screen_gate() {
        return;
    }
    if !wayland() {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), b"NOT RUN (needs a Wayland session)\n");
        return;
    }
    let _screen = screen_lock();
    setup();
    nuvira_capture::set_dmabuf_modifiers(vec![nuvira_capture::dmabuf::MODIFIER_LINEAR]);
    let display = primary();
    let window = pattern_over(&display);
    let mut capturer = nuvira_capture::open(None).expect("open capture");
    let start = std::time::Instant::now();
    let (mut dmabufs, mut ram, mut shift, mut matched) = (0, 0, 0, 0);
    let mut out = Vec::new();
    let mut last = None;
    while start.elapsed() < Duration::from_secs(16) && matched < 2 {
        let Some(f) = capturer.next_frame(Duration::from_millis(100)).expect("capture") else {
            window.set_shift(shift);
            continue;
        };
        let m = if let Some(buf) = &f.dmabuf {
            dmabufs += 1;
            assert!(f.data.is_empty(), "DMA-BUF frame: no picture in RAM");
            buf.read(&mut out).expect("linear DMA-BUF readable");
            common::compare(buf.width, buf.height, buf.width as usize * 4, &out, shift, TOLERANCE)
        } else {
            ram += 1;
            common::compare(f.width, f.height, f.stride, &f.data, shift, TOLERANCE)
        };
        if m.ok() {
            matched += 1;
            shift = 5;
            window.set_shift(5);
        }
        last = Some(m);
    }
    nuvira_capture::set_dmabuf_modifiers(Vec::new());
    report(&format!("DMA-BUF frames {dmabufs}, RAM frames {ram}, matched {matched}, last {last:?}"));
    assert!(matched >= 2, "the pattern and its change were not captured: {last:?}");
    // A compositor that renders without a GPU (virtual machines: llvmpipe)
    // offers no DMA-BUF and the stream stays in shared memory, which is what is
    // pinned then. `NUVIRA_TEST_EXPECT_DMABUF=1` on machines with a real GPU
    // (MSI-UBUNTU) makes DMA-BUFs required.
    if dmabufs == 0 && std::env::var("NUVIRA_TEST_EXPECT_DMABUF").as_deref() != Ok("1") {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"NOT RUN (needs a compositor that offers DMA-BUFs, NUVIRA_TEST_EXPECT_DMABUF=1: only the shared-memory fallback was checked)
",
        );
        return;
    }
    assert!(dmabufs > 0, "linear DMA-BUFs asked for but none came (RAM frames: {ram})");
}

/// KNOWN BEHAVIOUR (Phase 2 finding, 2026-11-18), machine dependent: while the
/// monitor is off for power saving, on PC-LABORATORIO (Windows 10, desktop
/// monitor) DWM composes nothing and DXGI duplication gives no frame at all,
/// not even the first one, while GDI still reads the windows: an agent started
/// there sends no picture until something wakes the display (the first injected
/// input does). On PC-LENOVO-ALE (Windows 11, laptop panel, hybrid GPU) DXGI
/// keeps giving a frame for each change (4 of 4). The count is reported, not
/// asserted; GDI with the display off and DXGI after waking are asserted.
#[cfg(windows)]
#[test]
fn characterises_dxgi_gives_no_frame_while_the_display_is_off() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, PostMessageW, SC_MONITORPOWER, WM_SYSCOMMAND,
    };
    if !screen_gate() {
        return;
    }
    let _screen = screen_lock();
    setup();
    let display = primary();
    let window = pattern_over(&display);
    // Monitor off (2), as the power plan does after its timeout.
    unsafe {
        let _ =
            PostMessageW(Some(HWND_BROADCAST), WM_SYSCOMMAND, WPARAM(SC_MONITORPOWER as usize), LPARAM(2));
    }
    std::thread::sleep(Duration::from_secs(3));
    let mut capturer = nuvira_capture::open(None).expect("open capture");
    let mut frames = 0;
    for i in 0..30 {
        if let Ok(Some(_)) = capturer.next_frame(Duration::from_millis(100)) {
            frames += 1;
        }
        if i % 10 == 5 {
            window.set_shift(i);
        }
    }
    // GDI reads the windows meanwhile.
    unsafe { std::env::set_var("NUVIRA_CAPTURE", "gdi") };
    let gdi = nuvira_capture::open(None).map(|mut c| wait_for_pattern(c.as_mut(), 25, TOLERANCE, TIMEOUT));
    unsafe { std::env::remove_var("NUVIRA_CAPTURE") };
    common::win::wake_display();
    let (after, _, _) = wait_for_pattern(capturer.as_mut(), 25, TOLERANCE, TIMEOUT);
    report(&format!(
        "display off: DXGI frames {frames}, GDI {:?}, DXGI after waking {after:?}",
        gdi.as_ref().map(|g| &g.0)
    ));
    // DXGI frames with the display off: 0 on PC-LABORATORIO, one per change (4)
    // on PC-LENOVO-ALE (see above): reported, not asserted.
    assert!(gdi.expect("GDI").0.expect("GDI frame").ok(), "GDI reads the windows with the display off");
    assert!(after.expect("DXGI frame after waking").ok(), "DXGI frames come back once the display is on");
}

/// Wayland: without an open remote desktop session the screens are not listed
/// (listing would need the permission dialog).
#[cfg(target_os = "linux")]
#[test]
fn wayland_lists_no_screen_without_a_session() {
    if !screen_gate() {
        return;
    }
    if !wayland() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"NOT RUN (needs a Wayland session)
",
        );
        return;
    }
    let _screen = screen_lock();
    common::refresh_displays();
    assert!(nuvira_portal::current().is_none());
    assert!(nuvira_capture::displays().expect("displays").is_empty(), "no session, no screens listed");
    assert!(!common::displays().is_empty(), "listed once a session is open");
}

/// macOS (ScreenCaptureKit): no pattern window is written for macOS yet, so only
/// the shape of the capture is pinned: frames of the screen's size, and "all
/// screens" refused with one screen (as on Windows). Needs Screen Recording
/// permission for the test binary's terminal. A manual entry until a Mac.
#[cfg(target_os = "macos")]
#[test]
fn macos_capture_gives_frames_of_the_screen_size() {
    if !screen_gate() {
        return;
    }
    let _screen = screen_lock();
    let list = common::displays();
    let display = primary();
    let mut capturer = nuvira_capture::open(None).expect("open capture (Screen Recording permission?)");
    let start = std::time::Instant::now();
    let mut size = None;
    while start.elapsed() < TIMEOUT && size.is_none() {
        if let Ok(Some(f)) = capturer.next_frame(Duration::from_millis(200)) {
            assert!(!f.data.is_empty() || f.pixel_buffer.is_some(), "a picture in RAM or in a pixel buffer");
            if !f.data.is_empty() {
                assert!(f.stride >= f.width as usize * 4);
            }
            size = Some((f.width, f.height));
        }
    }
    report(&format!("display {display:?}, frame {size:?}"));
    let (w, h) = size.expect("a frame within the timeout");
    assert!(w > 0 && h > 0);
    assert_eq!((w, h), (display.width, display.height), "frame size is the screen's (pixels)");
    drop(capturer);
    if list.len() < 2 {
        let err = nuvira_capture::open(Some(nuvira_capture::ALL_DISPLAYS)).err().map(|e| format!("{e:#}"));
        report(&format!("all screens with one screen: {err:?}"));
        assert!(err.as_deref().is_some_and(|e| e.contains("only one screen")));
    }
    let _ = std::io::Write::write_all(
        &mut std::io::stderr(),
        b"NOT RUN (needs a macOS pattern window, not written: pixels not compared on macOS)
",
    );
}
