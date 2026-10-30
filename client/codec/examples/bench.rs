//! Compares the encoders: `cargo run --release -p nuvira-codec --example bench [WxH]`.
//!
//! Encodes 120 frames of a desktop-like image with a moving window and checks
//! that OpenH264 can decode the result.

use std::time::Instant;

use nuvira_codec::{BgraImage, EncoderPlan, EncoderSettings, VideoDecoder, VideoEncoder};

const FRAMES: usize = 120;

fn desktop(w: usize, h: usize, t: usize) -> Vec<u8> {
    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let p = &mut data[(y * w + x) * 4..][..4];
            // Plain background, taskbar, fake text rows and a scrolling window.
            let mut c = [180u8, 120, 40, 255];
            if y > h - 48 {
                c = [30, 30, 30, 255];
            }
            let wx = (t * 6) % (w / 2);
            if (wx + 100..wx + 900).contains(&x) && (150..750).contains(&y) {
                c = [250, 250, 250, 255];
                if (y / 18) % 2 == 0 && (x / 7) % 3 != 0 && x % 7 != 0 {
                    c = [20, 20, 20, 255];
                }
            }
            p.copy_from_slice(&c);
        }
    }
    data
}

fn run(label: &str, mut enc: Box<dyn VideoEncoder>, w: usize, h: usize) {
    let frames: Vec<Vec<u8>> = (0..30).map(|t| desktop(w, h, t)).collect();
    let mut dec = nuvira_codec::OpenH264Decoder::new().unwrap();
    let (mut total, mut bytes, mut decoded, mut encoded) = (0u128, 0usize, 0, 0);
    let mut worst = 0u128;
    for i in 0..FRAMES {
        let data = &frames[i % frames.len()];
        let img = BgraImage { width: w as u32, height: h as u32, stride: w * 4, data };
        let t = Instant::now();
        let out = enc.encode(&img, i == 0).expect("encoding");
        let us = t.elapsed().as_micros();
        total += us;
        worst = worst.max(us);
        if let Some(out) = out {
            encoded += 1;
            bytes += out.data.len();
            if dec.decode(&out.data).expect("invalid stream").is_some() {
                decoded += 1;
            }
        }
    }
    println!(
        "{label:<28} {:>6.2} ms/frame (worst {:>6.2} ms), {:>5} kB/frame, {encoded}/{FRAMES} encoded, {decoded} decoded",
        total as f64 / FRAMES as f64 / 1000.0,
        worst as f64 / 1000.0,
        bytes / encoded.max(1) / 1024,
    );
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let size = std::env::args().nth(1).unwrap_or_else(|| "1920x1080".into());
    let (w, h) = size.split_once('x').expect("WxH format");
    let (w, h): (usize, usize) = (w.parse().unwrap(), h.parse().unwrap());
    let settings = EncoderSettings::default();

    #[cfg(windows)]
    match nuvira_codec::hardware_encoders() {
        Ok(list) => println!("hardware encoders found: {list:?}"),
        Err(e) => println!("hardware encoder enumeration failed: {e:#}"),
    }
    match nuvira_codec::create_encoder(EncoderPlan::Hardware, settings, None) {
        Ok(enc) => run("hardware (Media Foundation)", enc, w, h),
        Err(e) => println!("hardware not available: {e:#}"),
    }
    run(
        "software (OpenH264)",
        nuvira_codec::create_encoder(EncoderPlan::Software, settings, None).unwrap(),
        w,
        h,
    );
}
