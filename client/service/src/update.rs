//! Automatic updates of an installed NexoDesk (see `nexodesk_update`).
//!
//! The service asks its server for a newer version 2 minutes after starting and
//! then every 6 hours. A version correctly signed with the NexoDesk release key is
//! downloaded into the data folder (writable only by SYSTEM and administrators,
//! so nobody can swap it before it runs) and, once no session is in progress,
//! started as `nexodesk.exe service update`: that process stops this service,
//! replaces the installed program and starts the service again, keeping server,
//! ID and passwords.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::{self, ServiceConfig};

const FIRST_CHECK: Duration = Duration::from_secs(120);
const INTERVAL: Duration = Duration::from_secs(6 * 3600);
const RETRY: Duration = Duration::from_secs(3600);

/// Checks for updates until `stop` is set.
pub fn spawn(stop: &'static AtomicBool) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("updates".into())
        .spawn(move || {
            let mut next = Instant::now() + FIRST_CHECK;
            while !stop.load(Ordering::Relaxed) {
                if Instant::now() < next {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                next = Instant::now()
                    + match check_and_apply(stop) {
                        Ok(()) => INTERVAL,
                        Err(e) => {
                            warn!("update check failed: {e:#}");
                            RETRY
                        }
                    };
            }
        })
        .ok()
}

fn check_and_apply(stop: &AtomicBool) -> Result<()> {
    let service = ServiceConfig::load()?;
    let base = nexodesk_update::base_url(&service.server, service.server_ws.as_deref(), nexodesk_update::DEFAULT_PORT);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let Some(manifest) = rt.block_on(nexodesk_update::check(&base, &nexodesk_update::PUBLIC_KEY))? else {
        return Ok(());
    };
    info!("version {} available (this is {})", manifest.version, nexodesk_update::CURRENT);

    let dir = config::update_dir();
    std::fs::create_dir_all(&dir)?;
    let program = dir.join(format!("nexodesk-{}.exe", manifest.version));
    clean(&dir, &program);
    if !program.exists() {
        rt.block_on(nexodesk_update::download(&base, &manifest, &program))?;
        info!("version {} downloaded and checked", manifest.version);
    }

    // Not in the middle of someone's work: wait for the session to end.
    let busy = config::busy_path();
    if busy.exists() {
        info!("session in progress: the update waits for it to end");
        while busy.exists() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    info!("installing version {}", manifest.version);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(config::update_log())?;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    // It outlives this service, which it stops.
    Command::new(&program)
        .args(["service", "update"])
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    Ok(())
}

/// Removes older downloads (and the one that installed this version).
fn clean(dir: &Path, keep: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.path() != keep {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
