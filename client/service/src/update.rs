//! Automatic updates of an installed NuviraHub (see `nuvira_update`).
//!
//! The service asks its server for a newer version 2 minutes after starting and
//! then every 6 hours. A version correctly signed with the NuviraHub release key is
//! downloaded into the data folder (writable only by SYSTEM and administrators,
//! so nobody can swap it before it runs) and, once no session is in progress,
//! started as `nuvira.exe service update`: that process stops this service,
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
/// The downloaded program, in the update folder, whatever its version.
const UPDATE_EXE: &str = "nuvira-update.exe";
/// Installations started for one version ("0.22.3 2"), in the data folder: the
/// service starts again on the old program when one did not go through.
const ATTEMPTS_FILE: &str = "update-attempts";
/// From the second attempt at the same version the app says something stops it.
const BLOCKED_AFTER: u32 = 2;

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
    let server = nuvira_transport::signal::server_or_public(&service.server);
    let base = nuvira_update::base_url(server, service.server_ws.as_deref(), nuvira_update::DEFAULT_PORT);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let Some(manifest) = rt.block_on(nuvira_update::check(&base, &nuvira_update::PUBLIC_KEYS))? else {
        // Up to date (also right after an update that went through).
        let _ = std::fs::remove_file(config::data_dir().join(ATTEMPTS_FILE));
        let _ = std::fs::remove_file(config::data_dir().join(nuvira_update::BLOCKED_FILE));
        return Ok(());
    };
    info!("version {} available (this is {})", manifest.version, nuvira_update::CURRENT);

    let dir = config::update_dir();
    std::fs::create_dir_all(&dir)?;
    // Always the same name: an antivirus that trusts programs by path (Kaspersky
    // Anti-Ransomware Tool takes single files only) can be told once for every version.
    let program = dir.join(UPDATE_EXE);
    clean(&dir, &program);
    if !nuvira_update::file_matches(&program, &manifest) {
        let _ = std::fs::remove_file(&program);
        rt.block_on(nuvira_update::download(&base, &manifest, &program))?;
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

    let attempt = count_attempt(&config::data_dir().join(ATTEMPTS_FILE), &manifest.version);
    info!("installing version {} (attempt {attempt})", manifest.version);
    if attempt >= BLOCKED_AFTER {
        warn!("version {} did not install before: something on this PC may stop it", manifest.version);
        let _ = std::fs::write(config::data_dir().join(nuvira_update::BLOCKED_FILE), &manifest.version);
    }
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

/// Counts one more installation of `version` in `path` and returns how many there were.
fn count_attempt(path: &Path, version: &str) -> u32 {
    let before = std::fs::read_to_string(path).unwrap_or_default();
    let count = match before.split_once(' ') {
        Some((v, n)) if v == version => n.trim().parse::<u32>().unwrap_or(0) + 1,
        _ => 1,
    };
    let _ = std::fs::write(path, format!("{version} {count}"));
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    // The second installation of the same version says the first did not go
    // through; a new version starts again from one.
    #[test]
    fn attempts_per_version() {
        let path = std::env::temp_dir().join(format!("nuvira-attempts-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(count_attempt(&path, "0.22.3"), 1);
        assert_eq!(count_attempt(&path, "0.22.3"), BLOCKED_AFTER);
        assert_eq!(count_attempt(&path, "0.22.3"), 3);
        assert_eq!(count_attempt(&path, "0.22.4"), 1);
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(count_attempt(&path, "0.22.4"), 1);
        let _ = std::fs::remove_file(&path);
    }
}

/// Removes older downloads (and the one that installed this version).
fn clean(dir: &Path, keep: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.path() != keep {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
