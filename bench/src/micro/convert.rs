//! B-MICRO-01 BGRA -> I420, B-MICRO-02 BGRA -> NV12, B-MICRO-03 CPU scaling
//! (`nuvira_codec::convert`).
//!
//! Input: a few consecutive frames of the scene, cycled (generated before timing).
//! The production conversions run on several threads, so CPU time is the process's
//! and allocations are counted process-wide in a separate pass. Quality: PSNR of
//! the output against the bench's own BT.601 reference (01, 02) or box-filter
//! downscale (03, info); the output's SHA-256 is recorded (info: "changed").

use anyhow::Result;
use nuvira_codec::{BgraImage, YuvFrame, convert};

use super::{MicroCase, metric, process_allocs, res, sha_hex, single};
use crate::fixtures::{Frame, RESOLUTIONS, Scene};
use crate::measure::{Better, Gate, Reps};
use crate::quality::{self, Reference};
use crate::registry::{BOTH_TIERS, CaseResult, FULL_ONLY, Registry, RunCtx};

/// Frames cycled through by the timed loop.
const INPUT_FRAMES: u32 = 4;

pub fn register(reg: &mut Registry) {
    for (id, nv12) in [("B-MICRO-01", false), ("B-MICRO-02", true)] {
        for scene in [Scene::Static, Scene::Video, Scene::Drag] {
            for (w, h) in RESOLUTIONS {
                let tiers = if (w, h) == (1920, 1080) { BOTH_TIERS } else { FULL_ONLY };
                let kind = if nv12 { "nv12" } else { "i420" };
                reg.add_case(MicroCase::new(
                    id,
                    format!("{kind}/{}/{}", scene.id(), res(w, h)),
                    tiers,
                    move |ctx, r| run_convert(ctx, r, scene, w, h, nv12),
                ));
            }
        }
    }
    for ((sw, sh), (dw, dh)) in
        [((3840, 2160), (1920, 1080)), ((2560, 1440), (1920, 1080)), ((1920, 1080), (1280, 720))]
    {
        reg.add_case(MicroCase::new(
            "B-MICRO-03",
            format!("scale/S5/{}->{}", res(sw, sh), res(dw, dh)),
            BOTH_TIERS,
            move |ctx, r| run_scale(ctx, r, (sw, sh), (dw, dh)),
        ));
    }
}

fn inputs(scene: Scene, w: u32, h: u32) -> Vec<Frame> {
    scene.frames(w, h, INPUT_FRAMES).collect()
}

fn input_sha(frames: &[Frame]) -> String {
    let all: Vec<u8> = frames.iter().flat_map(|f| f.data.iter().copied()).collect();
    sha_hex(&all)
}

/// CPU of the process per call (the conversion uses worker threads).
fn record_times(r: &mut CaseResult, reps: &Reps) {
    r.reps(reps);
    r.metric("ns_frame", reps.ns_per_iter(Gate::Micro));
    r.metric(
        "cpu_ms_frame",
        reps.metric("ms", Better::Lower, Gate::Micro, |x| {
            x.cpu_process.as_secs_f64() * 1e3 / x.iterations.max(1) as f64
        }),
    );
}

fn run_convert(
    ctx: &mut RunCtx,
    mut r: CaseResult,
    scene: Scene,
    w: u32,
    h: u32,
    nv12: bool,
) -> Result<CaseResult> {
    let frames = inputs(scene, w, h);
    let (ew, eh) = ((w & !1) as usize, (h & !1) as usize);
    let (mut y, mut u, mut v) = (Vec::new(), Vec::new(), Vec::new());
    let mut out = vec![0u8; ew * eh * 3 / 2];
    let mut k = 0usize;
    let mut step = || {
        let img = frames[k % frames.len()].image();
        k += 1;
        if nv12 {
            convert::bgra_to_nv12(&img, &mut out, ew, eh);
            std::hint::black_box(&out);
        } else {
            convert::bgra_to_i420(&img, &mut y, &mut u, &mut v);
            std::hint::black_box((&y, &u, &v));
        }
    };
    let reps = super::sampler(ctx).run(&mut step);
    let (allocs, bytes) = process_allocs(reps.iterations.min(64), &mut step);
    drop(step);
    record_times(&mut r, &reps);
    r.metric("allocs_frame", single("1", Better::Lower, Gate::Micro, allocs));
    r.metric("alloc_bytes_frame", single("B", Better::Lower, Gate::Micro, bytes));

    // Quality and checksum of each input frame's output.
    let mut psnr = Vec::new();
    let mut hash = Vec::new();
    for f in &frames {
        let img = f.image();
        let yuv = if nv12 {
            convert::bgra_to_nv12(&img, &mut out, ew, eh);
            hash.extend_from_slice(&out);
            super::nv12_to_yuv(&out, ew, eh, ew, ew * eh)
        } else {
            convert::bgra_to_i420(&img, &mut y, &mut u, &mut v);
            hash.extend_from_slice(&y);
            hash.extend_from_slice(&u);
            hash.extend_from_slice(&v);
            YuvFrame {
                width: ew as u32,
                height: eh as u32,
                y: y.clone(),
                u: u.clone(),
                v: v.clone(),
                ..Default::default()
            }
        };
        psnr.push(quality::psnr(&yuv, &Reference::from_bgra(&img)));
    }
    let mean = |f: fn(&quality::Psnr) -> f64| {
        psnr.iter().map(|p| quality::cap(f(p))).sum::<f64>() / psnr.len() as f64
    };
    r.metric("psnr_yuv_db", single("dB", Better::Higher, Gate::Quality, mean(|p| p.yuv)));
    r.metric("psnr_y_db", single("dB", Better::Higher, Gate::Quality, mean(|p| p.y)));
    r.metric("psnr_u_db", single("dB", Better::Higher, Gate::Info, mean(|p| p.u)));
    r.metric("psnr_v_db", single("dB", Better::Higher, Gate::Info, mean(|p| p.v)));
    r.param("frames", INPUT_FRAMES).param("width", w).param("height", h);
    r.checksum("output", sha_hex(&hash));
    r.checksum("input", input_sha(&frames));
    Ok(r)
}

