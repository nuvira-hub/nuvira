//! Address books: the one on this computer ([`crate::local_book`]) and, when the
//! server has the address book module (NuviraHub Pro, feature "pro.address_book"),
//! the server's: sign-in with the account, device entries, management of accounts
//! and shared address books (administrators). See docs/17-address-book.md.
//!
//! The connection with the server opens at first use and reopens by itself if it
//! drops; it also tells which devices of the local book are online. The sign-in is
//! remembered with the token saved in `app.json`, never the password.

use nuvira_i18n::{t, tf};
use nuvira_proto::v1::{self as pb, api_request::Call, api_response::Result as ApiResult};
use nuvira_transport::api::ApiClient;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use crate::{NuviraApp, load_settings, local_book, save_settings};

/// Feature of servers with the address book module (accounts, shared books).
pub const PRO_ADDRESS_BOOK: &str = "pro.address_book";

#[derive(Debug, Clone, Serialize)]
pub struct AccountUi {
    pub id: i64,
    pub username: String,
    pub admin: bool,
    /// "admin", "technician" or "readonly".
    pub role: String,
    /// Two-factor authentication on (NuviraHub Pro).
    pub two_factor: bool,
}

impl From<pb::Account> for AccountUi {
    fn from(a: pb::Account) -> Self {
        // Servers before roles only say admin.
        let role = if !a.role.is_empty() {
            a.role
        } else if a.admin {
            "admin".into()
        } else {
            "technician".into()
        };
        Self { id: a.id, username: a.username, admin: a.admin, role, two_factor: a.two_factor }
    }
}

/// Member of a shared book with its level ("view", "edit", "manage").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberUi {
    pub account_id: i64,
    pub level: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BookUi {
    pub id: i64,
    pub name: String,
    pub personal: bool,
    /// The book kept on this computer.
    pub local: bool,
    /// What the account may do: "view", "edit", "manage", "owner", "admin"
    /// ("" from servers before roles: as "edit").
    pub access: String,
    pub member_ids: Vec<i64>,
    pub members: Vec<MemberUi>,
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
    /// From the server, read only: connected now, or last seen (seconds since 1970).
    /// Folder inside the book and free labels.
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub last_seen: i64,
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
            group: e.group,
            tags: e.tags,
            online: e.online,
            last_seen: e.last_seen,
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
            group: e.group,
            tags: e.tags,
            ..Default::default()
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
    pub fn client_slot(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<ApiClient>>, tokio::sync::TryLockError> {
        self.client.try_lock()
    }
}

/// Authenticated client: reopened if the connection dropped, with the saved token.
async fn client(app: &AppHandle) -> Result<ApiClient, String> {
    let state = app.state::<NuviraApp>();
    let mut guard = state.address_book.client.lock().await;
    if let Some(client) = guard.as_ref().filter(|c| !c.is_closed()) {
        return Ok(client.clone());
    }
    let client = open(app).await?;
    let features = client.features().to_vec();
    let pro = features.iter().any(|f| f == PRO_ADDRESS_BOOK);
    state.update(app, |ui| ui.features = features);
    if pro && let Some(token) = load_settings().token {
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
    }
    *guard = Some(client.clone());
    Ok(client)
}

/// New connection with the server set in the app.
async fn open(app: &AppHandle) -> Result<ApiClient, String> {
    let server_name = app.state::<NuviraApp>().ui.lock().expect("state").server.clone();
    if server_name.is_empty() {
        return Err(t("err.set_server_first"));
    }
    let server = nuvira_transport::resolve_addr(&server_name, nuvira_transport::signal::DEFAULT_PORT)
        .map_err(|e| format!("{e:#}"))?;
    let identity_path = nuvira_transport::identity::data_dir().join("app.key");
    let identity =
        nuvira_transport::identity::Identity::load_or_create(&identity_path).map_err(|e| format!("{e:#}"))?;
    let ws_url = nuvira_transport::ws::default_url(&server_name, server);
    let name = std::env::var("COMPUTERNAME").unwrap_or_default();
    ApiClient::connect(&server_name, server, &ws_url, Default::default(), &identity, &name)
        .await
        .map_err(|e| tf("err.server_connect", &[("e", &nuvira_i18n::message(&format!("{e:#}")))]))
}

/// Request with a new connection if the open one dropped in the meantime.
async fn call(app: &AppHandle, call: Call) -> Result<Option<ApiResult>, String> {
    let first = client(app).await?;
    let result = match first.call(call.clone()).await {
        Err(_) if first.is_closed() => client(app).await?.call(call).await,
        result => result,
    };
    result.map_err(|e| {
        let text = format!("{e:#}");
        // A server older than the app does not know the request.
        if text.ends_with("unknown request") { t("err.server_too_old") } else { text }
    })
}

fn set_account(app: &AppHandle, account: Option<pb::Account>) {
    let state = app.state::<NuviraApp>();
    state.update(app, |ui| ui.account = account.map(Into::into));
}

/// Forgets the sign-in (token, account, address books shown).
fn forget(app: &AppHandle) {
    let mut settings = load_settings();
    settings.token = None;
    let _ = save_settings(&settings);
    let state = app.state::<NuviraApp>();
    state.update(app, |ui| {
        ui.account = None;
        ui.books.clear();
    });
}

/// At start-up and when the server changes: loads the address books.
pub fn restore(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = refresh(&app).await {
            warn!("address book not loaded: {e}");
            app.state::<NuviraApp>().update(&app, |ui| ui.address_book_error = Some(e));
        }
    });
}

