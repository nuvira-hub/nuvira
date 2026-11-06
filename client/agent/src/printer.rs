//! Remote printing (docs/29-remote-printing.md): what is printed on the computer's
//! "NuviraHub" printer during a session reaches the viewer as PDF, to be printed on
//! the technician's printer.
//!
//! The printer (installed with the service, `nuvira_service::printer`) uses Windows'
//! "Microsoft Print To PDF" driver on a local port that is the named pipe [`PIPE`]:
//! the spooler opens it for every job and writes the PDF into it. The agent of a
//! session whose viewer prints listens on it; with no session the spooler finds no
//! pipe, marks the job in error and tries again every few seconds, so a job printed
//! before the session would arrive as soon as it starts: those are dropped.

#[cfg(windows)]
pub use windows_impl::listen;

/// Not on Linux and macOS yet.
#[cfg(not(windows))]
pub struct Listener;

#[cfg(not(windows))]
pub fn listen() -> Option<Listener> {
    None
}

#[cfg(not(windows))]
impl Listener {
    pub async fn serve(self, _conn: quinn::Connection, _allowed: impl Fn() -> bool) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

#[cfg(windows)]
mod windows_impl {
    use anyhow::{Context, Result};
    use tokio::io::AsyncReadExt;
    use tokio::net::windows::named_pipe::NamedPipeServer;
    use tracing::{info, warn};
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::Graphics::Printing::{
        ClosePrinter, EnumJobsW, JOB_INFO_1W, OpenPrinterW, PRINTER_ACCESS_USE, PRINTER_DEFAULTSW,
        PRINTER_HANDLE,
    };
    use windows::Win32::System::SystemInformation::GetSystemTime;
    use windows::core::HSTRING;

    /// Same names as `nuvira_service::printer`.
    const PRINTER: &str = "NuviraHub";
    const PIPE: &str = r"\\.\pipe\nuvira-print";
    /// Larger documents are not sent.
    const MAX_DOCUMENT: u64 = 512 << 20;
    /// The spooler (SYSTEM, or the user who printed when it impersonates them) writes;
    /// any signed-in user may print.
    const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";
    /// `JOB_STATUS_PRINTING`: the job the spooler is writing into the pipe.
    const PRINTING: u32 = 0x10;

    /// The pipe of this session: `None` without the printer, or when another agent
    /// (another Windows session) already listens on it.
    pub fn listen() -> Option<Listener> {
        Printer::open()?;
        match crate::ipc::create_pipe_with(PIPE, true, SDDL) {
            Ok(pipe) => Some(Listener { pipe, since: utc_now() }),
            Err(e) => {
                warn!("remote printing not available: {e:#}");
                None
            }
        }
    }

    pub struct Listener {
        pipe: NamedPipeServer,
        /// Jobs printed before this moment are not sent.
        since: SYSTEMTIME,
    }

    impl Listener {
        /// Every job printed during the session goes to the viewer on a stream of its
        /// own, while `allowed` (the person at the PC can stop it); it never ends by
        /// itself (the end of the session drops the pipe).
        pub async fn serve(self, conn: quinn::Connection, allowed: impl Fn() -> bool) -> Result<()> {
            let Listener { mut pipe, since } = self;
            loop {
                pipe.connect().await.context("printer pipe")?;
                // An instance ready for the next job while this one is read.
                let next = crate::ipc::create_pipe_with(PIPE, false, SDDL)?;
                let job = tokio::task::spawn_blocking(current_job).await?;
                let mut data = Vec::new();
                let read = (&mut pipe).take(MAX_DOCUMENT + 1).read_to_end(&mut data).await;
                drop(std::mem::replace(&mut pipe, next));
                read.context("reading the printed document")?;
                let (name, submitted) = job.unwrap_or_else(|| ("document".into(), since));
                if key(&submitted) < key(&since) {
                    info!(name, "document printed before the session: not sent");
                    continue;
                }
                if !allowed() {
                    info!(name, "document printed, but remote printing is no longer allowed: not sent");
                    continue;
                }
                if data.len() as u64 > MAX_DOCUMENT {
                    warn!(name, "printed document larger than {MAX_DOCUMENT} bytes: not sent");
                    continue;
                }
                if data.is_empty() {
                    continue;
                }
                info!(name, bytes = data.len(), "document printed: sending it to the viewer");
                let header = nuvira_proto::v1::FileChunk { name, print: true, ..Default::default() };
                let conn = conn.clone();
                tokio::spawn(async move {
                    if let Err(e) = nuvira_transport::send_stream(&conn, header, &data).await {
                        warn!("printed document not sent: {e:#}");
                    }
                });
            }
        }
    }

    struct Printer(PRINTER_HANDLE);

    impl Printer {
        fn open() -> Option<Self> {
            let defaults = PRINTER_DEFAULTSW { DesiredAccess: PRINTER_ACCESS_USE, ..Default::default() };
            let mut handle = PRINTER_HANDLE::default();
            unsafe { OpenPrinterW(&HSTRING::from(PRINTER), &mut handle, Some(&defaults)) }.ok()?;
            Some(Self(handle))
        }
    }

    impl Drop for Printer {
        fn drop(&mut self) {
            unsafe {
                let _ = ClosePrinter(self.0);
            }
        }
    }

    /// Title and submission time (UTC) of the job being written into the pipe.
    fn current_job() -> Option<(String, SYSTEMTIME)> {
        let printer = Printer::open()?;
        let (mut needed, mut count) = (0u32, 0u32);
        let _ = unsafe { EnumJobsW(printer.0, 0, 64, 1, None, &mut needed, &mut count) };
        if needed == 0 {
            return None;
        }
        let mut buffer = vec![0u8; needed as usize];
        unsafe { EnumJobsW(printer.0, 0, 64, 1, Some(&mut buffer), &mut needed, &mut count) }.ok()?;
        // SAFETY: the buffer holds `count` JOB_INFO_1W, followed by their strings.
        let jobs =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<JOB_INFO_1W>(), count as usize) };
        let job = jobs.iter().find(|j| j.Status & PRINTING != 0).or(jobs.first())?;
        let name = unsafe { job.pDocument.to_string() }.unwrap_or_default();
        let name = if name.trim().is_empty() { "document".to_string() } else { name };
        Some((name, job.Submitted))
    }

    fn utc_now() -> SYSTEMTIME {
        unsafe { GetSystemTime() }
    }

    /// Comparable form of a UTC time.
    fn key(t: &SYSTEMTIME) -> (u16, u16, u16, u16, u16, u16, u16) {
        (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
    }
}
