//! Starting the agent in the session of the person using the PC.
//!
//! The service runs in session 0, which has no screen. The agent is started with
//! a copy of the service's SYSTEM token moved into the console session
//! (`winsta0\default`): as SYSTEM it can switch to the secure desktop (UAC, lock
//! screen) and inject input into administrator windows too.
//! The agent is in a job object: if the service ends, even abruptly, Windows
//! closes the agent instead of leaving an orphan copy behind.

use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
    OpenProcessToken, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};
use windows::core::{HSTRING, PWSTR};

/// Session connected to the physical screen, keyboard and mouse; `None` during a user switch.
pub fn active_session() -> Option<u32> {
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

/// Handle closed automatically.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Job object that closes the agents when the service ends.
pub struct Job(Owned);

impl Job {
    pub fn new() -> Result<Self> {
        let job = Owned(unsafe { CreateJobObjectW(None, None) }.context("CreateJobObject")?);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of_val(&info) as u32,
            )
        }
        .context("SetInformationJobObject")?;
        Ok(Self(job))
    }
}

pub struct AgentProcess {
    process: Owned,
    pub session: u32,
    pub started: Instant,
}

impl AgentProcess {
    /// Starts `exe` with `args` in the given session. Output and errors go to `log`.
    pub fn spawn(job: &Job, session: u32, exe: &Path, args: &str, log: &Path, cwd: &Path) -> Result<Self> {
        let token = system_token_for(session)?;

        // The agent inherits the log file as stdout and stderr.
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .with_context(|| format!("opening {}", log.display()))?;
        let log_handle = HANDLE(log_file.as_raw_handle());
        unsafe { SetHandleInformation(log_handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .context("SetHandleInformation")?;

        let mut command: Vec<u16> =
            format!("\"{}\" {args}", exe.display()).encode_utf16().chain(Some(0)).collect();
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain(Some(0)).collect();
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdOutput: log_handle,
            hStdError: log_handle,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessAsUserW(
                Some(token.0),
                &HSTRING::from(exe.as_os_str()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_NO_WINDOW | CREATE_SUSPENDED,
                None,
                &HSTRING::from(cwd.as_os_str()),
                &startup,
                &mut info,
            )
        }
        .with_context(|| format!("starting {}", exe.display()))?;
        let process = Owned(info.hProcess);
        let thread = Owned(info.hThread);
        // Into the job before starting: that way it cannot escape it even for an instant.
        if let Err(e) = unsafe { AssignProcessToJobObject(job.0.0, process.0) } {
            let _ = unsafe { TerminateProcess(process.0, 1) };
            return Err(e).context("AssignProcessToJobObject");
        }
        unsafe { ResumeThread(thread.0) };
        Ok(Self { process, session, started: Instant::now() })
    }

    /// Waits up to `timeout` for the agent to end; returns its exit code.
    pub fn wait(&self, timeout: Duration) -> Option<u32> {
        if unsafe { WaitForSingleObject(self.process.0, timeout.as_millis() as u32) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0;
        let _ = unsafe { GetExitCodeProcess(self.process.0, &mut code) };
        Some(code)
    }

    pub fn kill(&self) {
        let _ = unsafe { TerminateProcess(self.process.0, 1) };
        self.wait(Duration::from_secs(5));
    }
}

/// Primary copy of the service's token (SYSTEM), moved into the given session.
/// Changing a token's session needs SeTcbPrivilege, which SYSTEM has.
fn system_token_for(session: u32) -> Result<Owned> {
    let mut own = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) }.context("OpenProcessToken")?;
    let own = Owned(own);
    let mut primary = HANDLE::default();
    unsafe { DuplicateTokenEx(own.0, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut primary) }
        .context("DuplicateTokenEx")?;
    let primary = Owned(primary);
    unsafe {
        SetTokenInformation(primary.0, TokenSessionId, (&raw const session).cast(), size_of::<u32>() as u32)
    }
    .context("SetTokenInformation(TokenSessionId): the service must run as SYSTEM")?;
    Ok(primary)
}
