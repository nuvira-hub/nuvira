//! Service configuration and paths.
//!
//! - Programs: `%ProgramFiles%\nuvira` (writable by administrators only: the
//!   service runs as SYSTEM and must not run files that users can modify).
//! - Data: `%ProgramData%\nuvira`, accessible only to SYSTEM and administrators:
//!   it holds the device key (hence the ID) and the permanent password.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const SERVICE_NAME: &str = "nuvira";
pub const SERVICE_DISPLAY_NAME: &str = "NuviraHub";
pub const SERVICE_DESCRIPTION: &str =
    "Unattended remote access with NuviraHub: starts the agent in the session of the person using the PC.";
/// The single program installed: the app, which also contains agent, viewer and
/// service (`nuvira.exe agent ...`, `nuvira.exe service run`).
pub const PROGRAM_EXE: &str = "nuvira.exe";
/// Programs of earlier installations, removed when updating.
pub const OLD_EXES: [&str; 2] = ["nuvira-agent.exe", "nuvira-service.exe"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// nuvira-server to register with (host[:port]).
    pub server: String,
    /// Server certificate fingerprint; without it, the one seen at the first connection is remembered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_key: Option<String>,
    /// Permanent password for unattended access, as a hash (`nuvira_secret`).
    /// Earlier versions stored it in clear text under "password": converted at load.
    #[serde(alias = "password")]
    pub password_hash: String,
    /// auto, udp or websocket.
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_ws: Option<String>,
}

fn default_transport() -> String {
    "auto".into()
}

impl ServiceConfig {
    pub fn load() -> Result<Self> {
        let path = config_path();
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self =
            serde_json::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
        if !nuvira_secret::is_hash(&config.password_hash) {
            // Written by an earlier version: from now on only the hash is kept.
            config.password_hash = nuvira_secret::hash(&config.password_hash);
            config.save()?;
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

fn env_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| fallback.into()).join("nuvira")
}

pub fn program_dir() -> PathBuf {
    env_dir("ProgramFiles", r"C:\Program Files")
}

pub fn data_dir() -> PathBuf {
    env_dir("ProgramData", r"C:\ProgramData")
}

pub fn config_path() -> PathBuf {
    data_dir().join("service.json")
}

/// Written by the agent as soon as it is registered on the server: "ID password".
pub fn online_path() -> PathBuf {
    data_dir().join("online")
}

pub fn identity_path() -> PathBuf {
    data_dir().join("device.key")
}

pub fn service_log() -> PathBuf {
    data_dir().join("service.log")
}

pub fn agent_log() -> PathBuf {
    data_dir().join("agent.log")
}

/// Present while the agent has a session in progress: updates wait for it to end.
pub fn busy_path() -> PathBuf {
    data_dir().join("busy")
}

/// Downloaded updates, in the protected data folder: nobody can swap them before they run.
pub fn update_dir() -> PathBuf {
    data_dir().join("update")
}

/// Copy of the program that ran well, kept by the service: an update stopped halfway
/// may lose the program in Program Files, and the rescue task puts this one back.
pub fn backup_program() -> PathBuf {
    data_dir().join("backup").join(PROGRAM_EXE)
}

pub fn update_log() -> PathBuf {
    data_dir().join("update.log")
}

/// Moves a log that is too large to `.1` (only the previous generation is kept).
pub fn rotate_log(path: &Path, max_bytes: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

/// Random 8-character password that is easy to dictate, like the agent's.
pub fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("random generator");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}
