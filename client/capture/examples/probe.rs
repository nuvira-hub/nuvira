//! Lists the screens and measures capture: `cargo run -p nuvira-capture --example probe`.
//! `-- FILE.ppm` also saves the first frame, to see what the capture gets (a
//! lock or sign-in screen, for example).

use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    for d in nuvira_capture::displays()? {
        println!("{d:?}");
    }
    let mut capturer = nuvira_capture::open(None)?;
    println!("capturing {:?}", capturer.display());

    let start = Instant::now();
    let (mut frames, mut timeouts) = (0, 0);
    while start.elapsed() < Duration::from_secs(3) {
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
                if frames <= 3 {
                    println!("frame {}x{} stride {} in {:?}", f.width, f.height, f.stride, t.elapsed());
                }
            }
            None => timeouts += 1,
        }
    }
    println!("{frames} frames, {timeouts} waits without changes in 3 s");
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
