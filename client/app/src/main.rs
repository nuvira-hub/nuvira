//! NexoDesk — applicazione desktop.
//!
//! Finestra principale (Tauri) con ID e password di questo dispositivo, stato
//! del collegamento a nexodesk-server, conferma delle richieste in arrivo e avvio
//! delle sessioni verso altri dispositivi. L'agent gira in questo processo
//! (libreria `nexodesk_agent`), oppure, se è installato nexodesk-service, è quello
//! del servizio e l'app gli parla attraverso il canale locale (`nexodesk_agent::ipc`).
//! La finestra della sessione remota è `nexodesk-viewer`, avviato come processo
//! separato (finestra nativa wgpu, decisione D5).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nexodesk_agent::{AgentCommand, AgentConfig, AgentEvent, ServerOptions};
use nexodesk_codec::{EncoderChoice, EncoderPlan};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, UserAttentionType};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::mpsc;
use tracing::{info, warn};

const SOURCE_URL: &str = "https://github.com/alexbn71/NexoDesk";

/// Stato mostrato dall'interfaccia (inviato con l'evento "state").
#[derive(Debug, Default, Clone, Serialize)]
struct UiState {
    server: String,
    /// "setup" (server da impostare), "connecting", "online", "offline".
    status: String,
    device_id: Option<String>,
    websocket: bool,
    /// ID e password sono quelli del servizio (accesso non presidiato).
    service: bool,
    password: String,
    error: Option<String>,
    incoming: Option<IncomingUi>,
    session: Option<SessionUi>,
    /// Messaggio dell'ultima sessione verso un altro dispositivo.
    viewer_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct IncomingUi {
    session_id: String,
    viewer_id: String,
    viewer_name: String,
    /// Scadenza della richiesta (ms dal 1970): l'interfaccia mostra il conto alla rovescia.
    deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
struct SessionUi {
    viewer_id: String,
    viewer_name: String,
    direct: bool,
}

/// Impostazioni salvate in `app.json` nella cartella dati.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Settings {
    server: String,
}

fn settings_path() -> PathBuf {
    nexodesk_transport::identity::data_dir().join("app.json")
}

fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
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

struct NexoApp {
    ui: Mutex<UiState>,
    password: Arc<Mutex<String>>,
    commands: Mutex<Option<mpsc::UnboundedSender<AgentCommand>>>,
    agent: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// Encoder scelto alla prima sessione (la prova dura un paio di secondi).
    plan: Mutex<Option<EncoderPlan>>,
}

impl NexoApp {
    fn update(&self, app: &AppHandle, change: impl FnOnce(&mut UiState)) {
        let state = {
            let mut ui = self.ui.lock().expect("stato");
            change(&mut ui);
            ui.clone()
        };
        let _ = app.emit("state", state);
    }

    fn send(&self, command: AgentCommand) {
        if let Some(tx) = self.commands.lock().expect("comandi").as_ref() {
            let _ = tx.send(command);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Porta la finestra in primo piano (richiesta di collegamento in arrivo).
fn bring_to_front(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        let _ = window.request_user_attention(Some(UserAttentionType::Critical));
    }
}

/// Avvia (o riavvia, se il server cambia) l'agent nel runtime di Tauri, oppure
/// il collegamento con l'agent del servizio se è installato.
fn start_agent(app: &AppHandle) {
    let state = app.state::<NexoApp>();
    if let Some(old) = state.agent.lock().expect("agent").take() {
        old.abort();
    }
    #[cfg(windows)]
    if nexodesk_agent::ipc::service_installed() {
        start_service_link(app);
        return;
    }
    let server = state.ui.lock().expect("stato").server.clone();
    if server.trim().is_empty() {
        state.update(app, |ui| ui.status = "setup".into());
        return;
    }
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    *state.commands.lock().expect("comandi") = Some(commands_tx);
    state.update(app, |ui| {
        ui.status = "connecting".into();
        ui.device_id = None;
        ui.error = None;
    });

    let app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let state = app.state::<NexoApp>();
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
                    nexodesk_codec::plan_encoder(EncoderChoice::Auto, settings)
                })
                .await
                .unwrap_or(EncoderPlan::Software);
                *state.plan.lock().expect("encoder") = Some(plan);
                plan
            }
        };
        let opts = ServerOptions {
            server_name: server,
            server_key: None,
            transport: nexodesk_transport::signal::TransportMode::Auto,
            server_ws: None,
            identity_path: nexodesk_transport::identity::data_dir().join("device.key"),
            confirm: true,
            permanent_password: None,
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
            nexodesk_agent::serve_via_server(config, opts, state.password.clone(), events_tx, commands_rx).await
        {
            warn!("agent terminato: {e:#}");
            state.update(&app, |ui| {
                ui.status = "offline".into();
                ui.error = Some(format!("{e:#}"));
            });
        }
    });
    *state.agent.lock().expect("agent") = Some(handle);
}

