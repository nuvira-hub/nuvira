//! Checks the scaling done by the GPU before the hardware encoder:
//! `cargo run --release -p nuvira-codec --example gpuscale [WxH] [OUTxOUT]`
//!
//! Encodes the same image reduced on the GPU, decodes it and compares the luma
//! with the same image reduced on the CPU: a crop or a wrong size would show as
//! a large difference.

#[cfg(windows)]
fn main() {
    use nuvira_codec::{
        BgraImage, EncoderSettings, GpuDevice, GpuTexture, MfEncoder, OpenH264Decoder, VideoDecoder,
        VideoEncoder, convert, gpu,
    };

    let parse = |s: String| {
        let (a, b) = s.split_once('x').expect("WxH format");
        (a.parse::<u32>().unwrap(), b.parse::<u32>().unwrap())
    };
    let (w, h) = parse(std::env::args().nth(1).unwrap_or_else(|| "1920x1080".into()));
    let (ow, oh) = parse(std::env::args().nth(2).unwrap_or_else(|| "1280x720".into()));

    // Tiles with gradients, like the agent's synthetic image: smooth inside, edges between.
    let mut data = vec![0u8; (w * h * 4) as usize];
    for (i, p) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i as u32 % w, i as u32 / w);
        let (tx, ty) = (x / 256, y / 256);
        *p = [((x % 256) as u8), ((y % 256) as u8), ((tx * 60 + ty * 30) % 256) as u8, 255];
    }
    let device = gpu::create_device().expect("D3D11 device");
    let texture = GpuTexture(gpu::upload_bgra(&device, w, h, &data).expect("texture"));
    let settings = EncoderSettings { bitrate_bps: 40_000_000, max_fps: 60 };
    let mut enc = MfEncoder::with_device(settings, GpuDevice(device)).expect("hardware encoder");
    let mut dec = OpenH264Decoder::new().unwrap();
    let mut decoded = None;
    for i in 0..30 {
        if let Some(f) = enc.encode_texture(&texture, (w, h), (ow, oh), i == 0).expect("encode")
            && let Some(yuv) = dec.decode(&f.data).expect("decode")
        {
            decoded = Some((yuv.width, yuv.height, yuv.y.clone()));
        }
    }
    let (dw, dh, y) = decoded.expect("no decoded frame");
    assert_eq!((dw, dh), (ow, oh), "decoded size");

    let full = BgraImage { width: w, height: h, stride: w as usize * 4, data: &data };
    let mut small = Vec::new();
    convert::scale_bgra(&full, ow as usize, oh as usize, &mut small);
    let reduced = BgraImage { width: ow, height: oh, stride: ow as usize * 4, data: &small };
    let (mut ry, mut ru, mut rv) = (Vec::new(), Vec::new(), Vec::new());
    convert::bgra_to_i420(&reduced, &mut ry, &mut ru, &mut rv);
    let diff = ry.iter().zip(&y).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>() as f64 / ry.len() as f64;
    println!(
        "{w}x{h} reduced on the GPU to {dw}x{dh}: mean luma difference from the CPU reduction {diff:.2} levels"
    );
    assert!(diff < 6.0, "the GPU image does not match");
}

#[cfg(not(windows))]
fn main() {}
