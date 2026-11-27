//! The settings file (`app.json` in the data folder) and where the server comes
//! from besides it: the program's file name, the customisation and the
//! organisation's policies. The `Settings` struct itself (the file's format) is
//! in main.rs.

use std::path::PathBuf;

use tracing::{info, warn};

use crate::{Settings, branding, invite, local_book};

/// Server written in the program's file name, e.g. `nuvira-host=nuvira.example.com.exe`
/// or `nuvira-host=example.com,port=21117,key=<fingerprint>.exe`: whoever hosts a
/// server hands out the same program, renamed, and it connects there with no setup.
/// Windows does not allow `:` in file names, hence `port=`. A copy saved twice by a
/// browser (`... (1).exe`) still works.
#[derive(Debug, Default, PartialEq)]
struct FileConfig {
    server: Option<String>,
    server_key: Option<String>,
    /// An invitation's code (`invite=`, the program downloaded from its page).
    invite: Option<String>,
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
    let (mut host, mut port, mut key, mut invite) = (None, None, None, None);
    for option in options.split(',') {
        match option.trim().split_once('=') {
            Some(("host", v)) if !v.is_empty() => host = Some(v.to_string()),
            Some(("port", v)) if v.parse::<u16>().is_ok() => port = Some(v.to_string()),
            Some(("key", v)) if nuvira_transport::parse_fingerprint(v).is_ok() => key = Some(v.to_string()),
            Some(("invite", v)) if invite::valid_code(&invite::normalize(v)) => invite = Some(invite::normalize(v)),
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
        invite,
    }
}

/// Server and language of a customised program (they win over the file name).
pub(crate) fn apply_branding(settings: &mut Settings) {
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

/// Server set by the organisation's policies (Group Policy, Intune; on Linux
/// /etc/nuvira/policy.json or the installation for the whole computer), with its
/// fingerprint.
pub(crate) fn policy_server() -> Option<(String, Option<String>)> {
    #[cfg(any(windows, unix))]
    {
        let policy = nuvira_service::policy::read();
        if let Some(server) = policy.server {
            return Some((server, policy.server_key));
        }
    }
    #[cfg(unix)]
    if let Some(state) = nuvira_service::managed::state() {
        return Some((state.server, state.server_key));
    }
    None
}

/// The organisation's policies win over the settings and the customisation: the
/// service applies them too (see nuvira_service::policy).
pub(crate) fn apply_policy(settings: &mut Settings) {
    let Some((server, key)) = policy_server() else { return };
    if settings.server.eq_ignore_ascii_case(&server) && (key.is_none() || settings.server_key == key) {
        return;
    }
    info!("server from the organisation's policies: {server}");
    if !settings.server.eq_ignore_ascii_case(&server) {
        settings.token = None;
    }
    settings.server = server;
    settings.server_key = key;
    if let Err(e) = save_settings(settings) {
        tracing::warn!("settings not saved: {e:#}");
    }
}

/// Applies the server in the file name, if any, to the saved settings.
pub(crate) fn apply_file_config(settings: &mut Settings) {
    let name =
        std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let FileConfig { server: Some(server), server_key, .. } = file_config(&name.unwrap_or_default()) else {
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

pub(crate) fn load_settings() -> Settings {
    load_settings_from(&settings_path())
}

pub(crate) fn save_settings(settings: &Settings) -> anyhow::Result<()> {
    save_settings_to(&settings_path(), settings)
}

/// The settings in `path`; none yet: the defaults. A file that cannot be read
/// (cut short by a crash in an older version, edited by hand) is kept aside as
/// `app.json.bad`, said in the log, and replaced with
/// [`Settings::after_a_bad_file`].
fn load_settings_from(path: &std::path::Path) -> Settings {
    let problem = match std::fs::read_to_string(path) {
        // Editors such as Notepad (and PowerShell 5) may add a UTF-8 byte order mark.
        Ok(text) => match serde_json::from_str::<Settings>(text.trim_start_matches('\u{feff}')) {
            Ok(mut settings) => {
                settings.token =
                    settings.token_protected.as_deref().map(local_book::unprotect).filter(|t| !t.is_empty());
                if settings.token_protected.is_some() && settings.token.is_none() {
                    // Another user's or computer's, or the key store locked:
                    // signed out, as after a sign-out.
                    warn!("the saved address book sign-in cannot be read here: sign in again");
                }
                if let Some(plain) = settings.plain_token.take() {
                    // Saved by an earlier version: from now on protected.
                    settings.token = Some(plain);
                    match save_settings_to(path, &settings) {
                        Ok(()) => info!("address book sign-in in {} now protected", path.display()),
                        Err(e) => warn!("settings not saved: {e:#}"),
                    }
                }
                return settings;
            }
            Err(e) => e.to_string(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(e) => e.to_string(),
    };
    let settings = Settings::after_a_bad_file();
    let aside = path.with_extension("json.bad");
    match std::fs::rename(path, &aside) {
        Ok(()) => {
            tracing::error!(
                "{} could not be read ({problem}): kept as {}; the permissions of this PC are off until chosen again",
                path.display(),
                aside.display()
            );
            if let Err(e) = save_settings_to(path, &settings) {
                warn!("settings not saved: {e:#}");
            }
        }
        Err(e) => tracing::error!(
            "{} could not be read ({problem}) nor kept aside ({e}): the permissions of this PC are off",
            path.display()
        ),
    }
    settings
}

/// Written aside and renamed: a crash halfway leaves the previous file whole.
/// The sign-in token only protected; on Linux and macOS the file is the
/// person's alone (0600).
fn save_settings_to(path: &std::path::Path, settings: &Settings) -> anyhow::Result<()> {
    let mut stored = settings.clone();
    stored.plain_token = None;
    stored.token_protected = match &settings.token {
        // The same token as loaded: its protected text as it is.
        Some(token)
            if settings.token_protected.as_deref().map(local_book::unprotect).as_deref() == Some(token) =>
        {
            settings.token_protected.clone()
        }
        Some(token) => match local_book::protect(token) {
            Ok(protected) => Some(protected),
            Err(e) => {
                warn!("address book sign-in not saved (it cannot be protected here: {e})");
                None
            }
        },
        None => None,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let part = path.with_extension("json.part");
    let text = serde_json::to_string_pretty(&stored)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(&part);
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&part)?;
        file.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&part, text)?;
    std::fs::rename(&part, path)?;
    Ok(())
}

/// Folder where the session window saves the recordings.
pub(crate) fn record_dir(settings: &Settings) -> PathBuf {
    settings.record_dir.as_ref().map(PathBuf::from).unwrap_or_else(nuvira_viewer::default_recording_dir)
}

/// The invitation's code and server in a program's file name (`invite=`).
pub(crate) fn file_invite(exe: &str) -> Option<(String, String)> {
    let name = std::path::Path::new(exe).file_name()?.to_string_lossy().into_owned();
    let config = file_config(&name);
    Some((config.invite?, config.server.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{FileConfig, file_config, load_settings_from, save_settings_to};
    use crate::Settings;

    const KEY: &str = "eb4a40cdaac1e2d3948c053723f7a42f9aa6bcbcc7520f228cea427a15070fe3";

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nuvira-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// APP-10: settings written whole or not at all; a file cut short is kept
    /// aside and the permissions of the PC stay off, not on.
    #[test]
    fn bad_settings_file_kept_aside() {
        let dir = test_dir("settings");
        let path = dir.join("app.json");
        // None yet: the defaults.
        let fresh = load_settings_from(&path);
        assert!(!fresh.no_terminal && fresh.server.is_empty());

        let saved = Settings { server: "nuvira.example.com".into(), no_print: true, ..Settings::default() };
        save_settings_to(&path, &saved).unwrap();
        assert!(!path.with_extension("json.part").exists(), "renamed into place");
        let loaded = load_settings_from(&path);
        assert_eq!(loaded.server, "nuvira.example.com");
        assert!(loaded.no_print && !loaded.no_terminal);

        // Cut short, as by a crash in the middle of a write in place.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, &text[..text.len() / 2]).unwrap();
        let loaded = load_settings_from(&path);
        assert!(
            loaded.no_privacy
                && loaded.no_terminal
                && loaded.no_tunnel
                && loaded.no_print
                && loaded.no_recording
        );
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.bad")).unwrap(),
            &text[..text.len() / 2]
        );
        // Saved at once: the next start does not fall back to the open defaults.
        let again = load_settings_from(&path);
        assert!(again.no_terminal && again.no_recording);
        // Not text at all.
        std::fs::write(&path, [0xffu8, 0xfe, 0x00]).unwrap();
        assert!(load_settings_from(&path).no_tunnel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// APP-6: the sign-in token is saved protected; a plain one of an older
    /// version is read once and saved protected.
    #[test]
    fn token_saved_protected() {
        let dir = test_dir("token");
        let path = dir.join("app.json");
        let token = "0123456789abcdef-token";
        std::fs::write(&path, format!(r#"{{"server": "nuvira.example.com", "token": "{token}"}}"#)).unwrap();
        let loaded = load_settings_from(&path);
        assert_eq!(loaded.token.as_deref(), Some(token));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(token) && !text.contains("\"token\""), "{text}");
        assert!(text.contains("\"token_protected\""), "{text}");
        assert_eq!(load_settings_from(&path).token.as_deref(), Some(token));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // A new sign-in, then a sign-out.
        let mut settings = load_settings_from(&path);
        settings.token = Some("another".into());
        save_settings_to(&path, &settings).unwrap();
        assert_eq!(load_settings_from(&path).token.as_deref(), Some("another"));
        settings.token = None;
        save_settings_to(&path, &settings).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("token"), "{text}");
        assert_eq!(load_settings_from(&path).token, None);

        // Protected for someone else (or garbage): signed out, nothing else lost.
        std::fs::write(&path, r#"{"server": "s", "no_print": true, "token_protected": "dpapi:00"}"#).unwrap();
        let loaded = load_settings_from(&path);
        assert_eq!(loaded.token, None);
        assert!(loaded.no_print && loaded.server == "s");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        // The program of an invitation's page.
        let c = file_config(&format!("NuviraHub-host=nuvira.example.com,port=21216,key={KEY},invite=abcdefghjk (1).exe"));
        assert_eq!((c.server.as_deref(), c.invite.as_deref()), (Some("nuvira.example.com:21216"), Some("abcdefghjk")));
        assert_eq!(file_config("NuviraHub-host=a.example,invite=short.exe").invite, None);
    }
}
