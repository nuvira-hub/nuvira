//! B-MICRO-04 OpenH264 encode, and the encode harness shared with B-MICRO-05.
//!
//! One case per (scene, resolution, ladder point). Each rep opens a new encoder
//! (not timed), renders the scene's frames one by one (not timed) and times each
//! `encode` call: wall time, process CPU time and the calling thread's
//! allocations. Hardware encoders also get `wait_ready` timed separately and their
//! first [`HW_WARM_FRAMES`] frames of every rep left out (start-up, plan 6.1).
//!
//! Quality (plan 6.4) is computed after the timed reps from the stream of the
//! first measured rep, decoded with OpenH264: coded quality (each decoded frame
//! against its source) and displayed quality (each source frame against the last
//! decoded frame at that moment; skipped frames count as stale). Outputs are paired
//! with source frames FIFO: a synchronous encoder's `None` is a skipped frame, a
//! hardware encoder's `None` a frame still inside it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nuvira_codec::{EncoderPlan, EncoderSettings, VideoEncoder, YuvFrame};

use super::{MicroCase, QualityAcc, RepLoop, metric, not_runnable, res, scaled_kbps, sha_hex};
use crate::alloc::ThreadAllocs;
use crate::fixtures::{Frame, RESOLUTIONS, Scene};
use crate::measure::{Better, Distribution, Gate, cpu_time_process};
use crate::registry::{BOTH_TIERS, CaseResult, FULL_ONLY, Registry, RunCtx};
use crate::report::LadderPoint;

/// Ladder at 1920x1080 in kbit/s (scaled by pixel count for other sizes).
pub const LADDER_1080P: [u32; 4] = [1000, 2000, 4000, 8000];
/// Ladder points of the quick tier.
pub const QUICK_LADDER: [u32; 2] = [2000, 8000];
/// Scenes of the full 4-point ladder at 1080p (the others get [`QUICK_LADDER`]).
const LADDER_SCENES: [Scene; 3] = [Scene::Terminal, Scene::Video, Scene::Drag];
/// Scenes at 720p (two ladder points).
const SIDE_RES_SCENES: [Scene; 3] = [Scene::Terminal, Scene::Video, Scene::Drag];
/// Scenes at 1440p and 4K (two ladder points): the static desktop and the typical
/// session (as the macro matrix, plan section 13).
const UHD_SCENES: [Scene; 2] = [Scene::Static, Scene::Drag];
/// Measured reps of every encode case in tier `full` (time budget; plan
/// "Changes during implementation": 10 reps x 96 cases x 300 frames per backend
/// took about 5 h on the NUC).
pub const FULL_REPS: u32 = 5;

/// Whether (scene, resolution, ladder point at 1080p) is in tier `full`: the
/// 4-point ladder for every scene at 1080p would cost the most; full keeps it for
/// the content that matters for BD-rate, two points elsewhere.
pub fn in_full(scene: Scene, (w, h): (u32, u32), k: u32) -> bool {
    let two = QUICK_LADDER.contains(&k);
    match (w, h) {
        (1920, 1080) => LADDER_SCENES.contains(&scene) || two,
        (1280, 720) => SIDE_RES_SCENES.contains(&scene) && two,
        _ => UHD_SCENES.contains(&scene) && two,
    }
}

/// Hardware encoders: frames of each rep left out of the statistics.
pub const HW_WARM_FRAMES: u32 = 30;
/// A hardware encoder holding more frames than this is taken to have skipped one.
const MAX_PENDING: usize = 8;

/// One encoder backend to benchmark.
#[derive(Clone, Copy)]
pub struct Backend {
    pub id: &'static str,
    /// Name prefix of the cases ("openh264", "mf", "nvenc", ...).
    pub prefix: &'static str,
    pub plan: EncoderPlan,
    /// `VideoEncoder::name()` it must report (None: any).
    pub expect_name: Option<&'static str>,
    pub quick_scenes: &'static [Scene],
    pub runnable: fn() -> Result<(), String>,
    pub env: &'static [(&'static str, &'static str)],
    pub config: Option<&'static [&'static str]>,
}

impl Backend {
    fn hardware(&self) -> bool {
        self.plan != EncoderPlan::Software
    }
}

pub fn register(reg: &mut Registry) {
    register_backend(
        reg,
        Backend {
            id: "B-MICRO-04",
            prefix: "openh264",
            plan: EncoderPlan::Software,
            expect_name: Some("openh264"),
            quick_scenes: &[Scene::Terminal, Scene::Video, Scene::Drag],
            runnable: super::always,
            env: &[],
            config: None,
        },
    );
}

