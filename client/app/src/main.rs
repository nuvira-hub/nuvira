//! NuviraHub — desktop application.
//!
//! Main window (Tauri) with the ID and password of this device, the state of the
//! connection to nuvira-server, confirmation of incoming requests and start of
//! sessions towards other devices. The agent runs in this process (the
//! `nuvira_agent` library) or, if nuvira-service is installed, it is the
//! service's and the app talks to it through the local channel (`nuvira_agent::ipc`).
//! The remote session window is `nuvira-viewer`, started as a separate process
//! (native wgpu window, decision D5).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod access_ui;
mod address_book;
mod app_update;
mod book_csv;
mod branding;
mod invite;
#[cfg(target_os = "macos")]
mod mac_links;
#[cfg(target_os = "linux")]
mod linux_links;
mod local_book;
mod outgoing;
mod settings;
mod settings_ui;

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nuvira_agent::{AgentCommand, AgentConfig, AgentEvent, ServerOptions};
use nuvira_codec::{EncoderChoice, EncoderPlan};
use nuvira_i18n::{t, tf};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, UserAttentionType};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::mpsc;
use tracing::{info, warn};

use app_update::{start_update_checks, started_as, wait_for_predecessor};
use outgoing::launch_viewer;
use settings::{
    apply_branding, apply_file_config, apply_policy, file_invite, load_settings, policy_server, record_dir, save_settings,
};
use settings_ui::print_choice;

const SOURCE_URL: &str = "https://github.com/nuvira-hub/nuvira";
/// Terms of use of the public server, in the interface language: `{lang}` is replaced.
const TERMS_URL: &str = "https://nuvirahub.dev/{lang}/terms/";
/// This PC can be controlled (capture, input): on Windows, on macOS
/// (ScreenCaptureKit, Quartz events) and on Linux with a Wayland session
/// (remote desktop portal) or an X11 one (XTest).
fn controllable() -> bool {
    cfg!(windows)
        || cfg!(target_os = "macos")
        || (cfg!(target_os = "linux") && ["WAYLAND_DISPLAY", "DISPLAY"].iter().any(|v| std::env::var_os(v).is_some()))
}

/// macOS gives the screen and accepts mouse and keyboard only with two
/// permissions the person grants in System Settings. Asked at start: the system
/// shows its own dialog for Screen Recording (once), and Accessibility's page of
/// the settings is opened while it is missing.
#[cfg(target_os = "macos")]
fn mac_permissions() {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    if !unsafe { CGPreflightScreenCaptureAccess() } {
        info!("asking for the Screen Recording permission");
        unsafe { CGRequestScreenCaptureAccess() };
    }
    if !unsafe { AXIsProcessTrusted() } {
        info!("opening the Accessibility settings");
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }
}

/// Unattended access: the system service on Windows, a user service on Linux
/// (systemd) and macOS (launchd).
const UNATTENDED: bool = cfg!(windows) || cfg!(target_os = "linux") || cfg!(target_os = "macos");

