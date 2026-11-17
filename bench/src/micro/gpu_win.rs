//! B-MICRO-06 Windows GPU texture path (BGRA texture -> NV12 on the GPU -> Media
//! Foundation, as the agent with DXGI capture and `EncoderPlan::HardwareGpu`, and as
//! `client/codec/examples/hwpace.rs`) and B-MICRO-07 GPU NV12 conversion + scaling
//! (`gpu::Nv12Converter`).
//!
//! The source textures are a ring of [`RING`] consecutive scene frames uploaded
//! before timing (`gpu::upload_bgra`), few like DXGI's own textures (the converter
//! keeps input views for up to 8). 06 times `wait_ready` and `encode_texture` per
//! frame and reports sustained frames per second; its first frames of each rep are
//! left out (start-up). 07 times each conversion until the GPU has finished it
//! (event query: CPU wall) and the GPU's own time (timestamp queries); readback is
//! outside the timing and used for the quality checks (luma difference against the
//! CPU `scale_bgra` + `bgra_to_nv12`, PSNR/SSIM against the bench reference).

use anyhow::Result;

use super::{MicroCase, res};
use crate::fixtures::Scene;
use crate::registry::{CaseResult, FULL_ONLY, Registry, RunCtx};

/// Ring of source textures.
pub const RING: u32 = 8;

const HW_CONFIG: &[&str] = &["WIN11", "WIN10", "GPU-INTEL-QSV", "GPU-NVENC", "WIN-HYBRID-GPU"];

pub fn register(reg: &mut Registry) {
    let sizes_06 = [((1920, 1080), (1920, 1080)), ((2560, 1440), (2560, 1440)), ((2560, 1440), (1920, 1080))];
    for scene in [Scene::Video, Scene::Drag] {
        for (input, output) in sizes_06 {
            reg.add_case(
                MicroCase::new(
                    "B-MICRO-06",
                    format!(
                        "mf-texture/{}/{}->{}",
                        scene.id(),
                        res(input.0, input.1),
                        res(output.0, output.1)
                    ),
                    FULL_ONLY,
                    move |ctx, r| texture(ctx, r, scene, input, output),
                )
                .runnable(super::only_windows)
                .config(HW_CONFIG),
            );
        }
    }
    let sizes_07 = [((1920, 1080), (1920, 1080)), ((2560, 1440), (1920, 1080)), ((3840, 2160), (1920, 1080))];
    for scene in [Scene::Scroll, Scene::Drag] {
        for (input, output) in sizes_07 {
            reg.add_case(
                MicroCase::new(
                    "B-MICRO-07",
                    format!("nv12/{}/{}->{}", scene.id(), res(input.0, input.1), res(output.0, output.1)),
                    FULL_ONLY,
                    move |ctx, r| nv12(ctx, r, scene, input, output),
                )
                .runnable(super::only_windows)
                .config(HW_CONFIG),
            );
        }
    }
}

#[cfg(windows)]
fn texture(
    ctx: &mut RunCtx,
    r: CaseResult,
    scene: Scene,
    input: (u32, u32),
    output: (u32, u32),
) -> Result<CaseResult> {
    imp::texture(ctx, r, scene, input, output)
}

#[cfg(windows)]
fn nv12(
    ctx: &mut RunCtx,
    r: CaseResult,
    scene: Scene,
    input: (u32, u32),
    output: (u32, u32),
) -> Result<CaseResult> {
    imp::nv12(ctx, r, scene, input, output)
}

#[cfg(not(windows))]
fn texture(_: &mut RunCtx, r: CaseResult, _: Scene, _: (u32, u32), _: (u32, u32)) -> Result<CaseResult> {
    Ok(super::not_runnable(r, "needs Windows"))
}

#[cfg(not(windows))]
fn nv12(_: &mut RunCtx, r: CaseResult, _: Scene, _: (u32, u32), _: (u32, u32)) -> Result<CaseResult> {
    Ok(super::not_runnable(r, "needs Windows"))
}

