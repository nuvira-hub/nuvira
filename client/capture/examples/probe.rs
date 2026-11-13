//! Lists the screens and measures capture: `cargo run -p nuvira-capture --example probe`.
//! `-- FILE.ppm` also saves the first frame, to see what the capture gets (a
//! lock or sign-in screen, for example). On GNOME `NUVIRA_GNOME_DIRECT=1` uses
//! its own remote desktop API, as unattended access does.

use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    // RUST_LOG=nuvira_capture=debug shows the capture's choices (format, DMA-BUF).
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    // NUVIRA_GNOME_DIRECT=1: GNOME's own remote desktop API instead of the portal.
    #[cfg(target_os = "linux")]
    nuvira_portal::set_direct(std::env::var_os("NUVIRA_GNOME_DIRECT").is_some());
    for d in nuvira_capture::displays()? {
        println!("{d:?}");
    }
    let mut capturer = nuvira_capture::open(None)?;
    println!("capturing {:?}", capturer.display());

    let start = Instant::now();
    let (mut frames, mut timeouts) = (0, 0);
    // NUVIRA_PROBE_SECS: how long to capture (3 s); the last frame is saved too.
    let secs = std::env::var("NUVIRA_PROBE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let mut last = None;
    while start.elapsed() < Duration::from_secs(secs) {
        let t = Instant::now();
        match capturer.next_frame(Duration::from_millis(100))? {
            Some(f) => {
                frames += 1;
                if frames == 1 {
                    if let Some(path) = std::env::args().nth(1) {
                        save_ppm(&path, &f)?;
                        println!("first frame saved to {path}");
                    }
                }
                if secs > 3 {
                    last = Some((f.width, f.height, f.stride, f.data.clone()));
                }
                if frames <= 3 {
                    println!("frame {}x{} stride {} in {:?}", f.width, f.height, f.stride, t.elapsed());
                }
            }
            None => timeouts += 1,
        }
    }
    println!("{frames} frames, {timeouts} waits without changes in {secs} s");
    if let (Some((width, height, stride, data)), Some(path)) = (last, std::env::args().nth(1)) {
        let frame = nuvira_capture::Frame {
            width,
            height,
            stride,
            data,
            captured_us: 0,
            #[cfg(windows)]
            texture: None,
            #[cfg(target_os = "linux")]
            dmabuf: None,
        };
        let path = format!("{path}.last.ppm");
        save_ppm(&path, &frame)?;
        println!("last frame saved to {path}");
    }
    Ok(())
}

fn save_ppm(path: &str, f: &nuvira_capture::Frame) -> anyhow::Result<()> {
    let mut out = format!("P6
{} {}
255
", f.width, f.height).into_bytes();
    for row in f.data.chunks(f.stride).take(f.height as usize) {
        for px in row[..f.width as usize * 4].chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    std::fs::write(path, out)?;
    Ok(())
}
