//! The link between the window and the agent: the service's agent reached over
//! the local channel when unattended access is installed, the events of either
//! agent turned into the interface's state, and what the person at this PC
//! allows sent to it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nuvira_agent::{AgentCommand, AgentEvent};
use nuvira_i18n::tf;
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    ChatLine, IncomingUi, NuviraApp, SessionUi, SupportUi, bring_to_front, invite, load_settings, save_settings,
};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The NuviraHub service is installed: no agent in the app, the service's one is
/// shown. If its agent does not answer (start-up, user switch) it is retried.
#[cfg(any(windows, unix))]
pub(crate) fn start_service_link(app: &AppHandle) {
    use nuvira_agent::ipc::ToApp;

    let state = app.state::<NuviraApp>();
    state.update(app, |ui| {
        ui.service = true;
        ui.status = "connecting".into();
        ui.device_id = None;
    });
    let app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let state = app.state::<NuviraApp>();
        let mut last_error: Option<String> = None;
        loop {
            match nuvira_agent::ipc::connect() {
                Ok((mut messages, commands)) => {
                    info!("connected to the service's agent");
                    last_error = None;
                    send_permissions(&commands);
                    *state.commands.lock().expect("commands") = Some(commands);
                    state.update(&app, |ui| {
                        ui.error = None;
                        ui.service_stopped = false;
                        ui.service_old = None;
                    });
                    while let Some(msg) = messages.recv().await {
                        match msg {
                            // With the service the server in the settings is the service's
                            // (empty = public), also for outgoing connections.
                            ToApp::Info { server, version, update_blocked, update_stale } => {
                                *state.service_server.lock().expect("service server") = Some(server.clone());
                                keep_service_server(&server);
                                state.update(&app, |ui| {
                                    ui.server = shown_server(&server);
                                    ui.installed = nuvira_update::newer(&version, nuvira_update::CURRENT)
                                        .then_some(version);
                                    ui.update_blocked = update_blocked;
                                    ui.update_stale = update_stale;
                                })
                            }
                            ToApp::Event { event } => on_agent_event(&app, event),
                        }
                    }
                    *state.commands.lock().expect("commands") = None;
                    warn!("link with the service's agent lost");
                    state.update(&app, |ui| {
                        ui.status = "connecting".into();
                        ui.incoming = None;
                        ui.session = None;
                        ui.error = Some("@status.service_lost".into());
                    });
                }
                Err(e) => {
                    // Every 2 s while the service starts: logged once per different reason.
                    let reason = format!("{e:#}");
                    if last_error.as_ref() != Some(&reason) {
                        warn!("service agent not reachable: {reason}");
                        last_error = Some(reason);
                    }
                    // Stopped (not starting): it will not answer by itself, the
                    // interface offers to start it again.
                    let stopped = nuvira_service::service_state()
                        .is_ok_and(|s| s == nuvira_service::ServiceState::Stopped);
                    let old = if stopped {
                        nuvira_service::installed_version()
                            .filter(|v| nuvira_update::newer(nuvira_update::CURRENT, v))
                    } else {
                        None
                    };
                    state.update(&app, |ui| {
                        ui.status = if stopped { "offline" } else { "connecting" }.into();
                        ui.error = Some(
                            if stopped { "@status.service_stopped" } else { "@status.service_waiting" }
                                .into(),
                        );
                        ui.service_stopped = stopped;
                        ui.service_old = old;
                    });
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    *state.agent.lock().expect("agent") = Some(handle);
}

pub(crate) fn on_agent_event(app: &AppHandle, event: AgentEvent) {
    let state = app.state::<NuviraApp>();
    match event {
        AgentEvent::Connecting => state.update(app, |ui| ui.status = "connecting".into()),
        AgentEvent::Alias { alias, error } => state.update(app, |ui| {
            ui.alias = alias;
            ui.alias_error = error;
        }),
        AgentEvent::Online { device_id, websocket } => {
            state.update(app, |ui| {
                ui.status = "online".into();
                ui.device_id = Some(device_id);
                ui.websocket = websocket;
                ui.error = None;
            });
            invite::try_send(app);
        }
        AgentEvent::Offline { reason } => state.update(app, |ui| {
            ui.status = "offline".into();
            ui.error = Some(reason);
        }),
        AgentEvent::Password { password } => state.update(app, |ui| ui.password = password),
        AgentEvent::Incoming { session_id, viewer_id, viewer_name, support } => {
            state.update(app, |ui| {
                ui.incoming = Some(IncomingUi {
                    session_id,
                    viewer_id,
                    viewer_name,
                    support,
                    deadline_ms: now_ms() + nuvira_agent::CONFIRM_TIMEOUT.as_millis() as u64,
                })
            });
            bring_to_front(app);
        }
        AgentEvent::Support { position, taken_by, closed, error, invited_by } => state.update(app, |ui| {
            let taken = !taken_by.is_empty();
            ui.support = (!closed).then_some(SupportUi { position, taken_by, invited_by });
            if !error.is_empty() {
                ui.support_error = Some(error);
            } else if closed || taken {
                // Taken (or ended): the invitation did its part.
                ui.invite = None;
            }
        }),
        AgentEvent::IncomingClosed { session_id } => state.update(app, |ui| {
            if ui.incoming.as_ref().is_some_and(|i| i.session_id == session_id) {
                ui.incoming = None;
            }
        }),
        AgentEvent::SessionStarted { viewer_id, viewer_name, direct, terminal } => state.update(app, |ui| {
            ui.session = Some(SessionUi {
                viewer_id,
                viewer_name,
                direct,
                terminal,
                recording: false,
                microphone: false,
                microphone_muted: false,
                offered: Vec::new(),
                off: Vec::new(),
                guests: Vec::new(),
            });
            ui.chat.clear();
        }),
        AgentEvent::FileReceived { name, path } => {
            info!("file received: {path}");
            let text = tf("chat.file_received", &[("name", &name)]);
            state.update(app, |ui| ui.chat.push(ChatLine { mine: false, system: true, text }));
            bring_to_front(app);
        }
        AgentEvent::Chat { text } => {
            state.update(app, |ui| ui.chat.push(ChatLine { mine: false, system: false, text }));
            bring_to_front(app);
        }
        AgentEvent::Microphone { on } => {
            info!(on, "technician's microphone played here");
            state.update(app, |ui| {
                if let Some(session) = ui.session.as_mut() {
                    session.microphone = on;
                    if on {
                        session.microphone_muted = false;
                    }
                }
            });
        }
        AgentEvent::Recording { on } => {
            info!(on, "session recorded by the controlling side");
            state.update(app, |ui| {
                if let Some(session) = ui.session.as_mut() {
                    session.recording = on;
                }
            });
            // The person at the PC must see it.
            if on {
                bring_to_front(app);
            }
        }
        AgentEvent::Permissions { offered, off } => state.update(app, |ui| {
            if let Some(session) = ui.session.as_mut() {
                session.offered = offered;
                session.off = off;
            }
        }),
        AgentEvent::Guests { names } => {
            let joined = state.ui.lock().expect("state").session.as_ref().is_some_and(|s| names.len() > s.guests.len());
            state.update(app, |ui| {
                if let Some(session) = ui.session.as_mut() {
                    session.guests = names;
                }
            });
            // The person at the PC must see who else is watching.
            if joined {
                bring_to_front(app);
            }
        }
        AgentEvent::Access { view } => state.update(app, |ui| ui.access = Some(view)),
        AgentEvent::Rules { recording_required, lock_required, idle_minutes } => state.update(app, |ui| {
            ui.recording_required = recording_required;
            ui.lock_required = lock_required;
            ui.idle_required = idle_minutes;
        }),
        AgentEvent::SessionEnded { error } => state.update(app, |ui| {
            ui.session = None;
            if let Some(e) = error {
                info!("session ended: {e}");
            }
        }),
    }
}

/// Server as shown in the settings: empty for the public one.
#[cfg(any(windows, unix))]
/// The app's settings take the server of the service's agent: the field shows that
/// one, so it is the one a later installation must use. Without it, unattended
/// access installed with another server (`service install --server`, the MSI, an
/// older app) moved to the app's own server, often the public one, with a new ID,
/// when the person removed it and installed it again from the app.
fn keep_service_server(server: &str) {
    let shown = shown_server(server);
    let mut settings = load_settings();
    if settings.server.trim().eq_ignore_ascii_case(&shown) {
        return;
    }
    info!(server = %shown, "the service's server kept in the app's settings");
    settings.server = shown;
    // The fingerprint went with the old server: the new one is checked on first use.
    settings.server_key = None;
    if let Err(e) = save_settings(&settings) {
        warn!("settings not saved: {e:#}");
    }
}

fn shown_server(server: &str) -> String {
    if server.trim().eq_ignore_ascii_case(nuvira_transport::signal::PUBLIC_SERVER) {
        String::new()
    } else {
        server.trim().to_string()
    }
}

/// What the person at this PC allows, for the agent (the app's or the service's).
pub(crate) fn send_permissions(commands: &mpsc::UnboundedSender<AgentCommand>) {
    let settings = load_settings();
    let _ = commands.send(AgentCommand::AllowPrivacy { allow: !settings.no_privacy });
    let _ = commands.send(AgentCommand::AllowTerminal { allow: !settings.no_terminal });
    let _ = commands.send(AgentCommand::AllowTunnel { allow: !settings.no_tunnel });
    let _ = commands.send(AgentCommand::AllowPrint { allow: !settings.no_print });
    let _ = commands.send(AgentCommand::AllowRecording { allow: !settings.no_recording });
    let _ = commands.send(AgentCommand::LockOnEnd { on: settings.lock_on_end });
    let _ = commands.send(AgentCommand::IdleMinutes { minutes: settings.idle_minutes });
    // The rules of unattended access as they are now (the agent's first report may
    // be older than the last change).
    let _ = commands.send(AgentCommand::GetAccess);
}
