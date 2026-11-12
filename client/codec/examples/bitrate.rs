//! Checks that the encoder follows a bitrate change during the session:
//! `cargo run --release -p nuvira-codec --example bitrate -- [hardware|software]`.
//!
//! Encodes 90 moving desktop-like frames at 6 Mbit/s, then asks for 1 Mbit/s for
//! another 90 frames and compares the bandwidth produced in the two halves.
//! `noisy` as second argument adds a changing photo-like area (harder than the
//! encoders' budget) and a third part back at 6 Mbit/s.

use nuvira_codec::{BgraImage, EncoderPlan, EncoderSettings};

fn frame(w: usize, h: usize, t: usize) -> Vec<u8> {
    // Shaded background and a window with rows of "text" moving around: like a desktop.
    let mut data = vec![0u8; w * h * 4];
    let wx = (t * 12) % (w / 2);
    for y in 0..h {
        for x in 0..w {
            let p = &mut data[(y * w + x) * 4..][..4];
            let in_window = (wx + 80..wx + 680).contains(&x) && (100..560).contains(&y);
            let c = if in_window && (y / 16) % 2 == 0 && (x / 6) % 4 != 0 {
                [20, 20, 20, 255]
            } else if in_window {
                [245, 245, 245, 255]
            } else {
                [(x * 255 / w) as u8, (y * 255 / h) as u8, 120, 255]
            };
            p.copy_from_slice(&c);
        }
    }
    data
}

/// A 640x360 area of grain changing with `t`, like a video or a photo scrolling.
fn add_noise(data: &mut [u8], w: usize, t: usize) {
    let mut seed = 0x9e37_79b9u32.wrapping_mul(t as u32 + 1);
    for y in 300..660 {
        for x in 600..1240 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let v = (seed >> 24) as u8;
            let base = ((x + y + t * 9) % 256) as u8;
            data[(y * w + x) * 4..][..4].copy_from_slice(&[base / 2 + v / 2, v, 255 - base / 2 - v / 4, 255]);
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let plan = match std::env::args().nth(1).as_deref() {
        Some("software") => EncoderPlan::Software,
        _ => EncoderPlan::Hardware,
    };
    let (w, h) = (1280usize, 720usize);
    let noisy = std::env::args().nth(2).as_deref() == Some("noisy");
    let frames: Vec<Vec<u8>> = (0..30)
        .map(|t| {
            let mut f = frame(w, h, t);
            if noisy {
                add_noise(&mut f, w, t);
            }
            f
        })
        .collect();
    let settings = EncoderSettings { bitrate_bps: 6_000_000, max_fps: 30, ..Default::default() };
    let mut enc = nuvira_codec::create_encoder(plan, settings, None).expect("encoder");
    let parts: &[(usize, u32)] =
        if noisy { &[(0, 6_000_000), (1, 1_000_000), (2, 6_000_000)] } else { &[(0, 6_000_000), (1, 1_000_000)] };
    for &(half, target) in parts {
        if half > 0 {
            enc.set_bitrate(target);
        }
        let mut bytes = 0usize;
        let started = std::time::Instant::now();
        for i in 0..90 {
            let data = &frames[(half * 90 + i) % frames.len()];
            let img = BgraImage { width: w as u32, height: h as u32, stride: w * 4, data };
            if let Some(out) = enc.encode(&img, half == 0 && i == 0).expect("encoding") {
                bytes += out.data.len();
            }
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
        // 90 frame a 30 fps = 3 s; the real time is longer by the encoding time.
        let real = bytes as f64 * 8.0 / started.elapsed().as_secs_f64() / 1000.0;
        println!(
            "{plan:?}: asked {} kbit/s → produced {} kbit/s at 30 fps, {real:.0} kbit/s in real time",
            target / 1000,
            bytes * 8 / 3 / 1000
        );
    }
}