/// State shown by the interface (sent with the "state" event).
#[derive(Debug, Default, Clone, Serialize)]
struct UiState {
    server: String,
    /// "connecting", "online", "offline", "viewer_only".
    status: String,
    /// See [`controllable`].
    controllable: bool,
    /// See [`UNATTENDED`].
    unattended: bool,
    /// "windows", "linux", "macos": some texts and buttons depend on it.
    platform: String,
    device_id: Option<String>,
    /// This computer's alias on the server (typed instead of the ID), and why the
    /// one asked for is not it.
    alias: String,
    alias_error: String,
    websocket: bool,
    /// ID and password are the service's (unattended access).
    service: bool,
    /// Unattended access installed for the whole computer by the organisation
    /// (Linux: `nuvira service deploy`): no Install or Remove here.
    service_managed: bool,
    /// The organisation's policies set the permanent password: not changed here
    /// (the service would put the policy's back).
    password_managed: bool,
    password: String,
    error: Option<String>,
    incoming: Option<IncomingUi>,
    session: Option<SessionUi>,
    /// Message of the last session towards another device.
    viewer_message: Option<String>,
    /// Chat with whoever is controlling this PC (current or last session).
    chat: Vec<ChatLine>,
    /// Account signed in to the server's address book.
    account: Option<address_book::AccountUi>,
    /// This computer's support request, while it is open (support queue).
    support: Option<SupportUi>,
    /// Why the last support request was refused ("@key"), e.g. an expired invitation.
    support_error: Option<String>,
    /// An invitation link received and not finished yet (invite.rs).
    invite: Option<invite::InviteUi>,
    /// A session just ended: the interface asks the technician for a note.
    note_request: Option<NoteRequestUi>,
    books: Vec<address_book::BookUi>,
    address_book_error: Option<String>,
    /// While a single sign-on waits in the browser: the ID its confirmation page
    /// names (the address book connection's own, not `device_id`).
    sso_device_id: Option<String>,
    /// Optional modules of the server (NuviraHub Pro): the interface shows their
    /// functions only when listed.
    features: Vec<String>,
    /// Newer version offered by the server (app not installed: the service updates
    /// an installed one by itself).
    update: Option<String>,
    /// Newer version installed by the service while this app was open: restart it.
    installed: Option<String>,
    /// A version the service keeps failing to install (something on the PC stops it).
    update_blocked: Option<String>,
    /// Updates look held back: the newest version the server offers is old or has
    /// no signed time (from the app's own check, or from the service's agent).
    update_stale: Option<nuvira_update::Stale>,
    /// The service is installed but stopped: this PC is not reachable.
    service_stopped: bool,
    /// Version of the stopped service's program, when older than this app (starting
    /// it again also updates it).
    service_old: Option<String>,
    /// Privacy mode allowed on this PC (settings).
    privacy_allowed: bool,
    /// Remote terminal allowed on this PC (settings).
    terminal_allowed: bool,
    /// TCP tunnels allowed on this PC (settings).
    tunnel_allowed: bool,
    /// Remote printing allowed on this PC (settings).
    print_allowed: bool,
    /// Recordings of the sessions allowed on this PC (settings).
    recording_allowed: bool,
    /// The server's rules record every session: the setting is locked.
    recording_required: bool,
    /// This PC is locked when a session ends (settings).
    lock_on_end: bool,
    /// The server's rules lock it at the end of every session: the setting is locked.
    lock_required: bool,
    /// The rules of unattended access, as the agent tells them (no secrets).
    access: Option<nuvira_agent::access::View>,
    /// Sessions end after this many minutes without activity (settings, 0 never).
    idle_minutes: u32,
    /// The server's rules end them after this many (0: no rule).
    idle_required: u32,
    /// Sessions towards other computers are recorded from the start (settings).
    record_sessions: bool,
    /// Sessions towards other computers hide their wallpaper (settings).
    hide_wallpaper: bool,
    /// Folder of the recordings.
    record_dir: String,
    /// Documents of remote computers go to this printer without asking (chosen in the
    /// session window's print window; `print.json` of the viewer).
    print_auto: Option<String>,
    /// GNOME: the lock-screen extension is installed but not loaded in this
    /// session yet; the person must sign out and in once.
    gnome_sign_in_again: bool,
    /// The server is set by the organisation's policies: the field is read-only.
    server_managed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ChatLine {
    /// Written by the person using this PC.
    mine: bool,
    /// App notice (e.g. file received), not a message.
    system: bool,
    text: String,
}

#[derive(Debug, Clone, Serialize)]
struct NoteRequestUi {
    device_id: String,
    name: String,
}

/// State of this computer's support request: its place in the queue (0 until the
/// server answers) and who took it.
#[derive(Debug, Clone, Default, Serialize)]
struct SupportUi {
    position: u32,
    taken_by: String,
    /// The technician it is for (an invitation, docs/47-invitations.md).
    invited_by: String,
}

#[derive(Debug, Clone, Serialize)]
struct IncomingUi {
    session_id: String,
    viewer_id: String,
    viewer_name: String,
    /// The technician who took this computer's support request.
    support: bool,
    /// Request deadline (ms since 1970): the interface shows the countdown.
    deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
struct SessionUi {
    viewer_id: String,
    viewer_name: String,
    direct: bool,
    /// Terminal only: nothing shows on the screen, the strip says it.
    terminal: bool,
    /// Whoever controls this PC is recording the session: the strip says it.
    recording: bool,
    /// The technician's microphone is played here: the strip says it.
    #[serde(default)]
    microphone: bool,
    /// The person here muted it.
    #[serde(default)]
    microphone_muted: bool,
    /// What the session has ("input", "clipboard", "files", "terminal", "tunnel",
    /// "print") and what the person here switched off of it, from the strip.
    offered: Vec<String>,
    off: Vec<String>,
    /// Viewers who joined the session (shared sessions): the strip names them.
    guests: Vec<String>,
}

/// Settings saved in `app.json` in the data folder.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Settings {
    server: String,
    /// Remembered address book sign-in (server token, not the password), in
    /// memory only: the file has it protected as the local address book's
    /// passwords are ([`Settings::token_protected`]).
    #[serde(skip)]
    token: Option<String>,
    /// The token protected with DPAPI on Windows, with the address book's key
    /// (the Keychain, the Secret Service) elsewhere: a copy of `app.json` alone
    /// does not sign anybody in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_protected: Option<String>,
    /// The token in clear text, as versions up to 0.28 saved it: read once and
    /// saved protected.
    #[serde(default, rename = "token", skip_serializing)]
    plain_token: Option<String>,
    /// Interface language ("auto" or a code from `nuvira_i18n::LANGUAGES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    /// Server certificate fingerprint, when it came with the server in the file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server_key: Option<String>,
    /// The person at this PC does not allow privacy mode (black screen during sessions).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    no_privacy: bool,
    /// The person at this PC does not allow the remote terminal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    no_terminal: bool,
    /// The person at this PC does not allow TCP tunnels to its network.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    no_tunnel: bool,
    /// The person at this PC does not allow remote printing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    no_print: bool,
    /// The person at this PC does not allow recordings of the sessions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    no_recording: bool,
    /// This PC is locked when a session ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    lock_on_end: bool,
    /// Sessions end after this many minutes without activity (0: never).
    #[serde(default, skip_serializing_if = "is_zero")]
    idle_minutes: u32,
    /// Sessions towards other computers are recorded from the start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    record_sessions: bool,
    /// Sessions towards other computers hide their wallpaper.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    hide_wallpaper: bool,
    /// Linux and macOS: the server the sign-in screen's agent was given last by this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signin_server: Option<String>,
    /// Folder of the recordings, when not the default one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record_dir: Option<String>,
}

/// Installed for the whole computer by the organisation (Linux, macOS).
/// The permanent password comes from the organisation's policies.
fn password_managed() -> bool {
    #[cfg(any(windows, unix))]
    return nuvira_service::policy::read().password_hash.is_some();
    #[cfg(not(any(windows, unix)))]
    false
}

/// The service of unattended access is installed (running or not).
fn service_installed() -> bool {
    nuvira_service::service_state().is_ok_and(|s| s != nuvira_service::ServiceState::NotInstalled)
}

fn service_managed() -> bool {
    #[cfg(unix)]
    return nuvira_service::managed::installed();
    #[cfg(not(unix))]
    false
}

/// The customisation for the interface (null for the standard program).
#[tauri::command]
fn get_branding() -> Option<branding::Branding> {
    branding::get().cloned()
}

/// Opens the support site of the customisation (only that address).
#[tauri::command]
fn open_support(app: AppHandle) -> Result<(), String> {
    let url = branding::get().map(|b| b.support_url.clone()).unwrap_or_default();
    if url.is_empty() {
        return Err(t("err.unexpected"));
    }
    app.opener().open_url(&url, None::<&str>).map_err(|e| e.to_string())
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Settings {
    /// In place of a settings file that cannot be read: everything the person
    /// at this PC can switch off stays off until they choose again (the agent
    /// gets these, and the defaults would allow everything).
    fn after_a_bad_file() -> Self {
        Self {
            no_privacy: true,
            no_terminal: true,
            no_tunnel: true,
            no_print: true,
            no_recording: true,
            ..Self::default()
        }
    }
}

struct NuviraApp {
    ui: Mutex<UiState>,
    password: Arc<Mutex<String>>,
    commands: Mutex<Option<mpsc::UnboundedSender<AgentCommand>>>,
    agent: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// Encoder chosen at the first session (the test takes a couple of seconds).
    plan: Mutex<Option<EncoderPlan>>,
    address_book: address_book::Link,
    /// Server the service's agent is registered on (from its Info), if linked.
    service_server: Mutex<Option<String>>,
}

impl NuviraApp {
    fn update(&self, app: &AppHandle, change: impl FnOnce(&mut UiState)) {
        let state = {
            let mut ui = self.ui.lock().expect("state");
            change(&mut ui);
            ui.clone()
        };
        let _ = app.emit("state", state);
    }

    fn send(&self, command: AgentCommand) {
        if let Some(tx) = self.commands.lock().expect("commands").as_ref() {
            let _ = tx.send(command);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Brings the window to the front (incoming connection request).
fn bring_to_front(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        let _ = window.request_user_attention(Some(UserAttentionType::Critical));
    }
}

/// Starts (or restarts, if the server changes) the agent in Tauri's runtime, or the
/// link with the service's agent if the service is installed.
fn start_agent(app: &AppHandle) {
    start_agent_with(app, true);
}

/// `ask`: macOS asks for its permissions (not again at every retry).
fn start_agent_with(app: &AppHandle, ask: bool) {
    let state = app.state::<NuviraApp>();
    if let Some(old) = state.agent.lock().expect("agent").take() {
        old.abort();
    }
    if !controllable() {
        return;
    }
    #[cfg(target_os = "macos")]
    if ask {
        mac_permissions();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = ask;
#[cfg(any(windows, unix))]
    if nuvira_agent::ipc::service_installed() {
        start_service_link(app);
        return;
    }
    let server =
        nuvira_transport::signal::server_or_public(&state.ui.lock().expect("state").server).to_string();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    send_permissions(&commands_tx);
    *state.commands.lock().expect("commands") = Some(commands_tx);
    state.update(app, |ui| {
        ui.status = "connecting".into();
        ui.device_id = None;
        ui.error = None;
    });

    let app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let state = app.state::<NuviraApp>();
        let mut config = AgentConfig {
            display: None,
            synthetic: None,
            bitrate_kbps: 8000,
            fps: 60,
            plan: EncoderPlan::Software,
            fixed_bitrate: false,
            view_only: false,
        };
        let cached = *state.plan.lock().expect("encoder");
        config.plan = match cached {
            Some(plan) => plan,
            None => {
                let settings = config.encoder_settings();
                let plan = tokio::task::spawn_blocking(move || {
                    nuvira_codec::plan_encoder(EncoderChoice::Auto, settings)
                })
                .await
                .unwrap_or(EncoderPlan::Software);
                *state.plan.lock().expect("encoder") = Some(plan);
                plan
            }
        };
        let opts = ServerOptions {
            server_name: server,
            server_key: load_settings().server_key.and_then(|k| nuvira_transport::parse_fingerprint(&k).ok()),
            transport: nuvira_transport::signal::TransportMode::Auto,
            server_ws: None,
            identity_path: nuvira_transport::identity::data_dir().join("device.key"),
            confirm: true,
            permanent_password_hash: None,
            info_file: None,
            unattended: false,
        };
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let pump_app = app.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                on_agent_event(&pump_app, event);
            }
        });
        if let Err(e) =
            nuvira_agent::serve_via_server(config, opts, state.password.clone(), events_tx, commands_rx).await
        {
            warn!("agent ended: {e:#}");
            state.update(&app, |ui| {
                ui.status = "offline".into();
                ui.error = Some(format!("{e:#}"));
            });
            // Started before the network was there (after a restart: the server's
            // name not resolved yet), or the server away: tried again.
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            info!("starting the agent again");
            start_agent_with(&app, false);
        }
    });
    *state.agent.lock().expect("agent") = Some(handle);
}