/// Cases of one backend: the scene x resolution x ladder points of [`in_full`]
/// (and the quick ones).
pub fn register_backend(reg: &mut Registry, b: Backend) {
    for scene in Scene::ALL {
        for (w, h) in RESOLUTIONS {
            for k in LADDER_1080P {
                let quick =
                    b.quick_scenes.contains(&scene) && (w, h) == (1920, 1080) && QUICK_LADDER.contains(&k);
                let full = in_full(scene, (w, h), k);
                let tiers = match (quick, full) {
                    (true, _) => BOTH_TIERS,
                    (false, true) => FULL_ONLY,
                    (false, false) => continue,
                };
                let kbps = scaled_kbps(k, w, h);
                let mut c = MicroCase::new(
                    b.id,
                    format!("{}/{}/{}/{kbps}k", b.prefix, scene.id(), res(w, h)),
                    tiers,
                    move |ctx, r| run_encode(ctx, r, &b, scene, w, h, kbps),
                )
                .runnable(b.runnable)
                .full_reps(FULL_REPS);
                if let Some(ids) = b.config {
                    c = c.config(ids);
                }
                for (k, v) in b.env {
                    c = c.env(k, v);
                }
                reg.add_case(c);
            }
        }
    }
}

struct Output {
    /// Source frame it encodes.
    src: u32,
    /// Encode call that returned it.
    at: u32,
    data: Vec<u8>,
}

#[derive(Default)]
struct RepOut {
    ms: Vec<f64>,
    wait_ms: Vec<f64>,
    cpu_ms: f64,
    counted: u32,
    bytes: u64,
    skipped: u32,
    key_bytes: Option<usize>,
    allocs: u64,
    alloc_bytes: u64,
    pending_end: usize,
}

fn make(b: &Backend, settings: EncoderSettings) -> Result<Box<dyn VideoEncoder>> {
    nuvira_codec::create_encoder(b.plan, settings, None)
}

/// Checks that the backend's encoder exists here: opens it and encodes one frame.
fn probe(b: &Backend, settings: EncoderSettings, frame: &Frame) -> Result<String> {
    let mut enc = make(b, settings)?;
    let mut got = enc.encode(&frame.image(), true)?.is_some();
    for _ in 0..8 {
        if got {
            break;
        }
        got = enc.encode(&frame.image(), false)?.is_some();
    }
    let name = enc.name().to_string();
    if let Some(expect) = b.expect_name
        && name != expect
    {
        anyhow::bail!("the encoder created is {name}, not {expect}");
    }
    anyhow::ensure!(got, "{name} gave no output for 9 frames");
    Ok(name)
}

