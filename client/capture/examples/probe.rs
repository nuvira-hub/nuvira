//! Elenca gli schermi e misura la cattura: `cargo run -p nexodesk-capture --example probe`.

use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    for d in nexodesk_capture::displays()? {
        println!("{d:?}");
    }
    let mut capturer = nexodesk_capture::open(None)?;
    println!("cattura di {:?}", capturer.display());

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
    println!("{frames} frame, {timeouts} attese senza cambiamenti in 3 s");
    Ok(())
}