/// The books are read again every 30 s: who is online changes.
pub fn start_presence(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if let Err(e) = refresh(&app).await {
                tracing::debug!("address book not refreshed: {e}");
            }
        }
    });
}

/// Reads the books again: the local one always, the server's when signed in.
pub async fn refresh(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<NuviraApp>();
    let mut local = local_book::book();
    let server_set = !state.ui.lock().expect("state").server.is_empty();
    let mut books = Vec::new();
    let mut error = None;
    if server_set {
        // Opens the connection (and the remembered sign-in) if needed.
        match client(app).await {
            Ok(_) => {
                if state.ui.lock().expect("state").account.is_some() {
                    match call(app, Call::GetBooks(pb::GetBooks {})).await {
                        Ok(Some(ApiResult::Books(list))) => {
                            books = list.books.into_iter().map(book_ui).collect()
                        }
                        Ok(_) => error = Some(t("err.unexpected")),
                        Err(e) => error = Some(e),
                    }
                }
                if let Err(e) = presence(app, &mut local).await {
                    tracing::debug!("presence of the local book not read: {e}");
                }
            }
            // Without a sign-in the local book needs no server: no error shown.
            Err(e) if state.ui.lock().expect("state").account.is_none() => {
                tracing::debug!("server not reachable for the address book: {e}");
            }
            Err(e) => error = Some(e),
        }
    }
    books.push(local);
    state.update(app, |ui| {
        ui.books = books;
        ui.address_book_error = error.clone();
    });
    error.map_or(Ok(()), Err)
}

fn book_ui(b: pb::Book) -> BookUi {
    BookUi {
        id: b.id,
        name: b.name,
        personal: b.personal,
        local: false,
        access: b.access,
        member_ids: b.member_ids,
        members: b
            .members
            .into_iter()
            .map(|m| MemberUi { account_id: m.account_id, level: m.level })
            .collect(),
        entries: b.entries.into_iter().map(Into::into).collect(),
    }
}

