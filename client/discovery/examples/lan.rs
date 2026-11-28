//! Tries the announcements by hand:
//!   cargo run -p nuvira-discovery --example lan -- announce ID NAME [SECONDS]
//!   cargo run -p nuvira-discovery --example lan -- scan [SECONDS]
use std::time::Duration;

use nuvira_discovery::{Announcement, announce, scan, this_os};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs = |i: usize, default: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(default);
    match args.first().map(String::as_str) {
        Some("announce") => {
            let a = Announcement {
                id: args.get(1).cloned().unwrap_or_default(),
                name: args.get(2).cloned().unwrap_or_default(),
                os: this_os().into(),
                server: "00000000".into(),
            };
            let _held = announce(&a).await?;
            println!("announced {a:?}");
            tokio::time::sleep(Duration::from_secs(secs(3, 60))).await;
            println!("done");
        }
        Some("scan") => {
            let started = std::time::Instant::now();
            let found = scan(Duration::from_secs(secs(1, 3))).await?;
            println!("{} found in {:?}", found.len(), started.elapsed());
            for f in found {
                println!("{} {:<30} {:<8} {}", f.id, f.name, f.os, f.server);
            }
        }
        _ => eprintln!("usage: announce ID NAME [SECONDS] | scan [SECONDS]"),
    }
    Ok(())
}
