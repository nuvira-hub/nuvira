//! How fast does the hardware encoder take GPU textures, frame after frame?
//! `cargo run --release -p nuvira-codec --example hwpace [WxH] [frames]`
//!
//! Same path as the agent (BGRA texture → NV12 on the GPU → Media Foundation
//! encoder), without capture and network: the agent waits until the encoder is
//! ready, then encodes. Prints frames per second and where the time goes.

#[cfg(windows)]
fn main() {
    use std::time::{Duration, Instant};

    use nuvira_codec::{EncoderSettings, GpuDevice, GpuTexture, MfEncoder, VideoEncoder, gpu};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let size = std::env::args().nth(1).unwrap_or_else(|| "1920x1080".into());
    let frames: usize = std::env::args().nth(2).map(|n| n.parse().expect("frames")).unwrap_or(300);
    let (w, h) = size.split_once('x').expect("WxH format");
    let (w, h): (u32, u32) = (w.parse().unwrap(), h.parse().unwrap());
    // Optional encoded size, smaller than the image (scaled on the GPU): OUT=1280x720.
    let encoded_size: (u32, u32) = std::env::var("OUT")
        .ok()
        .and_then(|s| s.split_once('x').map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap())))
        .unwrap_or((w, h));

    let device = gpu::create_device().expect("D3D11 device");
    // A few different images, so that every frame really changes.
    let textures: Vec<GpuTexture> = (0..8)
        .map(|t| {
            let mut data = vec![0u8; (w * h * 4) as usize];
            for (i, p) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (i as u32 % w, i as u32 / w);
                let v = ((x + y * 3 + t * 40) % 256) as u8;
                p.copy_from_slice(&[v, v.wrapping_mul(3), 255 - v, 255]);
            }
            GpuTexture(gpu::upload_bgra(&device, w, h, &data).expect("texture"))
        })
        .collect();

    let settings = EncoderSettings { bitrate_bps: 8_000_000, max_fps: 60 };
    let mut enc = MfEncoder::with_device(settings, GpuDevice(device)).expect("hardware encoder");
    println!(
        "encoder: {}, {w}x{h} encoded at {}x{}, {frames} frames",
        enc.device_name(),
        encoded_size.0,
        encoded_size.1
    );

    let (mut waited, mut encoded, mut out) = (Duration::ZERO, Duration::ZERO, 0usize);
    let mut start = Instant::now();
    for i in 0..frames {
        if i == 10 {
            // The first frames start the session: they are not counted.
            (waited, encoded, out) = (Duration::ZERO, Duration::ZERO, 0);
            start = Instant::now();
        }
        let t = Instant::now();
        enc.wait_ready(Duration::from_secs(2)).expect("wait");
        waited += t.elapsed();
        let t = Instant::now();
        if enc
            .encode_texture(&textures[i % textures.len()], (w, h), encoded_size, i == 0)
            .expect("encode")
            .is_some()
        {
            out += 1;
        }
        encoded += t.elapsed();
    }
    let n = (frames - 10) as f64;
    let total = start.elapsed();
    println!(
        "{:.1} fps, {out} outputs; per frame: waiting for the encoder {:.1} ms, encode call {:.1} ms",
        n / total.as_secs_f64(),
        waited.as_secs_f64() * 1000.0 / n,
        encoded.as_secs_f64() * 1000.0 / n,
    );
}

#[cfg(not(windows))]
fn main() {}