/// Online state of the local entries, from the server (no sign-in needed).
async fn presence(app: &AppHandle, book: &mut BookUi) -> Result<(), String> {
    if book.entries.is_empty() {
        return Ok(());
    }
    let device_ids = book.entries.iter().map(|e| e.device_id.clone()).collect();
    let Some(ApiResult::Presence(list)) =
        call(app, Call::GetPresence(pb::GetPresence { device_ids })).await?
    else {
        return Err(t("err.unexpected"));
    };
    for p in list.devices {
        for e in book.entries.iter_mut().filter(|e| e.device_id == p.device_id) {
            e.online = p.online;
            e.last_seen = p.last_seen;
        }
    }
    Ok(())
}

/// At the first successful connection to an ID that is in no address book, adds it
/// to the personal one on the server when signed in, otherwise to the local one.
pub fn auto_add(app: &AppHandle, device_id: String, name: String, password: String) {
    let app = app.clone();
    if app.state::<NuviraApp>().ui.lock().expect("state").account.is_none() {
        match local_book::auto_add(&device_id, &name, &password) {
            Ok(true) => {
                info!("device {device_id} added to the local address book");
                tauri::async_runtime::spawn(async move {
                    let _ = refresh(&app).await;
                });
            }
            Ok(false) => {}
            Err(e) => warn!("automatic add to the local address book failed: {e}"),
        }
        return;
    }
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
pub async fn address_book_login(
    app: AppHandle,
    username: String,
    password: String,
    otp: Option<String>,
) -> Result<(), String> {
    let client = open(&app).await?;
    if !client.features().iter().any(|f| f == PRO_ADDRESS_BOOK) {
        return Err(t("server.pro_required"));
    }
    let Some(ApiResult::Session(session)) = client
        .call(Call::Login(pb::Login { username, password, otp: otp.unwrap_or_default() }))
        .await
        .map_err(|e| format!("{e:#}"))?
    else {
        return Err(t("err.unexpected"));
    };
    let mut settings = load_settings();
    settings.token = Some(session.token.clone());
    save_settings(&settings).map_err(|e| e.to_string())?;
    set_account(&app, session.account);
    *app.state::<NuviraApp>().address_book.client.lock().await = Some(client);
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_logout(app: AppHandle) {
    let _ = call(&app, Call::Logout(pb::Logout {})).await;
    *app.state::<NuviraApp>().address_book.client.lock().await = None;
    forget(&app);
    let _ = refresh(&app).await;
}

#[tauri::command]
pub async fn address_book_refresh(app: AppHandle) -> Result<(), String> {
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_save_entry(app: AppHandle, entry: EntryUi) -> Result<(), String> {
    if entry.book_id == local_book::BOOK_ID {
        local_book::put(entry)?;
    } else {
        call(&app, Call::PutEntry(entry.into())).await?;
    }
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_delete_entry(app: AppHandle, id: i64) -> Result<(), String> {
    // Local entries have negative IDs.
    if id < 0 {
        local_book::delete(id)?;
    } else {
        call(&app, Call::DeleteEntry(pb::DeleteEntry { id })).await?;
    }
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_change_password(app: AppHandle, old: String, new: String) -> Result<(), String> {
    call(&app, Call::ChangePassword(pb::ChangePassword { old_password: old, new_password: new.clone() }))
        .await?;
    // The server closes the remembered sign-ins: signing in again with the new password.
    let username =
        app.state::<NuviraApp>().ui.lock().expect("state").account.as_ref().map(|a| a.username.clone());
    *app.state::<NuviraApp>().address_book.client.lock().await = None;
    match username {
        // The code of the authenticator app is not at hand here: with two-factor
        // authentication the app goes back to the sign-in panel.
        Some(username) => address_book_login(app, username, new, None).await,
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
    role: String,
) -> Result<(), String> {
    let admin = role == "admin";
    call(&app, Call::PutAccount(pb::PutAccount { id, username, password, admin, role })).await?;
    Ok(())
}

#[tauri::command]
pub async fn address_book_delete_account(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::DeleteAccount(pb::DeleteAccount { id })).await?;
    refresh(&app).await
}

#[tauri::command]
pub async fn address_book_save_book(
    app: AppHandle,
    id: i64,
    name: String,
    members: Vec<MemberUi>,
) -> Result<(), String> {
    let member_ids = members.iter().map(|m| m.account_id).collect();
    let members =
        members.into_iter().map(|m| pb::BookMember { account_id: m.account_id, level: m.level }).collect();
    call(&app, Call::PutBook(pb::PutBook { id, name, member_ids, members })).await?;
    refresh(&app).await
}

/// Rules of the server for all its devices.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyUi {
    pub confirm_always: bool,
    pub no_files: bool,
    pub no_clipboard: bool,
}

impl From<pb::Policy> for PolicyUi {
    fn from(p: pb::Policy) -> Self {
        Self { confirm_always: p.confirm_always, no_files: p.no_files, no_clipboard: p.no_clipboard }
    }
}

#[tauri::command]
pub async fn address_book_policy(app: AppHandle) -> Result<PolicyUi, String> {
    match call(&app, Call::GetPolicy(pb::GetPolicy {})).await? {
        Some(ApiResult::Policy(policy)) => Ok(policy.into()),
        _ => Err(t("err.unexpected")),
    }
}

/// Administrators only; the server sends the new rules to every device connected.
#[tauri::command]
pub async fn address_book_save_policy(app: AppHandle, policy: PolicyUi) -> Result<PolicyUi, String> {
    let policy = pb::Policy {
        confirm_always: policy.confirm_always,
        no_files: policy.no_files,
        no_clipboard: policy.no_clipboard,
    };
    match call(&app, Call::PutPolicy(policy)).await? {
        Some(ApiResult::Policy(policy)) => Ok(policy.into()),
        _ => Err(t("err.unexpected")),
    }
}

/// New secret for two-factor authentication, not active until confirmed.
#[derive(Debug, Clone, Serialize)]
pub struct TwoFactorUi {
    pub secret: String,
    pub uri: String,
    /// QR code of the URI (PNG): the interface makes a data URL of it.
    pub qr_png: Vec<u8>,
}

#[tauri::command]
pub async fn address_book_two_factor_setup(app: AppHandle) -> Result<TwoFactorUi, String> {
    match call(&app, Call::TwoFactorSetup(pb::TwoFactorSetup {})).await? {
        Some(ApiResult::TwoFactorSecret(s)) => {
            Ok(TwoFactorUi { secret: s.secret, uri: s.uri, qr_png: s.qr_png })
        }
        _ => Err(t("err.unexpected")),
    }
}

/// Confirms the secret with a code; answers the recovery codes (shown once).
#[tauri::command]
pub async fn address_book_two_factor_enable(app: AppHandle, code: String) -> Result<Vec<String>, String> {
    let Some(ApiResult::RecoveryCodes(codes)) =
        call(&app, Call::TwoFactorEnable(pb::TwoFactorEnable { code })).await?
    else {
        return Err(t("err.unexpected"));
    };
    set_two_factor(&app, true);
    Ok(codes.codes)
}

#[tauri::command]
pub async fn address_book_two_factor_disable(
    app: AppHandle,
    password: String,
    code: String,
) -> Result<(), String> {
    call(&app, Call::TwoFactorDisable(pb::TwoFactorDisable { password, code })).await?;
    set_two_factor(&app, false);
    Ok(())
}

/// Administrators: removes the two-factor authentication of an account.
#[tauri::command]
pub async fn address_book_two_factor_reset(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::TwoFactorReset(pb::TwoFactorReset { account_id: id })).await?;
    Ok(())
}

fn set_two_factor(app: &AppHandle, on: bool) {
    app.state::<NuviraApp>().update(app, |ui| {
        if let Some(account) = ui.account.as_mut() {
            account.two_factor = on;
        }
    });
}

#[tauri::command]
pub async fn address_book_delete_book(app: AppHandle, id: i64) -> Result<(), String> {
    call(&app, Call::DeleteBook(pb::DeleteBook { id })).await?;
    refresh(&app).await
}
