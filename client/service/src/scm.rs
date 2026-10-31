//! Running as a service: dialogue with the service manager and the loop that keeps
//! an agent running in the console session.
//!
//! The loop checks the active session every half second: at every change
//! (disconnection, user switch) the agent is closed and restarted in the new one.
//! If the agent ends on its own it is restarted, waiting longer and longer if it
//! keeps failing right away (up to 30 s).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{error, info, warn};
use windows::Win32::Foundation::{CloseHandle, ERROR_CALL_NOT_IMPLEMENTED, HANDLE, NO_ERROR, WAIT_OBJECT_0};
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HSTRING, PWSTR};

use crate::config::{self, ServiceConfig};
use crate::launch::{AgentProcess, Job};

/// Exit code of a process closed by the system at the end of its session.
const DBG_TERMINATE_PROCESS: u32 = 0x4001_0004;

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Hands the thread over to the service manager, which calls [`service_main`]. Returns when the service stops.
pub fn dispatch() -> Result<()> {
    let mut name: Vec<u16> = config::SERVICE_NAME.encode_utf16().chain(Some(0)).collect();
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
        .context("StartServiceCtrlDispatcher: `run` is reserved for the service manager, use `install`")
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(&HSTRING::from(config::SERVICE_NAME), Some(handler), None)
    } {
        Ok(h) => h,
        Err(e) => {
            error!("RegisterServiceCtrlHandlerEx: {e}");
            return;
        }
    };
    STATUS.store(handle.0, Ordering::Relaxed);
    set_status(SERVICE_START_PENDING, 0);
    set_status(SERVICE_RUNNING, 0);
    info!("service started");
    // Running: a rescue task left by an update is no longer needed.
    crate::install::remove_rescue();
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            error!("service ended with an error: {e:#}");
            1
        }
    };
    info!("service stopped");
    set_status(SERVICE_STOPPED, code);
}

unsafe extern "system" fn handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::Relaxed);
            set_status(SERVICE_STOP_PENDING, 0);
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

fn set_status(state: SERVICE_STATUS_CURRENT_STATE, exit_code: u32) {
    let pending = state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if pending { 10_000 } else { 0 },
    };
    let handle = SERVICE_STATUS_HANDLE(STATUS.load(Ordering::Relaxed));
    if let Err(e) = unsafe { SetServiceStatus(handle, &status) } {
        warn!("SetServiceStatus: {e}");
    }
}

/// Event the agent uses to ask for Ctrl+Alt+Del (same name in nuvira-input).
const SAS_EVENT: &str = r"Global\nuvira-sas";

/// Thread that performs Ctrl+Alt+Del when the agent signals [`SAS_EVENT`]. Windows
/// accepts `SendSAS` only from a service, not from the agent even though it is SYSTEM.
/// The event created by SYSTEM, with the default security, can be signalled only by SYSTEM.
fn sas_listener() -> Option<std::thread::JoinHandle<()>> {
    let event = match unsafe { CreateEventW(None, false, false, &HSTRING::from(SAS_EVENT)) } {
        Ok(event) => event,
        Err(e) => {
            warn!("Ctrl+Alt+Del not available: CreateEvent: {e}");
            return None;
        }
    };
    // HANDLE is not Send: it goes to the thread as a number.
    let raw = event.0 as usize;
    let thread = std::thread::Builder::new().name("ctrl-alt-canc".into()).spawn(move || {
        let event = HANDLE(raw as *mut c_void);
        while !STOP.load(Ordering::Relaxed) {
            if unsafe { WaitForSingleObject(event, 500) } == WAIT_OBJECT_0 {
                info!("Ctrl+Alt+Del requested by the agent");
                unsafe { SendSAS(false) };
            }
        }
        let _ = unsafe { CloseHandle(event) };
    });
    thread.ok()
}

fn agent_args(config: &ServiceConfig) -> String {
    let mut args = format!(
        "--ipc --server \"{}\" --transport {} --identity \"{}\" --fingerprint-file \"{}\" --busy-file \"{}\"",
        config.server,
        config.transport,
        config::identity_path().display(),
        config::online_path().display(),
        config::busy_path().display()
    );
    if let Some(key) = &config.server_key {
        args += &format!(" --server-key {key}");
    }
    if let Some(url) = &config.server_ws {
        args += &format!(" --server-ws \"{url}\"");
    }
    args
}

fn run() -> Result<()> {
    let config = ServiceConfig::load()?;
    let args = format!("agent {}", agent_args(&config));
    let exe = std::env::current_exe()?;
    let data = config::data_dir();
    let job = Job::new()?;
    let _sas = sas_listener();
    let _updates = crate::update::spawn(&STOP);

    let mut agent: Option<AgentProcess> = None;
    let mut backoff = Duration::from_secs(1);
    let mut next_start = Instant::now();
    while !STOP.load(Ordering::Relaxed) {
        let session = crate::launch::active_session();
        if let Some(running) = &agent {
            if Some(running.session) != session {
                info!("console session changed ({} → {session:?}): restarting the agent", running.session);
                running.kill();
                agent = None;
                next_start = Instant::now();
                continue;
            }
            if let Some(code) = running.wait(Duration::from_millis(500)) {
                if code == DBG_TERMINATE_PROCESS {
                    // Closed by Windows: end of the session or shutdown, which closes the sessions
                    // before stopping the services. It is not a crash: if it is a shutdown the stop
                    // comes shortly, otherwise it starts again in the new session.
                    info!("agent closed by Windows (end of session or shutdown)");
                    agent = None;
                    next_start = Instant::now() + Duration::from_secs(3);
                    continue;
                }
                let lived = running.started.elapsed();
                // If it ran for a long time it restarts right away, otherwise it waits longer.
                backoff = if lived > Duration::from_secs(60) {
                    Duration::from_secs(1)
                } else {
                    (backoff * 2).min(Duration::from_secs(30))
                };
                warn!(
                    "agent ended with code {code} after {} s: restarting in {} s",
                    lived.as_secs(),
                    backoff.as_secs()
                );
                agent = None;
                next_start = Instant::now() + backoff;
            }
            continue;
        }
        match session {
            Some(session) if Instant::now() >= next_start => {
                config::rotate_log(&config::agent_log(), 10 << 20);
                match AgentProcess::spawn(&job, session, &exe, &args, &config::agent_log(), &data) {
                    Ok(started) => {
                        info!("agent started in session {session}");
                        agent = Some(started);
                    }
                    Err(e) => {
                        warn!("starting the agent failed: {e:#}; retrying in 5 s");
                        next_start = Instant::now() + Duration::from_secs(5);
                    }
                }
            }
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    if let Some(running) = agent {
        running.kill();
    }
    Ok(())
}
