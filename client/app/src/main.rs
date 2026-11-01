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

mod address_book;
mod branding;
mod local_book;

use std::path::PathBuf;
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

const SOURCE_URL: &str = "https://github.com/nuvira-hub/nuvira";
/// Terms of use of the public server, in the interface language: `{lang}` is replaced.
const TERMS_URL: &str = "https://nuvirahub.dev/{lang}/terms/";

/// State shown by the interface (sent with the "state" event).
#[derive(Debug, Default, Clone, Serialize)]
struct UiState {
    server: String,
    /// "connecting", "online", "offline".
    status: String,
    device_id: Option<String>,
    websocket: bool,
    /// ID and password are the service's (unattended access).
    service: bool,
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
    books: Vec<address_book::BookUi>,
    address_book_error: Option<String>,
    /// Optional modules of the server (NuviraHub Pro): the interface shows their
    /// functions only when listed.
    features: Vec<String>,
    /// Newer version offered by the server (app not installed: the service updates
    /// an installed one by itself).
    update: Option<String>,
    /// Newer version installed by the service while this app was open: restart it.
    installed: Option<String>,
    /// The service is installed but stopped: this PC is not reachable.
    service_stopped: bool,
    /// Version of the stopped service's program, when older than this app (starting
    /// it again also updates it).
    service_old: Option<String>,
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
struct IncomingUi {
    session_id: String,
    viewer_id: String,
    viewer_name: String,
    /// Request deadline (ms since 1970): the interface shows the countdown.
    deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
struct SessionUi {
    viewer_id: String,
    viewer_name: String,
    direct: bool,
}

/// Settings saved in `app.json` in the data folder.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Settings {
    server: String,
    /// Remembered address book sign-in (server token, not the password).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    /// Interface language ("auto" or a code from `nuvira_i18n::LANGUAGES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    /// Server certificate fingerprint, when it came with the server in the file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server_key: Option<String>,
}

/// Server written in the program's file name, e.g. `nuvira-host=nuvira.example.com.exe`
/// or `nuvira-host=example.com,port=21117,key=<fingerprint>.exe`: whoever hosts a
/// server hands out the same program, renamed, and it connects there with no setup.
/// Windows does not allow `:` in file names, hence `port=`. A copy saved twice by a
/// browser (`... (1).exe`) still works.
#[derive(Debug, Default, PartialEq)]
struct FileConfig {
    server: Option<String>,
    server_key: Option<String>,
}

fn file_config(file_name: &str) -> FileConfig {
    let stem = file_name.strip_suffix(".exe").or_else(|| file_name.strip_suffix(".EXE")).unwrap_or(file_name);
    // " (1)" added by browsers to a repeated download.
    let stem = match stem.rsplit_once(" (") {
        Some((head, tail))
            if tail.strip_suffix(')').is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())) =>
        {
            head
        }
        _ => stem,
    };
    let Some((_, options)) = stem.split_once('-') else { return FileConfig::default() };
    let (mut host, mut port, mut key) = (None, None, None);
    for option in options.split(',') {
        match option.trim().split_once('=') {
            Some(("host", v)) if !v.is_empty() => host = Some(v.to_string()),
            Some(("port", v)) if v.parse::<u16>().is_ok() => port = Some(v.to_string()),
            Some(("key", v)) if nuvira_transport::parse_fingerprint(v).is_ok() => key = Some(v.to_string()),
            _ => {}
        }
    }
    let Some(host) = host else { return FileConfig::default() };
    FileConfig {
        server: Some(match port {
            Some(port) => format!("{host}:{port}"),
            None => host,
        }),
        server_key: key,
    }
}