pub fn run_encode(
    ctx: &mut RunCtx,
    mut r: CaseResult,
    b: &Backend,
    scene: Scene,
    w: u32,
    h: u32,
    kbps: u32,
) -> Result<CaseResult> {
    let n = ctx.frames();
    let hardware = b.hardware();
    let settings = EncoderSettings { bitrate_bps: kbps * 1000, max_fps: 60, ltr: false };
    let warm = if hardware { HW_WARM_FRAMES.min(n / 4) } else { 0 };
    let mut generator = scene.generator(w, h, n);
    let mut frame = Frame::default();
    generator.render(0, &mut frame);
    let name = match probe(b, settings, &frame) {
        Ok(name) => name,
        Err(e) if hardware => return Ok(not_runnable(r, format!("hardware encoder not available: {e:#}"))),
        Err(e) => return Err(e),
    };
    #[cfg(windows)]
    if hardware && let Ok(list) = nuvira_codec::hardware_encoders() {
        r.param("hardware_encoders", list.join(", "));
    }

    let mut stream: Vec<Output> = Vec::new();
    let mut keep_next = true;
    let mut lp = RepLoop::new(ctx);
    let reps = lp.run(|_, measured| {
        let keep = measured && keep_next;
        if measured {
            keep_next = false;
        }
        let mut enc = make(b, settings)?;
        let mut pending: VecDeque<u32> = VecDeque::new();
        let mut o = RepOut::default();
        for i in 0..n {
            generator.render(i, &mut frame);
            let img = frame.image();
            let counted = i >= warm;
            let tw = Instant::now();
            if hardware {
                enc.wait_ready(Duration::from_secs(2)).context("wait_ready")?;
            }
            let wait = tw.elapsed();
            let a0 = ThreadAllocs::take();
            let p0 = cpu_time_process();
            let t0 = Instant::now();
            let out = enc.encode(&img, i == 0).with_context(|| format!("encoding frame {i}"))?;
            let dt = t0.elapsed();
            let cpu = cpu_time_process().saturating_sub(p0);
            let al = ThreadAllocs::take().since(&a0);
            pending.push_back(i);
            match out {
                Some(f) => {
                    let src = pending.pop_front().expect("pending");
                    if counted {
                        o.bytes += f.data.len() as u64;
                    }
                    if f.keyframe && o.key_bytes.is_none() {
                        o.key_bytes = Some(f.data.len());
                    }
                    if keep {
                        stream.push(Output { src, at: i, data: f.data });
                    }
                }
                None => {
                    if !hardware || pending.len() > MAX_PENDING {
                        pending.pop_front();
                        if counted {
                            o.skipped += 1;
                        }
                    }
                }
            }
            if counted {
                o.counted += 1;
                o.ms.push(dt.as_secs_f64() * 1e3);
                o.wait_ms.push(wait.as_secs_f64() * 1e3);
                o.cpu_ms += cpu.as_secs_f64() * 1e3;
                o.allocs += al.allocs;
                o.alloc_bytes += al.bytes;
            }
        }
        o.pending_end = pending.len();
        Ok(o)
    })?;
    lp.record(&mut r, n as u64);

    let per = |f: &dyn Fn(&RepOut) -> f64| reps.iter().map(f).collect::<Vec<_>>();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    r.metric("encode_ms", metric("ms", Better::Lower, Gate::Micro, per(&|o| mean(&o.ms))));
    r.metric(
        "cpu_ms_frame",
        super::cpu_metric(per(&|o| o.cpu_ms / o.counted.max(1) as f64), per(&|o| o.cpu_ms)),
    );
    let bytes_frame = per(&|o| o.bytes as f64 / o.counted.max(1) as f64);
    let kbps_actual = bytes_frame.iter().map(|b| b * 8.0 * 60.0 / 1000.0).collect::<Vec<_>>();
    r.metric("bytes_frame", metric("B", Better::Lower, Gate::Quality, bytes_frame));
    r.metric("kbps_at_60fps", metric("kbit/s", Better::Lower, Gate::Info, kbps_actual.clone()));
    r.metric("skipped", metric("1", Better::Lower, Gate::Info, per(&|o| o.skipped as f64)));
    r.metric(
        "keyframe_bytes",
        metric("B", Better::Lower, Gate::Info, per(&|o| o.key_bytes.map(|b| b as f64).unwrap_or(f64::NAN))),
    );
    r.metric(
        "allocs_frame",
        metric("1", Better::Lower, Gate::Micro, per(&|o| o.allocs as f64 / o.counted.max(1) as f64)),
    );
    r.metric(
        "alloc_bytes_frame",
        metric("B", Better::Lower, Gate::Micro, per(&|o| o.alloc_bytes as f64 / o.counted.max(1) as f64)),
    );
    let dists: Vec<Distribution> = reps.iter().map(|o| Distribution::of(&o.ms)).collect();
    r.distribution("encode_ms_per_frame", Distribution::median_of(&dists));
    if hardware {
        // Info: the wait for the MFT's input slot is mostly 0 with driver-timed
        // spikes (bimodal; a ratio against a 0 median is meaningless).
        r.metric("wait_ready_ms", metric("ms", Better::Lower, Gate::Info, per(&|o| mean(&o.wait_ms))));
        r.metric("pending_at_end", metric("1", Better::Lower, Gate::Info, per(&|o| o.pending_end as f64)));
        r.param("warmup_frames", warm);
    }
    r.param("frames", n)
        .param("bitrate_kbps", kbps)
        .param("fps", 60)
        .param("width", w)
        .param("height", h)
        .param("encoder", name.as_str())
        .param("quality_rep", 1);
    if let Some(s) = crate::fixtures::Manifest::embedded()?.scene_sha256(scene, w, h, n) {
        r.checksum("input", s);
    }
    let all: Vec<u8> = stream.iter().flat_map(|o| o.data.iter().copied()).collect();
    r.checksum("output", sha_hex(&all));

    // Quality of the kept stream.
    let q = stream_quality(&mut r, &stream, &mut generator, n, |_, f| Arc::new(f))?;
    let kbps_median = crate::measure::median(&kbps_actual);
    r.ladder.push(LadderPoint {
        kbps_target: kbps as f64,
        kbps_actual: kbps_median,
        psnr_yuv_db: q.psnr_yuv,
        ssim_y: q.ssim_y,
    });
    Ok(r)
}

/// Decodes `stream` with OpenH264 and scores it against the regenerated sources
/// (`source` turns frame `i` into the reference picture, e.g. scaled).
fn stream_quality(
    r: &mut CaseResult,
    stream: &[Output],
    generator: &mut crate::fixtures::scenes::SceneGen,
    n: u32,
    source: impl Fn(u32, Frame) -> Arc<Frame>,
) -> Result<super::QualitySummary> {
    let mut dec = nuvira_codec::software_decoder()?;
    let mut acc = QualityAcc::new();
    let mut recent: VecDeque<(u32, Arc<Frame>)> = VecDeque::new();
    let mut last: Option<Arc<YuvFrame>> = None;
    let mut last_src: Option<u32> = None;
    let mut si = 0;
    let mut not_decoded = 0u32;
    for i in 0..n {
        let f = source(i, generator.frame(i));
        recent.push_back((i, f.clone()));
        while recent.len() > 2 * MAX_PENDING + 2 {
            recent.pop_front();
        }
        while si < stream.len() && stream[si].at == i {
            let o = &stream[si];
            si += 1;
            match dec.decode(&o.data).context("decoding for quality")? {
                Some(y) => {
                    let y = Arc::new(y.clone());
                    if let Some((_, src)) = recent.iter().find(|(k, _)| *k == o.src) {
                        acc.coded(o.src, src.clone(), y.clone(), o.src == i);
                    }
                    last = Some(y);
                    last_src = Some(o.src);
                }
                None => not_decoded += 1,
            }
        }
        if last_src != Some(i) {
            acc.stale(i, f, last.clone());
        }
    }
    r.param("quality_not_decoded", not_decoded);
    Ok(acc.finish(r))
}
