//! Address book on the server: sign-in with the account, device entries,
//! management of accounts and shared address books (administrators). See
//! docs/17-address-book.md.
//!
//! The connection with the server opens at first use and reopens by itself if it
//! drops; the sign-in is remembered with the token saved in `app.json`, never the password.

use nexodesk_proto::v1::{self as pb, api_request::Call, api_response::Result as ApiResult};
use nexodesk_transport::api::ApiClient;
use serde::{Deserialize, Serialize};
use nexodesk_i18n::{t, tf};
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

/// Open (if any) and already authenticated connection.
#[derive(Default)]
pub struct Link {
    client: tokio::sync::Mutex<Option<ApiClient>>,
}

impl Link {
    /// Access without waiting (from synchronous commands): fails if it is in use.
    pub fn client_slot(&self) -> Result<tokio::sync::MutexGuard<'_, Option<ApiClient>>, tokio::sync::TryLockError> {
        self.client.try_lock()
    }
}

/// Authenticated client: reopened if the connection dropped, with the saved token.
async fn client(app: &AppHandle) -> Result<ApiClient, String> {
    let state = app.state::<NexoApp>();
    let mut guard = state.address_book.client.lock().await;
    if let Some(client) = guard.as_ref().filter(|c| !c.is_closed()) {
        return Ok(client.clone());
    }
    let token = load_settings().token.ok_or_else(|| t("err.login_required"))?;
    let client = open(app).await?;
    match client.call(Call::LoginToken(pb::LoginToken { token })).await {
        Ok(Some(ApiResult::Session(session))) => set_account(app, session.account),
        Ok(_) => return Err(t("err.unexpected")),
        Err(e) => {
            // Expired or revoked token: back to the sign-in panel.
            warn!("remembered sign-in not valid: {e:#}");
            forget(app);
            return Err(t("err.session_expired"));
        }
    }
    *guard = Some(client.clone());
    Ok(client)
}

/// New connection with the server set in the app.
async fn open(app: &AppHandle) -> Result<ApiClient, String> {
    let server_name = app.state::<NexoApp>().ui.lock().expect("state").server.clone();
    if server_name.is_empty() {
        return Err(t("err.set_server_first"));
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
        .map_err(|e| tf("err.server_connect", &[("e", &nexodesk_i18n::message(&format!("{e:#}")))]))
}

/// Request with a new connection if the open one dropped in the meantime.
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

/// Forgets the sign-in (token, account, address books shown).
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

/// At start-up: if there is a remembered sign-in, loads the address book.
pub fn restore(app: &AppHandle) {
    if load_settings().token.is_none() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = refresh(&app).await {
            warn!("address book not loaded: {e}");
            app.state::<NexoApp>().update(&app, |ui| ui.address_book_error = Some(e));
        }
    });
}

pub async fn refresh(app: &AppHandle) -> Result<(), String> {
    let Some(ApiResult::Books(books)) = call(app, Call::GetBooks(pb::GetBooks {})).await? else {
        return Err(t("err.unexpected"));
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
        ui.address_book_error = None;
    });
    Ok(())
}

/// At the first successful connection to an ID that is in no address book, adds it to the personal one.
pub fn auto_add(app: &AppHandle, device_id: String, name: String, password: String) {
    if app.state::<NexoApp>().ui.lock().expect("state").account.is_none() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let entry = pb::Entry { device_id, name, password, ..Default::default() };
        match call(&app, Call::AutoAdd(entry)).await {
            Ok(Some(ApiResult::Entry(e))) => {
                info!("device {} added to the personal address book", e.device_id);
                let _ = refresh(&app).await;
            }
            Ok(_) => {}
            Err(e) => warn!("automatic add to the address book failed: {e}"),
        }
    });
}

// --- interface commands ---

#[tauri::command]
pub async fn address_book_login(app: AppHandle, username: String, password: String) -> Result<(), String> {
    let client = open(&app).await?;
    let Some(ApiResult::Session(session)) = client
        .call(Call::Login(pb::Login { username, password }))
        .await
        .map_err(|e| format!("{e:#}"))?
    else {
        return Err(t("err.unexpected"));
    };
    let mut settings = load_settings();
    settings.token = Some(session.token.clone());
    save_settings(&settings).map_err(|e| e.to_string())?;
    set_account(&app, session.account);
    *app.state::<NexoApp>().address_book.client.lock().await = Some(client);
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_logout(app: AppHandle) {
    let _ = call(&app, Call::Logout(pb::Logout {})).await;
    *app.state::<NexoApp>().address_book.client.lock().await = None;
    forget(&app);
}

#[tauri::command]
pub async fn address_book_refresh(app: AppHandle) -> Result<(), String> {
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_save_entry(app: AppHandle, entry: EntryUi) -> Result<(), String> {
    call(&app, Call::PutEntry(entry.into())).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_delete_entry(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::DeleteEntry(pb::DeleteEntry { id })).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_change_password(app: AppHandle, old: String, new: String) -> Result<(), String> {
    call(&app, Call::ChangePassword(pb::ChangePassword { old_password: old, new_password: new.clone() })).await?;
    // The server closes the remembered sign-ins: signing in again with the new password.
    let username = app.state::<NexoApp>().ui.lock().expect("state").account.as_ref().map(|a| a.username.clone());
    *app.state::<NexoApp>().address_book.client.lock().await = None;
    match username {
        Some(username) => address_book_login(app, username, new).await,
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn address_book_accounts(app: AppHandle) -> Result<Vec<AccountUi>, String> {
    match call(&app, Call::ListAccounts(pb::ListAccounts {})).await? {
        Some(ApiResult::Accounts(list)) => Ok(list.accounts.into_iter().map(Into::into).collect()),
        _ => Err(t("err.unexpected")),
    }
}

#[tauri::command]
pub async fn address_book_save_account(
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
pub async fn address_book_delete_account(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::DeleteAccount(pb::DeleteAccount { id })).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_save_book(app: AppHandle, id: i64, name: String, member_ids: Vec<i64>) -> Result<(), String> {
    call(&app, Call::PutBook(pb::PutBook { id, name, member_ids })).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_delete_book(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::DeleteBook(pb::DeleteBook { id })).await?;
    refresh(&app).await
}
