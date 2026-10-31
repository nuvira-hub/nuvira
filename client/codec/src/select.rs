//! Automatic encoder choice.
//!
//! The hardware encoder is not always the fastest: on an i7-10710U with Intel UHD,
//! OpenH264 with NASM encodes 1080p in ~9 ms against ~11 for Quick Sync, and the
//! graphics card may be busy with something else. At start-up software, hardware
//! with the image from RAM and hardware with the image already on the GPU are
//! tried on a desktop-like image; the fastest is chosen and the decision holds
//! for the whole life of the process.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tracing::info;

use crate::{BgraImage, EncoderPlan, EncoderSettings, VideoEncoder};

/// Test frames per encoder; the first `WARMUP` do not count (start-up, first keyframe).
const FRAMES: usize = 24;
const WARMUP: usize = 4;
/// Frame arrival rate during the test, like a 60 fps capture.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Hardware is preferred unless software is faster by more than this factor:
/// with equal times it leaves the CPU free.
const SOFTWARE_MARGIN: f64 = 1.25;

fn test_frames(w: usize, h: usize) -> [Vec<u8>; 2] {
    let frame = |shift: usize| {
        let mut data = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = &mut data[(y * w + x) * 4..][..4];
                // Background, window with rows of "text" moved by `shift` pixels.
                let in_window = (200 + shift..1000 + shift).contains(&x) && (150..750).contains(&y);
                let c = if in_window && (y / 18) % 2 == 0 && (x / 7) % 3 != 0 {
                    [20, 20, 20, 255]
                } else if in_window {
                    [250, 250, 250, 255]
                } else {
                    [((x + y) % 256) as u8, 120, 40, 255]
                };
                p.copy_from_slice(&c);
            }
        }
        data
    };
    [frame(0), frame(40)]
}

/// Average steady-state encoding time, in microseconds.
fn measure(encoder: &mut dyn VideoEncoder, frames: &[Vec<u8>; 2], w: u32, h: u32) -> anyhow::Result<f64> {
    measure_with(|i| {
        let image = BgraImage { width: w, height: h, stride: w as usize * 4, data: &frames[i % 2] };
        encoder.encode(&image, i == 0).map(drop)
    })
}

/// Like [`measure`], with the images uploaded to the GPU first.
#[cfg(windows)]
fn measure_gpu(settings: EncoderSettings, frames: &[Vec<u8>; 2], w: u32, h: u32) -> anyhow::Result<f64> {
    let device = crate::gpu::create_device()?;
    let textures = [
        crate::gpu::GpuTexture(crate::gpu::upload_bgra(&device, w, h, &frames[0])?),
        crate::gpu::GpuTexture(crate::gpu::upload_bgra(&device, w, h, &frames[1])?),
    ];
    let mut encoder =
        crate::create_encoder(EncoderPlan::HardwareGpu, settings, Some(crate::GpuDevice(device)))?;
    measure_with(|i| encoder.encode_texture(&textures[i % 2], (w, h), (w, h), i == 0).map(drop))
}

fn measure_with(mut encode: impl FnMut(usize) -> anyhow::Result<()>) -> anyhow::Result<f64> {
    let mut total = Duration::ZERO;
    for i in 0..FRAMES {
        let t = Instant::now();
        encode(i)?;
        let elapsed = t.elapsed();
        if i >= WARMUP {
            total += elapsed;
        }
        std::thread::sleep(FRAME_INTERVAL.saturating_sub(elapsed));
    }
    Ok(total.as_micros() as f64 / (FRAMES - WARMUP) as f64)
}

/// Encoding plan in automatic mode (computed only once).
pub(crate) fn auto_plan(settings: EncoderSettings) -> EncoderPlan {
    static PLAN: OnceLock<EncoderPlan> = OnceLock::new();
    *PLAN.get_or_init(|| {
        let (w, h) = (1920u32, 1080u32);
        let frames = test_frames(w as usize, h as usize);
        let try_plan = |plan| {
            crate::create_encoder(plan, settings, None).and_then(|mut e| measure(e.as_mut(), &frames, w, h))
        };
        let software = try_plan(EncoderPlan::Software);
        let hardware = try_plan(EncoderPlan::Hardware);
        #[cfg(windows)]
        let hardware_gpu = measure_gpu(settings, &frames, w, h);
        #[cfg(not(windows))]
        let hardware_gpu: anyhow::Result<f64> = Err(anyhow::anyhow!("Windows only"));

        // Best hardware path, preferring the GPU with equal times.
        let best_hw = match (&hardware_gpu, &hardware) {
            (Ok(g), Ok(r)) if r < g => Some((EncoderPlan::Hardware, *r)),
            (Ok(g), _) => Some((EncoderPlan::HardwareGpu, *g)),
            (Err(_), Ok(r)) => Some((EncoderPlan::Hardware, *r)),
            (Err(_), Err(_)) => None,
        };
        let plan = match (best_hw, &software) {
            (Some((_, hw)), Ok(sw)) if sw * SOFTWARE_MARGIN < hw => EncoderPlan::Software,
            (Some((plan, _)), _) => plan,
            (None, _) => EncoderPlan::Software,
        };
        let fmt = |r: &anyhow::Result<f64>| match r {
            Ok(us) => format!("{:.1} ms", us / 1000.0),
            Err(e) => format!("not available ({e:#})"),
        };
        info!(
            "1080p encoder test: software {}, hardware from RAM {}, hardware from GPU {} → {:?}",
            fmt(&software),
            fmt(&hardware),
            fmt(&hardware_gpu),
            plan
        );
        plan
    })
}