/// The NuviraHub service is installed: no agent in the app, the service's one is
/// shown. If its agent does not answer (start-up, user switch) it is retried.
#[cfg(any(windows, unix))]
fn start_service_link(app: &AppHandle) {
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

fn on_agent_event(app: &AppHandle, event: AgentEvent) {
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

#[tauri::command]
fn get_state(state: State<NuviraApp>) -> UiState {
    let mut ui = state.ui.lock().expect("state");
    // The session window changes it: read again every time.
    ui.print_auto = print_choice()
        .filter(|c| c["automatic"].as_bool() == Some(true))
        .and_then(|c| c["printer"].as_str().map(String::from));
    ui.gnome_sign_in_again = gnome_sign_in_again();
    ui.clone()
}

/// See [`UiState::gnome_sign_in_again`]: asked to GNOME Shell at most every 30 s.
#[cfg(target_os = "linux")]
fn gnome_sign_in_again() -> bool {
    static LAST: Mutex<Option<(std::time::Instant, bool)>> = Mutex::new(None);
    let mut last = LAST.lock().expect("gnome");
    if let Some((at, value)) = *last
        && at.elapsed() < std::time::Duration::from_secs(30)
    {
        return value;
    }
    let value = nuvira_service::gnome::sign_in_again_needed();
    *last = Some((std::time::Instant::now(), value));
    value
}

#[cfg(not(target_os = "linux"))]
fn gnome_sign_in_again() -> bool {
    false
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

/// Saves the server. With the service installed it is the service's server: the
/// service is installed again with it (administrator prompt), keeping device key and
/// permanent password; nothing changes if the prompt is cancelled.
#[tauri::command]
async fn set_server(app: AppHandle, server: String) -> Result<Option<String>, String> {
    if branding::get().is_some_and(|b| b.lock_server) {
        return Err(t("err.locked"));
    }
    if policy_server().is_some() {
        return Err(t("err.policy"));
    }
    let server = server.trim().to_string();
    if !server.is_empty() {
        nuvira_transport::resolve_addr(&server, nuvira_transport::signal::DEFAULT_PORT)
            .map_err(|e| tf("err.invalid_address", &[("e", &e.to_string())]))?;
    }
    // Shown under the field when the server was saved but not everything followed it.
    #[allow(unused_mut)]
    let mut note: Option<String> = None;
#[cfg(any(windows, unix))]
    if nuvira_agent::ipc::service_installed() {
        let current = app.state::<NuviraApp>().service_server.lock().expect("service server").clone();
        let wanted = nuvira_transport::signal::server_or_public(&server);
        if current.as_deref().is_none_or(|s| !s.eq_ignore_ascii_case(wanted)) {
            #[cfg(windows)]
            {
                let params = format!("service install --server \"{server}\"");
                tauri::async_runtime::spawn_blocking(move || run_service_command(params))
                    .await
                    .map_err(|e| e.to_string())??;
            }
            #[cfg(unix)]
            {
                let opts = nuvira_service::user::InstallOptions { server: server.clone(), server_key: None, password: None };
                tauri::async_runtime::spawn_blocking(move || nuvira_service::user::install(&opts))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("{e:#}"))?;
            }
            info!("service moved to {wanted}");
        }
        // The sign-in screen's agent has its own copy of the server, written as root and
        // not readable here: moved when the one it was last given is not this one (also
        // when the person declined the administrator prompt at the previous save).
        #[cfg(unix)]
        if nuvira_service::signin::installed()
            && load_settings().signin_server.as_deref().is_none_or(|s| !s.eq_ignore_ascii_case(wanted))
        {
            let server = server.clone();
            note = tauri::async_runtime::spawn_blocking(move || move_signin(&server))
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    let state = app.state::<NuviraApp>();
    let mut settings = load_settings();
    let changed = settings.server != server;
    settings.server = server.clone();
    if changed {
        // The address book sign-in applies to the server it was made on, and so does the
        // fingerprint that came with it in the file name.
        settings.token = None;
        settings.server_key = None;
    }
    save_settings(&settings).map_err(|e| e.to_string())?;
    state.update(&app, |ui| {
        ui.server = server;
        if changed {
            ui.account = None;
            ui.books.clear();
            ui.features.clear();
        }
    });
    if changed && let Ok(mut client) = state.address_book.client_slot() {
        *client = None;
    }
    start_agent(&app);
    if changed {
        address_book::restore(&app);
    }
    Ok(note)
}

/// Linux and macOS: the system's part of unattended access installed again with the
/// new server (one administrator prompt), so that the sign-in screen is reached on
/// the same server as the desktop. A note for the person when it did not happen.
#[cfg(unix)]
fn move_signin(server: &str) -> Option<String> {
    match nuvira_service::system::run_as_admin(false, &t("install.signin_server_reason"), server, None) {
        Ok(true) => {
            info!("sign-in screen moved to {}", nuvira_transport::signal::server_or_public(server));
            remember_signin_server(server);
            None
        }
        Ok(false) => Some(t("install.signin_server_cancelled")),
        Err(e) => {
            warn!("sign-in screen not moved to the new server: {e:#}");
            Some(tf("install.signin_server_failed", &[("e", &format!("{e:#}"))]))
        }
    }
}

/// The server the sign-in screen's agent was given last (see `set_server`).
#[cfg(unix)]
fn remember_signin_server(server: &str) {
    let mut settings = load_settings();
    settings.signin_server = Some(nuvira_transport::signal::server_or_public(server).to_string());
    if let Err(e) = save_settings(&settings) {
        warn!("settings not saved: {e:#}");
    }
}

/// The person at this PC mutes the technician's microphone, or not (this session).
#[tauri::command]
fn mute_microphone(app: AppHandle, state: State<NuviraApp>, muted: bool) {
    state.send(AgentCommand::MuteMicrophone { muted });
    state.update(&app, |ui| {
        if let Some(session) = ui.session.as_mut() {
            session.microphone_muted = muted;
        }
    });
}

/// What the person at this PC allows, for the agent (the app's or the service's).
fn send_permissions(commands: &mpsc::UnboundedSender<AgentCommand>) {
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

#[tauri::command]
fn respond(state: State<NuviraApp>, session_id: String, accept: bool) {
    state.send(AgentCommand::Respond { session_id, accept });
}

#[tauri::command]
fn send_chat(app: AppHandle, state: State<NuviraApp>, text: String) {
    let text = text.trim().to_string();
    if text.is_empty() || state.ui.lock().expect("state").session.is_none() {
        return;
    }
    state.send(AgentCommand::Chat { text: text.clone() });
    state.update(&app, |ui| ui.chat.push(ChatLine { mine: true, system: false, text }));
}

#[tauri::command]
fn end_session(state: State<NuviraApp>) {
    state.send(AgentCommand::EndSession);
}

/// Switches something off (or back on) for the session in progress, from the strip.
#[tauri::command]
fn switch_permission(state: State<NuviraApp>, name: String, on: bool) {
    info!(name, on, "switched from the strip");
    state.send(AgentCommand::Switch { name, on });
}

/// Asks the server's team for support (support queue, NuviraHub Pro).
#[tauri::command]
fn request_support(
    app: AppHandle,
    state: State<NuviraApp>,
    message: String,
    invite: Option<String>,
) -> Result<(), String> {
    // An invitation's code typed by the person (docs/47-invitations.md).
    let invite = invite.map(|c| invite::normalize(&c)).unwrap_or_default();
    if !invite.is_empty() && !invite::valid_code(&invite) {
        return Err(t("support.invite_code_bad"));
    }
    let commands = state.commands.lock().expect("commands").clone();
    let commands = commands.ok_or_else(|| t("support.not_online"))?;
    commands.send(AgentCommand::RequestSupport { message, invite }).map_err(|_| t("support.not_online"))?;
    state.update(&app, |ui| {
        ui.support = Some(SupportUi::default());
        ui.support_error = None;
    });
    Ok(())
}

/// The technician's attention for a request made for them (the taskbar flashes,
/// the dock bounces), without taking the focus.
#[tauri::command]
fn request_attention(app: AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.request_user_attention(Some(UserAttentionType::Informational));
    }
}

#[tauri::command]
fn cancel_support(app: AppHandle, state: State<NuviraApp>) {
    if let Some(commands) = state.commands.lock().expect("commands").as_ref() {
        let _ = commands.send(AgentCommand::CancelSupport);
    }
    state.update(&app, |ui| ui.support = None);
}

#[tauri::command]
fn open_source(app: AppHandle) -> Result<(), String> {
    app.opener().open_url(SOURCE_URL, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_terms(app: AppHandle) -> Result<(), String> {
    let url = TERMS_URL.replace("{lang}", nuvira_i18n::lang());
    app.opener().open_url(&url, None::<&str>).map_err(|e| e.to_string())
}

/// Windows: the window is a WebView2. Without the runtime Tauri stops with WebView2's
/// own error, which does not say what to install: ask, install it (for this person,
/// no administrator needed; see nuvira_service::webview2) and go on, or explain.
#[cfg(windows)]
fn ensure_webview2() {
    use nuvira_service::webview2;
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_YESNO, MESSAGEBOX_STYLE, MessageBoxW,
    };
    use windows::core::HSTRING;

    if webview2::installed().is_some() {
        return;
    }
    let ask = |text: String, style: MESSAGEBOX_STYLE| {
        // SAFETY: modal message box without an owner window.
        unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("NuviraHub"), style) }
    };
    tracing::warn!("Microsoft Edge WebView2 Runtime missing");
    if ask(nuvira_i18n::t("app.webview2_missing"), MB_YESNO | MB_ICONQUESTION) != IDYES {
        std::process::exit(1);
    }
    if let Err(e) = webview2::ensure(&|line| tracing::info!("{line}")) {
        tracing::error!("WebView2 Runtime: {e:#}");
        let text = nuvira_i18n::tf(
            "app.webview2_failed",
            &[("e", &format!("{e:#}")), ("url", webview2::DOWNLOAD_PAGE)],
        );
        ask(text, MB_ICONERROR);
        std::process::exit(1);
    }
}

fn init_logging() {
    let dir = nuvira_transport::identity::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("nuvira-app.log"));
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn,zbus=error".into());
    match file {
        Ok(file) => tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .with_env_filter(filter)
            .init(),
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

/// The other programs contained in the app, started as `nuvira.exe <name> ...`:
/// one file to download, copy and sign instead of four.
const SUBCOMMANDS: [&str; 4] = ["agent", "viewer", "service", "access"];

/// Switch sides, at the technician's: this computer's screen served by the agent's
/// code on the endpoint the viewer opened inside the session (screen, keyboard
/// and mouse only: `nuvira_agent::serve_reverse`).
fn reverse_host(
    endpoint: nuvira_transport::Endpoint,
    password: String,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>> {
    Box::pin(async move {
        let mut config = AgentConfig {
            display: None,
            synthetic: None,
            bitrate_kbps: 8000,
            fps: 30,
            plan: EncoderPlan::Software,
            fixed_bitrate: false,
            view_only: false,
        };
        let settings = config.encoder_settings();
        config.plan = tokio::task::spawn_blocking(move || {
            nuvira_codec::plan_encoder(nuvira_codec::EncoderChoice::Auto, settings)
        })
        .await
        .unwrap_or(EncoderPlan::Software);
        nuvira_agent::serve_reverse(endpoint, config, nuvira_agent::Reverse { password }).await
    })
}

fn main() {
    // This program is both: the agent can open a viewer (switch sides), the viewer
    // can serve this computer's screen.
    nuvira_agent::reverse::set_viewer_available();
    let _ = nuvira_viewer::REVERSE_HOST.set(reverse_host);
    // macOS: the black screens of privacy mode are this program started again.
    nuvira_input::privacy::overlay_if_asked();
    // macOS: the wallpaper's helper (see nuvira_capture::wallpaper).
    nuvira_capture::wallpaper::helper_if_asked();
    // Before anything can replace the file (an update).
    started_as();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    #[cfg(target_os = "macos")]
    mac_keyboard_from_policy(args.get(1).and_then(|a| a.to_str()));
    if let Some(sub) = args.get(1).and_then(|a| a.to_str()).filter(|a| SUBCOMMANDS.contains(a)) {
        // macOS: a person's agent takes the links Launch Services sends it (see
        // mac_links); not root's (no window server to talk to at the sign-in screen).
        #[cfg(target_os = "macos")]
        if sub == "agent" && unsafe { libc::getuid() } != 0 {
            let rest = args[2..].to_vec();
            mac_links::agent_with_links(move || run_subcommand("agent", &rest));
        }
        std::process::exit(run_subcommand(sub, &args[2..]));
    }
    // As the subcommands: the version, without opening the window (a script asking
    // for it would otherwise wait for the app to be closed).
    if matches!(args.get(1).and_then(|a| a.to_str()), Some("--version" | "-V")) {
        println!("nuvira {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    init_logging();
    wait_for_predecessor();
    nuvira_input::set_dpi_awareness();
    let mut settings = load_settings();
    apply_file_config(&mut settings);
    apply_branding(&mut settings);
    apply_policy(&mut settings);
    nuvira_i18n::init(settings.language.as_deref());
    #[cfg(windows)]
    ensure_webview2();
    let password = nuvira_agent::random_password();
    let recordings = record_dir(&settings).display().to_string();
    let state = NuviraApp {
        ui: Mutex::new(UiState {
            server: settings.server,
            status: if controllable() { "connecting" } else { "viewer_only" }.into(),
            controllable: controllable(),
            unattended: UNATTENDED,
            platform: std::env::consts::OS.into(),
            password: password.clone(),
            privacy_allowed: !settings.no_privacy,
            terminal_allowed: !settings.no_terminal,
            tunnel_allowed: !settings.no_tunnel,
            print_allowed: !settings.no_print,
            recording_allowed: !settings.no_recording,
            lock_on_end: settings.lock_on_end,
            idle_minutes: settings.idle_minutes,
            record_sessions: settings.record_sessions,
            hide_wallpaper: settings.hide_wallpaper,
            record_dir: recordings,
            server_managed: policy_server().is_some(),
            service_managed: service_managed(),
            password_managed: password_managed(),
            ..Default::default()
        }),
        password: Arc::new(Mutex::new(password)),
        commands: Mutex::new(None),
        agent: Mutex::new(None),
        plan: Mutex::new(None),
        address_book: Default::default(),
        service_server: Mutex::new(None),
    };

    let started_invite = std::env::current_exe().ok().and_then(|p| file_invite(&p.to_string_lossy()));
    tauri::Builder::default()
        // One app at a time: another start (a nuvirahub:// link opened in the
        // browser) hands its link to this one and closes. First of the plugins.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            bring_to_front(app);
            // The program of an invitation's page started while this one runs.
            if let Some((code, server)) = argv.first().and_then(|exe| file_invite(exe)) {
                invite::receive(app, code, server);
            }
            // macOS: a link in the arguments (the agent starting the app with it);
            // Windows and Linux get it through the deep-link plugin.
            #[cfg(target_os = "macos")]
            for url in argv.iter().skip(1).filter(|a| a.starts_with("nuvirahub:")) {
                invite::open_url(app, url);
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        // The window comes back with the size, position and maximized state it
        // had when the app last closed (a position off every connected screen is
        // not restored). Not its visibility: how the app starts decides that.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .build(),
        )
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_state,
            settings_ui::key_protection,
            set_server,
            settings_ui::new_password,
            settings_ui::set_privacy_allowed,
            settings_ui::set_recording_allowed,
            settings_ui::set_lock_on_end,
            settings_ui::set_idle_minutes,
            mute_microphone,
            settings_ui::set_alias,
            access_ui::access_totp_new,
            access_ui::access_totp_check,
            access_ui::access_save,
            settings_ui::set_record_sessions,
            settings_ui::set_hide_wallpaper,
            settings_ui::choose_record_dir,
            settings_ui::open_record_dir,
            settings_ui::set_terminal_allowed,
            settings_ui::set_tunnel_allowed,
            settings_ui::set_print_allowed,
            settings_ui::print_ask_again,
            respond,
            send_chat,
            outgoing::connect_entry,
            install_service,
            repair_service,
            request_support,
            cancel_support,
            request_attention,
            invite::accept_invite,
            invite::dismiss_invite,
            address_book::invite_create,
            address_book::invites_list,
            address_book::invite_delete,
            outgoing::session_note,
            address_book::support_queue,
            address_book::device_inventory,
            address_book::wake_device,
            address_book::support_take,
            uninstall_service,
            app_update::apply_update,
            app_update::restart_app,
            address_book::address_book_login,
            address_book::address_book_logout,
            address_book::address_book_refresh,
            address_book::address_book_save_entry,
            address_book::address_book_import,
            address_book::address_book_export,
            address_book::address_book_delete_entry,
            address_book::address_book_change_password,
            address_book::address_book_accounts,
            address_book::address_book_save_account,
            address_book::address_book_delete_account,
            address_book::address_book_save_book,
            address_book::address_book_delete_book,
            address_book::address_book_sso_login,
            address_book::address_book_two_factor_setup,
            address_book::address_book_two_factor_enable,
            address_book::address_book_two_factor_disable,
            address_book::address_book_two_factor_reset,
            address_book::address_book_policy,
            address_book::address_book_save_policy,
            address_book::address_book_anonymous_limits,
            address_book::address_book_save_anonymous_limits,
            end_session,
            switch_permission,
            outgoing::connect,
            open_source,
            open_terms,
            get_branding,
            open_support,
            settings_ui::i18n,
            settings_ui::set_language
        ])
        .setup(move |app| {
            if let Some(b) = branding::get().filter(|b| !b.name.is_empty())
                && let Some(window) = app.get_webview_window("main")
            {
                let _ = window.set_title(&b.name);
            }
            start_update_checks(app.handle());
            start_agent(app.handle());
            start_links(app.handle(), started_invite.clone());
            address_book::restore(app.handle());
            address_book::start_presence(app.handle());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("starting the application");
    // Leaves time for the agent's connections to close.
    std::thread::sleep(Duration::from_millis(100));
}

/// nuvirahub:// links (invitations): this program is the scheme's handler (Windows:
/// in the user's registry; Linux: a hidden .desktop file; macOS: the app's
/// Info.plist), the link it was started with, the ones handed by later starts, and
/// the invitation in the file name.
fn start_links(app: &AppHandle, file_invite: Option<(String, String)>) {
    use tauri_plugin_deep_link::DeepLinkExt;
    #[cfg(windows)]
    if let Err(e) = app.deep_link().register_all() {
        tracing::warn!("nuvirahub:// links not registered: {e}");
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = linux_links::register() {
        tracing::warn!("nuvirahub:// links not registered: {e}");
    }
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        for url in urls {
            invite::open_url(app, url.as_str());
        }
    }
    #[cfg(target_os = "macos")]
    for url in std::env::args().skip(1).filter(|a| a.starts_with("nuvirahub:")) {
        invite::open_url(app, &url);
    }
    let handle = app.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            invite::open_url(&handle, url.as_str());
        }
    });
    if let Some((code, server)) = file_invite {
        invite::receive(app, code, server);
    }
}

/// The app has no console of its own: from a command prompt its output must go to
/// that prompt's console. Windows does not hand a windowed program the console's
/// handles, even after attaching, so they are opened here. Output already redirected
/// (the app reading its viewer, the service's agent log) is left alone.
#[cfg(windows)]
fn attach_console() {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::core::w;

    let redirected = |which| unsafe { GetStdHandle(which) }.is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
    if redirected(STD_OUTPUT_HANDLE) || unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
        return;
    }
    let Ok(console) = (unsafe {
        CreateFileW(
            w!("CONOUT$"),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
    }) else {
        return;
    };
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if !redirected(which) {
            let _ = unsafe { SetStdHandle(which, console) };
        }
    }
}

/// Installs NuviraHub on this PC for unattended access: `nuvira service install`
/// started as administrator (UAC prompt). The password travels in a temporary file,
/// not on the command line, and the outcome comes back the same way. Returns the
/// message to show, with the permanent password if it was generated.
///
/// With `change` it is already installed and only the permanent password changes:
/// installed again with the same server, so the ID stays (on Linux and macOS the
/// system's part takes the new password's hash too).
#[tauri::command]
async fn install_service(app: AppHandle, password: String, change: Option<bool>) -> Result<String, String> {
    let change = change.unwrap_or(false);
    if change && password.chars().count() < 8 {
        return Err(t("settings.install_pw_short"));
    }
    if change && password_managed() {
        return Err(t("settings.pw_managed"));
    }
    // Quick support runs and goes: it does not install.
    if branding::get().is_some_and(branding::Branding::quick) {
        return Err(t("err.locked"));
    }
    if service_managed() {
        return Err(t("install.managed"));
    }
    #[cfg(unix)]
    {
        let settings = load_settings();
        let result = tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
            // On Wayland the screen needs the person's permission: asked now, while
            // they are here, and kept for the service.
            nuvira_service::user::ask_screen_permission().map_err(|e| nuvira_i18n::message(&format!("{e:#}")))?;
            nuvira_service::user::ask_folder_access();
            let opts = nuvira_service::user::InstallOptions {
                server: settings.server,
                server_key: settings.server_key,
                password: (!password.is_empty()).then_some(password),
            };
            let generated = nuvira_service::user::install(&opts).map_err(|e| format!("{e:#}"))?;
            Ok(match generated {
                Some(generated) => tf("install.done_password", &[("password", &generated)]),
                None if change => t("settings.pw_changed"),
                None => t("install.done"),
            })
        })
        .await
        .map_err(|e| e.to_string())?;
        // macOS opened System Settings at Screen Recording but cannot bring it in
        // front of this window (apps no longer activate others): out of the way.
        #[cfg(target_os = "macos")]
        if result.as_ref().is_err_and(|e| *e == t("install.screen_permission_mac")) {
            let _ = app.hide();
        }
        // The service's agent is running with the same ID: the app's own stops now,
        // not after the printer's administrator prompt (meanwhile the two would take
        // the registration on the server from each other).
        if result.is_ok() {
            start_agent(&app);
        }
        let done = result?;
        let settings = load_settings();
        // Changing the password: the sign-in screen keeps a copy of the old one until
        // the system's part is brought up to date.
        let signin_before = change && nuvira_service::system::installed();
        let note = tauri::async_runtime::spawn_blocking(move || install_system(&settings.server, settings.server_key.as_deref()))
            .await
            .ok()
            .flatten();
        // Not brought up to date (no administrator password, or an error): said plainly.
        let hid_note = t("install.hid_allow_mac");
        let note = note.map(|n| if signin_before && n != hid_note { t("settings.pw_signin_old") } else { n });
        Ok(match note {
            Some(note) => format!("{done} {note}"),
            None => done,
        })
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (app, password, change);
        Err(t("install.windows_only"))
    }
    #[cfg(windows)]
    {
        let settings = load_settings();
        let result = tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
            let tag = nuvira_agent::random_password();
            let password_file = std::env::temp_dir().join(format!("nuvira-{tag}.pw"));
            let mut params = format!("service install --server \"{}\"", settings.server);
            if let Some(key) = &settings.server_key {
                params += &format!(" --server-key {key}");
            }
            if !password.is_empty() {
                std::fs::write(&password_file, &password).map_err(|e| e.to_string())?;
                params += &format!(" --password-file \"{}\"", password_file.display());
            }
            let report = run_service_command(params);
            let _ = std::fs::remove_file(&password_file);
            let report = report?;
            // Present only if the installation generated it.
            Ok(match report["password"].as_str() {
                Some(generated) => tf("install.done_password", &[("password", generated)]),
                None if change => t("settings.pw_changed"),
                None => t("install.done"),
            })
        })
        .await
        .map_err(|e| e.to_string())?;
        // Installed: from now on the ID and passwords are the service's.
        if result.is_ok() {
            start_agent(&app);
        }
        result
    }
}

