//! Rubrica sul server: accesso con l'account, schede dei dispositivi, gestione
//! di account e rubriche condivise (amministratori). Vedi docs/17-rubrica.md.
//!
//! La connessione con il server si apre al primo uso e si riapre da sola se cade;
//! l'accesso è ricordato con il gettone salvato in `app.json`, mai con la password.

use nexodesk_proto::v1::{self as pb, api_request::Call, api_response::Result as ApiResult};
use nexodesk_transport::api::ApiClient;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use crate::{NexoApp, load_settings, save_settings};

#[derive(Debug, Clone, Serialize)]
pub struct AccountUi {
    pub id: i64,
    pub username: String,
    pub admin: bool,
}

impl From<pb::Account> for AccountUi {
    fn from(a: pb::Account) -> Self {
        Self { id: a.id, username: a.username, admin: a.admin }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BookUi {
    pub id: i64,
    pub name: String,
    pub personal: bool,
    pub member_ids: Vec<i64>,
    pub entries: Vec<EntryUi>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntryUi {
    #[serde(default)]
    pub id: i64,
    pub book_id: i64,
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub company: String,
    #[serde(default)]
    pub contact: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub password: String,
}

impl From<pb::Entry> for EntryUi {
    fn from(e: pb::Entry) -> Self {
        Self {
            id: e.id,
            book_id: e.book_id,
            device_id: e.device_id,
            name: e.name,
            company: e.company,
            contact: e.contact,
            email: e.email,
            phone: e.phone,
            notes: e.notes,
            password: e.password,
        }
    }
}

impl From<EntryUi> for pb::Entry {
    fn from(e: EntryUi) -> Self {
        Self {
            id: e.id,
            book_id: e.book_id,
            device_id: e.device_id,
            name: e.name,
            company: e.company,
            contact: e.contact,
            email: e.email,
            phone: e.phone,
            notes: e.notes,
            password: e.password,
            updated: 0,
        }
    }
}

/// Connessione aperta (se c'è) e già autenticata.
#[derive(Default)]
pub struct Link {
    client: tokio::sync::Mutex<Option<ApiClient>>,
}

impl Link {
    /// Accesso senza attesa (dai comandi sincroni): fallisce se è in uso.
    pub fn client_slot(&self) -> Result<tokio::sync::MutexGuard<'_, Option<ApiClient>>, tokio::sync::TryLockError> {
        self.client.try_lock()
    }
}

/// Client autenticato: riaperto se la connessione è caduta, con il gettone salvato.
async fn client(app: &AppHandle) -> Result<ApiClient, String> {
    let state = app.state::<NexoApp>();
    let mut guard = state.rubrica.client.lock().await;
    if let Some(client) = guard.as_ref().filter(|c| !c.is_closed()) {
        return Ok(client.clone());
    }
    let token = load_settings().token.ok_or("accesso alla rubrica necessario")?;
    let client = open(app).await?;
    match client.call(Call::LoginToken(pb::LoginToken { token })).await {
        Ok(Some(ApiResult::Session(session))) => set_account(app, session.account),
        Ok(_) => return Err("risposta inattesa del server".into()),
        Err(e) => {
            // Gettone scaduto o revocato: si torna al riquadro di accesso.
            warn!("accesso ricordato non valido: {e:#}");
            forget(app);
            return Err("accesso scaduto: entra di nuovo nella rubrica".into());
        }
    }
    *guard = Some(client.clone());
    Ok(client)
}

/// Nuova connessione con il server impostato nell'app.
async fn open(app: &AppHandle) -> Result<ApiClient, String> {
    let server_name = app.state::<NexoApp>().ui.lock().expect("stato").server.clone();
    if server_name.is_empty() {
        return Err("imposta prima il server".into());
    }
    let server = nexodesk_transport::resolve_addr(&server_name, nexodesk_transport::signal::DEFAULT_PORT)
        .map_err(|e| format!("{e:#}"))?;
    let identity_path = nexodesk_transport::identity::data_dir().join("app.key");
    let identity =
        nexodesk_transport::identity::Identity::load_or_create(&identity_path).map_err(|e| format!("{e:#}"))?;
    let ws_url = nexodesk_transport::ws::default_url(&server_name, server);
    let name = std::env::var("COMPUTERNAME").unwrap_or_default();
    ApiClient::connect(&server_name, server, &ws_url, Default::default(), &identity, &name)
        .await
        .map_err(|e| format!("collegamento al server: {e:#}"))
}

/// Richiesta con una nuova connessione se quella aperta è caduta nel frattempo.
async fn call(app: &AppHandle, call: Call) -> Result<Option<ApiResult>, String> {
    let first = client(app).await?;
    match first.call(call.clone()).await {
        Err(_) if first.is_closed() => {
            let retry = client(app).await?;
            retry.call(call).await.map_err(|e| format!("{e:#}"))
        }
        result => result.map_err(|e| format!("{e:#}")),
    }
}

fn set_account(app: &AppHandle, account: Option<pb::Account>) {
    let state = app.state::<NexoApp>();
    state.update(app, |ui| ui.account = account.map(Into::into));
}

/// Dimentica l'accesso (gettone, account, rubriche mostrate).
fn forget(app: &AppHandle) {
    let mut settings = load_settings();
    settings.token = None;
    let _ = save_settings(&settings);
    let state = app.state::<NexoApp>();
    state.update(app, |ui| {
        ui.account = None;
        ui.books.clear();
    });
}

/// All'avvio: se c'è un accesso ricordato, carica la rubrica.
pub fn restore(app: &AppHandle) {
    if load_settings().token.is_none() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = refresh(&app).await {
            warn!("rubrica non caricata: {e}");
            app.state::<NexoApp>().update(&app, |ui| ui.rubrica_error = Some(e));
        }
    });
}