/// Il servizio NexoDesk è installato: niente agent nell'app, si mostra quello del
/// servizio. Se il suo agent non risponde (avvio, cambio utente) si riprova.
#[cfg(windows)]
fn start_service_link(app: &AppHandle) {
    use nexodesk_agent::ipc::ToApp;

    let state = app.state::<NexoApp>();
    state.update(app, |ui| {
        ui.service = true;
        ui.status = "connecting".into();
        ui.device_id = None;
    });
    let app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let state = app.state::<NexoApp>();
        loop {
            match nexodesk_agent::ipc::connect() {
                Ok((mut messages, commands)) => {
                    info!("collegato all'agent del servizio");
                    *state.commands.lock().expect("comandi") = Some(commands);
                    state.update(&app, |ui| ui.error = None);
                    while let Some(msg) = messages.recv().await {
                        match msg {
                            // Il server del servizio vale anche per i collegamenti in uscita, se non ne è impostato un altro.
                            ToApp::Info { server } => state.update(&app, |ui| {
                                if ui.server.is_empty() {
                                    ui.server = server;
                                }
                            }),
                            ToApp::Event { event } => on_agent_event(&app, event),
                        }
                    }
                    *state.commands.lock().expect("comandi") = None;
                    warn!("collegamento con l'agent del servizio perso");
                    state.update(&app, |ui| {
                        ui.status = "connecting".into();
                        ui.incoming = None;
                        ui.session = None;
                        ui.error = Some("Collegamento con il servizio NexoDesk perso, riprovo…".into());
                    });
                }
                Err(e) => {
                    tracing::debug!("agent del servizio non raggiungibile: {e:#}");
                    state.update(&app, |ui| {
                        ui.status = "connecting".into();
                        ui.error = Some("In attesa del servizio NexoDesk…".into());
                    });
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    *state.agent.lock().expect("agent") = Some(handle);
}

fn on_agent_event(app: &AppHandle, event: AgentEvent) {
    let state = app.state::<NexoApp>();
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
                    deadline_ms: now_ms() + nexodesk_agent::CONFIRM_TIMEOUT.as_millis() as u64,
                })
            });
            bring_to_front(app);
        }
        AgentEvent::IncomingClosed { session_id } => state.update(app, |ui| {
            if ui.incoming.as_ref().is_some_and(|i| i.session_id == session_id) {
                ui.incoming = None;
            }
        }),
        AgentEvent::SessionStarted { viewer_id, viewer_name, direct } => {
            state.update(app, |ui| ui.session = Some(SessionUi { viewer_id, viewer_name, direct }))
        }
        AgentEvent::SessionEnded { error } => state.update(app, |ui| {
            ui.session = None;
            if let Some(e) = error {
                info!("sessione terminata: {e}");
            }
        }),
    }
}

#[tauri::command]
fn get_state(state: State<NexoApp>) -> UiState {
    state.ui.lock().expect("stato").clone()
}

