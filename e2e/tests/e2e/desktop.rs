//! Windows only, real desktop (manual entry, `scripts/run-real-desktop.ps1` in the
//! signed-in session of a test machine whose screen may be used): what the real
//! desktop turns on besides the file manager. The agent (`serve_direct`, real
//! capture, as in the `real-desktop/files/*` cases) gets
//! - the shared clipboard both ways: `Clipboard` from the viewer lands in the
//!   Windows clipboard, text copied on Windows comes to the viewer as `Clipboard`
//!   (the clipboard text found before the case is put back after it);
//! - input: `MouseMove` moves the Windows pointer (read back with `GetCursorPos`),
//!   and a `Key` for ShiftLeft (harmless alone) is seen pressed and released by
//!   `GetAsyncKeyState`. No clicks, wheel or characters: they would act on
//!   whatever window has the focus. The pointer is put back where it was.

use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_e2e::runner::{Case, Ctx, Group, Need, ensure};
use nuvira_proto::v1::control_message::Msg;
use nuvira_proto::v1::{InputEvent, Key, MouseMove, input_event::Event};

use crate::xvfb::{Rig, wait_msg};

pub fn cases() -> Vec<Case> {
    vec![
        Case::new("real-desktop/clipboard/both-ways", clipboard)
            .group(Group::RealDesktop)
            .needs(&[Need::Wildcard]),
        Case::new("real-desktop/input/mouse-and-shift-key", input)
            .group(Group::RealDesktop)
            .needs(&[Need::Wildcard]),
    ]
}

fn powershell(script: &str) -> Result<String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .context("powershell")?;
    if !out.status.success() {
        bail!("powershell {script:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches(['\r', '\n']).to_string())
}

fn get_clipboard() -> Result<String> {
    powershell("$t = Get-Clipboard -Raw; if ($t) { [Console]::Out.Write($t) }")
}

fn set_clipboard(text: &str) -> Result<()> {
    // The text goes through an environment variable: no quoting of the script.
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", "Set-Clipboard -Value $env:E2E_CLIP"])
        .env("E2E_CLIP", text)
        .output()
        .context("powershell")?;
    ensure(out.status.success(), || format!("Set-Clipboard: {}", String::from_utf8_lossy(&out.stderr)))
}

/// Viewer -> Windows clipboard, then Windows clipboard -> viewer.
fn clipboard(ctx: &mut Ctx) -> Result<()> {
    let before = get_clipboard().unwrap_or_default();
    let rig = Rig::start(ctx)?;
    let r = rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        let text = format!("e2e from the viewer {}", std::process::id());
        let clip = nuvira_proto::v1::Clipboard { text: text.clone(), ..Default::default() };
        s.control().send_msg(Msg::Clipboard(clip)).await?;
        let t = Instant::now();
        let mut seen = String::new();
        while t.elapsed() < Duration::from_secs(10) {
            seen = get_clipboard().unwrap_or_default();
            if seen == text {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ctx.note(format!(
            "viewer -> Windows clipboard seen in {} ms (PowerShell polling): {seen:?}",
            t.elapsed().as_millis()
        ));
        ensure(seen == text, || format!("Windows clipboard {seen:?}"))?;
        let local = format!("e2e from Windows {}", std::process::id());
        let t = Instant::now();
        set_clipboard(&local)?;
        let got = wait_msg(&mut s, Duration::from_secs(15), |m| match m {
            Msg::Clipboard(c) if c.text == local => Some(c.text.clone()),
            _ => None,
        })
        .await?;
        ctx.note(format!(
            "Windows clipboard -> viewer in {} ms (Set-Clipboard included): {got:?}",
            t.elapsed().as_millis()
        ));
        s.close().await;
        anyhow::Ok(())
    });
    if let Err(e) = &r {
        ctx.note(format!("failed before teardown: {e:#}"));
    }
    drop(rig);
    if !before.is_empty() {
        let _ = set_clipboard(&before);
    }
    ctx.note(format!("clipboard text before the case put back ({} chars)", before.chars().count()));
    r
}

/// Wakes the monitor (a 1-pixel pointer move there and back, through SendInput)
/// and keeps it on while the case runs (`SetThreadExecutionState`, released when
/// the case process ends).
pub(crate) fn keep_display_on(ctx: &Ctx) {
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
    };
    let previous =
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };
    let mv = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 },
        },
    };
    let inputs = [mv(1), mv(-1)];
    let sent =
        unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    // Time for the monitor to come back and DWM to compose again.
    std::thread::sleep(Duration::from_secs(2));
    ctx.note(format!("display woken (SendInput sent {sent}) and kept on (previous state {previous:#x})"));
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos, SetProcessDPIAware};

    pub fn dpi_aware() {
        unsafe { SetProcessDPIAware() };
    }

    pub fn cursor() -> Option<(i32, i32)> {
        let mut p = POINT { x: 0, y: 0 };
        (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
    }

    pub fn put_cursor(at: (i32, i32)) {
        unsafe { SetCursorPos(at.0, at.1) };
    }

    /// VK_LSHIFT held down.
    pub fn left_shift_down() -> bool {
        (unsafe { GetAsyncKeyState(0xA0) } as u16 & 0x8000) != 0
    }
}

