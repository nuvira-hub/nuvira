//! B-MICRO-18 clipboard poll with unchanged content (`nuvira_clipboard::Clipboard::poll`,
//! what the clipboard sync thread of every session does every 400 ms; review finding
//! VWR-6, Phase 3 EXP-6: 18 ms per poll with a 1080p image on MSI-UBUNTU).
//!
//! Linux X11 only. The case runs in a child process of its own whose `DISPLAY` is a
//! private Xvfb that it starts ([`DISPLAY`]); the CLIPBOARD is held by `xclip`, another
//! process, as a program's copy is. Cases: `poll-unchanged/image-1920x1080` (a 1080p
//! PNG of a desktop-like picture, the "screenshot in the clipboard" case) and
//! `poll-unchanged/text` (a short text). Each poll must report nothing (the content
//! was there when the clipboard was opened). Needs `Xvfb` and `xclip` in the PATH and
//! no `WAYLAND_DISPLAY`; otherwise not-runnable. A poll that reads the clipboard makes
//! Xvfb and xclip work too, so the noise guard marks those reps `noisy` (the work of
//! the other processes is part of what a poll costs).

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::{MicroCase, not_runnable, record_sampler};
use crate::measure::Gate;
use crate::registry::{BOTH_TIERS, CaseResult, Registry, RunCtx};

/// The private X display of the case (its child process gets it as `DISPLAY`).
const DISPLAY: &str = ":118";

#[derive(Clone, Copy)]
enum Content {
    Image,
    Text,
}

pub fn register(reg: &mut Registry) {
    for (name, content) in
        [("poll-unchanged/image-1920x1080", Content::Image), ("poll-unchanged/text", Content::Text)]
    {
        reg.add_case(
            MicroCase::new("B-MICRO-18", name, BOTH_TIERS, move |ctx, r| run(ctx, r, content))
                .runnable(runnable)
                .config(&["LINUX-X11"])
                .env("DISPLAY", DISPLAY),
        );
    }
}

fn runnable() -> Result<(), String> {
    super::only_linux()?;
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Err("measures the X11 path: run it without WAYLAND_DISPLAY (e.g. over SSH)".into());
    }
    for tool in ["Xvfb", "xclip"] {
        if !in_path(tool) {
            return Err(format!("needs {tool} in the PATH"));
        }
    }
    Ok(())
}

fn in_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

/// Ends the child processes (xclip, then Xvfb) at the end, also on an error:
/// SIGTERM first, so that Xvfb removes its socket and lock file.
struct Stop(Vec<Child>);

impl Drop for Stop {
    fn drop(&mut self) {
        for c in self.0.iter_mut().rev() {
            #[cfg(unix)]
            {
                // SAFETY: kill(2) on our own child's pid.
                unsafe { libc::kill(c.id() as libc::pid_t, libc::SIGTERM) };
                let end = Instant::now() + Duration::from_secs(3);
                while Instant::now() < end && matches!(c.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// A server answers on the display's socket (a stale socket file refuses).
fn display_answers() -> bool {
    #[cfg(unix)]
    return std::os::unix::net::UnixStream::connect(format!("/tmp/.X11-unix/X{}", &DISPLAY[1..])).is_ok();
    #[cfg(not(unix))]
    false
}

fn run(ctx: &mut RunCtx, mut r: CaseResult, content: Content) -> Result<CaseResult> {
    if std::env::var("DISPLAY").as_deref() != Ok(DISPLAY) {
        bail!("DISPLAY is not {DISPLAY}: the case must run in its own child process");
    }
    if display_answers() {
        return Ok(not_runnable(r, format!("display {DISPLAY} is already in use")));
    }
    let mut stop = Stop(Vec::new());
    stop.0.push(
        Command::new("Xvfb")
            .args([DISPLAY, "-screen", "0", "1920x1080x24", "-nolisten", "tcp"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting Xvfb")?,
    );
    let start = Instant::now();
    while !display_answers() {
        if !matches!(stop.0[0].try_wait(), Ok(None)) {
            return Ok(not_runnable(r, format!("Xvfb {DISPLAY} ended at start (display in use?)")));
        }
        if start.elapsed() > Duration::from_secs(10) {
            bail!("Xvfb {DISPLAY} did not start");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The content, held by xclip (it serves the selection until it is killed).
    let (target, bytes) = match content {
        Content::Image => ("image/png", desktop_png(1920, 1080)?),
        Content::Text => ("UTF8_STRING", "NuviraHub clipboard poll: unchanged text".as_bytes().to_vec()),
    };
    r.param("target", target).param("bytes", bytes.len());
    let dir = ctx.inputs.join("clipboard");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("content-{}", std::process::id()));
    std::fs::write(&file, &bytes)?;
    stop.0.push(
        Command::new("xclip")
            .args(["-quiet", "-selection", "clipboard", "-t", target, "-i"])
            .arg(&file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting xclip")?,
    );
    let start = Instant::now();
    loop {
        let out = Command::new("xclip").args(["-o", "-selection", "clipboard", "-t", "TARGETS"]).output()?;
        if out.status.success() && String::from_utf8_lossy(&out.stdout).contains(target) {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            bail!("xclip does not hold the clipboard");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(&file);

    // Opened with the content already there: nothing to report from now on.
    let mut clipboard = nuvira_clipboard::Clipboard::new().context("clipboard")?;
    for _ in 0..5 {
        if let Some(c) = clipboard.poll() {
            bail!("unchanged clipboard reported as new: {c:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut reported = 0u64;
    let reps = super::sampler(ctx).run(|| {
        if std::hint::black_box(clipboard.poll()).is_some() {
            reported += 1;
        }
    });
    if reported > 0 {
        bail!("unchanged clipboard reported as new {reported} times");
    }
    record_sampler(&mut r, &reps, "ms_poll", false, Gate::Micro);
    Ok(r)
}

/// A desktop-like 1080p picture as PNG: flat bars, gradients, text-like detail
/// and a photo-like window with noise (about the size of a real screenshot,
/// some hundreds of KB, neither a flat image nor pure noise).
fn desktop_png(width: u32, height: u32) -> Result<Vec<u8>> {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    let mut seed = 0x2545_f491_u32;
    for y in 0..height {
        for x in 0..width {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 24) as u8;
            let px = if y < 40 {
                [32, 34, 40, 255]
            } else if x < 300 {
                let v = 230 - ((y / 36) % 2 * 12) as u8;
                [v, v, v + 10, 255]
            } else if (900..1500).contains(&x) && (300..700).contains(&y) {
                [(x / 4) as u8 ^ (noise >> 2), (y / 3) as u8 ^ (noise >> 3), 90 + (noise >> 2), 255]
            } else if (x / 7 + y / 3) % 11 == 0 && (y / 18) % 3 != 2 {
                [20, 20, 20, 255]
            } else {
                [(x * 255 / width) as u8, (y * 255 / height) as u8, 200, 255]
            };
            rgba.extend_from_slice(&px);
        }
    }
    let mut png = Vec::new();
    let mut enc = png::Encoder::new(&mut png, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.write_header()?.write_image_data(&rgba)?;
    Ok(png)
}
