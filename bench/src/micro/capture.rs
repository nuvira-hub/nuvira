//! B-MICRO-17 real screen capture per frame (`nuvira_capture::open(None)`,
//! `Capturer::next_frame`; GDI with `NUVIRA_CAPTURE=gdi`, GPU textures with
//! `enable_gpu_output` on Windows).
//!
//! It reads the real screen, so it runs only when `NUVIRA_BENCH_SCREEN=1` is set
//! (PC-LABORATORIO, or the NUC with the user's OK): otherwise `not-runnable`. For
//! motion the animation of `client/viewer/examples/animate.rs` (built from the same
//! side) is started full screen when `NUVIRA_BENCH_ANIMATE` names its executable.
//! Each rep takes [`FRAMES_PER_REP`] new images (a timeout without change is not
//! counted as a frame) and records the time per `next_frame` that returned one,
//! frames per second, CPU and allocations per frame. The first frame's SHA-256 and
//! a PPM dump (in the shared inputs directory) are recorded (info).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::{MicroCase, RepLoop, metric, not_runnable};
use crate::alloc::ThreadAllocs;
use crate::measure::{Better, Distribution, Gate, cpu_time_process};
use crate::registry::{CaseResult, FULL_ONLY, Registry, RunCtx};

const FRAMES_PER_REP: u32 = 120;
const REP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Default,
    Gdi,
    Gpu,
}

fn screen_allowed() -> Result<(), String> {
    if std::env::var("NUVIRA_BENCH_SCREEN").as_deref() == Ok("1") {
        Ok(())
    } else {
        Err("reads the real screen: set NUVIRA_BENCH_SCREEN=1 (PC-LABORATORIO, or this PC with the user's OK)".into())
    }
}

fn screen_windows() -> Result<(), String> {
    super::only_windows()?;
    screen_allowed()
}

pub fn register(reg: &mut Registry) {
    reg.add_case(
        MicroCase::new("B-MICRO-17", "screen/default", FULL_ONLY, |ctx, r| run(ctx, r, Mode::Default))
            .runnable(screen_allowed)
            .config(&[
                "WIN11",
                "WIN10",
                "MULTI-MON",
                "LINUX-X11",
                "LINUX-WAYLAND-GNOME",
                "LINUX-WAYLAND-KDE",
                "MAC-ARM",
            ])
            .isolated(),
    );
    reg.add_case(
        MicroCase::new("B-MICRO-17", "screen/gdi", FULL_ONLY, |ctx, r| run(ctx, r, Mode::Gdi))
            .runnable(screen_windows)
            .config(&["WIN-RDP"])
            .env("NUVIRA_CAPTURE", "gdi"),
    );
    reg.add_case(
        MicroCase::new("B-MICRO-17", "screen/gpu", FULL_ONLY, |ctx, r| run(ctx, r, Mode::Gpu))
            .runnable(screen_windows)
            .config(&["WIN11", "WIN10", "GPU-INTEL-QSV"])
            .isolated(),
    );
}

#[derive(Default)]
struct RepOut {
    ms: Vec<f64>,
    frames: u32,
    timeouts: u32,
    wall: f64,
    cpu_ms: f64,
    allocs: u64,
    alloc_bytes: u64,
}

