//! Configurazione e percorsi del servizio.
//!
//! - Programmi: `%ProgramFiles%\NexoDesk` (scrivibile solo dagli amministratori:
//!   il servizio gira come SYSTEM e non deve eseguire file modificabili da utenti).
//! - Dati: `%ProgramData%\NexoDesk` con accesso solo per SYSTEM e amministratori:
//!   contiene la chiave del dispositivo (quindi l'ID) e la password permanente.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const SERVICE_NAME: &str = "NexoDesk";
pub const SERVICE_DISPLAY_NAME: &str = "NexoDesk";
pub const SERVICE_DESCRIPTION: &str =
    "Accesso remoto non presidiato con NexoDesk: avvia l'agent nella sessione di chi usa il PC.";
pub const AGENT_EXE: &str = "nexodesk-agent.exe";
pub const SERVICE_EXE: &str = "nexodesk-service.exe";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// nexodesk-server a cui registrarsi (host[:porta]).
    pub server: String,
    /// Impronta del certificato del server; senza, si memorizza quella vista al primo collegamento.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_key: Option<String>,
    /// Password permanente per l'accesso non presidiato.
    pub password: String,
    /// auto, udp o websocket.
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
        let text = std::fs::read_to_string(&path).with_context(|| format!("lettura di {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("{} non valido", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("scrittura di {}", path.display()))
    }
}

fn env_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| fallback.into()).join("NexoDesk")
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

/// Scritto dall'agent appena registrato sul server: "ID password".
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

/// Sposta un log troppo grande in `.1` (si tiene solo la generazione precedente).
pub fn rotate_log(path: &Path, max_bytes: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

/// Password casuale di 8 caratteri facili da dettare, come quelle dell'agent.
pub fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("generatore casuale");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}
