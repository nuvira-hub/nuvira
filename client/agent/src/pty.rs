//! A shell in a pseudo-terminal, for the remote terminal.
//!
//! - Windows: ConPTY (`CreatePseudoConsole`) with Windows PowerShell. As SYSTEM (the
//!   service's agent) the shell is started with the token of the user signed in to
//!   the session, with that user's environment and profile folder: the technician
//!   has the rights of that user, not of the system. Without a user signed in there
//!   is no terminal (`@agent.no_user`).
//! - Linux and macOS: `openpty` with the user's login shell (`$SHELL`, else /bin/sh),
//!   in a session of its own (it gets SIGHUP when the terminal closes).

use std::io::{Read, Write};

use anyhow::Result;

/// The shell running: write the keys with [`Shell::writer`], read the output
/// from the reader returned by [`spawn`] (it ends when the shell ends).
pub struct Shell {
    pub writer: Box<dyn Write + Send>,
    pub process: std::sync::Arc<Process>,
}

/// Starts the shell with the terminal of `cols` x `rows` characters.
pub fn spawn(cols: u16, rows: u16) -> Result<(Shell, Box<dyn Read + Send>)> {
    imp::spawn(cols.max(1), rows.max(1))
}

pub use imp::Process;

#[cfg(windows)]
mod imp {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
    };
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };
    use windows::Win32::UI::Shell::GetUserProfileDirectoryW;
    use windows::core::{PCWSTR, PWSTR};

    use super::Shell;

    /// A handle that can travel between threads (the shell's I/O and waiting).
    struct Owned(HANDLE);
    unsafe impl Send for Owned {}
    unsafe impl Sync for Owned {}
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    pub struct Process {
        handle: Owned,
        /// Closed when the shell ends: only then does the output pipe reach its end.
        console: Mutex<Option<HPCON>>,
    }
    unsafe impl Send for Process {}
    unsafe impl Sync for Process {}

    impl Process {
        pub fn resize(&self, cols: u16, rows: u16) {
            if let Some(console) = *self.console.lock().expect("console") {
                let size = COORD { X: cols.max(1) as i16, Y: rows.max(1) as i16 };
                let _ = unsafe { ResizePseudoConsole(console, size) };
            }
        }

        /// Waits for the end of the shell and returns its exit code.
        pub fn wait(&self) -> i32 {
            let mut code = 1u32;
            unsafe {
                if WaitForSingleObject(self.handle.0, INFINITE) == WAIT_OBJECT_0 {
                    let _ = GetExitCodeProcess(self.handle.0, &mut code);
                }
            }
            self.close_console();
            code as i32
        }

        pub fn kill(&self) {
            let _ = unsafe { TerminateProcess(self.handle.0, 1) };
            self.close_console();
        }

        fn close_console(&self) {
            if let Some(console) = self.console.lock().expect("console").take() {
                unsafe { ClosePseudoConsole(console) };
            }
        }
    }

    struct PipeReader(Owned);
    impl Read for PipeReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut read = 0u32;
            match unsafe { ReadFile(self.0.0, Some(buf), Some(&mut read), None) } {
                Ok(()) => Ok(read as usize),
                // The other end closed (the console was closed): the end of the output.
                Err(_) => Ok(0),
            }
        }
    }

    struct PipeWriter(Owned);
    impl Write for PipeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut written = 0u32;
            unsafe { WriteFile(self.0.0, Some(buf), Some(&mut written), None) }
                .map_err(std::io::Error::other)?;
            Ok(written as usize)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub fn spawn(cols: u16, rows: u16) -> Result<(Shell, Box<dyn Read + Send>)> {
        let token = crate::files::session_user_token()?.map(Owned);
        let (mut input_read, mut input_write) = (HANDLE::default(), HANDLE::default());
        let (mut output_read, mut output_write) = (HANDLE::default(), HANDLE::default());
        unsafe {
            CreatePipe(&mut input_read, &mut input_write, None, 0).context("input pipe")?;
            CreatePipe(&mut output_read, &mut output_write, None, 0).context("output pipe")?;
        }
        let (input_read, input_write) = (Owned(input_read), Owned(input_write));
        let (output_read, output_write) = (Owned(output_read), Owned(output_write));
        let size = COORD { X: cols as i16, Y: rows as i16 };
        let console = unsafe { CreatePseudoConsole(size, input_read.0, output_write.0, 0) }
            .context("pseudo console")?;
        // The console keeps its own copies.
        drop((input_read, output_write));

        let started = start_shell(console, token.as_ref().map(|t| t.0));
        let handle = match started {
            Ok(handle) => handle,
            Err(e) => {
                unsafe { ClosePseudoConsole(console) };
                return Err(e);
            }
        };
        let process = Arc::new(Process { handle, console: Mutex::new(Some(console)) });
        let shell = Shell { writer: Box::new(PipeWriter(input_write)), process };
        Ok((shell, Box::new(PipeReader(output_read))))
    }

    /// Windows PowerShell, which every Windows has, in the user's profile folder.
    fn start_shell(console: HPCON, token: Option<HANDLE>) -> Result<Owned> {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let mut command: Vec<u16> =
            format!(r#""{system_root}\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo"#)
                .encode_utf16()
                .chain(Some(0))
                .collect();

        let mut list_size = 0usize;
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut list_size) };
        let mut list_buffer = vec![0u8; list_size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(list_buffer.as_mut_ptr().cast());
        unsafe {
            InitializeProcThreadAttributeList(Some(list), 1, None, &mut list_size)
                .context("attribute list")?;
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(console.0 as *const std::ffi::c_void),
                size_of::<HPCON>(),
                None,
                None,
            )
            .context("pseudo console attribute")?;
        }
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        // Empty standard handles: otherwise, when the agent's own output goes to a file
        // (service, tests), the shell writes there instead of to the pseudo console.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.lpAttributeList = list;
        let mut info = PROCESS_INFORMATION::default();

        let result = match token {
            Some(token) => {
                let mut environment = std::ptr::null_mut();
                unsafe { CreateEnvironmentBlock(&mut environment, Some(token), false) }
                    .context("user environment")?;
                let mut profile = [0u16; 512];
                let mut len = profile.len() as u32;
                let dir = match unsafe {
                    GetUserProfileDirectoryW(token, Some(PWSTR(profile.as_mut_ptr())), &mut len)
                } {
                    Ok(()) => PCWSTR(profile.as_ptr()),
                    Err(_) => PCWSTR::null(),
                };
                let created = unsafe {
                    CreateProcessAsUserW(
                        Some(token),
                        PCWSTR::null(),
                        Some(PWSTR(command.as_mut_ptr())),
                        None,
                        None,
                        false,
                        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                        Some(environment),
                        dir,
                        &startup.StartupInfo,
                        &mut info,
                    )
                };
                let _ = unsafe { DestroyEnvironmentBlock(environment) };
                created
            }
            None => {
                let home: Vec<u16> = std::env::var_os("USERPROFILE")
                    .map(|h| {
                        use std::os::windows::ffi::OsStrExt;
                        h.encode_wide().chain(Some(0)).collect()
                    })
                    .unwrap_or_default();
                let dir = if home.is_empty() { PCWSTR::null() } else { PCWSTR(home.as_ptr()) };
                unsafe {
                    CreateProcessW(
                        PCWSTR::null(),
                        Some(PWSTR(command.as_mut_ptr())),
                        None,
                        None,
                        false,
                        EXTENDED_STARTUPINFO_PRESENT,
                        None,
                        dir,
                        &startup.StartupInfo,
                        &mut info,
                    )
                }
            }
        };
        unsafe { DeleteProcThreadAttributeList(list) };
        result.context("starting PowerShell")?;
        let _ = unsafe { CloseHandle(info.hThread) };
        Ok(Owned(info.hProcess))
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::Arc;

    use anyhow::{Context, Result};

    use super::Shell;

    pub struct Process {
        pid: libc::pid_t,
        master: OwnedFd,
    }

    impl Process {
        pub fn resize(&self, cols: u16, rows: u16) {
            let size = winsize(cols, rows);
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
        }

        /// Waits for the end of the shell and returns its exit code.
        pub fn wait(&self) -> i32 {
            let mut status = 0;
            loop {
                let r = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                if r == self.pid {
                    break;
                }
                if r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return 1;
                }
            }
            if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) }
        }

        /// The whole session of the shell (it is the leader): what it started too.
        pub fn kill(&self) {
            unsafe {
                libc::kill(-self.pid, libc::SIGHUP);
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
    }

    fn winsize(cols: u16, rows: u16) -> libc::winsize {
        libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
    }

    pub fn spawn(cols: u16, rows: u16) -> Result<(Shell, Box<dyn Read + Send>)> {
        let (mut master, mut slave) = (0, 0);
        let mut size = winsize(cols, rows);
        if unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut size)
        } != 0
        {
            return Err(std::io::Error::last_os_error()).context("openpty");
        }
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
        let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
        let mut command = Command::new(&shell);
        // A login shell, as on a new terminal: its profile is read.
        command
            .arg("-l")
            .current_dir(&home)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                // A session of its own with the terminal as the controlling one.
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().with_context(|| format!("starting {shell}"))?;
        // Dropping `command` closed our copies of the terminal side.
        drop(command);
        let reader = File::from(master.try_clone()?);
        let writer = File::from(master.try_clone()?);
        let process = Arc::new(Process { pid: child.id() as libc::pid_t, master });
        Ok((Shell { writer: Box::new(writer), process }, Box::new(EioIsEnd(reader))))
    }

    /// On Linux reading the master side gives EIO once the shell has ended.
    struct EioIsEnd(File);
    impl Read for EioIsEnd {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.read(buf) {
                Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(0),
                other => other,
            }
        }
    }
}
