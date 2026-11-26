use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub adapt_resolution: bool,
}

fn path() -> std::path::PathBuf {
    nuvira_transport::identity::data_dir().join("viewer.json")
}

pub fn load() -> Prefs {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(prefs: &Prefs) {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = serde_json::to_vec_pretty(prefs).map(|b| std::fs::write(&path, b)) {
        tracing::warn!("{}: {e}", path.display());
    }
}
