//! B-MICRO-08 OpenH264 decode, B-MICRO-09 hardware decode, B-MICRO-10 copies of
//! decoded frames on the viewer path.
//!
//! Decode streams (08, 09) are encoded once with OpenH264 and cached in the run's
//! shared inputs directory (`<inputs>/decode/`), so both sides of an A/B decode the
//! same bytes; their SHA-256 is recorded as the `input` checksum. bench-compare
//! produces them with the A binary (plan 9.2 step 5); a missing file is made by
//! this build and the result says so.
//!
//! 09 selects the hardware decoder as the viewer does (`NUVIRA_DECODER=hardware`,
//! `best_decoder`, so the case runs in a child process); "native" keeps the pictures
//! on the GPU (`set_native_frames(true)`, Windows: shared textures) and times their
//! readback (`YuvFrame::with_planes`) separately.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use nuvira_codec::{EncoderPlan, EncoderSettings, VideoDecoder, YuvFrame};

use super::{MicroCase, QualityAcc, RepLoop, metric, not_runnable, res, scaled_kbps, sha_hex, single};
use crate::alloc::ThreadAllocs;
use crate::fixtures::{Frame, RESOLUTIONS, Scene};
use crate::measure::{Better, Distribution, Gate, cpu_time_process};
use crate::quality::Reference;
use crate::registry::{BOTH_TIERS, CaseResult, FULL_ONLY, Registry, RunCtx, Tier};

/// Decode stream bitrates at 1920x1080 (scaled by pixel count).
const BITRATES_1080P: [u32; 2] = [2000, 8000];
const SCENES: [Scene; 3] = [Scene::Terminal, Scene::Video, Scene::Drag];
const MAGIC: &[u8; 8] = b"NBH264S1";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Software,
    MfPlanes,
    MfNative,
    Vt,
}

impl Backend {
    fn prefix(self) -> &'static str {
        match self {
            Backend::Software => "openh264",
            Backend::MfPlanes => "mf/planes",
            Backend::MfNative => "mf/native",
            Backend::Vt => "vt",
        }
    }
    fn expect(self) -> &'static str {
        match self {
            Backend::Software => "openh264",
            Backend::MfPlanes | Backend::MfNative => "mediafoundation",
            Backend::Vt => "videotoolbox",
        }
    }
}

pub fn register(reg: &mut Registry) {
    for backend in [Backend::Software, Backend::MfPlanes, Backend::MfNative, Backend::Vt] {
        for scene in SCENES {
            for (w, h) in RESOLUTIONS {
                for k in BITRATES_1080P {
                    let kbps = scaled_kbps(k, w, h);
                    let quick = backend == Backend::Software && (w, h) == (1920, 1080);
                    let id = if backend == Backend::Software { "B-MICRO-08" } else { "B-MICRO-09" };
                    let mut c = MicroCase::new(
                        id,
                        format!("{}/{}/{}/{kbps}k", backend.prefix(), scene.id(), res(w, h)),
                        if quick { BOTH_TIERS } else { FULL_ONLY },
                        move |ctx, r| run_decode(ctx, r, backend, scene, w, h, kbps),
                    );
                    if w * h > 1920 * 1080 {
                        // Long cases (1440p, 4K): fewer reps in tier full (time budget).
                        c = c.full_reps(super::MIN_REPS);
                    }
                    c = match backend {
                        Backend::Software => c,
                        Backend::MfPlanes | Backend::MfNative => c
                            .runnable(super::only_windows)
                            .config(&["GPU-INTEL-QSV", "GPU-NVENC"])
                            .env("NUVIRA_DECODER", "hardware"),
                        Backend::Vt => c
                            .runnable(super::only_macos)
                            .config(&["MAC-VIDEOTOOLBOX"])
                            .env("NUVIRA_DECODER", "hardware"),
                    };
                    reg.add_case(c);
                }
            }
        }
    }
    for (w, h) in RESOLUTIONS {
        for from in [false, true] {
            let kind = if from { "clone_from" } else { "clone" };
            reg.add_case(MicroCase::new(
                "B-MICRO-10",
                format!("{kind}/{}", res(w, h)),
                BOTH_TIERS,
                move |ctx, r| run_copy(ctx, r, w, h, from),
            ));
        }
    }
}

