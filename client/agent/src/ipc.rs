//! Local channel between the service's agent and the app of the person at the PC.
//!
//! With nuvira-service the agent runs as SYSTEM and the app as the user. To show
//! the ID and the temporary password and to confirm requests, the app connects to
//! the named pipe `\\.\pipe\nuvira-agent-<session>` created by the agent:
//! - access allowed to SYSTEM and interactive users; the agent accepts only apps
//!   of its own Windows session;
//! - one JSON line per message: [`ToApp`] towards the app, [`AgentCommand`] towards the agent;
//! - one app at a time, which receives the current state as soon as it connects;
//! - with no app connected nobody can confirm: requests are rejected.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Pipes::GetNamedPipeClientSessionId;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::{HSTRING, PCWSTR};

use crate::{AgentCommand, AgentEvent};

/// Messages from the agent to the app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToApp {
    /// First message: server the agent is registered on.
    Info {
        server: String,
    },
    Event {
        event: AgentEvent,
    },
}

/// Windows session of the current process.
pub fn current_session() -> u32 {
    let mut session = 0;
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    session
}

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\nuvira-agent-{session}")
}

/// The NuviraHub service is installed (a normal user can check this too).
pub fn service_installed() -> bool {
    unsafe {
        let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
            return false;
        };
        let service = OpenServiceW(scm, &HSTRING::from("nuvira"), SERVICE_QUERY_STATUS);
        if let Ok(service) = service {
            let _ = CloseServiceHandle(service);
        }
        let _ = CloseServiceHandle(scm);
        service.is_ok()
    }
}

/// State to repeat to every app that connects.
#[derive(Default)]
struct Snapshot {
    status: Option<AgentEvent>,
    password: Option<AgentEvent>,
    session: Option<AgentEvent>,
    incoming: Option<AgentEvent>,
}

impl Snapshot {
    fn apply(&mut self, event: &AgentEvent) {
        let slot = match event {
            AgentEvent::Connecting | AgentEvent::Online { .. } | AgentEvent::Offline { .. } => {
                &mut self.status
            }
            AgentEvent::Password { .. } => &mut self.password,
            AgentEvent::SessionStarted { .. } => &mut self.session,
            AgentEvent::Incoming { .. } => &mut self.incoming,
            AgentEvent::SessionEnded { .. } => {
                self.session = None;
                return;
            }
            AgentEvent::IncomingClosed { .. } => {
                self.incoming = None;
                return;
            }
            // Chat messages only matter to the app connected at that moment.
            AgentEvent::Chat { .. } | AgentEvent::FileReceived { .. } => return,
        };
        *slot = Some(event.clone());
    }

    fn events(&self) -> impl Iterator<Item = &AgentEvent> {
        [&self.status, &self.password, &self.session, &self.incoming].into_iter().flatten()
    }

    /// Nobody can confirm the pending request any more: it is rejected.
    fn reject_pending(&self, commands: &mpsc::UnboundedSender<AgentCommand>) {
        if let Some(AgentEvent::Incoming { session_id, .. }) = &self.incoming {
            let _ = commands.send(AgentCommand::Respond { session_id: session_id.clone(), accept: false });
        }
    }
}

/// Agent side: forwards events and commands to the connected app. Ends when `events` closes.
pub async fn serve(
    server: String,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    commands: mpsc::UnboundedSender<AgentCommand>,
) -> Result<()> {
    let session = current_session();
    let name = pipe_name(session);
    let mut snapshot = Snapshot::default();
    // The first instance is created only if the name is free (no other process has
    // taken it); afterwards a new one is always prepared before using the connected
    // one, so the name stays ours even while an app disconnects.
    let mut first = true;
    let mut next: Option<NamedPipeServer> = None;
    loop {
        let pipe = match next.take() {
            Some(pipe) => pipe,
            None => match create_pipe(&name, first) {
                Ok(pipe) => {
                    first = false;
                    pipe
                }
                Err(e) => {
                    warn!("creating {name} failed: {e:#}; retrying in 2 s");
                    let pause = tokio::time::sleep(Duration::from_secs(2));
                    tokio::pin!(pause);
                    loop {
                        tokio::select! {
                            _ = &mut pause => break,
                            event = events.recv() => {
                                let Some(event) = event else { return Ok(()) };
                                without_app(&event, &mut snapshot, &commands);
                            }
                        }
                    }
                    continue;
                }
            },
        };
        // While waiting for the app, events update the state.
        let connected = {
            let connect = pipe.connect();
            tokio::pin!(connect);
            loop {
                tokio::select! {
                    r = &mut connect => break r,
                    event = events.recv() => {
                        let Some(event) = event else { return Ok(()) };
                        without_app(&event, &mut snapshot, &commands);
                    }
                }
            }
        };
        next = match create_pipe(&name, false) {
            Ok(pipe) => Some(pipe),
            Err(e) => {
                warn!("next instance of {name}: {e:#}");
                None
            }
        };
        if let Err(e) = connected {
            debug!("app connection failed: {e}");
            continue;
        }
        let mut client_session = u32::MAX;
        let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&pipe);
        let _ = unsafe { GetNamedPipeClientSessionId(HANDLE(raw), &mut client_session) };
        if client_session != session {
            warn!("app from another Windows session ({client_session}) rejected");
            continue;
        }
        info!("app connected");
        if let Err(e) = serve_app(pipe, &server, &mut snapshot, &mut events, &commands).await {
            debug!("channel with the app: {e:#}");
        }
        info!("app disconnected");
        snapshot.reject_pending(&commands);
    }
}