/// Server and language of a customised program (they win over the file name).
fn apply_branding(settings: &mut Settings) {
    let Some(b) = branding::get() else { return };
    let mut changed = false;
    if !b.server.is_empty() && settings.server != b.server {
        info!("server from the customisation: {}", b.server);
        settings.token = None;
        settings.server = b.server.clone();
        settings.server_key = (!b.server_key.is_empty()).then(|| b.server_key.clone());
        changed = true;
    }
    if !b.language.is_empty()
        && (b.lock_language || settings.language.is_none())
        && settings.language.as_deref() != Some(&b.language)
    {
        settings.language = Some(b.language.clone());
        changed = true;
    }
    if changed && let Err(e) = save_settings(settings) {
        tracing::warn!("settings not saved: {e:#}");
    }
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

/// Applies the server in the file name, if any, to the saved settings.
fn apply_file_config(settings: &mut Settings) {
    let name =
        std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let FileConfig { server: Some(server), server_key } = file_config(&name.unwrap_or_default()) else {
        return;
    };
    if settings.server == server && settings.server_key == server_key {
        return;
    }
    info!("server from the file name: {server}");
    if settings.server != server {
        // The address book sign-in applies to the server it was made on.
        settings.token = None;
    }
    settings.server = server;
    settings.server_key = server_key;
    if let Err(e) = save_settings(settings) {
        tracing::warn!("settings not saved: {e:#}");
    }
}

fn settings_path() -> PathBuf {
    nuvira_transport::identity::data_dir().join("app.json")
}

fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        // Editors such as Notepad (and PowerShell 5) may add a UTF-8 byte order mark.
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

fn save_settings(settings: &Settings) -> anyhow::Result<()> {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(settings)?)?;
    Ok(())
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
    let state = app.state::<NuviraApp>();
    if let Some(old) = state.agent.lock().expect("agent").take() {
        old.abort();
    }
    #[cfg(windows)]
    if nuvira_agent::ipc::service_installed() {
        start_service_link(app);
        return;
    }
    let server =
        nuvira_transport::signal::server_or_public(&state.ui.lock().expect("state").server).to_string();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
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
        }
    });
    *state.agent.lock().expect("agent") = Some(handle);
}

