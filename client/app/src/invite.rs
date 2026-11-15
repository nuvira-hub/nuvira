//! Invitation links (NuviraHub Pro, feature "pro.invite", docs/47-invitations.md).
//!
//! A technician sends a link; its page opens this app with
//! `nuvirahub://invite/CODE?server=HOST[:PORT]&key=FINGERPRINT` (or the Windows
//! program downloaded from it carries the code in its file name, `invite=`). The
//! app then asks for support with the code, and the request goes to that
//! technician. On the same server it is asked at once; on another server the
//! person confirms moving to it first. The person at the PC still accepts the
//! connection, as for every support request.

use nuvira_agent::AgentCommand;
use serde::Serialize;
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use crate::{NuviraApp, SupportUi};

/// Length of a code (the server's invite.CodeLen).
const CODE_LEN: usize = 10;

/// An invitation received and not finished yet.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct InviteUi {
    /// As people read it: "abcde-fghjk".
    code: String,
    /// The server it is for ("" this app's).
    server: String,
    /// For another server than this app's: moving needs the person's OK.
    other_server: bool,
    /// The server is locked here (customised program, organisation's rule).
    server_locked: bool,
    /// The request went out.
    sent: bool,
}

/// A code as typed or linked: lower case, letters and digits only.
pub(crate) fn normalize(code: &str) -> String {
    code.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

fn format(code: &str) -> String {
    if code.len() == CODE_LEN { format!("{}-{}", &code[..5], &code[5..]) } else { code.to_string() }
}

pub(crate) fn valid_code(code: &str) -> bool {
    code.len() == CODE_LEN && code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// A server address as a link may carry it: a host name or address, a port.
fn valid_server(server: &str) -> bool {
    server.len() <= 255
        && server.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

/// The code and the server of a `nuvirahub://invite/...` link.
pub(crate) fn from_url(link: &str) -> Option<(String, String)> {
    let url = url::Url::parse(link).ok()?;
    if url.scheme() != "nuvirahub" || url.host_str()? != "invite" {
        return None;
    }
    let code = normalize(url.path().trim_start_matches('/'));
    let server = url.query_pairs().find(|(k, _)| k == "server").map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    (valid_code(&code) && valid_server(&server)).then_some((code, server))
}

/// The same server, whatever the default port or the case.
fn same_server(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = nuvira_transport::signal::server_or_public(s).to_ascii_lowercase();
        let suffix = format!(":{}", nuvira_transport::signal::DEFAULT_PORT);
        s.strip_suffix(&suffix).map(str::to_string).unwrap_or(s)
    };
    norm(a) == norm(b)
}

/// A link the app was opened with (or handed by another start of it).
pub(crate) fn open_url(app: &AppHandle, link: &str) {
    match from_url(link) {
        Some((code, server)) => receive(app, code, server),
        None => warn!("link not understood: {link}"),
    }
}

/// An invitation arrived: asked for at once on this server, kept for the person's
/// OK on another one.
pub(crate) fn receive(app: &AppHandle, code: String, server: String) {
    let state = app.state::<NuviraApp>();
    let current = state.ui.lock().expect("state").server.clone();
    let other_server = !server.is_empty() && !same_server(&server, &current);
    let server_locked = crate::branding::get().is_some_and(|b| b.lock_server) || crate::policy_server().is_some();
    info!(other_server, "invitation received");
    state.update(app, |ui| {
        ui.invite = Some(InviteUi { code: format(&code), server, other_server, server_locked, sent: false });
        ui.support_error = None;
    });
    crate::bring_to_front(app);
    try_send(app);
}

/// Sends the request of the invitation once this app is online on its server.
pub(crate) fn try_send(app: &AppHandle) {
    let state = app.state::<NuviraApp>();
    let code = {
        let ui = state.ui.lock().expect("state");
        match &ui.invite {
            Some(i) if !i.sent && !i.other_server && ui.status == "online" => normalize(&i.code),
            _ => return,
        }
    };
    let Some(commands) = state.commands.lock().expect("commands").clone() else { return };
    if commands.send(AgentCommand::RequestSupport { message: String::new(), invite: code }).is_ok() {
        info!("support asked with the invitation");
        state.update(app, |ui| {
            if let Some(i) = ui.invite.as_mut() {
                i.sent = true;
            }
            ui.support = Some(SupportUi::default());
        });
    }
}

/// The person agrees to move to the invitation's server: then the request goes.
#[tauri::command]
pub(crate) async fn accept_invite(app: AppHandle) -> Result<Option<String>, String> {
    let server = {
        let state = app.state::<NuviraApp>();
        let ui = state.ui.lock().expect("state");
        match &ui.invite {
            Some(i) if i.other_server => i.server.clone(),
            _ => return Ok(None),
        }
    };
    let note = crate::set_server(app.clone(), server).await?;
    app.state::<NuviraApp>().update(&app, |ui| {
        if let Some(i) = ui.invite.as_mut() {
            i.other_server = false;
        }
    });
    try_send(&app);
    Ok(note)
}

#[tauri::command]
pub(crate) fn dismiss_invite(app: AppHandle) {
    app.state::<NuviraApp>().update(&app, |ui| {
        ui.invite = None;
        ui.support_error = None;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert_eq!(
            from_url("nuvirahub://invite/ABCDE-fghjk?server=nuvira.example.com%3A21216&key=ab12"),
            Some(("abcdefghjk".into(), "nuvira.example.com:21216".into()))
        );
        assert_eq!(from_url("nuvirahub://invite/abcdefghjk"), Some(("abcdefghjk".into(), String::new())));
        assert_eq!(from_url("nuvirahub://invite/abc"), None);
        assert_eq!(from_url("nuvirahub://other/abcdefghjk"), None);
        assert_eq!(from_url("https://invite/abcdefghjk"), None);
        assert_eq!(from_url("nuvirahub://invite/abcdefghjk?server=a%20b"), None);
        assert_eq!(from_url("nuvirahub://invite/abcdefghjk?server=x%22%20--rm"), None);
    }

    #[test]
    fn servers() {
        assert!(same_server("Nuvira.example.com", "nuvira.example.com:21116"));
        assert!(!same_server("nuvira.example.com:21216", "nuvira.example.com"));
        assert!(same_server("", nuvira_transport::signal::server_or_public("")));
        assert_eq!(format("abcdefghjk"), "abcde-fghjk");
    }
}
