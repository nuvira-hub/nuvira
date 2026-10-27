//! nexodesk-release — release key and signed updates.
//!
//! ```text
//! nexodesk-release keygen                      creates the release key (once)
//! nexodesk-release sign target\release\nexodesk.exe --out dist\update
//! ```
//!
//! `sign` writes `<out>/windows-x64.json` and `<out>/windows-x64/nexodesk-<version>.exe`:
//! the contents of the `updates` folder in the server's data.
//!
//! The private key stays on the machine that builds the releases, outside the
//! repository: whoever has it can get any program installed on every NexoDesk
//! device, and without it no further update can be published (devices would need
//! a new program with the new public key, installed by hand). Back it up offline.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nexodesk_update::{Manifest, hex, public_key_of};
use ring::signature::Ed25519KeyPair;

#[derive(Parser)]
#[command(version, about = "NexoDesk release key and signed updates")]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Creates the release key and prints the public key to build into the program.
    Keygen {
        #[arg(long, value_name = "FILE")]
        key: Option<PathBuf>,
    },
    /// Signs a program and writes the files to publish on the server.
    Sign {
        /// The program to publish (nexodesk.exe).
        program: PathBuf,
        /// Version of the program; default: this tool's (same workspace).
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value = "windows-x64")]
        platform: String,
        #[arg(long, value_name = "FILE")]
        key: Option<PathBuf>,
        /// Output folder (the server's `updates`).
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
    },
}

/// Default key file: in the user's profile, never in the repository.
fn default_key() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".nexodesk")
        .join("release-signing.pk8")
}

fn main() -> Result<()> {
    match Args::parse().command {
        Cmd::Keygen { key } => {
            let path = key.unwrap_or_else(default_key);
            if path.exists() {
                bail!("{} already exists: a new key would make the published one useless", path.display());
            }
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .map_err(|_| anyhow::anyhow!("key generation failed"))?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, pkcs8.as_ref()).with_context(|| format!("writing {}", path.display()))?;
            let public = public_key_of(pkcs8.as_ref())?;
            println!("release key written to {} (back it up offline)", path.display());
            println!("public key: {}", hex(&public));
            println!("PUBLIC_KEY for client/update/src/lib.rs:");
            for row in public.chunks(16) {
                println!("    {},", row.iter().map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(", "));
            }
        }
        Cmd::Sign { program, version, platform, key, out } => {
            let path = key.unwrap_or_else(default_key);
            let pkcs8 = std::fs::read(&path)
                .with_context(|| format!("reading the release key {}", path.display()))?;
            let key = Ed25519KeyPair::from_pkcs8(&pkcs8)
                .map_err(|_| anyhow::anyhow!("not an Ed25519 PKCS#8 key"))?;
            if public_key_of(&pkcs8)? != nexodesk_update::PUBLIC_KEY {
                bail!(
                    "this key does not match the public key built into the program (client/update/src/lib.rs)"
                );
            }
            let version = version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
            let data = std::fs::read(&program).with_context(|| format!("reading {}", program.display()))?;
            let manifest = Manifest::sign(&key, &version, &platform, &data);
            let target = out.join(manifest.program_path());
            std::fs::create_dir_all(target.parent().context("output folder")?)?;
            std::fs::write(&target, &data)?;
            let manifest_path = out.join(format!("{platform}.json"));
            std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
            println!("{} {version} ({} bytes)", platform, data.len());
            println!("  {}", manifest_path.display());
            println!("  {}", target.display());
        }
    }
    Ok(())
}