/// The system's part of unattended access, on Linux and macOS: the NuviraHub printer
/// (remote printing) and, on macOS, the sign-in screen's agent. They need root, so
/// the system asks for the administrator password once; without it the rest works
/// the same. Returns a note for the person when it was not installed.
#[cfg(unix)]
fn install_system(server: &str, server_key: Option<&str>) -> Option<String> {
    use nuvira_service::system;
    if !system::wanted() {
        return None;
    }
    match system::run_as_admin(false, &t("install.printer_reason"), server, server_key) {
        Ok(true) => {
            info!("system part of unattended access installed");
            remember_signin_server(server);
            // macOS: the virtual keyboard for the lock screen is a driver extension,
            // which the person allows once in System Settings (the page is opened).
            #[cfg(target_os = "macos")]
            match nuvira_service::hid::activate() {
                Ok(true) => {}
                Ok(false) => return Some(t("install.hid_allow_mac")),
                Err(e) => warn!("lock screen keyboard not activated: {e:#}"),
            }
            None
        }
        Ok(false) => Some(t("install.no_printer_cancelled")),
        Err(e) => {
            warn!("NuviraHub printer not installed: {e:#}");
            Some(tf("install.no_printer", &[("e", &format!("{e:#}"))]))
        }
    }
}

/// Starts the stopped service again (UAC prompt), first installing this program if the
/// installed one is older: see `nuvira service repair`. The link with the service picks
/// it up by itself.
#[tauri::command]
async fn repair_service() -> Result<(), String> {
    #[cfg(unix)]
    {
        tauri::async_runtime::spawn_blocking(nuvira_service::user::start)
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{e:#}"))
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err(t("install.windows_only"))
    }
    #[cfg(windows)]
    {
        info!("starting the stopped service again");
        tauri::async_runtime::spawn_blocking(|| run_service_command("service repair".into()))
            .await
            .map_err(|e| e.to_string())?
            .map(|_| ())
            .map_err(|e| {
                if e == t("install.cancelled") {
                    t("service.cancelled")
                } else {
                    tf("service.failed", &[("e", &e)])
                }
            })
    }
}

/// Removes the user service (Linux and macOS; on Windows NuviraHub is removed
/// from the installed apps of the system settings). The app runs its own agent again.
#[tauri::command]
async fn uninstall_service(app: AppHandle) -> Result<String, String> {
    if service_managed() {
        return Err(t("install.managed"));
    }
    #[cfg(unix)]
    {
        tauri::async_runtime::spawn_blocking(|| {
            nuvira_service::user::uninstall()?;
            // Installed with unattended access, removed with it (the .deb and .rpm
            // packages keep their printer until they are removed).
            use nuvira_service::system;
            if system::installed() {
                #[cfg(target_os = "macos")]
                nuvira_service::hid::deactivate();
                match system::run_as_admin(true, &t("install.printer_remove_reason"), "", None) {
                    Ok(true) => info!("system part of unattended access removed"),
                    Ok(false) => info!("system part kept: administrator prompt declined"),
                    Err(e) => warn!("system part not removed: {e:#}"),
                }
            }
            anyhow::Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
        app.state::<NuviraApp>().update(&app, |ui| {
            ui.service = false;
            ui.service_stopped = false;
        });
        start_agent(&app);
        Ok(t("install.removed"))
    }
    #[cfg(not(unix))]
    {
        let _ = app;
        Err(t("install.windows_only"))
    }
}

/// Runs `nuvira service ...` with `params` as administrator (UAC prompt) and returns
/// its report; the outcome comes back in a temporary file.
#[cfg(windows)]
fn run_service_command(mut params: String) -> Result<serde_json::Value, String> {
    let tag = nuvira_agent::random_password();
    let report = std::env::temp_dir().join(format!("nuvira-{tag}.json"));
    params += &format!(" --report \"{}\"", report.display());
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let outcome = nuvira_service::run_elevated(&exe, &params);
    let text = std::fs::read_to_string(&report);
    let _ = std::fs::remove_file(&report);
    match outcome {
        Ok(None) => return Err(t("install.cancelled")),
        Err(e) => return Err(format!("{e:#}")),
        Ok(Some(_)) => {}
    }
    let report: serde_json::Value =
        text.ok().and_then(|t| serde_json::from_str(&t).ok()).ok_or_else(|| t("install.no_report"))?;
    if report["ok"] != true {
        return Err(report["error"].as_str().unwrap_or_default().to_string());
    }
    Ok(report)
}

/// macOS: the kind of keyboard the organisation set for the whole Mac (the
/// `Keyboard` key of the managed preferences, module `policy`) wins over
/// `NUVIRA_MAC_KEYBOARD`, for every agent: the app's own, the person's
/// installation (their LaunchAgent), the managed one and the sign-in screen's.
/// Read once at start: a change counts from the agent's next start.
#[cfg(target_os = "macos")]
fn mac_keyboard_from_policy(sub: Option<&str>) {
    if matches!(sub, Some("viewer" | "service")) {
        return;
    }
    if let Some(keyboard) = nuvira_service::policy::read().keyboard {
        // SAFETY: at the very start of main, before any other thread exists.
        unsafe { std::env::set_var("NUVIRA_MAC_KEYBOARD", keyboard) };
    }
}

fn run_subcommand(sub: &str, rest: &[std::ffi::OsString]) -> i32 {
    #[cfg(windows)]
    attach_console();
    let args = std::iter::once(format!("nuvira {sub}").into()).chain(rest.iter().cloned());
    let result = match sub {
        "agent" => nuvira_agent::cli::run(args),
        "viewer" => nuvira_viewer::run(args),
        "access" => access_ui::run(rest),
        _ => nuvira_service::run(args),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    }
}
