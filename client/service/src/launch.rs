//! Starting the agent in the session of the person using the PC.
//!
//! The service runs in session 0, which has no screen. The agent is started with
//! a copy of the service's SYSTEM token moved into the session of the person using
//! the PC (`winsta0\default`): as SYSTEM it can switch to the secure desktop (UAC,
//! lock screen) and inject input into administrator windows too.
//!
//! That session is the console's when someone is signed in there; otherwise a
//! Remote Desktop session in use. When a user connects to the PC with Remote
//! Desktop their session leaves the console, which is left on the sign-in screen:
//! following only the console, NuviraHub showed that screen instead of their desktop.
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
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSClientProtocolType, WTSEnumerateSessionsW,
    WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSUserName,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
    OpenProcessToken, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};
use windows::core::{HSTRING, PWSTR};

/// Session to show: see the module documentation. `None` during a user switch.
pub fn active_session() -> Option<u32> {
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    let console = (console != u32::MAX).then_some(console);
    choose_session(console, console.is_some_and(has_user), &remote_sessions())
}

/// The console if someone is signed in there, otherwise the first Remote Desktop
/// session in use, otherwise the console (its sign-in screen).
fn choose_session(console: Option<u32>, console_has_user: bool, remote: &[u32]) -> Option<u32> {
    if console_has_user {
        return console;
    }
    remote.iter().copied().find(|&id| Some(id) != console).or(console)
}

/// Remote Desktop sessions in use with a user signed in.
fn remote_sessions() -> Vec<u32> {
    let mut list: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    if unsafe { WTSEnumerateSessionsW(Some(WTS_CURRENT_SERVER_HANDLE), 0, 1, &mut list, &mut count) }.is_err()
    {
        return Vec::new();
    }
    // SAFETY: WTSEnumerateSessionsW returned `count` entries, freed below.
    let sessions = unsafe { std::slice::from_raw_parts(list, count as usize) };
    let found = sessions
        .iter()
        .filter(|s| s.State == WTSActive && is_remote(s.SessionId) && has_user(s.SessionId))
        .map(|s| s.SessionId)
        .collect();
    unsafe { WTSFreeMemory(list.cast()) };
    found
}

/// Bytes of a session property (freed here).
fn session_info(id: u32, class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS) -> Option<Vec<u8>> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0u32;
    unsafe {
        WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), id, class, &mut buffer, &mut bytes)
    }
    .ok()?;
    // SAFETY: `bytes` readable bytes until WTSFreeMemory.
    let data = unsafe { std::slice::from_raw_parts(buffer.0 as *const u8, bytes as usize) }.to_vec();
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    Some(data)
}

fn has_user(id: u32) -> bool {
    // A UTF-16 string: more than its terminating zero means a name.
    session_info(id, WTSUserName).is_some_and(|name| name.len() > 2)
}

fn is_remote(id: u32) -> bool {
    // WTSClientProtocolType: 0 console, 2 RDP.
    session_info(id, WTSClientProtocolType).is_some_and(|p| p.first() == Some(&2))
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
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) }
        .context("OpenProcessToken")?;
    let own = Owned(own);
    let mut primary = HANDLE::default();
    unsafe {
        DuplicateTokenEx(own.0, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut primary)
    }
    .context("DuplicateTokenEx")?;
    let primary = Owned(primary);
    unsafe {
        SetTokenInformation(primary.0, TokenSessionId, (&raw const session).cast(), size_of::<u32>() as u32)
    }
    .context("SetTokenInformation(TokenSessionId): the service must run as SYSTEM")?;
    Ok(primary)
}

#[cfg(test)]
mod tests {
    use super::choose_session;

    #[test]
    fn follows_the_person_using_the_pc() {
        // Someone at the console: the console, even with Remote Desktop sessions around.
        assert_eq!(choose_session(Some(1), true, &[3]), Some(1));
        // Connected with Remote Desktop: the console shows only the sign-in screen.
        assert_eq!(choose_session(Some(2), false, &[1]), Some(1));
        // Nobody anywhere: the console's sign-in screen.
        assert_eq!(choose_session(Some(2), false, &[]), Some(2));
        // During a user switch there is no console session.
        assert_eq!(choose_session(None, false, &[]), None);
        assert_eq!(choose_session(None, false, &[4]), Some(4));
    }
}