#[cfg(windows)]
mod imp {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow};
    use nuvira_codec::{EncoderPlan, EncoderSettings, GpuDevice, GpuTexture, VideoEncoder, convert, gpu};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::core::BOOL;

    use super::super::{
        QualityAcc, RepLoop, luma_diff, metric, not_runnable, nv12_to_yuv, scaled_kbps, single,
    };
    use super::RING;
    use crate::alloc::ThreadAllocs;
    use crate::fixtures::{Frame, Scene};
    use crate::measure::{Better, Gate, MAX_NOISY_RETRIES, Reps, cpu_time_process};
    use crate::quality::{self, Reference};
    use crate::registry::{CaseResult, RunCtx};

    const WARM_FRAMES: u32 = super::super::codec_sw::HW_WARM_FRAMES;

    struct Gpu {
        device: ID3D11Device,
        frames: Vec<Frame>,
        textures: Vec<GpuTexture>,
    }

    fn setup(scene: Scene, (w, h): (u32, u32)) -> Result<Gpu> {
        let device = gpu::create_device()?;
        let frames: Vec<Frame> = scene.frames(w, h, RING).collect();
        let textures = frames
            .iter()
            .map(|f| gpu::upload_bgra(&device, w, h, &f.data).map(GpuTexture))
            .collect::<Result<Vec<_>>>()?;
        Ok(Gpu { device, frames, textures })
    }

    /// The picture an output of size `out` should match: the source, reduced on the CPU.
    fn reduced(f: &Frame, out: (u32, u32)) -> Frame {
        if (f.width, f.height) == out {
            return f.clone();
        }
        let mut data = Vec::new();
        convert::scale_bgra(&f.image(), out.0 as usize, out.1 as usize, &mut data);
        Frame { index: f.index, width: out.0, height: out.1, data }
    }

    #[derive(Default)]
    struct RepOut {
        wait_ms: f64,
        encode_ms: f64,
        cpu_ms: f64,
        counted: u32,
        wall: f64,
        bytes: u64,
        allocs: u64,
        alloc_bytes: u64,
    }

    pub fn texture(
        ctx: &mut RunCtx,
        mut r: CaseResult,
        scene: Scene,
        input: (u32, u32),
        output: (u32, u32),
    ) -> Result<CaseResult> {
        let g = match setup(scene, input) {
            Ok(g) => g,
            Err(e) => return Ok(not_runnable(r, format!("Direct3D 11: {e:#}"))),
        };
        let kbps = scaled_kbps(8000, output.0, output.1);
        let settings = EncoderSettings { bitrate_bps: kbps * 1000, max_fps: 60, ltr: false };
        let make = || {
            nuvira_codec::create_encoder(
                EncoderPlan::HardwareGpu,
                settings,
                Some(GpuDevice(g.device.clone())),
            )
        };
        let probe = (|| -> Result<String> {
            let mut enc = make()?;
            anyhow::ensure!(enc.accepts_textures(), "{} does not take textures", enc.name());
            for i in 0..8 {
                enc.encode_texture(&g.textures[i % g.textures.len()], input, output, i == 0)?;
            }
            Ok(enc.name().to_string())
        })();
        let name = match probe {
            Ok(n) => n,
            Err(e) => {
                return Ok(not_runnable(r, format!("hardware encoder with textures not available: {e:#}")));
            }
        };
        if let Ok(list) = nuvira_codec::hardware_encoders() {
            r.param("hardware_encoders", list.join(", "));
        }
        let n = ctx.frames();
        let warm = WARM_FRAMES.min(n / 4);
        let mut stream: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut keep_next = true;
        let mut lp = RepLoop::new(ctx);
        let reps = lp.run(|_, measured| {
            let keep = measured && keep_next;
            if measured {
                keep_next = false;
            }
            let mut enc: Box<dyn VideoEncoder> = make()?;
            let mut pending: VecDeque<u32> = VecDeque::new();
            let mut o = RepOut::default();
            let mut start = Instant::now();
            for i in 0..n {
                if i == warm {
                    start = Instant::now();
                }
                let tex = &g.textures[(i % RING) as usize];
                let tw = Instant::now();
                enc.wait_ready(Duration::from_secs(2)).context("wait_ready")?;
                let wait = tw.elapsed();
                let a0 = ThreadAllocs::take();
                let p0 = cpu_time_process();
                let t0 = Instant::now();
                let out =
                    enc.encode_texture(tex, input, output, i == 0).with_context(|| format!("frame {i}"))?;
                let dt = t0.elapsed();
                let cpu = cpu_time_process().saturating_sub(p0);
                let al = ThreadAllocs::take().since(&a0);
                pending.push_back(i);
                if let Some(f) = out {
                    let src = pending.pop_front().expect("pending");
                    if i >= warm {
                        o.bytes += f.data.len() as u64;
                    }
                    if keep {
                        stream.push((src, f.data));
                    }
                } else if pending.len() > 8 {
                    pending.pop_front();
                }
                if i >= warm {
                    o.counted += 1;
                    o.wait_ms += wait.as_secs_f64() * 1e3;
                    o.encode_ms += dt.as_secs_f64() * 1e3;
                    o.cpu_ms += cpu.as_secs_f64() * 1e3;
                    o.allocs += al.allocs;
                    o.alloc_bytes += al.bytes;
                }
            }
            o.wall = start.elapsed().as_secs_f64();
            Ok(o)
        })?;
        lp.record(&mut r, n as u64);
        let per = |f: &dyn Fn(&RepOut) -> f64| reps.iter().map(f).collect::<Vec<_>>();
        let k = |o: &RepOut| o.counted.max(1) as f64;
        r.metric("fps", metric("1/s", Better::Higher, Gate::Micro, per(&|o| o.counted as f64 / o.wall)));
        r.metric("wait_ready_ms", metric("ms", Better::Lower, Gate::Micro, per(&|o| o.wait_ms / k(o))));
        r.metric("encode_call_ms", metric("ms", Better::Lower, Gate::Micro, per(&|o| o.encode_ms / k(o))));
        r.metric("cpu_ms_frame", crate::micro::cpu_metric(per(&|o| o.cpu_ms / k(o)), per(&|o| o.cpu_ms)));
        r.metric("bytes_frame", metric("B", Better::Lower, Gate::Info, per(&|o| o.bytes as f64 / k(o))));
        r.metric("allocs_frame", metric("1", Better::Lower, Gate::Micro, per(&|o| o.allocs as f64 / k(o))));
        r.metric(
            "alloc_bytes_frame",
            metric("B", Better::Lower, Gate::Micro, per(&|o| o.alloc_bytes as f64 / k(o))),
        );
        r.param("frames", n)
            .param("warmup_frames", warm)
            .param("ring", RING)
            .param("bitrate_kbps", kbps)
            .param("encoder", name.as_str());

        // Quality: decoded (OpenH264) vs the source reduced on the CPU.
        let refs: Vec<Arc<Frame>> = g.frames.iter().map(|f| Arc::new(reduced(f, output))).collect();
        let mut dec = nuvira_codec::software_decoder()?;
        let mut acc = QualityAcc::new();
        for (src, data) in &stream {
            if let Some(y) = dec.decode(data)? {
                acc.coded(*src, refs[(*src % RING) as usize].clone(), Arc::new(y.clone()), true);
            }
        }
        acc.finish(&mut r);
        Ok(r)
    }

    struct Queries {
        event: ID3D11Query,
        disjoint: ID3D11Query,
        ts0: ID3D11Query,
        ts1: ID3D11Query,
    }

    fn query(device: &ID3D11Device, kind: D3D11_QUERY) -> Result<ID3D11Query> {
        let desc = D3D11_QUERY_DESC { Query: kind, MiscFlags: 0 };
        let mut q = None;
        unsafe { device.CreateQuery(&desc, Some(&mut q)) }?;
        q.ok_or_else(|| anyhow!("missing query"))
    }

    /// Reads a query's data, waiting at most 1 s for it.
    fn get<T: Default + Copy>(
        context: &ID3D11DeviceContext,
        q: &ID3D11Query,
        done: impl Fn(&T) -> bool,
    ) -> Result<T> {
        let start = Instant::now();
        loop {
            let mut v = T::default();
            let ok =
                unsafe { context.GetData(q, Some((&raw mut v).cast()), std::mem::size_of::<T>() as u32, 0) };
            if ok.is_ok() && done(&v) {
                return Ok(v);
            }
            if start.elapsed() > Duration::from_secs(1) {
                anyhow::bail!("GPU query not done after 1 s");
            }
            std::hint::spin_loop();
        }
    }

    pub fn nv12(
        ctx: &mut RunCtx,
        mut r: CaseResult,
        scene: Scene,
        input: (u32, u32),
        output: (u32, u32),
    ) -> Result<CaseResult> {
        let g = match setup(scene, input) {
            Ok(g) => g,
            Err(e) => return Ok(not_runnable(r, format!("Direct3D 11: {e:#}"))),
        };
        let mut conv = match gpu::Nv12Converter::new(&g.device, input, output) {
            Ok(c) => c,
            Err(e) => return Ok(not_runnable(r, format!("video processor: {e:#}"))),
        };
        let context = unsafe { g.device.GetImmediateContext() }?;
        let q = Queries {
            event: query(&g.device, D3D11_QUERY_EVENT)?,
            disjoint: query(&g.device, D3D11_QUERY_TIMESTAMP_DISJOINT)?,
            ts0: query(&g.device, D3D11_QUERY_TIMESTAMP)?,
            ts1: query(&g.device, D3D11_QUERY_TIMESTAMP)?,
        };
        let gpu_sum = Cell::new(0f64);
        let gpu_n = Cell::new(0u64);
        let mut err: Option<anyhow::Error> = None;
        let mut k = 0usize;
        let mut step = || {
            let tex = &g.textures[k % g.textures.len()];
            k += 1;
            let res = (|| -> Result<()> {
                unsafe {
                    context.Begin(&q.disjoint);
                    context.End(&q.ts0);
                }
                let out = conv.convert(&tex.0)?;
                unsafe {
                    context.End(&q.ts1);
                    context.End(&q.disjoint);
                    context.End(&q.event);
                    context.Flush();
                }
                let _ = get::<BOOL>(&context, &q.event, |b| b.as_bool())?;
                let dj =
                    get::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(&context, &q.disjoint, |d| d.Frequency != 0)?;
                let t0 = get::<u64>(&context, &q.ts0, |&t| t != 0)?;
                let t1 = get::<u64>(&context, &q.ts1, |&t| t != 0)?;
                if !dj.Disjoint.as_bool() && t1 >= t0 {
                    gpu_sum.set(gpu_sum.get() + (t1 - t0) as f64 * 1e3 / dj.Frequency as f64);
                    gpu_n.set(gpu_n.get() + 1);
                }
                std::hint::black_box(out);
                Ok(())
            })();
            if let Err(e) = res {
                err.get_or_insert(e);
            }
        };
        // Sampler reps by hand, so the GPU time of each rep can be read.
        let mut s = super::super::sampler(ctx);
        s.calibrate(&mut step);
        for _ in 0..ctx.warmup.max(1) {
            s.rep(&mut step);
        }
        let mut reps = Reps { warmup_reps: ctx.warmup.max(1), ..Default::default() };
        let mut gpu_ms = Vec::new();
        while reps.reps.len() < ctx.reps.max(1) as usize {
            gpu_sum.set(0.0);
            gpu_n.set(0);
            let rep = s.rep(&mut step);
            if rep.noisy && reps.retries < MAX_NOISY_RETRIES {
                reps.retries += 1;
                continue;
            }
            reps.noisy |= rep.noisy;
            reps.reps.push(rep);
            gpu_ms.push(if gpu_n.get() > 0 { gpu_sum.get() / gpu_n.get() as f64 } else { f64::NAN });
        }
        reps.iterations = s.iterations.unwrap_or(1);
        drop(step);
        if let Some(e) = err {
            return Err(e);
        }
        r.reps(&reps);
        r.metric("ms_frame", reps.ms_per_iter(Gate::Micro));
        // Timestamps of the immediate context: the video processor may run on another
        // engine, so this is informational (`ms_frame`, until the GPU is done, gates).
        r.metric("gpu_ms_frame", metric("ms", Better::Lower, Gate::Info, gpu_ms));
        r.metric("cpu_ms_frame", reps.cpu_ms_per_iter(Gate::Micro));
        r.metric("allocs_frame", reps.allocs_per_iter(Gate::Micro));

        // Readback (not timed): GPU vs CPU luma, and PSNR/SSIM vs the bench reference.
        let (ow, oh) = (output.0 as usize, output.1 as usize);
        let mut diffs = Vec::new();
        let mut psnr = Vec::new();
        let mut ssim = Vec::new();
        for (i, f) in g.frames.iter().enumerate() {
            let out = conv.convert(&g.textures[i].0)?;
            let yuv = read_nv12(&g.device, &context, &out, ow, oh)?;
            let cpu = reduced(f, output);
            let mut buf = vec![0u8; ow * oh * 3 / 2];
            convert::bgra_to_nv12(&cpu.image(), &mut buf, ow, oh);
            diffs.push(luma_diff(&yuv.y, ow, &buf, ow, ow, oh));
            let reference = Reference::from_bgra(&cpu.image());
            psnr.push(quality::cap(quality::psnr(&yuv, &reference).yuv));
            ssim.push(quality::ssim_y(&yuv, &reference));
        }
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        r.metric("luma_diff_vs_cpu", single("levels", Better::Lower, Gate::Info, mean(&diffs)));
        r.metric("psnr_yuv_db", single("dB", Better::Higher, Gate::Quality, mean(&psnr)));
        r.metric("ssim_y", single("1", Better::Higher, Gate::Quality, mean(&ssim)));
        r.param("ring", RING).param("readback_frames", g.frames.len());
        Ok(r)
    }

    fn read_nv12(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        w: usize,
        h: usize,
    ) -> Result<nuvira_codec::YuvFrame> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }?;
        let staging = staging.ok_or_else(|| anyhow!("missing staging texture"))?;
        unsafe {
            context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).context("Map")?;
            let pitch = mapped.RowPitch as usize;
            let rows = desc.Height as usize;
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * rows * 3 / 2);
            let yuv = nv12_to_yuv(data, w, h, pitch, pitch * rows);
            context.Unmap(&staging, 0);
            Ok(yuv)
        }
    }
}
