//! What the user services of Linux (systemd) and macOS (launchd) share: the
//! states shown by the app, the install options and the permanent password,
//! kept as a hash in `service.env` (mode 0600) in the user's data folder.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The same states as the Windows service, for the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    Running,
    Changing,
}

pub struct InstallOptions {
    pub server: String,
    pub server_key: Option<String>,
    /// `None`: the current one is kept, or one is generated the first time.
    pub password: Option<String>,
}

pub(crate) fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME not set")
}

pub(crate) fn env_path() -> PathBuf {
    nuvira_transport::identity::data_dir().join("service.env")
}

const HASH_VAR: &str = "NUVIRA_PASSWORD_HASH=";

/// Saves the permanent password chosen in `opts`, keeps the current one, or
/// generates one the first time. Returns it when generated, to show it once.
pub(crate) fn store_password(opts: &InstallOptions) -> Result<Option<String>> {
    let store = |password: &str| {
        write_private(&env_path(), &format!("{HASH_VAR}{}\n", nuvira_secret::hash(password)))
    };
    Ok(match (&opts.password, env_path().exists()) {
        (Some(password), _) => {
            store(password)?;
            None
        }
        (None, true) => None,
        (None, false) => {
            let password = crate::random_password();
            store(&password)?;
            Some(password)
        }
    })
}

/// The hash saved by [`store_password`].
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn password_hash() -> Result<String> {
    let text = std::fs::read_to_string(env_path()).context("service.env")?;
    text.lines()
        .find_map(|l| l.strip_prefix(HASH_VAR))
        .map(|h| h.trim().to_string())
        .context("no password in service.env")
}

/// The service runs this same program: there is never an older installed one.
pub fn installed_version() -> Option<String> {
    None
}

pub(crate) fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file =
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(text.as_bytes())?;
    // An existing file keeps its mode with `mode()`: set it again.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}