/// Makes every decode stream the B-MICRO-08/09 cases of `tier` read, in
/// `<inputs>/decode/` (kept when already there). `bench-compare` runs this once
/// with the A (baseline) binary before the rounds (plan 9.2 step 5), so both sides
/// decode bytes encoded by the baseline's OpenH264. Returns (path, SHA-256, made).
pub fn make_streams(inputs: &Path, tier: Tier) -> Result<Vec<(PathBuf, String, bool)>> {
    let n = crate::fixtures::frames_for(tier);
    let mut out = Vec::new();
    for scene in SCENES {
        for (w, h) in RESOLUTIONS {
            // Quick tier: only the 1080p software cases run.
            if tier == Tier::Quick && (w, h) != (1920, 1080) {
                continue;
            }
            for k in BITRATES_1080P {
                let kbps = scaled_kbps(k, w, h);
                let (_, sha, made) = load_or_make_stream(inputs, scene, w, h, kbps, n)?;
                out.push((stream_path(inputs, scene, w, h, kbps, n), sha, made));
            }
        }
    }
    Ok(out)
}

/// One encoded frame of a cached stream.
pub struct Entry {
    pub src: u32,
    pub data: Vec<u8>,
}

fn stream_path(dir: &Path, scene: Scene, w: u32, h: u32, kbps: u32, n: u32) -> PathBuf {
    dir.join("decode").join(format!("{}-{}-{kbps}k-{n}f.h264s", scene.id(), res(w, h)))
}

fn parse_stream(bytes: &[u8]) -> Result<Vec<Entry>> {
    if bytes.len() < 12 || &bytes[..8] != MAGIC {
        bail!("not a bench decode stream");
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into()?) as usize;
    let mut p = 12;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let src = u32::from_le_bytes(bytes.get(p..p + 4).context("truncated")?.try_into()?);
        let len = u32::from_le_bytes(bytes.get(p + 4..p + 8).context("truncated")?.try_into()?) as usize;
        let data = bytes.get(p + 8..p + 8 + len).context("truncated")?.to_vec();
        p += 8 + len;
        out.push(Entry { src, data });
    }
    Ok(out)
}