/// Reference downscale: each output pixel is the exact area-weighted mean of the
/// source pixels it covers (floating point), then rounded.
fn box_downscale(src: &BgraImage, w: usize, h: usize) -> Vec<u8> {
    let (sw, sh) = (src.width as usize, src.height as usize);
    let (fx, fy) = (sw as f64 / w as f64, sh as f64 / h as f64);
    let mut out = vec![0u8; w * h * 4];
    // Column weights per output column: (source column, weight).
    let spans = |n: usize, f: f64, limit: usize| -> Vec<Vec<(usize, f64)>> {
        (0..n)
            .map(|o| {
                let (a, b) = (o as f64 * f, (o + 1) as f64 * f);
                let mut v = Vec::new();
                let mut s = a.floor() as usize;
                while (s as f64) < b && s < limit {
                    let lo = a.max(s as f64);
                    let hi = b.min((s + 1) as f64);
                    if hi > lo {
                        v.push((s, hi - lo));
                    }
                    s += 1;
                }
                v
            })
            .collect()
    };
    let cols = spans(w, fx, sw);
    let rows = spans(h, fy, sh);
    for (oy, rspan) in rows.iter().enumerate() {
        for (ox, cspan) in cols.iter().enumerate() {
            let mut acc = [0f64; 4];
            let mut wsum = 0f64;
            for &(sy, wy) in rspan {
                let row = &src.data[sy * src.stride..];
                for &(sx, wx) in cspan {
                    let wgt = wy * wx;
                    for c in 0..4 {
                        acc[c] += row[sx * 4 + c] as f64 * wgt;
                    }
                    wsum += wgt;
                }
            }
            for c in 0..4 {
                out[(oy * w + ox) * 4 + c] = (acc[c] / wsum).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

fn run_scale(ctx: &mut RunCtx, mut r: CaseResult, src: (u32, u32), dst: (u32, u32)) -> Result<CaseResult> {
    let frames = inputs(Scene::Drag, src.0, src.1);
    let (dw, dh) = (dst.0 as usize, dst.1 as usize);
    let mut out = Vec::new();
    let mut k = 0usize;
    let mut step = || {
        let img = frames[k % frames.len()].image();
        k += 1;
        convert::scale_bgra(&img, dw, dh, &mut out);
        std::hint::black_box(&out);
    };
    let reps = super::sampler(ctx).run(&mut step);
    let (allocs, bytes) = process_allocs(reps.iterations.min(64), &mut step);
    drop(step);
    record_times(&mut r, &reps);
    r.metric("allocs_frame", single("1", Better::Lower, Gate::Micro, allocs));
    r.metric("alloc_bytes_frame", single("B", Better::Lower, Gate::Micro, bytes));
    let mut hash = Vec::new();
    let mut psnr = Vec::new();
    for f in &frames {
        let img = f.image();
        convert::scale_bgra(&img, dw, dh, &mut out);
        hash.extend_from_slice(&out);
        let reference = box_downscale(&img, dw, dh);
        psnr.push(quality::cap(super::psnr_bgra(&out, dw * 4, &reference, dw * 4, dw, dh)));
    }
    r.metric("psnr_vs_box_db", metric("dB", Better::Higher, Gate::Info, psnr));
    r.param("frames", INPUT_FRAMES);
    r.checksum("output", sha_hex(&hash));
    r.checksum("input", input_sha(&frames));
    Ok(r)
}
