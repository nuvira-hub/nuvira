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
//!
//! On Linux and macOS the printer is a CUPS queue with our backend (`nuvira_cups`):
//! the backend connects to this agent (TCP on 127.0.0.1 on Linux, a Unix socket on
//! macOS) and writes the PDF; without a session it cancels the job by itself.

#[cfg(windows)]
pub use windows_impl::listen;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use cups_impl::listen;

/// Larger documents are not sent.
const MAX_DOCUMENT: u64 = 512 << 20;

/// Not on other systems.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub struct Listener;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn listen() -> Option<Listener> {
    None
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
impl Listener {
    pub async fn serve(self, _conn: quinn::Connection, _allowed: impl Fn() -> bool) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

/// Sends a printed document to the viewer on a stream of its own.
fn send_document(conn: &quinn::Connection, name: String, data: Vec<u8>) {
    tracing::info!(name, bytes = data.len(), "document printed: sending it to the viewer");
    let header = nuvira_proto::v1::FileChunk { name, print: true, ..Default::default() };
    let conn = conn.clone();
    tokio::spawn(async move {
        if let Err(e) = nuvira_transport::send_stream(&conn, header, &data).await {
            tracing::warn!("printed document not sent: {e:#}");
        }
    });
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod cups_impl {
    use super::MAX_DOCUMENT;
    use anyhow::{Context, Result};
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
    use tracing::{info, warn};

    /// Listening for the backend during this session: `None` without the printer, or
    /// when another agent (another user's session) already listens.
    pub fn listen() -> Option<Listener> {
        if !nuvira_cups::backend_path().exists() {
            return None;
        }
        match bind() {
            Ok(listener) => Some(listener),
            Err(e) => {
                warn!("remote printing not available: {e:#}");
                None
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub struct Listener(tokio::net::TcpListener);

    #[cfg(target_os = "linux")]
    fn bind() -> Result<Listener> {
        use std::net::{Ipv4Addr, SocketAddrV4};

        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, nuvira_cups::PORT);
        let socket = std::net::TcpListener::bind(address).with_context(|| format!("listening on {address}"))?;
        socket.set_nonblocking(true)?;
        Ok(Listener(tokio::net::TcpListener::from_std(socket)?))
    }

    #[cfg(target_os = "linux")]
    impl Listener {
        /// The next connection of the backend: only root's (CUPS runs the backend as
        /// root), not another local program's.
        async fn accept(&self) -> Result<impl AsyncRead + AsyncWrite + Unpin> {
            loop {
                let (stream, peer) = self.0.accept().await.context("printer port")?;
                let (std::net::SocketAddr::V4(peer), std::net::SocketAddr::V4(local)) = (peer, stream.local_addr()?)
                else {
                    continue;
                };
                match nuvira_cups::tcp_owner(peer, Some(local)) {
                    Some(0) => return Ok(stream),
                    owner => warn!(?owner, "connection to the printer port not from CUPS: closed"),
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub struct Listener {
        socket: tokio::net::UnixListener,
        path: std::path::PathBuf,
    }

    #[cfg(target_os = "macos")]
    fn bind() -> Result<Listener> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let path = nuvira_cups::socket_path(uid);
        // Left by an agent of ours that ended without removing it: nobody answers.
        if let Ok(meta) = std::fs::symlink_metadata(&path)
            && meta.uid() == uid
            && std::os::unix::net::UnixStream::connect(&path).is_err()
        {
            let _ = std::fs::remove_file(&path);
        }
        let socket = tokio::net::UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))?;
        // Only root (the backend) and this user connect.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener { socket, path })
    }

    #[cfg(target_os = "macos")]
    impl Listener {
        async fn accept(&self) -> Result<impl AsyncRead + AsyncWrite + Unpin> {
            loop {
                let (stream, _) = self.socket.accept().await.context("printer socket")?;
                match stream.peer_cred().map(|c| c.uid()) {
                    Ok(0) => return Ok(stream),
                    owner => warn!(?owner, "connection to the printer socket not from CUPS: closed"),
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    impl Listener {
        /// Every job printed during the session goes to the viewer on a stream of its
        /// own, while `allowed` (the person at the PC can stop it); it never ends by
        /// itself (the end of the session drops the listener).
        pub async fn serve(self, conn: quinn::Connection, allowed: impl Fn() -> bool) -> Result<()> {
            loop {
                let stream = self.accept().await?;
                let (read, mut write) = tokio::io::split(stream);
                let answer = match receive(read).await {
                    Ok((name, data)) => {
                        let answer = if !allowed() {
                            info!(name, "document printed, but remote printing is no longer allowed: not sent");
                            "remote printing is not allowed"
                        } else if data.len() as u64 > MAX_DOCUMENT {
                            warn!(name, "printed document larger than {MAX_DOCUMENT} bytes: not sent");
                            "the document is too large"
                        } else if data.is_empty() {
                            "the document is empty"
                        } else {
                            super::send_document(&conn, name, data);
                            nuvira_cups::SENT
                        };
                        answer.to_string()
                    }
                    Err(e) => {
                        warn!("printed document not received: {e:#}");
                        format!("{e:#}")
                    }
                };
                // The backend shows it in the queue (it may have gone: no matter).
                let _ = write.write_all(format!("{answer}\n").as_bytes()).await;
                let _ = write.shutdown().await;
            }
        }
    }

    /// Title and PDF written by the backend.
    async fn receive(read: impl AsyncRead + Unpin) -> Result<(String, Vec<u8>)> {
        let mut read = BufReader::new(read);
        let mut line = String::new();
        (&mut read).take(64).read_line(&mut line).await?;
        anyhow::ensure!(line.trim_end() == nuvira_cups::HEADER, "not a NuviraHub printer job");
        let mut title = String::new();
        (&mut read).take(nuvira_cups::MAX_TITLE as u64 + 1).read_line(&mut title).await?;
        let name = match title.trim() {
            "" => "document".to_string(),
            t => t.to_string(),
        };
        let mut data = Vec::new();
        read.take(MAX_DOCUMENT + 1).read_to_end(&mut data).await.context("reading the printed document")?;
        Ok((name, data))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn job_from_the_backend() {
            let job = format!("{}\nInvoice 12\n%PDF-1.7 ...", nuvira_cups::HEADER);
            let (name, data) = receive(job.as_bytes()).await.unwrap();
            assert_eq!((name.as_str(), data.as_slice()), ("Invoice 12", b"%PDF-1.7 ...".as_slice()));
            let (name, _) = receive(format!("{}\n\n%PDF", nuvira_cups::HEADER).as_bytes()).await.unwrap();
            assert_eq!(name, "document");
            assert!(receive(b"GET / HTTP/1.1\r\n".as_slice()).await.is_err());
        }
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
    use super::MAX_DOCUMENT;
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
                super::send_document(&conn, name, data);
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
