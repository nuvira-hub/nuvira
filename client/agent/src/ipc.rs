//! Canale locale tra l'agent del servizio e l'app di chi usa il PC.
//!
//! Con nexodesk-service l'agent gira come SYSTEM e l'app come utente. Per mostrare
//! ID e password temporanea e confermare le richieste, l'app si collega alla
//! named pipe `\\.\pipe\nexodesk-agent-<sessione>` creata dall'agent:
//! - accesso consentito a SYSTEM e agli utenti interattivi; l'agent accetta solo
//!   app della propria sessione di Windows;
//! - una riga JSON per messaggio: verso l'app [`ToApp`], verso l'agent [`AgentCommand`];
//! - un'app alla volta, che appena collegata riceve lo stato attuale;
//! - senza app collegata nessuno può confermare: le richieste vengono rifiutate.

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

/// Messaggi dall'agent all'app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToApp {
    /// Primo messaggio: server a cui è registrato l'agent.
    Info { server: String },
    Event { event: AgentEvent },
}

/// Sessione di Windows del processo corrente.
pub fn current_session() -> u32 {
    let mut session = 0;
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    session
}

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\nexodesk-agent-{session}")
}

/// Il servizio NexoDesk è installato (lo può verificare anche un utente normale).
pub fn service_installed() -> bool {
    unsafe {
        let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else { return false };
        let service = OpenServiceW(scm, &HSTRING::from("NexoDesk"), SERVICE_QUERY_STATUS);
        if let Ok(service) = service {
            let _ = CloseServiceHandle(service);
        }
        let _ = CloseServiceHandle(scm);
        service.is_ok()
    }
}

/// Stato da ripetere a ogni app che si collega.
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
            AgentEvent::Connecting | AgentEvent::Online { .. } | AgentEvent::Offline { .. } => &mut self.status,
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
        };
        *slot = Some(event.clone());
    }

    fn events(&self) -> impl Iterator<Item = &AgentEvent> {
        [&self.status, &self.password, &self.session, &self.incoming].into_iter().flatten()
    }

    /// Nessuno può più confermare la richiesta in attesa: la si rifiuta.
    fn reject_pending(&self, commands: &mpsc::UnboundedSender<AgentCommand>) {
        if let Some(AgentEvent::Incoming { session_id, .. }) = &self.incoming {
            let _ = commands.send(AgentCommand::Respond { session_id: session_id.clone(), accept: false });
        }
    }
}

/// Lato agent: inoltra eventi e comandi all'app collegata. Termina quando si chiude `events`.
pub async fn serve(
    server: String,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    commands: mpsc::UnboundedSender<AgentCommand>,
) -> Result<()> {
    let session = current_session();
    let name = pipe_name(session);
    let mut snapshot = Snapshot::default();
    // La prima istanza si crea solo se il nome è libero (nessun altro processo lo ha
    // preso); poi se ne prepara sempre una nuova prima di usare quella collegata,
    // così il nome resta nostro anche mentre un'app si scollega.
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
                    warn!("creazione di {name} non riuscita: {e:#}; riprovo tra 2 s");
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
        // In attesa dell'app, gli eventi aggiornano lo stato.
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
                warn!("istanza successiva di {name}: {e:#}");
                None
            }
        };
        if let Err(e) = connected {
            debug!("collegamento dell'app non riuscito: {e}");
            continue;
        }
        let mut client_session = u32::MAX;
        let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&pipe);
        let _ = unsafe { GetNamedPipeClientSessionId(HANDLE(raw), &mut client_session) };
        if client_session != session {
            warn!("app di un'altra sessione di Windows ({client_session}) rifiutata");
            continue;
        }
        info!("app collegata");
        if let Err(e) = serve_app(pipe, &server, &mut snapshot, &mut events, &commands).await {
            debug!("canale con l'app: {e:#}");
        }
        info!("app scollegata");
        snapshot.reject_pending(&commands);
    }
}

/// Evento mentre nessuna app è collegata: aggiorna lo stato; una richiesta da
/// confermare viene rifiutata subito, perché non c'è nessuno a cui chiedere.
fn without_app(event: &AgentEvent, snapshot: &mut Snapshot, commands: &mpsc::UnboundedSender<AgentCommand>) {
    snapshot.apply(event);
    if matches!(event, AgentEvent::Incoming { .. }) {
        info!("nessuna app aperta per confermare la richiesta: rifiutata");
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
                    Err(e) => warn!("comando dell'app non valido: {e}"),
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

/// Pipe accessibile a SYSTEM e agli utenti interattivi. Con `first` la creazione
/// fallisce se un altro processo ha già una pipe con lo stesso nome.
fn create_pipe(name: &str, first: bool) -> Result<NamedPipeServer> {
    let sddl = HSTRING::from("D:P(A;;GA;;;SY)(A;;GRGW;;;IU)");
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, 1, &mut descriptor, None) }
        .context("descrittore di sicurezza della pipe")?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: gli attributi sono validi per tutta la chiamata.
    let pipe = unsafe {
        ServerOptions::new().first_pipe_instance(first).create_with_security_attributes_raw(name, (&raw mut attributes).cast())
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

/// Lato app: collegamento all'agent del servizio nella propria sessione di Windows.
/// Il canale dei messaggi si chiude quando l'agent si scollega.
pub fn connect() -> Result<(mpsc::UnboundedReceiver<ToApp>, mpsc::UnboundedSender<AgentCommand>)> {
    let pipe = ClientOptions::new().open(pipe_name(current_session())).context("agent del servizio non raggiungibile")?;
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
                Err(e) => warn!("messaggio dell'agent non valido: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let msg = ToApp::Event { event: AgentEvent::Online { device_id: "123456789".into(), websocket: false } };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"event","event":{"event":"online","device_id":"123456789","websocket":false}}"#);
        assert!(matches!(serde_json::from_str::<ToApp>(&json).unwrap(), ToApp::Event { .. }));
        let command: AgentCommand = serde_json::from_str(r#"{"command":"respond","session_id":"s","accept":true}"#).unwrap();
        assert!(matches!(command, AgentCommand::Respond { accept: true, .. }));
    }
}