/// The cached stream (made with OpenH264 when missing): entries, file SHA-256 and
/// whether this build made it.
pub fn load_or_make_stream(
    inputs: &Path,
    scene: Scene,
    w: u32,
    h: u32,
    kbps: u32,
    n: u32,
) -> Result<(Vec<Entry>, String, bool)> {
    let path = stream_path(inputs, scene, w, h, kbps, n);
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok((parse_stream(&bytes)?, sha_hex(&bytes), false));
    }
    let settings = EncoderSettings { bitrate_bps: kbps * 1000, max_fps: 60, ltr: false };
    let mut enc = nuvira_codec::create_encoder(EncoderPlan::Software, settings, None)?;
    let mut generator = scene.generator(w, h, n);
    let mut frame = Frame::default();
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&0u32.to_le_bytes());
    let mut count = 0u32;
    for i in 0..n {
        generator.render(i, &mut frame);
        if let Some(f) = enc.encode(&frame.image(), i == 0)? {
            bytes.extend_from_slice(&i.to_le_bytes());
            bytes.extend_from_slice(&(f.data.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&f.data);
            count += 1;
        }
    }
    bytes[8..12].copy_from_slice(&count.to_le_bytes());
    let dir = path.parent().expect("parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::File::create(&tmp)?.write_all(&bytes)?;
    if std::fs::rename(&tmp, &path).is_err() {
        // Another process made it meanwhile: use that one.
        std::fs::remove_file(&tmp).ok();
        let bytes = std::fs::read(&path)?;
        return Ok((parse_stream(&bytes)?, sha_hex(&bytes), true));
    }
    Ok((parse_stream(&bytes)?, sha_hex(&bytes), true))
}

fn open_decoder(backend: Backend) -> Result<Box<dyn VideoDecoder>> {
    match backend {
        Backend::Software => nuvira_codec::software_decoder(),
        _ => nuvira_codec::best_decoder(),
    }
}

#[derive(Default)]
struct RepOut {
    ms: Vec<f64>,
    readback_ms: Vec<f64>,
    cpu_ms: f64,
    late: u32,
    allocs: u64,
    alloc_bytes: u64,
}

fn run_decode(
    ctx: &mut RunCtx,
    mut r: CaseResult,
    backend: Backend,
    scene: Scene,
    w: u32,
    h: u32,
    kbps: u32,
) -> Result<CaseResult> {
    if backend == Backend::MfNative {
        nuvira_codec::set_native_frames(true);
    }
    let name = match open_decoder(backend) {
        Ok(d) => d.name().to_string(),
        Err(e) if backend != Backend::Software => return Ok(not_runnable(r, format!("decoder: {e:#}"))),
        Err(e) => return Err(e),
    };
    if name != backend.expect() {
        return Ok(not_runnable(r, format!("hardware decoder not available (got {name})")));
    }
    let n = ctx.frames();
    let (stream, sha, made) = load_or_make_stream(&ctx.inputs, scene, w, h, kbps, n)?;
    r.checksum("input", sha);
    if made {
        r.note("decode stream made by this build (no shared input given)");
    }
    let native = backend == Backend::MfNative;
    let mut lp = RepLoop::new(ctx);
    let reps = lp.run(|_, _| {
        let mut dec = open_decoder(backend)?;
        let mut o = RepOut::default();
        for e in &stream {
            let a0 = ThreadAllocs::take();
            let p0 = cpu_time_process();
            let t0 = Instant::now();
            let out = dec.decode(&e.data).context("decoding")?;
            let dt = t0.elapsed();
            match out {
                Some(y) if native => {
                    let t1 = Instant::now();
                    let planes = y.with_planes();
                    std::hint::black_box(planes.y.len());
                    drop(planes);
                    o.readback_ms.push(t1.elapsed().as_secs_f64() * 1e3);
                }
                Some(y) => {
                    std::hint::black_box(y.y.len());
                }
                None => o.late += 1,
            }
            let cpu = cpu_time_process().saturating_sub(p0);
            let al = ThreadAllocs::take().since(&a0);
            o.ms.push(dt.as_secs_f64() * 1e3);
            o.cpu_ms += cpu.as_secs_f64() * 1e3;
            o.allocs += al.allocs;
            o.alloc_bytes += al.bytes;
        }
        Ok(o)
    })?;
    lp.record(&mut r, stream.len() as u64);
    let k = stream.len().max(1) as f64;
    let per = |f: &dyn Fn(&RepOut) -> f64| reps.iter().map(f).collect::<Vec<_>>();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    r.metric("decode_ms", metric("ms", Better::Lower, Gate::Micro, per(&|o| mean(&o.ms))));
    r.metric("cpu_ms_frame", super::cpu_metric(per(&|o| o.cpu_ms / k), per(&|o| o.cpu_ms)));
    r.metric("late_frames", metric("1", Better::Lower, Gate::Info, per(&|o| o.late as f64)));
    r.metric("allocs_frame", metric("1", Better::Lower, Gate::Micro, per(&|o| o.allocs as f64 / k)));
    r.metric(
        "alloc_bytes_frame",
        metric("B", Better::Lower, Gate::Micro, per(&|o| o.alloc_bytes as f64 / k)),
    );
    if native {
        r.metric("readback_ms", metric("ms", Better::Lower, Gate::Micro, per(&|o| mean(&o.readback_ms))));
    }
    let dists: Vec<Distribution> = reps.iter().map(|o| Distribution::of(&o.ms)).collect();
    r.distribution("decode_ms_per_frame", Distribution::median_of(&dists));
    let bytes: usize = stream.iter().map(|e| e.data.len()).sum();
    r.param("frames", n)
        .param("stream_frames", stream.len())
        .param("stream_bytes", bytes)
        .param("bitrate_kbps", kbps)
        .param("width", w)
        .param("height", h)
        .param("decoder", name.as_str());

    // Quality: decoded vs source; hardware also vs OpenH264's pictures.
    let mut dec = open_decoder(backend)?;
    let mut soft = (backend != Backend::Software).then(nuvira_codec::software_decoder).transpose()?;
    let mut generator = scene.generator(w, h, n);
    let mut acc = QualityAcc::new();
    let (mut ydiff, mut ypsnr) = (Vec::new(), Vec::new());
    let mut last: Option<(u32, Arc<Frame>)> = None;
    for e in &stream {
        let src = match &last {
            Some((i, f)) if *i == e.src => f.clone(),
            _ => {
                let f = Arc::new(generator.frame(e.src));
                last = Some((e.src, f.clone()));
                f
            }
        };
        let decoded: Option<Arc<YuvFrame>> =
            dec.decode(&e.data)?.map(|y| Arc::new(y.with_planes().into_owned()));
        let reference = match soft.as_mut() {
            Some(s) => s.decode(&e.data)?.map(|y| y.y.clone()),
            None => None,
        };
        if let Some(y) = decoded {
            if let Some(sy) = reference
                && sy.len() == y.y.len()
            {
                let (dw, dh) = (y.width as usize, y.height as usize);
                ydiff.push(super::luma_diff(&y.y, dw, &sy, dw, dw, dh));
                ypsnr.push(crate::quality::cap(crate::quality::psnr_db(y_mse(&y.y, &sy))));
            }
            acc.coded(e.src, src, y, true);
        }
    }
    acc.finish(&mut r);
    if backend != Backend::Software {
        r.metric("y_diff_vs_openh264", single("levels", Better::Lower, Gate::Info, mean(&ydiff)));
        r.metric("psnr_y_vs_openh264_db", single("dB", Better::Higher, Gate::Info, mean(&ypsnr)));
    }
    Ok(r)
}

fn y_mse(a: &[u8], b: &[u8]) -> f64 {
    let s: f64 = a.iter().zip(b).map(|(&p, &q)| (p as f64 - q as f64).powi(2)).sum();
    s / a.len().max(1) as f64
}

fn run_copy(ctx: &mut RunCtx, mut r: CaseResult, w: u32, h: u32, from: bool) -> Result<CaseResult> {
    let frame = Scene::Drag.generator(w, h, 1).frame(0);
    let yuv = Reference::from_bgra(&frame.image()).to_yuv();
    let mut last = YuvFrame::default();
    let reps = super::sampler(ctx).run(|| {
        if from {
            // As the viewer keeps the last frame (`viewer/src/lib.rs` FrameReady).
            let planes = yuv.with_planes();
            last.width = planes.width;
            last.height = planes.height;
            last.y.clone_from(&planes.y);
            last.u.clone_from(&planes.u);
            last.v.clone_from(&planes.v);
            std::hint::black_box(&last);
        } else {
            // As the decode thread hands a frame on (`viewer/src/net.rs` DecodedFrame).
            let c = yuv.clone();
            std::hint::black_box(&c);
        }
    });
    super::record_sampler(&mut r, &reps, "ns_frame", true, Gate::Micro);
    let copy = if from { last.clone() } else { yuv.clone() };
    let exact = copy.width == yuv.width && copy.y == yuv.y && copy.u == yuv.u && copy.v == yuv.v;
    r.metric("copy_exact", single("1", Better::Higher, Gate::Exact, f64::from(u8::from(exact))));
    let bytes = yuv.y.len() + yuv.u.len() + yuv.v.len();
    r.param("frame_bytes", bytes).param("width", w).param("height", h);
    Ok(r)
}