fn run(ctx: &mut RunCtx, mut r: CaseResult, mode: Mode) -> Result<CaseResult> {
    // The animation, if given (killed at the end).
    let mut animation = match std::env::var_os("NUVIRA_BENCH_ANIMATE") {
        Some(exe) => {
            let secs = (ctx.reps + ctx.warmup + 2) * 15;
            let child = std::process::Command::new(&exe)
                .arg(secs.to_string())
                .spawn()
                .with_context(|| format!("starting the animation {}", exe.to_string_lossy()))?;
            std::thread::sleep(Duration::from_secs(2));
            r.param("animation", true);
            Some(child)
        }
        None => {
            r.note("no NUVIRA_BENCH_ANIMATE: frames come only from real screen changes");
            None
        }
    };
    let result = (|| -> Result<CaseResult> {
        let mut capturer = match nuvira_capture::open(None) {
            Ok(c) => c,
            Err(e) => return Ok(not_runnable(r.clone(), format!("capture not available: {e:#}"))),
        };
        #[cfg(windows)]
        if mode == Mode::Gpu && capturer.enable_gpu_output().is_none() {
            return Ok(not_runnable(r.clone(), "this capture source has no GPU output"));
        }
        let display = format!("{:?}", capturer.display());
        r.param("display", display.as_str()).param(
            "mode",
            match mode {
                Mode::Default => "default",
                Mode::Gdi => "gdi",
                Mode::Gpu => "gpu",
            },
        );
        let mut first: Option<(u32, u32, Vec<u8>)> = None;
        let mut lp = RepLoop::new(ctx);
        let reps = lp.run(|_, _| {
            let mut o = RepOut::default();
            let start = Instant::now();
            while o.frames < FRAMES_PER_REP && start.elapsed() < REP_TIMEOUT {
                let a0 = ThreadAllocs::take();
                let p0 = cpu_time_process();
                let t0 = Instant::now();
                let got = capturer.next_frame(Duration::from_millis(100))?;
                let dt = t0.elapsed();
                let cpu = cpu_time_process().saturating_sub(p0);
                let al = ThreadAllocs::take().since(&a0);
                match got {
                    Some(f) => {
                        if first.is_none() && !f.data.is_empty() {
                            let mut rgb = Vec::with_capacity(f.width as usize * f.height as usize * 3);
                            for row in f.data.chunks(f.stride).take(f.height as usize) {
                                for px in row[..f.width as usize * 4].as_chunks::<4>().0 {
                                    rgb.extend_from_slice(&[px[2], px[1], px[0]]);
                                }
                            }
                            first = Some((f.width, f.height, rgb));
                        }
                        o.frames += 1;
                        o.ms.push(dt.as_secs_f64() * 1e3);
                        o.cpu_ms += cpu.as_secs_f64() * 1e3;
                        o.allocs += al.allocs;
                        o.alloc_bytes += al.bytes;
                    }
                    None => o.timeouts += 1,
                }
            }
            o.wall = start.elapsed().as_secs_f64();
            Ok(o)
        })?;
        lp.record(&mut r, FRAMES_PER_REP as u64);
        let per = |f: &dyn Fn(&RepOut) -> f64| reps.iter().map(f).collect::<Vec<_>>();
        let k = |o: &RepOut| o.frames.max(1) as f64;
        r.metric(
            "next_frame_ms",
            metric("ms", Better::Lower, Gate::Micro, per(&|o| o.ms.iter().sum::<f64>() / k(o))),
        );
        r.metric("fps", metric("1/s", Better::Higher, Gate::Micro, per(&|o| o.frames as f64 / o.wall)));
        r.metric("cpu_ms_frame", super::cpu_metric(per(&|o| o.cpu_ms / k(o)), per(&|o| o.cpu_ms)));
        r.metric("allocs_frame", metric("1", Better::Lower, Gate::Micro, per(&|o| o.allocs as f64 / k(o))));
        r.metric(
            "alloc_bytes_frame",
            metric("B", Better::Lower, Gate::Micro, per(&|o| o.alloc_bytes as f64 / k(o))),
        );
        r.metric("timeouts", metric("1", Better::Lower, Gate::Info, per(&|o| o.timeouts as f64)));
        let dists: Vec<Distribution> = reps.iter().map(|o| Distribution::of(&o.ms)).collect();
        r.distribution("next_frame_ms_per_frame", Distribution::median_of(&dists));
        if let Some((w, h, rgb)) = first {
            r.checksum("first_frame", super::sha_hex(&rgb));
            let dir = ctx.inputs.join("capture");
            if std::fs::create_dir_all(&dir).is_ok() {
                let path = dir.join(format!("first-{}.ppm", r.case.replace('/', "-")));
                let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
                ppm.extend_from_slice(&rgb);
                if std::fs::write(&path, ppm).is_ok() {
                    r.param("first_frame_ppm", path.to_string_lossy().as_ref());
                }
            }
        }
        Ok(r.clone())
    })();
    if let Some(c) = animation.as_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
    result
}
