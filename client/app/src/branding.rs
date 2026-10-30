//! Customisation of a NuviraHub Pro installer (see nuvira_service::branding):
//! name, logo, colour, support contact, preset and locked server and language,
//! and the kind of program ("quick": quick support, only this computer's ID and
//! password; "full": the whole app).
//!
//! Read from the program at start-up and kept in the app's data folder, because
//! an automatic update replaces the program with the standard one; an installed
//! program also has the copy next to it.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Branding {
    pub kind: String,
    pub name: String,
    /// data:image/png;base64,…
    pub logo: String,
    /// #rrggbb
    pub color: String,
    pub support_text: String,
    pub support_phone: String,
    pub support_email: String,
    pub support_url: String,
    pub server: String,
    pub server_key: String,
    pub lock_server: bool,
    pub language: String,
    pub lock_language: bool,
}

impl Branding {
    /// Drops what is not valid: the document comes from a file anyone can edit.
    fn cleaned(mut self) -> Self {
        let cut = |s: &mut String, n: usize| {
            *s = s.trim().chars().take(n).collect();
        };
        cut(&mut self.name, 60);
        cut(&mut self.support_text, 500);
        cut(&mut self.support_phone, 40);
        cut(&mut self.support_email, 120);
        cut(&mut self.support_url, 300);
        cut(&mut self.server, 255);
        cut(&mut self.language, 5);
        if !matches!(self.kind.as_str(), "quick" | "full" | "auto") {
            self.kind = "full".into();
        }
        let hex = self.color.strip_prefix('#').unwrap_or("");
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            self.color.clear();
        }
        if !self.logo.starts_with("data:image/png;base64,") || self.logo.len() > 700_000 {
            self.logo.clear();
        }
        if !self.support_url.starts_with("https://") {
            self.support_url.clear();
        }
        if !self.server_key.is_empty() && nuvira_transport::parse_fingerprint(&self.server_key).is_err() {
            self.server_key.clear();
        }
        if !nuvira_i18n::LANGUAGES.iter().any(|(code, _)| *code == self.language) {
            self.language.clear();
            self.lock_language = false;
        }
        if self.server.is_empty() {
            self.lock_server = false;
        }
        self
    }

    pub fn quick(&self) -> bool {
        self.kind == "quick"
    }
}

static BRANDING: OnceLock<Option<Branding>> = OnceLock::new();

/// The customisation of this program, if any (loaded once).
pub fn get() -> Option<&'static Branding> {
    BRANDING.get_or_init(load).as_ref()
}

fn saved_path() -> std::path::PathBuf {
    nuvira_transport::identity::data_dir().join("branding.json")
}

fn load() -> Option<Branding> {
    let parse = |doc: &[u8]| serde_json::from_slice::<Branding>(doc).ok().map(Branding::cleaned);
    if let Some(doc) = std::env::current_exe().ok().and_then(|p| nuvira_service::branding::read_trailer(&p)) {
        match parse(&doc) {
            Some(b) => {
                info!("customised program: {}", b.name);
                let path = saved_path();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) = std::fs::write(&path, &doc) {
                    warn!("customisation not saved: {e}");
                }
                return Some(b);
            }
            None => warn!("customisation of the program not readable"),
        }
    }
    for path in [saved_path(), nuvira_service::branding::installed_path()] {
        if let Some(b) = std::fs::read(&path).ok().and_then(|doc| parse(&doc)) {
            return Some(b);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_fields_are_dropped() {
        let doc = r#"{"kind":"evil","name":"Rossi","color":"red","logo":"data:image/svg+xml;base64,AA",
            "support_url":"http://x","server_key":"123","language":"xx","lock_language":true,
            "server":"","lock_server":true}"#;
        let b = serde_json::from_str::<Branding>(doc).unwrap().cleaned();
        assert_eq!(b.kind, "full");
        assert_eq!(b.name, "Rossi");
        assert!(b.color.is_empty() && b.logo.is_empty() && b.support_url.is_empty() && b.server_key.is_empty());
        assert!(b.language.is_empty() && !b.lock_language && !b.lock_server);

        let key = "ab".repeat(32);
        let doc = format!(
            r##"{{"kind":"quick","color":"#B5442A","support_url":"https://rossi.example","server":"s.example",
            "server_key":"{key}","lock_server":true,"language":"it"}}"##
        );
        let b = serde_json::from_str::<Branding>(&doc).unwrap().cleaned();
        assert!(b.quick() && b.lock_server);
        assert_eq!((b.color.as_str(), b.server_key.as_str(), b.language.as_str()), ("#B5442A", key.as_str(), "it"));
    }
}
