//! Loads (or creates) a device key and tells how it is kept, for live tests:
//! `cargo run -p nuvira-transport --example key-probe -- PATH [--hand-over TO | --import FROM]`.

use std::path::Path;

use nuvira_transport::identity::{self, Identity};
use nuvira_transport::keystore;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = Path::new(args.first().map(String::as_str).unwrap_or("device.key"));
    match args.get(1).map(String::as_str) {
        Some("--hand-over") => identity::hand_over(path, Path::new(&args[2]))?,
        Some("--import") => println!("imported: {}", identity::import(Path::new(&args[2]), path)?.name()),
        _ => {}
    }
    println!("preferred here: {}", keystore::preferred().name());
    println!("header before: {:?}", keystore::protection_of(path).map(|p| p.name()));
    let id = Identity::load_or_create(path)?;
    let hex: String = id.public_key().iter().map(|b| format!("{b:02x}")).collect();
    println!("protection: {}", id.protection().name());
    println!("public key: {hex}");
    Ok(())
}
