//! Updates of the app itself when the service does not do it: the checks with the
//! server at start-up and every 6 hours, the download and replacement of this
//! program, and the restart, where the new copy waits for the old one to end.

use std::path::PathBuf;
use std::time::Duration;

use nuvira_i18n::{t, tf};
use tauri::{AppHandle, Manager};
use tracing::info;

use crate::{NuviraApp, load_settings};

/// Set by a start of this program that replaces the running one (restart, update):
/// the new one waits for it to end, or it would hand over to it and close (one app
/// at a time).
const WAIT_PID: &str = "NUVIRA_WAIT_PID";

pub(crate) fn wait_for_predecessor() {
    let Some(pid) = std::env::var(WAIT_PID).ok().and_then(|p| p.parse::<u32>().ok()) else { return };
    // SAFETY: at the start of main, before other threads read the environment.
    unsafe { std::env::remove_var(WAIT_PID) };
    let started = std::time::Instant::now();
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
        if let Ok(process) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            unsafe {
                WaitForSingleObject(process, 15_000);
                let _ = CloseHandle(process);
            }
        }
    }
    #[cfg(unix)]
    while started.elapsed() < Duration::from_secs(15) && unsafe { libc::kill(pid as i32, 0) } == 0 {
        std::thread::sleep(Duration::from_millis(100));
    }
    info!(pid, waited_ms = started.elapsed().as_millis() as u64, "previous app ended");
}

/// The app's own copy before the last update (see `apply_update`).
#[cfg(not(unix))]
fn previous_copy() -> Option<PathBuf> {
    std::env::current_exe().ok().map(|exe| exe.with_extension("old.exe"))
}

/// This program's path at start-up: on Linux `current_exe` follows the file, so
/// after an update it would name the old copy put aside.
pub(crate) fn started_as() -> PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| std::env::current_exe().unwrap_or_default()).clone()
}

/// How this copy is updated by the app (Linux and macOS): `None` when the user
/// service or root's timer does it, or when this user cannot replace it.
#[cfg(unix)]
fn app_updates() -> Option<nuvira_update::unix::Install> {
    let installed = crate::service_installed();
    let install = nuvira_update::unix::this_install(&started_as());
    (!installed && install.channel().is_some()).then_some(install)
}

/// Without the service (which updates an installed NuviraHub by itself) the app asks
/// its server for a newer version at start-up and every 6 hours, and offers it.
pub(crate) fn start_update_checks(app: &AppHandle) {
    if nuvira_update::platform() == "unsupported" {
        return;
    }
    started_as();
    #[cfg(not(unix))]
    if let Some(old) = previous_copy() {
        let _ = std::fs::remove_file(old);
    }
    #[cfg(unix)]
    if let Some(install) = app_updates() {
        nuvira_update::unix::clean(&install);
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            #[cfg(windows)]
            let installed = nuvira_agent::ipc::service_installed();
            #[cfg(unix)]
            let installed = app_updates().is_none();
            #[cfg(not(any(windows, unix)))]
            let installed = false;
            let settings = load_settings();
            if !installed {
                let server = nuvira_transport::signal::server_or_public(&settings.server);
                let base = nuvira_update::update_base(server, None);
                match nuvira_update::check_offer(
                    &base,
                    &nuvira_update::PUBLIC_KEYS,
                    nuvira_update::platform(),
                )
                .await
                {
                    Ok((found, stale)) => {
                        let version = found.map(|m| m.version);
                        app.state::<NuviraApp>().update(&app, |ui| {
                            ui.update = version;
                            ui.update_stale = stale;
                        });
                    }
                    Err(e) => tracing::debug!("update check: {e:#}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}

/// Downloads the newer version, checks it (signature, size, SHA-256) and replaces
/// this program, keeping its file name (which may carry the server), then starts it.
/// A running program cannot be overwritten but can be renamed: the old copy is
/// moved aside and deleted at the next start.
#[tauri::command]
pub(crate) async fn apply_update(app: AppHandle) -> Result<(), String> {
    let settings = load_settings();
    let server = nuvira_transport::signal::server_or_public(&settings.server);
    let base = nuvira_update::update_base(server, None);
    let error = |e: anyhow::Error| tf("update.failed", &[("e", &format!("{e:#}"))]);
    let manifest = nuvira_update::check(&base, &nuvira_update::PUBLIC_KEYS)
        .await
        .map_err(error)?
        .ok_or_else(|| t("update.none"))?;
    // Linux and macOS: in this user's data folder, not the shared /tmp of Linux,
    // where another user can take the names first (APP-9).
    #[cfg(unix)]
    let folder = nuvira_transport::identity::data_dir().join("update");
    #[cfg(not(unix))]
    let folder = std::env::temp_dir();
    std::fs::create_dir_all(&folder).map_err(|e| error(e.into()))?;
    let download =
        folder.join(format!("nuvira-{}{}", manifest.version, nuvira_update::extension(&manifest.platform)));
    nuvira_update::download(&base, &manifest, &download).await.map_err(error)?;

    // Linux: the program; macOS: the whole app.
    #[cfg(unix)]
    {
        let install = app_updates().ok_or_else(|| t("update.none"))?;
        let applied = tauri::async_runtime::spawn_blocking({
            let download = download.clone();
            move || nuvira_update::unix::apply(&install, &download)
        })
        .await
        .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&download);
        applied.map_err(error)?;
        info!("updated to {}: restarting", manifest.version);
        std::process::Command::new(started_as())
            .env(WAIT_PID, std::process::id().to_string())
            .spawn()
            .map_err(|e| error(e.into()))?;
        app.exit(0);
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let aside = previous_copy().ok_or("program path")?;
        let _ = std::fs::remove_file(&aside);
        std::fs::rename(&exe, &aside).map_err(|e| error(e.into()))?;
        if let Err(e) = std::fs::copy(&download, &exe) {
            let _ = std::fs::rename(&aside, &exe);
            return Err(error(e.into()));
        }
        let _ = std::fs::remove_file(&download);
        info!("updated to {}: restarting", manifest.version);
        std::process::Command::new(&exe)
            .env(WAIT_PID, std::process::id().to_string())
            .spawn()
            .map_err(|e| error(e.into()))?;
        app.exit(0);
        Ok(())
    }
}

/// Starts the program installed by the service (updated while this app was open)
/// in place of this one.
#[tauri::command]
pub(crate) fn restart_app(app: AppHandle) -> Result<(), String> {
    #[cfg(windows)]
    let program = nuvira_service::installed_program();
    #[cfg(not(windows))]
    let program = started_as();
    info!("restarting with the installed program {}", program.display());
    std::process::Command::new(&program)
        .env(WAIT_PID, std::process::id().to_string())
        .spawn()
        .map_err(|e| tf("update.failed", &[("e", &e.to_string())]))?;
    app.exit(0);
    Ok(())
}
