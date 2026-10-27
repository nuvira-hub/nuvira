//! Avvio dell'agent nella sessione di chi usa il PC.
//!
//! Il servizio gira nella sessione 0, che non ha schermo. L'agent viene avviato
//! con una copia del token SYSTEM del servizio spostata nella sessione della
//! console (`winsta0\default`): da SYSTEM può passare sul desktop protetto (UAC,
//! schermata di blocco) e iniettare input anche nelle finestre da amministratore.
//! L'agent sta in un job object: se il servizio termina, anche all'improvviso,
//! Windows chiude l'agent invece di lasciarne una copia orfana.

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

/// Sessione collegata a schermo, tastiera e mouse fisici; `None` durante un cambio utente.
pub fn active_session() -> Option<u32> {
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

/// Handle chiuso automaticamente.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Job object che chiude gli agent quando il servizio termina.
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
    /// Avvia `exe` con `args` nella sessione indicata. Output e errori vanno in `log`.
    pub fn spawn(job: &Job, session: u32, exe: &Path, args: &str, log: &Path, cwd: &Path) -> Result<Self> {
        let token = system_token_for(session)?;

        // L'agent eredita il file di log come stdout e stderr.
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .with_context(|| format!("apertura di {}", log.display()))?;
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
        .with_context(|| format!("avvio di {}", exe.display()))?;
        let process = Owned(info.hProcess);
        let thread = Owned(info.hThread);
        // Nel job prima di partire: così non può sfuggirgli nemmeno per un istante.
        if let Err(e) = unsafe { AssignProcessToJobObject(job.0.0, process.0) } {
            let _ = unsafe { TerminateProcess(process.0, 1) };
            return Err(e).context("AssignProcessToJobObject");
        }
        unsafe { ResumeThread(thread.0) };
        Ok(Self { process, session, started: Instant::now() })
    }

    /// Attende fino a `timeout` che l'agent termini; restituisce il codice di uscita.
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

/// Copia primaria del token del servizio (SYSTEM), spostata nella sessione indicata.
/// Cambiare sessione a un token richiede SeTcbPrivilege, che SYSTEM ha.
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
    .context("SetTokenInformation(TokenSessionId): il servizio deve girare come SYSTEM")?;
    Ok(primary)
}
