//! Recovery after a loss with long-term references, without a keyframe:
//! `cargo run --release -p nuvira-codec --example ltrtest [WxH]`
//!
//! Encodes a desktop-like image with a moving window on the hardware encoder with
//! long-term references, keeps frame 30 in slot 1, drops frames 60-62 as the
//! network would, and encodes frame 63 referring only to a long-term reference.
//! Both decoders then get the stream without the dropped frames, and every frame
//! decoded is compared with the original image (PSNR of the luminance): a correct
//! recovery stays as close to the original as without losses, a broken one drifts.
//!
//! Scenarios: `ref` (no loss), `none` (loss, no recovery: the decoder goes on with
//! the broken frames), `slot0` (recovery from the slot the encoder fills with the
//! keyframe on its own), `slot1` (recovery from frame 30). `NOLTR=1` encodes without
//! long-term references (only `ref` is meaningful then); `LAG=1` compares frame 40
//! with the images before it, for decoders that give the image late.
//!
//! Found on 2026-11-02 (docs/13-phase1-network.md): the hardware decoder recovers on
//! Quick Sync and NVIDIA; OpenH264 decodes streams with long-term references wrongly
//! even without losses, and refuses every frame after a gap.

#[cfg(windows)]
fn main() {
    use nuvira_codec::{
        BgraImage, EncoderSettings, MfDecoder, MfEncoder, OpenH264Decoder, VideoDecoder, VideoEncoder,
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let size = std::env::args().nth(1).unwrap_or_else(|| "1280x720".into());
    let (w, h) = size.split_once('x').expect("WxH format");
    let (w, h): (usize, usize) = (w.parse().unwrap(), h.parse().unwrap());
    const FRAMES: usize = 150;
    const MARKED: usize = 30;
    const LOST: std::ops::RangeInclusive<usize> = 60..=62;
    const RECOVERY: usize = 63;

    let image = |t: usize| -> Vec<u8> {
        let mut data = vec![0u8; w * h * 4];
        let wx = (t * 6) % (w / 2);
        for (i, p) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            *p = if (wx + 50..wx + 600).contains(&x) && (100..500).contains(&y) {
                if (y / 18) % 2 == 0 && (x / 7) % 3 != 0 { [20, 20, 20, 255] } else { [250, 250, 250, 255] }
            } else {
                [180, 120, 40, 255]
            };
        }
        data
    };
    // Luminance of the original (BT.601, limited range: what the decoder gives back
    // within a few levels).
    let luma = |data: &[u8]| -> Vec<u8> {
        data.as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                let (b, g, r) = (p[0] as u32, p[1] as u32, p[2] as u32);
                (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
            })
            .collect()
    };
    let psnr = |a: &[u8], b: &[u8]| -> f64 {
        let mse = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
    };

    for scenario in ["ref", "none", "slot0", "slot1"] {
        let settings = EncoderSettings { bitrate_bps: 4_000_000, max_fps: 60, ltr: std::env::var_os("NOLTR").is_none() };
        let mut enc = MfEncoder::new(settings).expect("hardware encoder");
        let mut stream = Vec::new();
        for t in 0..FRAMES {
            if t == MARKED {
                let _ = enc.mark_ltr(1);
            }
            if t == RECOVERY {
                match scenario {
                    "slot0" => assert!(enc.use_ltr(0)),
                    "slot1" => assert!(enc.use_ltr(1)),
                    _ => {}
                }
            }
            let data = image(t);
            let img = BgraImage { width: w as u32, height: h as u32, stride: w * 4, data: &data };
            if let Some(f) = enc.encode(&img, t == 0).expect("encode") {
                stream.push(f);
            }
        }
        let tags: Vec<String> = stream
            .iter()
            .enumerate()
            .filter(|(_, f)| f.keyframe || f.ltr_mark.is_some() || f.ltr_ref.is_some())
            .map(|(i, f)| format!("{i}:{}{:?}{:?}", if f.keyframe { "K" } else { "" }, f.ltr_mark, f.ltr_ref))
            .collect();
        println!("== {scenario}: {} frames from {}, tags {}", stream.len(), enc.device_name(), tags.join(" "));
        let sizes: Vec<String> =
            [0, 1, MARKED, 59, RECOVERY, RECOVERY + 1].iter().map(|&t| format!("{t}:{}", stream[t].data.len())).collect();
        println!("   bytes {}", sizes.join(" "));

        let lose = scenario != "ref";
        let decoders: Vec<Box<dyn VideoDecoder>> = vec![
            Box::new(OpenH264Decoder::new().unwrap()),
            Box::new(MfDecoder::new().expect("hardware decoder")),
        ];
        for mut dec in decoders {
            let (mut errors, mut late, mut worst_after) = (0, 0, 99.0f64);
            let mut around = Vec::new();
            for (t, frame) in stream.iter().enumerate() {
                if lose && LOST.contains(&t) {
                    continue;
                }
                let out = match dec.decode(&frame.data) {
                    Ok(out) => out,
                    Err(e) => {
                        errors += 1;
                        if errors <= 2 {
                            println!("   {} frame {t}: {e:#}", dec.name());
                        }
                        continue;
                    }
                };
                let Some(out) = out else {
                    late += 1;
                    continue;
                };
                // Some decoders give the image one frame late: the closer of the two.
                let p = psnr(&out.y, &luma(&image(t))).max(psnr(&out.y, &luma(&image(t.saturating_sub(1)))));
                if t >= RECOVERY {
                    worst_after = worst_after.min(p);
                }
                if t == 40 && std::env::var_os("LAG").is_some() {
                    let lags: Vec<String> =
                        (0..6).map(|k| format!("-{k}:{:.1}", psnr(&out.y, &luma(&image(t - k))))).collect();
                    println!("   {} frame 40 against earlier images {}", dec.name(), lags.join(" "));
                }
                if (55..=70).contains(&t) || t == FRAMES - 1 {
                    around.push(format!("{t}:{p:.1}"));
                }
            }
            println!(
                "   {scenario:6} {:16} errors {errors}, late {late}, worst PSNR from frame {RECOVERY}: {worst_after:.1} dB",
                dec.name()
            );
            println!("   {:16} {}", "", around.join(" "));
        }
    }
}

#[cfg(not(windows))]
fn main() {}
