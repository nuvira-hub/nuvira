//! Compares the decoders: `cargo run --release -p nuvira-codec --example decbench [WxH]`.
//!
//! Encodes a desktop-like image with a moving window, then decodes the stream with
//! OpenH264 and with the hardware decoder: time per frame, frames that come out
//! late (a frame must come out of the same call that takes it), and difference
//! between the two images.

#[cfg(windows)]
fn main() {
    use std::time::Instant;

    use nuvira_codec::{BgraImage, EncoderPlan, EncoderSettings, MfDecoder, OpenH264Decoder, VideoDecoder};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let size = std::env::args().nth(1).unwrap_or_else(|| "1920x1080".into());
    let (w, h) = size.split_once('x').expect("WxH format");
    let (w, h): (usize, usize) = (w.parse().unwrap(), h.parse().unwrap());

    let plan = if std::env::var("ENCODER").as_deref() == Ok("software") {
        EncoderPlan::Software
    } else {
        EncoderPlan::Hardware
    };
    let mut enc = nuvira_codec::create_encoder(plan, EncoderSettings::default(), None).expect("encoder");
    let mut stream = Vec::new();
    for t in 0..240 {
        let mut data = vec![0u8; w * h * 4];
        for (i, p) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            let wx = (t * 6) % (w / 2);
            *p = if (wx + 100..wx + 900).contains(&x) && (150..750).contains(&y) {
                if (y / 18) % 2 == 0 && (x / 7) % 3 != 0 { [20, 20, 20, 255] } else { [250, 250, 250, 255] }
            } else {
                [180, 120, 40, 255]
            };
        }
        let img = BgraImage { width: w as u32, height: h as u32, stride: w * 4, data: &data };
        if let Some(f) = enc.encode(&img, t == 0).expect("encode") {
            stream.push(f.data);
        }
    }
    println!("{} frames encoded with {}, {w}x{h}", stream.len(), enc.name());

    let mut soft = OpenH264Decoder::new().unwrap();
    let mut hard = match MfDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            println!("hardware decoder not available: {e:#}");
            return;
        }
    };
    let (mut t_soft, mut t_hard, mut late, mut diff_sum, mut diff_max) = (0u128, 0u128, 0, 0f64, 0f64);
    for (i, frame) in stream.iter().enumerate() {
        let t = Instant::now();
        let a = soft.decode(frame).expect("openh264").map(|f| f.y.clone());
        t_soft += t.elapsed().as_micros();
        let t = Instant::now();
        let b = hard.decode(frame).expect("hardware");
        t_hard += t.elapsed().as_micros();
        let Some(b) = b else {
            late += 1;
            continue;
        };
        if let Some(a) = a {
            assert_eq!((b.width as usize, b.height as usize), (w, h), "frame {i}: size");
            let d =
                a.iter().zip(&b.y).map(|(x, y)| x.abs_diff(*y) as u64).sum::<u64>() as f64 / a.len() as f64;
            diff_sum += d;
            diff_max = diff_max.max(d);
        }
    }
    let n = stream.len() as f64;
    println!(
        "OpenH264 {:.2} ms/frame, hardware {:.2} ms/frame (with readback to RAM); \
         {late} frames late; Y difference: mean {:.3}, worst {:.3} levels",
        t_soft as f64 / n / 1000.0,
        t_hard as f64 / n / 1000.0,
        diff_sum / n,
        diff_max
    );
}

#[cfg(not(windows))]
fn main() {}