#[tauri::command]
fn set_server(app: AppHandle, state: State<NexoApp>, server: String) -> Result<(), String> {
    let server = server.trim().to_string();
    if !server.is_empty() {
        nexodesk_transport::resolve_addr(&server, nexodesk_transport::signal::DEFAULT_PORT)
            .map_err(|e| format!("indirizzo non valido: {e}"))?;
    }
    save_settings(&Settings { server: server.clone() }).map_err(|e| e.to_string())?;
    state.update(&app, |ui| ui.server = server);
    start_agent(&app);
    Ok(())
}

/// La genera l'agent (dell'app o del servizio), che risponde con l'evento Password.
#[tauri::command]
fn new_password(state: State<NexoApp>) {
    state.send(AgentCommand::NewPassword);
}

#[tauri::command]
fn respond(state: State<NexoApp>, session_id: String, accept: bool) {
    state.send(AgentCommand::Respond { session_id, accept });
}

#[tauri::command]
fn end_session(state: State<NexoApp>) {
    state.send(AgentCommand::EndSession);
}

/// Apre la finestra della sessione verso un altro dispositivo (processo `nexodesk-viewer`).
#[tauri::command]
fn connect(app: AppHandle, state: State<NexoApp>, device_id: String, password: String) -> Result<(), String> {
    let device_id: String = device_id.chars().filter(|c| c.is_ascii_digit()).collect();
    if device_id.len() != 9 {
        return Err("l'ID è di 9 cifre".into());
    }
    let server = state.ui.lock().expect("stato").server.clone();
    if server.is_empty() {
        return Err("imposta prima il server".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let viewer = exe.with_file_name(if cfg!(windows) { "nexodesk-viewer.exe" } else { "nexodesk-viewer" });
    if !viewer.exists() {
        return Err(format!("{} non trovato", viewer.display()));
    }
    // La password passa nell'ambiente del processo, non sulla riga di comando
    // (che altri programmi dello stesso PC possono leggere).
    let mut child = std::process::Command::new(&viewer)
        .args(["--server", &server, &device_id])
        .env("NEXODESK_PASSWORD", password)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("avvio di nexodesk-viewer: {e}"))?;
    info!("sessione verso {device_id} avviata");
    state.update(&app, |ui| ui.viewer_message = None);

    // Se il viewer termina con un errore, il motivo (ultima riga) arriva nell'interfaccia.
    std::thread::spawn(move || {
        let mut stderr = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut err, &mut stderr);
        }
        let status = child.wait();
        let reason = stderr
            .lines()
            .rev()
            .find(|l| l.contains("Sessione chiusa") || l.contains("ERROR") || l.contains("Error"))
            .map(|l| l.trim().to_string());
        let failed = !matches!(status, Ok(s) if s.success());
        let message = match (failed, reason) {
            (_, Some(r)) => Some(r),
            (true, None) => Some("la sessione si è chiusa con un errore".into()),
            _ => None,
        };
        let state = app.state::<NexoApp>();
        state.update(&app, |ui| ui.viewer_message = message);
    });
    Ok(())
}

#[tauri::command]
fn open_source(app: AppHandle) -> Result<(), String> {
    app.opener().open_url(SOURCE_URL, None::<&str>).map_err(|e| e.to_string())
}

fn init_logging() {
    let dir = nexodesk_transport::identity::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("nexodesk-app.log"));
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

fn main() {
    init_logging();
    nexodesk_input::set_dpi_awareness();
    let settings = load_settings();
    let password = nexodesk_agent::random_password();
    let state = NexoApp {
        ui: Mutex::new(UiState {
            server: settings.server,
            status: "setup".into(),
            password: password.clone(),
            ..Default::default()
        }),
        password: Arc::new(Mutex::new(password)),
        commands: Mutex::new(None),
        agent: Mutex::new(None),
        plan: Mutex::new(None),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_server,
            new_password,
            respond,
            end_session,
            connect,
            open_source
        ])
        .setup(|app| {
            start_agent(app.handle());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("avvio dell'applicazione");
    // Lascia il tempo alla chiusura delle connessioni dell'agent.
    std::thread::sleep(Duration::from_millis(100));
}
