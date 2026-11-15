//! Switch sides, at the controlled computer: a window with the technician's screen.
//!
//! The viewer opens a stream of the session with `StreamOpen.reverse` and serves,
//! inside it, a session of its own (`nuvira_transport::nested`). Here the stream
//! is relayed to a UDP socket on the loopback, and this program is started as a
//! viewer for the person signed in, towards it, with the certificate pinned and
//! the session's password (both from the viewer). Closing the window ends it and
//! the technician is told (`SwitchSides`); the technician taking control back
//! closes the stream, and the window with it.
//!
//! Only the app's program has a viewer in it: the app tells at start
//! ([`set_viewer_available`]); the command-line agent does not offer this.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use nuvira_proto::v1::{ReverseOpen, SwitchSides};
use tokio::sync::mpsc;
use tracing::{info, warn};

static VIEWER: AtomicBool = AtomicBool::new(false);

/// Called by the app: this program can also be a viewer (`nuvira viewer`).
pub fn set_viewer_available() {
    VIEWER.store(true, Ordering::Relaxed);
}

pub fn viewer_available() -> bool {
    VIEWER.load(Ordering::Relaxed)
}

/// A window gives no sign of life for this long: the relay gives up.
const IDLE: Duration = Duration::from_secs(120);

/// Serves one `StreamOpen.reverse` stream: the window, until it or the stream ends.
pub async fn open_window(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    open: ReverseOpen,
    report: mpsc::UnboundedSender<SwitchSides>,
) -> Result<()> {
    let fail = |error: String| {
        let _ = report.send(SwitchSides { on: false, error, ..Default::default() });
    };
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.context("local socket")?;
    let port = udp.local_addr()?.port();
    let mut window = match Window::start(port, &open) {
        Ok(window) => window,
        Err(e) => {
            warn!("switch sides: the window did not open: {e:#}");
            fail(e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@')).unwrap_or_else(|| format!("{e:#}")));
            return Ok(());
        }
    };
    info!(name = open.name, "switch sides: window with the other screen opened");
    let _ = report.send(SwitchSides { on: true, ..Default::default() });
    tokio::select! {
        // The technician took control back, or the session ended.
        r = nuvira_transport::nested::bridge_to_udp(send, recv, udp, IDLE) => {
            window.close();
            info!("switch sides: ended by the technician");
            if let Err(e) = r {
                tracing::debug!("switch sides relay: {e:#}");
            }
        }
        () = window.wait() => {
            info!("switch sides: window closed");
            fail(String::new());
        }
    }
    Ok(())
}

/// The viewer process: started as this process, or (Windows, as SYSTEM) with
/// the token of the person signed in.
enum Window {
    Child(Box<tokio::process::Child>),
    #[cfg(windows)]
    AsUser(win::Child),
}

impl Window {
    fn start(port: u16, open: &ReverseOpen) -> Result<Self> {
        if !viewer_available() {
            anyhow::bail!("@agent.switch_unavailable");
        }
        let exe = std::env::current_exe().context("program path")?;
        let mut args = vec!["viewer".to_string(), format!("127.0.0.1:{port}"), "--fingerprint".into(), open.fingerprint.clone()];
        // Diagnosis and tests: more options for the window.
        if let Ok(extra) = std::env::var("NUVIRA_REVERSE_ARGS") {
            args.extend(extra.split_whitespace().map(str::to_string));
        }
        let env = [("NUVIRA_PASSWORD", open.password.as_str()), ("NUVIRA_REVERSE_OF", open.name.as_str())];
        #[cfg(windows)]
        if nuvira_clipboard::running_as_system() || std::env::var_os("NUVIRA_REVERSE_AS_USER").is_some() {
            return Ok(Self::AsUser(win::Child::start(&exe, &args, &env)?));
        }
        let mut command = tokio::process::Command::new(exe);
        command.args(args).envs(env).kill_on_drop(true).stdin(std::process::Stdio::null());
        // Diagnosis: the window's log in this file.
        if let Some(path) = std::env::var_os("NUVIRA_REVERSE_LOG")
            && let Ok(file) = std::fs::File::create(path)
            && let Ok(copy) = file.try_clone()
        {
            command.stdout(copy).stderr(file);
        }
        #[cfg(target_os = "linux")]
        graphical_env(&mut command);
        Ok(Self::Child(Box::new(command.spawn().context("starting the window")?)))
    }

    async fn wait(&mut self) {
        match self {
            Self::Child(child) => {
                let _ = child.wait().await;
            }
            #[cfg(windows)]
            Self::AsUser(child) => child.wait().await,
        }
    }

    fn close(&mut self) {
        match self {
            Self::Child(child) => {
                let _ = child.start_kill();
            }
            #[cfg(windows)]
            Self::AsUser(child) => child.kill(),
        }
    }
}

/// A user service may lack the display's variables: those of the session's
/// own sockets (Wayland first, X11 otherwise).
#[cfg(target_os = "linux")]
fn graphical_env(command: &mut tokio::process::Command) {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    if std::env::var_os("WAYLAND_DISPLAY").is_none()
        && let Some(dir) = &runtime
        && dir.join("wayland-0").exists()
    {
        command.env("WAYLAND_DISPLAY", "wayland-0");
    }
    if std::env::var_os("DISPLAY").is_none() && std::path::Path::new("/tmp/.X11-unix/X0").exists() {
        command.env("DISPLAY", ":0");
    }
}

#[cfg(windows)]
mod win {
    //! As SYSTEM (the service's agent) the window is started with the token of the
    //! person signed in, on their desktop; otherwise as this process.

    use std::path::Path;

    use anyhow::{Context, Result};
    use std::os::windows::io::IntoRawHandle;

    use windows::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0};
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOW, TerminateProcess, WaitForSingleObject,
    };
    use windows::core::{HSTRING, PWSTR};

    /// The process handle, closed when the last user lets it go (the wait may
    /// still be running on another thread).
    struct Process(HANDLE);

    // SAFETY: a process handle, used from any thread.
    unsafe impl Send for Process {}
    unsafe impl Sync for Process {}

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: closed once, by its last owner.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub struct Child {
        process: std::sync::Arc<Process>,
    }

    impl Child {
        pub fn start(exe: &Path, args: &[String], env: &[(&str, &str)]) -> Result<Self> {
            let mut line = format!("\"{}\"", exe.display());
            for a in args {
                line.push(' ');
                line.push_str(a);
            }
            let mut command: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
            let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain(Some(0)).collect();
            let token = crate::files::session_user_token()?;
            // SAFETY: the environment block as Windows makes it (double-NUL ended
            // UTF-16), copied with the variables added; the process started with
            // the person's token or as this one; every handle closed.
            unsafe {
                let mut base = std::ptr::null_mut();
                let _ = CreateEnvironmentBlock(&mut base, token, false);
                let mut block: Vec<u16> = Vec::new();
                if !base.is_null() {
                    let mut p = base as *const u16;
                    loop {
                        let start = p;
                        let mut len = 0;
                        while *p != 0 {
                            p = p.add(1);
                            len += 1;
                        }
                        if len == 0 {
                            break;
                        }
                        let var = std::slice::from_raw_parts(start, len);
                        let name_end = var.iter().skip(1).position(|&c| c == '=' as u16).map_or(len, |i| i + 1);
                        let name = String::from_utf16_lossy(&var[..name_end]);
                        if !env.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)) {
                            block.extend_from_slice(var);
                            block.push(0);
                        }
                        p = p.add(1);
                    }
                    let _ = DestroyEnvironmentBlock(base);
                } else {
                    for (k, v) in std::env::vars() {
                        if !env.iter().any(|(n, _)| n.eq_ignore_ascii_case(&k)) {
                            block.extend(format!("{k}={v}").encode_utf16());
                            block.push(0);
                        }
                    }
                }
                for (k, v) in env {
                    block.extend(format!("{k}={v}").encode_utf16());
                    block.push(0);
                }
                block.push(0);
                let mut startup = STARTUPINFOW {
                    cb: size_of::<STARTUPINFOW>() as u32,
                    lpDesktop: PWSTR(desktop.as_mut_ptr()),
                    ..Default::default()
                };
                // Diagnosis: the window's log in this file (an inherited handle).
                let log = std::env::var_os("NUVIRA_REVERSE_LOG")
                    .and_then(|path| std::fs::File::create(path).ok())
                    .map(|f| HANDLE(f.into_raw_handle()));
                if let Some(log) = log {
                    let _ = SetHandleInformation(log, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
                    startup.dwFlags = STARTF_USESTDHANDLES;
                    startup.hStdOutput = log;
                    startup.hStdError = log;
                }
                let inherit = log.is_some();
                let mut info = PROCESS_INFORMATION::default();
                let exe = HSTRING::from(exe.as_os_str());
                let started = match token {
                    Some(token) => {
                        let r = CreateProcessAsUserW(
                            Some(token),
                            &exe,
                            Some(PWSTR(command.as_mut_ptr())),
                            None,
                            None,
                            inherit,
                            CREATE_UNICODE_ENVIRONMENT,
                            Some(block.as_ptr() as *const _),
                            None,
                            &startup,
                            &mut info,
                        );
                        let _ = CloseHandle(token);
                        r
                    }
                    None => CreateProcessW(
                        &exe,
                        Some(PWSTR(command.as_mut_ptr())),
                        None,
                        None,
                        inherit,
                        CREATE_UNICODE_ENVIRONMENT,
                        Some(block.as_ptr() as *const _),
                        None,
                        &startup,
                        &mut info,
                    ),
                };
                if let Some(log) = log {
                    let _ = CloseHandle(log);
                }
                started.context("starting the window")?;
                let _ = CloseHandle(info.hThread);
                Ok(Self { process: std::sync::Arc::new(Process(info.hProcess)) })
            }
        }

        pub async fn wait(&mut self) {
            let process = self.process.clone();
            let _ = tokio::task::spawn_blocking(move || {
                // SAFETY: the handle stays open while `process` is held here.
                unsafe { WaitForSingleObject(process.0, u32::MAX) == WAIT_OBJECT_0 }
            })
            .await;
        }

        pub fn kill(&mut self) {
            // SAFETY: our process handle.
            unsafe {
                let _ = TerminateProcess(self.process.0, 0);
            }
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            // The window goes with the session.
            self.kill();
        }
    }
}
