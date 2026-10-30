//! Lists the screens and measures capture: `cargo run -p nuvira-capture --example probe`.

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