/// The NuviraHub service is installed: no agent in the app, the service's one is
/// shown. If its agent does not answer (start-up, user switch) it is retried.
#[cfg(windows)]
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
                            ToApp::Info { server, version } => {
                                *state.service_server.lock().expect("service server") = Some(server.clone());
                                state.update(&app, |ui| {
                                    ui.server = shown_server(&server);
                                    ui.installed = nuvira_update::newer(&version, nuvira_update::CURRENT)
                                        .then_some(version);
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
        AgentEvent::Online { device_id, websocket } => state.update(app, |ui| {
            ui.status = "online".into();
            ui.device_id = Some(device_id);
            ui.websocket = websocket;
            ui.error = None;
        }),
        AgentEvent::Offline { reason } => state.update(app, |ui| {
            ui.status = "offline".into();
            ui.error = Some(reason);
        }),
        AgentEvent::Password { password } => state.update(app, |ui| ui.password = password),
        AgentEvent::Incoming { session_id, viewer_id, viewer_name } => {
            state.update(app, |ui| {
                ui.incoming = Some(IncomingUi {
                    session_id,
                    viewer_id,
                    viewer_name,
                    deadline_ms: now_ms() + nuvira_agent::CONFIRM_TIMEOUT.as_millis() as u64,
                })
            });
            bring_to_front(app);
        }
        AgentEvent::IncomingClosed { session_id } => state.update(app, |ui| {
            if ui.incoming.as_ref().is_some_and(|i| i.session_id == session_id) {
                ui.incoming = None;
            }
        }),
        AgentEvent::SessionStarted { viewer_id, viewer_name, direct } => state.update(app, |ui| {
            ui.session = Some(SessionUi { viewer_id, viewer_name, direct });
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
    state.ui.lock().expect("state").clone()
}

/// Server as shown in the settings: empty for the public one.
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
async fn set_server(app: AppHandle, server: String) -> Result<(), String> {
    if branding::get().is_some_and(|b| b.lock_server) {
        return Err(t("err.locked"));
    }
    let server = server.trim().to_string();
    if !server.is_empty() {
        nuvira_transport::resolve_addr(&server, nuvira_transport::signal::DEFAULT_PORT)
            .map_err(|e| tf("err.invalid_address", &[("e", &e.to_string())]))?;
    }
    #[cfg(windows)]
    if nuvira_agent::ipc::service_installed() {
        let current = app.state::<NuviraApp>().service_server.lock().expect("service server").clone();
        let wanted = nuvira_transport::signal::server_or_public(&server);
        if current.as_deref().is_none_or(|s| !s.eq_ignore_ascii_case(wanted)) {
            let params = format!("service install --server \"{server}\"");
            tauri::async_runtime::spawn_blocking(move || run_service_command(params))
                .await
                .map_err(|e| e.to_string())??;
            info!("service moved to {wanted}");
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
    Ok(())
}

/// Generated by the agent (the app's or the service's), which answers with the Password event.
#[tauri::command]
fn new_password(state: State<NuviraApp>) {
    state.send(AgentCommand::NewPassword);
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

/// Opens the session window towards another device (`nuvira-viewer` process).
#[tauri::command]
fn connect(
    app: AppHandle,
    state: State<NuviraApp>,
    device_id: String,
    password: String,
) -> Result<(), String> {
    launch_viewer(&app, &state, device_id, password)
}

/// Connection from the address book, with the password saved in the entry.
#[tauri::command]
fn connect_entry(app: AppHandle, state: State<NuviraApp>, id: i64) -> Result<(), String> {
    let entry = state
        .ui
        .lock()
        .expect("state")
        .books
        .iter()
        .flat_map(|b| &b.entries)
        .find(|e| e.id == id)
        .cloned()
        .ok_or_else(|| t("err.entry_not_found"))?;
    launch_viewer(&app, &state, entry.device_id, entry.password)
}

fn launch_viewer(
    app: &AppHandle,
    state: &NuviraApp,
    device_id: String,
    password: String,
) -> Result<(), String> {
    let device_id: String = device_id.chars().filter(|c| c.is_ascii_digit()).collect();
    if device_id.len() != 9 {
        return Err(t("err.id_digits"));
    }
    let server =
        nuvira_transport::signal::server_or_public(&state.ui.lock().expect("state").server).to_string();
    // The viewer is in this same program (`nuvira viewer ...`).
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // The password goes in the process environment, not on the command line
    // (which other programs on the same PC can read).
    let mut command = std::process::Command::new(&exe);
    command.args(["viewer", "--server", &server, &device_id]);
    if let Some(key) = load_settings().server_key {
        command.args(["--server-key", &key]);
    }
    let mut child = command
        .env("NUVIRA_PASSWORD", &password)
        // The session window uses the app's language.
        .env("NUVIRA_LANG", nuvira_i18n::lang())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("starting the viewer: {e}"))?;
    info!("session towards {device_id} started");
    state.update(app, |ui| ui.viewer_message = None);

    // The viewer reports on its standard output: session started (time for the
    // address book's automatic add) and, at the end, why it closed.
    let closed_reason = std::sync::Arc::new(Mutex::new(None::<String>));
    let reader = child.stdout.take().map(|stdout| {
        let (app, closed_reason) = (app.clone(), closed_reason.clone());
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if line == "@pending" {
                    app.state::<NuviraApp>()
                        .update(&app, |ui| ui.viewer_message = Some("@app.waiting_confirmation".into()));
                } else if let Some(name) = line.strip_prefix("@connected\t") {
                    app.state::<NuviraApp>().update(&app, |ui| ui.viewer_message = None);
                    address_book::auto_add(&app, device_id.clone(), name.to_string(), password.clone());
                } else if let Some(reason) = line.strip_prefix("@closed\t") {
                    // Already translated by the viewer.
                    *closed_reason.lock().expect("reason") = Some(reason.to_string());
                }
            }
        })
    });
    let app = app.clone();

    // When the viewer exits with an error, the reason reaches the interface.
    std::thread::spawn(move || {
        let mut stderr = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut err, &mut stderr);
        }
        let status = child.wait();
        if let Some(reader) = reader {
            let _ = reader.join();
        }
        let failed = !matches!(status, Ok(s) if s.success());
        let reason = closed_reason.lock().expect("reason").take();
        let message = match (failed, reason) {
            (_, Some(r)) => Some(r),
            (true, None) => {
                info!("viewer error output: {}", stderr.trim());
                Some(t("err.session_error"))
            }
            _ => None,
        };
        let state = app.state::<NuviraApp>();
        state.update(&app, |ui| ui.viewer_message = message);
    });
    Ok(())
}

/// Strings for the interface in the current language.
#[derive(Serialize)]
struct I18nUi {
    lang: &'static str,
    /// "auto" or the chosen code.
    preference: String,
    languages: Vec<(&'static str, &'static str)>,
    strings: serde_json::Value,
}

fn i18n_ui() -> I18nUi {
    I18nUi {
        lang: nuvira_i18n::lang(),
        preference: load_settings().language.unwrap_or_else(|| "auto".into()),
        languages: nuvira_i18n::LANGUAGES.to_vec(),
        strings: serde_json::from_str(&nuvira_i18n::json()).unwrap_or_default(),
    }
}

#[tauri::command]
fn i18n() -> I18nUi {
    i18n_ui()
}

#[tauri::command]
fn set_language(language: String) -> Result<I18nUi, String> {
    if branding::get().is_some_and(|b| b.lock_language) {
        return Err(t("err.locked"));
    }
    let mut settings = load_settings();
    settings.language = (language != "auto").then_some(language);
    save_settings(&settings).map_err(|e| e.to_string())?;
    nuvira_i18n::init(settings.language.as_deref());
    Ok(i18n_ui())
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

fn init_logging() {
    let dir = nuvira_transport::identity::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("nuvira-app.log"));
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn".into());
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
const SUBCOMMANDS: [&str; 3] = ["agent", "viewer", "service"];

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(sub) = args.get(1).and_then(|a| a.to_str()).filter(|a| SUBCOMMANDS.contains(a)) {
        std::process::exit(run_subcommand(sub, &args[2..]));
    }
    init_logging();
    nuvira_input::set_dpi_awareness();
    let mut settings = load_settings();
    apply_file_config(&mut settings);
    apply_branding(&mut settings);
    nuvira_i18n::init(settings.language.as_deref());
    let password = nuvira_agent::random_password();
    let state = NuviraApp {
        ui: Mutex::new(UiState {
            server: settings.server,
            status: "connecting".into(),
            password: password.clone(),
            ..Default::default()
        }),
        password: Arc::new(Mutex::new(password)),
        commands: Mutex::new(None),
        agent: Mutex::new(None),
        plan: Mutex::new(None),
        address_book: Default::default(),
        service_server: Mutex::new(None),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_server,
            new_password,
            respond,
            send_chat,
            connect_entry,
            install_service,
            repair_service,
            apply_update,
            restart_app,
            address_book::address_book_login,
            address_book::address_book_logout,
            address_book::address_book_refresh,
            address_book::address_book_save_entry,
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
            end_session,
            connect,
            open_source,
            open_terms,
            get_branding,
            open_support,
            i18n,
            set_language
        ])
        .setup(|app| {
            if let Some(b) = branding::get().filter(|b| !b.name.is_empty())
                && let Some(window) = app.get_webview_window("main")
            {
                let _ = window.set_title(&b.name);
            }
            start_update_checks(app.handle());
            start_agent(app.handle());
            address_book::restore(app.handle());
            address_book::start_presence(app.handle());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("starting the application");
    // Leaves time for the agent's connections to close.
    std::thread::sleep(Duration::from_millis(100));
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
#[tauri::command]
async fn install_service(app: AppHandle, password: String) -> Result<String, String> {
    // Quick support runs and goes: it does not install.
    if branding::get().is_some_and(branding::Branding::quick) {
        return Err(t("err.locked"));
    }
    #[cfg(not(windows))]
    {
        let _ = (app, password);
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

/// Starts the stopped service again (UAC prompt), first installing this program if the
/// installed one is older: see `nuvira service repair`. The link with the service picks
/// it up by itself.
#[tauri::command]
async fn repair_service() -> Result<(), String> {
    #[cfg(not(windows))]
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

/// The app's own copy before the last update (see `apply_update`).
fn previous_copy() -> Option<PathBuf> {
    std::env::current_exe().ok().map(|exe| exe.with_extension("old.exe"))
}

/// Without the service (which updates an installed NuviraHub by itself) the app asks
/// its server for a newer version at start-up and every 6 hours, and offers it.
fn start_update_checks(app: &AppHandle) {
    if let Some(old) = previous_copy() {
        let _ = std::fs::remove_file(old);
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            #[cfg(windows)]
            let installed = nuvira_agent::ipc::service_installed();
            #[cfg(not(windows))]
            let installed = false;
            let settings = load_settings();
            if !installed {
                let server = nuvira_transport::signal::server_or_public(&settings.server);
                let base = nuvira_update::base_url(server, None, nuvira_update::DEFAULT_PORT);
                match nuvira_update::check(&base, &nuvira_update::PUBLIC_KEY).await {
                    Ok(found) => {
                        let version = found.map(|m| m.version);
                        app.state::<NuviraApp>().update(&app, |ui| ui.update = version);
                    }
                    Err(e) => tracing::debug!("update check: {e:#}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}

/// Downloads the newer version, checks it (signature, size, SHA-256) and replaces
/// this program, keeping its file name (which may carry the server), then starts it.
/// A running program cannot be overwritten but can be renamed: the old copy is
/// moved aside and deleted at the next start.
#[tauri::command]
async fn apply_update(app: AppHandle) -> Result<(), String> {
    let settings = load_settings();
    let server = nuvira_transport::signal::server_or_public(&settings.server);
    let base = nuvira_update::base_url(server, None, nuvira_update::DEFAULT_PORT);
    let error = |e: anyhow::Error| tf("update.failed", &[("e", &format!("{e:#}"))]);
    let manifest = nuvira_update::check(&base, &nuvira_update::PUBLIC_KEY)
        .await
        .map_err(error)?
        .ok_or_else(|| t("update.none"))?;
    let download = std::env::temp_dir().join(format!("nuvira-{}.exe", manifest.version));
    nuvira_update::download(&base, &manifest, &download).await.map_err(error)?;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let aside = previous_copy().ok_or("program path")?;
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(&exe, &aside).map_err(|e| error(e.into()))?;
    if let Err(e) = std::fs::copy(&download, &exe) {
        let _ = std::fs::rename(&aside, &exe);
        return Err(error(e.into()));
    }
    let _ = std::fs::remove_file(&download);
    info!("updated to {}: restarting", manifest.version);
    std::process::Command::new(&exe).spawn().map_err(|e| error(e.into()))?;
    app.exit(0);
    Ok(())
}

/// Starts the program installed by the service (updated while this app was open)
/// in place of this one.
#[tauri::command]
fn restart_app(app: AppHandle) -> Result<(), String> {
    #[cfg(windows)]
    let program = nuvira_service::installed_program();
    #[cfg(not(windows))]
    let program = std::env::current_exe().map_err(|e| e.to_string())?;
    info!("restarting with the installed program {}", program.display());
    std::process::Command::new(&program)
        .spawn()
        .map_err(|e| tf("update.failed", &[("e", &e.to_string())]))?;
    app.exit(0);
    Ok(())
}

fn run_subcommand(sub: &str, rest: &[std::ffi::OsString]) -> i32 {
    #[cfg(windows)]
    attach_console();
    let args = std::iter::once(format!("nuvira {sub}").into()).chain(rest.iter().cloned());
    let result = match sub {
        "agent" => nuvira_agent::cli::run(args),
        "viewer" => nuvira_viewer::run(args),
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

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "eb4a40cdaac1e2d3948c053723f7a42f9aa6bcbcc7520f228cea427a15070fe3";

    #[test]
    fn server_from_the_file_name() {
        let server = |name: &str| file_config(name).server;
        assert_eq!(server("nuvira.exe"), None);
        assert_eq!(server("nuvira-host=alexbn.ddns.net.exe").as_deref(), Some("alexbn.ddns.net"));
        assert_eq!(server("Nuvira-host=10.0.0.5,port=21117.EXE").as_deref(), Some("10.0.0.5:21117"));
        // Repeated download.
        assert_eq!(server("nuvira-host=alexbn.ddns.net (2).exe").as_deref(), Some("alexbn.ddns.net"));
        // A bad port or key is ignored, the rest is kept.
        let c = file_config(&format!("nuvira-host=a.example,port=99999,key={KEY}.exe"));
        assert_eq!((c.server.as_deref(), c.server_key.as_deref()), (Some("a.example"), Some(KEY)));
        assert_eq!(file_config("nuvira-host=a.example,key=123.exe").server_key, None);
        // Without host nothing applies.
        assert_eq!(file_config(&format!("nuvira-key={KEY}.exe")), FileConfig::default());
    }
}
