//! User interface strings (decision D15): English by default, plus Italian,
//! French, German and Spanish.
//!
//! - Strings live in `locales/<lang>.json` as flat `key -> text` maps; English is
//!   the reference and the fallback for missing keys.
//! - The language is chosen with [`init`]: an explicit choice, else `NEXODESK_LANG`,
//!   else the Windows display language if supported, else English. Calling it
//!   again changes the language (choice made in the app's settings).
//! - Placeholders are written `{name}` and filled by [`tf`].
//! - Messages that cross process boundaries (agent -> viewer, viewer -> app) can
//!   carry a key prefixed with `@` instead of text: [`message`] translates them.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// Supported languages: code and native name.
pub const LANGUAGES: [(&str, &str); 5] =
    [("en", "English"), ("it", "Italiano"), ("fr", "Français"), ("de", "Deutsch"), ("es", "Español")];

const SOURCES: [(&str, &str); 5] = [
    ("en", include_str!("../locales/en.json")),
    ("it", include_str!("../locales/it.json")),
    ("fr", include_str!("../locales/fr.json")),
    ("de", include_str!("../locales/de.json")),
    ("es", include_str!("../locales/es.json")),
];

static LANG: RwLock<Option<&'static str>> = RwLock::new(None);
static TABLES: OnceLock<HashMap<&'static str, HashMap<String, String>>> = OnceLock::new();

fn tables() -> &'static HashMap<&'static str, HashMap<String, String>> {
    TABLES.get_or_init(|| {
        SOURCES
            .iter()
            .map(|(lang, json)| (*lang, serde_json::from_str(json).expect("valid locale JSON")))
            .collect()
    })
}

fn supported(code: &str) -> Option<&'static str> {
    let code = code.trim().to_ascii_lowercase();
    let base = code.split(['-', '_', '.']).next().unwrap_or_default();
    LANGUAGES.iter().map(|(c, _)| *c).find(|c| *c == base)
}

/// Chooses (or changes) the language. `preference` is a language code or
/// `None`/"auto" (then `NEXODESK_LANG`, then the system language).
pub fn init(preference: Option<&str>) -> &'static str {
    let chosen = preference
        .filter(|p| *p != "auto")
        .and_then(supported)
        .or_else(|| std::env::var("NEXODESK_LANG").ok().as_deref().and_then(supported))
        .unwrap_or_else(system_language);
    *LANG.write().expect("language") = Some(chosen);
    chosen
}

/// Current language (chosen automatically on first use if [`init`] was not called).
pub fn lang() -> &'static str {
    let current = *LANG.read().expect("language");
    current.unwrap_or_else(|| init(None))
}

/// Language of the operating system if supported, else English.
pub fn system_language() -> &'static str {
    #[cfg(windows)]
    {
        // Primary language identifiers (low 10 bits of the LANGID).
        let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() } & 0x3ff;
        match id {
            0x10 => "it",
            0x0c => "fr",
            0x07 => "de",
            0x0a => "es",
            _ => "en",
        }
    }
    #[cfg(not(windows))]
    {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|v| std::env::var(v).ok().as_deref().and_then(supported))
            .unwrap_or("en")
    }
}

/// Text for `key` in the current language (English if missing, the key itself if unknown).
pub fn t(key: &str) -> String {
    let tables = tables();
    tables
        .get(lang())
        .and_then(|t| t.get(key))
        .or_else(|| tables["en"].get(key))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

/// Like [`t`], filling `{name}` placeholders.
pub fn tf(key: &str, args: &[(&str, &str)]) -> String {
    let mut text = t(key);
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// A message received from another program: `@key` is translated, anything else is shown as is.
pub fn message(text: &str) -> String {
    match text.strip_prefix('@') {
        Some(key) if tables()["en"].contains_key(key) => t(key),
        _ => text.to_string(),
    }
}

/// All strings of the current language (English for missing keys), as JSON, for web pages.
pub fn json() -> String {
    let tables = tables();
    let mut merged = tables["en"].clone();
    if let Some(current) = tables.get(lang()) {
        merged.extend(current.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    serde_json::to_string(&merged).expect("strings serialise")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_every_key() {
        let tables = tables();
        let en = &tables["en"];
        for (lang, _) in LANGUAGES {
            let table = &tables[lang];
            let missing: Vec<_> = en.keys().filter(|k| !table.contains_key(*k)).collect();
            let extra: Vec<_> = table.keys().filter(|k| !en.contains_key(*k)).collect();
            assert!(missing.is_empty() && extra.is_empty(), "{lang}: missing {missing:?}, extra {extra:?}");
            // Same placeholders as English.
            for (key, text) in table {
                let names = |s: &str| {
                    let mut v: Vec<String> = s
                        .split('{')
                        .skip(1)
                        .filter_map(|p| p.split_once('}').map(|(n, _)| n.to_string()))
                        .collect();
                    v.sort();
                    v
                };
                assert_eq!(names(text), names(&en[key]), "{lang}: placeholders of {key}");
            }
        }
    }

    #[test]
    fn language_codes() {
        assert_eq!(supported("it-IT"), Some("it"));
        assert_eq!(supported("de_DE.UTF-8"), Some("de"));
        assert_eq!(supported("pt-BR"), None);
    }
}