/// Event while no app is connected: it updates the state; a request to confirm is
/// rejected right away, since there is nobody to ask.
fn without_app(event: &AgentEvent, snapshot: &mut Snapshot, commands: &mpsc::UnboundedSender<AgentCommand>) {
    snapshot.apply(event);
    if matches!(event, AgentEvent::Incoming { .. }) {
        info!("no app open to confirm the request: rejected");
        snapshot.reject_pending(commands);
    }
}

async fn serve_app(
    pipe: NamedPipeServer,
    server: &str,
    snapshot: &mut Snapshot,
    events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    commands: &mpsc::UnboundedSender<AgentCommand>,
) -> Result<()> {
    let (read, mut write) = tokio::io::split(pipe);
    let mut lines = BufReader::new(read).lines();
    send_line(&mut write, &ToApp::Info { server: server.to_string() }).await?;
    for event in snapshot.events() {
        send_line(&mut write, &ToApp::Event { event: event.clone() }).await?;
    }
    loop {
        tokio::select! {
            line = lines.next_line() => match line? {
                Some(line) => match serde_json::from_str::<AgentCommand>(&line) {
                    Ok(command) => {
                        let _ = commands.send(command);
                    }
                    Err(e) => warn!("invalid command from the app: {e}"),
                },
                None => return Ok(()),
            },
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                snapshot.apply(&event);
                send_line(&mut write, &ToApp::Event { event }).await?;
            }
        }
    }
}

/// Pipe accessible to SYSTEM and interactive users. With `first` creation fails
/// if another process already has a pipe with the same name.
fn create_pipe(name: &str, first: bool) -> Result<NamedPipeServer> {
    let sddl = HSTRING::from("D:P(A;;GA;;;SY)(A;;GRGW;;;IU)");
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, 1, &mut descriptor, None) }
        .context("pipe security descriptor")?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: the attributes are valid for the whole call.
    let pipe = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
    };
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    Ok(pipe?)
}

async fn send_line(write: &mut (impl AsyncWrite + Unpin), msg: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    Ok(())
}

/// App side: connection to the service's agent in its own Windows session.
/// The message channel closes when the agent disconnects.
pub fn connect() -> Result<(mpsc::UnboundedReceiver<ToApp>, mpsc::UnboundedSender<AgentCommand>)> {
    let pipe =
        ClientOptions::new().open(pipe_name(current_session())).context("service agent not reachable")?;
    check_server(&pipe)?;
    let (read, mut write) = tokio::io::split(pipe);
    let (to_app_tx, to_app_rx) = mpsc::unbounded_channel();
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel::<AgentCommand>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<ToApp>(&line) {
                Ok(msg) => {
                    if to_app_tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(e) => warn!("invalid message from the agent: {e}"),
            }
        }
    });
    tokio::spawn(async move {
        while let Some(command) = commands_rx.recv().await {
            if send_line(&mut write, &command).await.is_err() {
                break;
            }
        }
    });
    Ok((to_app_rx, commands_tx))
}

/// The other end must be the service's agent, not a program that took the pipe name
/// first (malware running as the user), which would otherwise receive the confirmation
/// requests and could fake them. A process of the current user can always be
/// examined by it; the service's agent runs as SYSTEM and Windows does not even let a
/// user query it. So: not accessible means not the user's, and is accepted;
/// accessible means it must be the installed program (in Program Files, where only
/// administrators write) running as SYSTEM.
fn check_server(pipe: &tokio::net::windows::named_pipe::NamedPipeClient) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
    };
    use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    let mut pid = 0u32;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }
        .context("agent process")?;
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(process) => process,
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => return Ok(()),
        Err(e) => return Err(e).context("agent process"),
    };
    let result = (|| {
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut len,
            )
        }
        .context("agent program")?;
        let image = std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]));
        let expected = std::env::var_os("ProgramFiles")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| r"C:\Program Files".into())
            .join(r"nuvira\nuvira.exe");
        if !image.to_string_lossy().eq_ignore_ascii_case(&expected.to_string_lossy()) {
            anyhow::bail!(
                "the local channel is held by {} instead of the NuviraHub service",
                image.display()
            );
        }
        // An administrator's prompt can open it: the account must still be SYSTEM.
        let mut token = HANDLE::default();
        match unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } {
            Ok(()) => {
                let mut info = [0u8; 256];
                let mut size = 0u32;
                let ok = unsafe {
                    GetTokenInformation(
                        token,
                        TokenUser,
                        Some(info.as_mut_ptr().cast()),
                        info.len() as u32,
                        &mut size,
                    )
                };
                let _ = unsafe { CloseHandle(token) };
                ok.context("agent account")?;
                let user = unsafe { &*(info.as_ptr() as *const TOKEN_USER) };
                if !unsafe { windows::Win32::Security::IsWellKnownSid(user.User.Sid, WinLocalSystemSid) }
                    .as_bool()
                {
                    anyhow::bail!("the local channel is held by a program not running as SYSTEM");
                }
                Ok(())
            }
            Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => Ok(()),
            Err(e) => Err(e).context("agent account"),
        }
    })();
    let _ = unsafe { CloseHandle(process) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let msg =
            ToApp::Event { event: AgentEvent::Online { device_id: "123456789".into(), websocket: false } };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"event","event":{"event":"online","device_id":"123456789","websocket":false}}"#
        );
        assert!(matches!(serde_json::from_str::<ToApp>(&json).unwrap(), ToApp::Event { .. }));
        let command: AgentCommand =
            serde_json::from_str(r#"{"command":"respond","session_id":"s","accept":true}"#).unwrap();
        assert!(matches!(command, AgentCommand::Respond { accept: true, .. }));
    }
}
