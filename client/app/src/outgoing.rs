//! Sessions towards other devices: the session window (`nuvira viewer ...`, a
//! process of its own) started with the ID and password typed or saved in the
//! address book, what it reports back, and the technician's note at the end.

use std::sync::Mutex;

use nuvira_i18n::t;
use tauri::{AppHandle, Manager, State};
use tracing::info;

use crate::{NoteRequestUi, NuviraApp, address_book, load_settings, record_dir};

/// Opens the session window towards another device (`nuvira-viewer` process).
#[tauri::command]
pub(crate) fn connect(
    app: AppHandle,
    state: State<NuviraApp>,
    device_id: String,
    password: String,
    terminal: Option<bool>,
) -> Result<(), String> {
    launch_viewer(&app, &state, device_id, password, terminal.unwrap_or_default())
}

/// Connection from the address book, with the password saved in the entry.
#[tauri::command]
pub(crate) fn connect_entry(app: AppHandle, state: State<NuviraApp>, id: i64, terminal: Option<bool>) -> Result<(), String> {
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
    launch_viewer(&app, &state, entry.device_id, entry.password, terminal.unwrap_or_default())
}

pub(crate) fn launch_viewer(
    app: &AppHandle,
    state: &NuviraApp,
    device_id: String,
    password: String,
    terminal: bool,
) -> Result<(), String> {
    // An ID, or a computer's alias on the server (docs/46-aliases.md).
    let device_id = nuvira_transport::signal::target(&device_id);
    if !nuvira_transport::signal::valid_target(&device_id) {
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
    // A session that ends by itself says why in the viewer, before it closes.
    command.arg("--error-dialog");
    // A shell of the remote computer, without its desktop.
    if terminal {
        command.arg("--terminal");
    }
    if let Some(key) = load_settings().server_key {
        command.args(["--server-key", &key]);
    }
    // Signed in to the address book: the session log records the account, and at
    // the end the technician may write what they did.
    let settings = load_settings();
    if let Some(token) = &settings.token {
        command.env("NUVIRA_ACCOUNT_TOKEN", token);
    }
    // Recordings: from the start when the technician asked for it, and where.
    if !terminal {
        if settings.record_sessions {
            command.arg("--record");
        }
        if settings.hide_wallpaper {
            command.arg("--hide-wallpaper");
        }
        command.arg("--record-dir").arg(record_dir(&settings));
    }
    let ask_note = settings.token.is_some()
        && state.ui.lock().expect("state").features.iter().any(|f| f == "pro.session_log");
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
    let connected_name = std::sync::Arc::new(Mutex::new(None::<String>));
    let note_device = device_id.clone();
    let reader = child.stdout.take().map(|stdout| {
        let (app, closed_reason, connected_name) = (app.clone(), closed_reason.clone(), connected_name.clone());
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if line == "@pending" {
                    app.state::<NuviraApp>()
                        .update(&app, |ui| ui.viewer_message = Some("@app.waiting_confirmation".into()));
                } else if let Some(name) = line.strip_prefix("@connected\t") {
                    app.state::<NuviraApp>().update(&app, |ui| ui.viewer_message = None);
                    *connected_name.lock().expect("name") = Some(name.to_string());
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
        let name = connected_name.lock().expect("name").take();
        let state = app.state::<NuviraApp>();
        state.update(&app, |ui| {
            ui.viewer_message = message;
            // A session was held: what was done goes in the session log (reports).
            if let Some(name) = name.filter(|_| ask_note) {
                ui.note_request = Some(NoteRequestUi { device_id: note_device, name });
            }
        });
    });
    Ok(())
}

/// The technician's note on the session just ended (NuviraHub Pro session log);
/// an empty note only closes the question.
#[tauri::command]
pub(crate) async fn session_note(app: AppHandle, device_id: String, note: String) -> Result<(), String> {
    if !note.trim().is_empty() {
        // On error the question stays open, with the note still written.
        address_book::session_note(&app, device_id, note).await?;
    }
    app.state::<NuviraApp>().update(&app, |ui| ui.note_request = None);
    Ok(())
}
