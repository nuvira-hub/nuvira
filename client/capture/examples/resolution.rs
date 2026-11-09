//! Tries the resolution changes on this computer.
//!
//! ```text
//! resolution list                      screens, their modes and the current one
//! resolution fit W H [SECONDS] [N]     screen N (0) to the mode for a W x H area, then back
//! resolution leave W H STATE [N]       changes it and ends without putting it back
//! resolution recover STATE             puts back what `leave` left
//! ```
use std::path::Path;
use std::time::Duration;

use nuvira_capture::resolution::{self, Size};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |i: usize, default: u32| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(default);
    let displays = nuvira_capture::displays()?;
    match args.first().map(String::as_str) {
        Some("list") | None => {
            println!("supported: {}", resolution::supported());
            for d in &displays {
                println!("{} {} {}x{} at {},{}", d.index, d.name, d.width, d.height, d.left, d.top);
                match resolution::modes(d) {
                    Ok((modes, current)) => {
                        println!("  current {current}");
                        println!(
                            "  modes {}",
                            modes.iter().map(Size::to_string).collect::<Vec<_>>().join(" ")
                        );
                    }
                    Err(e) => println!("  modes: {e:#}"),
                }
            }
        }
        Some("fit") => {
            let target = Size { width: num(1, 1280), height: num(2, 720) };
            let display = &displays[num(4, 0) as usize];
            let got = resolution::fit(1, display, target)?;
            println!("now {got}; after: {:?}", nuvira_capture::displays()?.get(display.index));
            std::thread::sleep(Duration::from_secs(num(3, 5).into()));
            println!("put back: {}", resolution::restore(1, None));
            println!("after: {:?}", nuvira_capture::displays()?.get(display.index));
        }
        Some("leave") => {
            let target = Size { width: num(1, 1280), height: num(2, 720) };
            resolution::recover(Path::new(&args[3]));
            let got = resolution::fit(1, &displays[num(4, 0) as usize], target)?;
            println!("now {got}, ending without putting it back");
            std::process::exit(0);
        }
        Some("recover") => {
            resolution::recover(Path::new(&args[1]));
            println!("after: {:?}", nuvira_capture::displays()?);
        }
        Some(other) => anyhow::bail!("unknown command {other}"),
    }
    Ok(())
}