/// `MouseMove` moves the pointer; ShiftLeft down and up is seen by Windows.
fn input(ctx: &mut Ctx) -> Result<()> {
    win::dpi_aware();
    let rig = Rig::start(ctx)?;
    let start_at = win::cursor();
    let r = rig.vrt.block_on(async {
        let mut s = rig.session().await?;
        let (w, h) = (s.format.width as f32, s.format.height as f32);
        ctx.note(format!("captured screen {w}x{h}, pointer before {start_at:?}"));
        for (x, y) in [(0.25f32, 0.5f32), (0.75, 0.25), (0.5, 0.75)] {
            let ev = InputEvent { event: Some(Event::MouseMove(MouseMove { x, y })) };
            s.input().send(&ev).await?;
            // The agent maps 0..1 onto 0..(size - 1) of the captured screen.
            let (want_x, want_y) = ((x * (w - 1.0)).round() as i32, (y * (h - 1.0)).round() as i32);
            let t = Instant::now();
            let mut at = (0, 0);
            while t.elapsed() < Duration::from_secs(5) {
                at = win::cursor().context("GetCursorPos")?;
                if (at.0 - want_x).abs() <= 2 && (at.1 - want_y).abs() <= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            ctx.note(format!(
                "({x}, {y}) -> pointer {at:?}, expected ({want_x}, {want_y}), {} ms",
                t.elapsed().as_millis()
            ));
            ensure((at.0 - want_x).abs() <= 2 && (at.1 - want_y).abs() <= 2, || {
                format!("pointer at {at:?}, expected ({want_x}, {want_y})")
            })?;
        }
        ensure(!win::left_shift_down(), || "ShiftLeft already down before the case".into())?;
        for down in [true, false] {
            let ev = InputEvent { event: Some(Event::Key(Key { code: "ShiftLeft".into(), down })) };
            s.input().send(&ev).await?;
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(5) && win::left_shift_down() != down {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let state = win::left_shift_down();
            ctx.note(format!(
                "ShiftLeft {} -> GetAsyncKeyState down={state} in {} ms",
                if down { "down" } else { "up" },
                t.elapsed().as_millis()
            ));
            ensure(state == down, || format!("ShiftLeft down={state}, expected {down}"))?;
        }
        s.close().await;
        anyhow::Ok(())
    });
    // Release Shift whatever happened (the agent also releases held keys at the end).
    if let Err(e) = &r {
        ctx.note(format!("failed before teardown: {e:#}"));
    }
    drop(rig);
    if let Some(at) = start_at {
        win::put_cursor(at);
    }
    ensure(!win::left_shift_down(), || "ShiftLeft still down after the session".into())?;
    r
}
