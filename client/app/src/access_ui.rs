//! Settings of the rules of unattended access ([`nuvira_agent::access`]): who may
//! connect, the two-factor code, the profiles.
//!
//! The rules are in the folder of the agent's device key. With the Windows
//! service that is `%ProgramData%\nuvira`, which the app cannot read or write: it
//! shows what the service's agent tells it ([`AgentEvent::Access`]) and saves
//! through `nuvira access save`, started as administrator, which keeps from the
//! file what the app does not know (hashes, the secret). Elsewhere the app writes
//! its own data folder, and on Linux and macOS brings the sign-in screen's copy up
//! to date (administrator password, as for a new permanent password).

use std::path::PathBuf;

use nuvira_agent::access::{Allowed, Edit, ProfileEdit, TotpEdit};
use nuvira_i18n::t;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::NuviraApp;

/// A change from the settings page: profile passwords in clear text, hashed here.
#[derive(Debug, Deserialize)]
pub struct EditUi {
    allow: Vec<Allowed>,
    allow_attended: bool,
    /// "keep", "off", or a new secret confirmed with a code ([`access_totp_check`]).
    totp: TotpUi,
    profiles: Vec<ProfileUi>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TotpUi {
    Keep,
    Off,
    Set(String),
}

#[derive(Debug, Deserialize)]
struct ProfileUi {
    name: String,
    off: Vec<String>,
    /// A new password (at least 8 characters); empty keeps the profile's own.
    #[serde(default)]
    password: String,
}

impl EditUi {
    fn into_edit(self) -> Result<Edit, String> {
        let mut profiles = Vec::new();
        for p in self.profiles {
            let password_hash = match p.password.as_str() {
                "" => None,
                short if short.chars().count() < 8 => return Err(t("settings.install_pw_short")),
                password => Some(nuvira_secret::hash(password)),
            };
            profiles.push(ProfileEdit { name: p.name, off: p.off, password_hash });
        }
        let totp = match self.totp {
            TotpUi::Keep => TotpEdit::Keep,
            TotpUi::Off => TotpEdit::Off,
            TotpUi::Set(secret) => TotpEdit::Set(secret),
        };
        Ok(Edit { allow: self.allow, allow_attended: self.allow_attended, totp, profiles })
    }
}

/// A new two-factor secret to show: the text to type and the QR code of its URI.
#[derive(Debug, Serialize)]
pub struct TotpSetup {
    secret: String,
    uri: String,
    qr_svg: String,
}

#[tauri::command]
pub fn access_totp_new() -> TotpSetup {
    let secret = nuvira_secret::totp_secret();
    let uri = nuvira_secret::totp_uri(&secret, "NuviraHub", &nuvira_transport::device_name());
    let qr_svg = qr_svg(&uri);
    TotpSetup { secret, uri, qr_svg }
}

/// The code typed is the one the authenticator app shows for `secret` now.
#[tauri::command]
pub fn access_totp_check(secret: String, code: String) -> bool {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    nuvira_secret::totp_verify(&secret, &code, now).is_some()
}

/// Saves the rules; the message to show.
#[tauri::command]
pub async fn access_save(state: State<'_, NuviraApp>, edit: EditUi) -> Result<String, String> {
    let edit = edit.into_edit()?;
    let service = crate::service_installed();
    let message = tauri::async_runtime::spawn_blocking(move || save(edit, service))
        .await
        .map_err(|e| e.to_string())??;
    // The agent reads the file at every session; it tells the app what is there now.
    state.send(nuvira_agent::AgentCommand::GetAccess);
    Ok(message)
}

#[cfg(windows)]
fn save(edit: Edit, service: bool) -> Result<String, String> {
    if !service {
        edit.save_in(&nuvira_transport::identity::data_dir())?;
        return Ok(t("access.saved"));
    }
    // The service's folder: as administrator. The change travels in a file, which
    // the command deletes.
    let tag = nuvira_agent::random_password();
    let file = std::env::temp_dir().join(format!("nuvira-access-{tag}.json"));
    std::fs::write(&file, serde_json::to_vec(&edit).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let result = crate::run_service_command(format!("access save --file \"{}\"", file.display()));
    let _ = std::fs::remove_file(&file);
    result.map(|_| t("access.saved"))
}

#[cfg(unix)]
fn save(edit: Edit, _service: bool) -> Result<String, String> {
    edit.save_in(&nuvira_transport::identity::data_dir())?;
    // The sign-in screen's agent has its own copy (root's), brought up to date.
    if nuvira_service::system::installed() {
        let settings = crate::load_settings();
        // Not brought up to date (no administrator password, or an error): said plainly.
        let note = crate::install_system(&settings.server, settings.server_key.as_deref());
        if note.is_some_and(|n| n != t("install.hid_allow_mac")) {
            return Ok(format!("{} {}", t("access.saved"), t("access.signin_old")));
        }
    }
    Ok(t("access.saved"))
}

#[cfg(not(any(windows, unix)))]
fn save(_edit: Edit, _service: bool) -> Result<String, String> {
    Err(t("install.windows_only"))
}

/// `nuvira access …`: the rules of unattended access from the command line (the app
/// runs `save` as administrator; administrators can use both in scripts).
///
/// - `nuvira access show [--dir DIR]`: the rules, without hashes and secret;
/// - `nuvira access save --file FILE [--dir DIR] [--report FILE]`: applies a change
///   (JSON of [`Edit`]) and deletes FILE.
///
/// DIR is by default the folder of the agent that uses them: the Windows service's,
/// or this user's data folder.
pub fn run(args: &[std::ffi::OsString]) -> anyhow::Result<()> {
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let dir = value("--dir").map(PathBuf::from).unwrap_or_else(default_dir);
    match args.first().map(String::as_str) {
        Some("show") => {
            println!("{}", serde_json::to_string_pretty(&nuvira_agent::access::View::of(&dir))?);
            Ok(())
        }
        Some("save") => {
            let file = value("--file").ok_or_else(|| anyhow::anyhow!("--file is needed"))?;
            let text = std::fs::read_to_string(&file);
            let _ = std::fs::remove_file(&file);
            let result = text
                .map_err(|e| format!("{file}: {e}"))
                .and_then(|text| serde_json::from_str::<Edit>(&text).map_err(|e| e.to_string()))
                .and_then(|edit| edit.save_in(&dir));
            if let Some(report) = value("--report") {
                let json = match &result {
                    Ok(_) => serde_json::json!({ "ok": true }),
                    Err(e) => serde_json::json!({ "ok": false, "error": e }),
                };
                std::fs::write(report, json.to_string())?;
            }
            result
                .map(|_| println!("rules of unattended access saved in {}", dir.display()))
                .map_err(anyhow::Error::msg)
        }
        _ => anyhow::bail!("usage: nuvira access show|save --file FILE [--dir DIR]"),
    }
}

fn default_dir() -> PathBuf {
    #[cfg(windows)]
    if crate::service_installed() {
        return nuvira_service::service_identity_path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
    }
    nuvira_transport::identity::data_dir()
}

/// The QR code of `text` as SVG (dark modules on white, with the quiet zone).
fn qr_svg(text: &str) -> String {
    use qrcodegen::{QrCode, QrCodeEcc};
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else { return String::new() };
    let border = 4;
    let size = qr.size() + 2 * border;
    let mut path = String::new();
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if qr.get_module(x, y) {
                path += &format!("M{},{}h1v1h-1z", x + border, y + border);
            }
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" shape-rendering=\"crispEdges\">\
         <rect width=\"100%\" height=\"100%\" fill=\"#fff\"/><path d=\"{path}\" fill=\"#000\"/></svg>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_from_the_page() {
        let json = r#"{"allow":[{"id":"123 456 789","name":"Office"}],"allow_attended":true,
            "totp":{"set":"JBSWY3DPEHPK3PXP"},"profiles":[{"name":"View","off":["input"],"password":"look-at-it"}]}"#;
        let edit: EditUi = serde_json::from_str(json).unwrap();
        let edit = edit.into_edit().unwrap();
        assert_eq!(edit.totp, TotpEdit::Set("JBSWY3DPEHPK3PXP".into()));
        let rules = edit.apply(None).unwrap();
        assert!(nuvira_secret::verify("look-at-it", &rules.profiles[0].password_hash));
        let keep: EditUi =
            serde_json::from_str(r#"{"allow":[],"allow_attended":false,"totp":"keep","profiles":[]}"#)
                .unwrap();
        assert_eq!(keep.into_edit().unwrap().totp, TotpEdit::Keep);
        let short: EditUi = serde_json::from_str(
            r#"{"allow":[],"allow_attended":false,"totp":"off","profiles":[{"name":"x","off":[],"password":"short"}]}"#,
        )
        .unwrap();
        assert!(short.into_edit().is_err());
        assert!(qr_svg("otpauth://totp/NuviraHub:PC?secret=ABC").starts_with("<svg"));
    }
}