pub async fn refresh(app: &AppHandle) -> Result<(), String> {
    let Some(ApiResult::Books(books)) = call(app, Call::GetBooks(pb::GetBooks {})).await? else {
        return Err("risposta inattesa del server".into());
    };
    let books = books
        .books
        .into_iter()
        .map(|b| BookUi {
            id: b.id,
            name: b.name,
            personal: b.personal,
            member_ids: b.member_ids,
            entries: b.entries.into_iter().map(Into::into).collect(),
        })
        .collect();
    app.state::<NexoApp>().update(app, |ui| {
        ui.books = books;
        ui.rubrica_error = None;
    });
    Ok(())
}

/// Al primo collegamento riuscito a un ID che non è in nessuna rubrica, lo aggiunge a quella personale.
pub fn auto_add(app: &AppHandle, device_id: String, name: String, password: String) {
    if app.state::<NexoApp>().ui.lock().expect("stato").account.is_none() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let entry = pb::Entry { device_id, name, password, ..Default::default() };
        match call(&app, Call::AutoAdd(entry)).await {
            Ok(Some(ApiResult::Entry(e))) => {
                info!("dispositivo {} aggiunto alla rubrica personale", e.device_id);
                let _ = refresh(&app).await;
            }
            Ok(_) => {}
            Err(e) => warn!("aggiunta automatica alla rubrica non riuscita: {e}"),
        }
    });
}

// --- comandi dell'interfaccia ---

#[tauri::command]
pub async fn rubrica_login(app: AppHandle, username: String, password: String) -> Result<(), String> {
    let client = open(&app).await?;
    let Some(ApiResult::Session(session)) = client
        .call(Call::Login(pb::Login { username, password }))
        .await
        .map_err(|e| format!("{e:#}"))?
    else {
        return Err("risposta inattesa del server".into());
    };
    let mut settings = load_settings();
    settings.token = Some(session.token.clone());
    save_settings(&settings).map_err(|e| e.to_string())?;
    set_account(&app, session.account);
    *app.state::<NexoApp>().rubrica.client.lock().await = Some(client);
    refresh(&app).await
}

#[tauri::command]
pub async fn rubrica_logout(app: AppHandle) {
    let _ = call(&app, Call::Logout(pb::Logout {})).await;
    *app.state::<NexoApp>().rubrica.client.lock().await = None;
    forget(&app);
}

#[tauri::command]
pub async fn rubrica_refresh(app: AppHandle) -> Result<(), String> {
    refresh(&app).await
}

#[tauri::command]
pub async fn rubrica_save_entry(app: AppHandle, entry: EntryUi) -> Result<(), String> {
    call(&app, Call::PutEntry(entry.into())).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn rubrica_delete_entry(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::DeleteEntry(pb::DeleteEntry { id })).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn rubrica_change_password(app: AppHandle, old: String, new: String) -> Result<(), String> {
    call(&app, Call::ChangePassword(pb::ChangePassword { old_password: old, new_password: new.clone() })).await?;
    // Il server chiude gli accessi ricordati: si rientra con la password nuova.
    let username = app.state::<NexoApp>().ui.lock().expect("stato").account.as_ref().map(|a| a.username.clone());
    *app.state::<NexoApp>().rubrica.client.lock().await = None;
    match username {
        Some(username) => rubrica_login(app, username, new).await,
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn rubrica_accounts(app: AppHandle) -> Result<Vec<AccountUi>, String> {
    match call(&app, Call::ListAccounts(pb::ListAccounts {})).await? {
        Some(ApiResult::Accounts(list)) => Ok(list.accounts.into_iter().map(Into::into).collect()),
        _ => Err("risposta inattesa del server".into()),
    }
}

#[tauri::command]
pub async fn rubrica_save_account(
    app: AppHandle,
    id: i64,
    username: String,
    password: String,
    admin: bool,
) -> Result<(), String> {
    call(&app, Call::PutAccount(pb::PutAccount { id, username, password, admin })).await?;
    Ok(())
}

#[tauri::command]
pub async fn rubrica_delete_account(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::DeleteAccount(pb::DeleteAccount { id })).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn rubrica_save_book(app: AppHandle, id: i64, name: String, member_ids: Vec<i64>) -> Result<(), String> {
    call(&app, Call::PutBook(pb::PutBook { id, name, member_ids })).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn rubrica_delete_book(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::DeleteBook(pb::DeleteBook { id })).await?;
    refresh(&app).await
}
