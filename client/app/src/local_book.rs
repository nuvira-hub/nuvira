//! Address book kept on this computer, in `address-book.json` next to `app.json`.
//! It is the only one with the open server; with a server that has the address book
//! module (NuviraHub Pro) it is shown next to the server's books.
//!
//! The saved passwords are protected with DPAPI: only the same Windows user on the
//! same computer can read them back. Entries have negative IDs so that they never
//! collide with the server's.

use nuvira_i18n::t;
use serde::{Deserialize, Serialize};

use crate::address_book::{BookUi, EntryUi};

/// ID of the local book in the interface.
pub const BOOK_ID: i64 = -1;

#[derive(Debug, Default, Serialize, Deserialize)]
struct BookFile {
    #[serde(default)]
    entries: Vec<Stored>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    id: i64,
    device_id: String,
    name: String,
    company: String,
    contact: String,
    email: String,
    phone: String,
    notes: String,
    /// "dpapi:<hex>", or empty.
    password: String,
    group: String,
    tags: Vec<String>,
}

fn path() -> std::path::PathBuf {
    nuvira_transport::identity::data_dir().join("address-book.json")
}

fn load() -> BookFile {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

fn save(book: &BookFile) -> Result<(), String> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(book).map_err(|e| e.to_string())?;
    // Written aside and renamed: a crash halfway does not lose the book.
    let part = path.with_extension("json.part");
    std::fs::write(&part, text).map_err(|e| e.to_string())?;
    std::fs::rename(&part, &path).map_err(|e| e.to_string())
}

/// The local book as the interface shows it (passwords readable).
pub fn book() -> BookUi {
    let mut entries: Vec<EntryUi> = load()
        .entries
        .into_iter()
        .map(|s| EntryUi {
            id: s.id,
            book_id: BOOK_ID,
            device_id: s.device_id,
            name: s.name,
            company: s.company,
            contact: s.contact,
            email: s.email,
            phone: s.phone,
            notes: s.notes,
            password: unprotect(&s.password),
            group: s.group,
            tags: s.tags,
            online: false,
            last_seen: 0,
        })
        .collect();
    // Like the server: by group, then by name.
    entries.sort_by_key(|e| (e.group.to_lowercase(), e.name.to_lowercase(), e.device_id.clone()));
    BookUi { id: BOOK_ID, name: String::new(), personal: false, local: true, member_ids: Vec::new(), entries }
}

/// Adds (id 0) or changes an entry.
pub fn put(entry: EntryUi) -> Result<(), String> {
    let device_id: String = entry.device_id.chars().filter(|c| c.is_ascii_digit()).collect();
    if device_id.len() != 9 {
        return Err(t("err.id_digits"));
    }
    let mut book = load();
    if book.entries.iter().any(|e| e.device_id == device_id && e.id != entry.id) {
        return Err(t("err.already_in_book"));
    }
    let id = if entry.id == 0 {
        book.entries.iter().map(|e| e.id).min().unwrap_or(0).min(0) - 1
    } else {
        entry.id
    };
    let stored = Stored {
        id,
        device_id,
        name: entry.name.trim().to_string(),
        company: entry.company.trim().to_string(),
        contact: entry.contact.trim().to_string(),
        email: entry.email.trim().to_string(),
        phone: entry.phone.trim().to_string(),
        notes: entry.notes.trim().to_string(),
        password: if entry.password.is_empty() { String::new() } else { protect(&entry.password)? },
        group: entry.group.trim().to_string(),
        tags: entry.tags.iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
    };
    match book.entries.iter_mut().find(|e| e.id == id) {
        Some(existing) => *existing = stored,
        None if entry.id == 0 => book.entries.push(stored),
        None => return Err(t("err.entry_not_found")),
    }
    save(&book)
}

pub fn delete(id: i64) -> Result<(), String> {
    let mut book = load();
    let before = book.entries.len();
    book.entries.retain(|e| e.id != id);
    if book.entries.len() == before {
        return Err(t("err.entry_not_found"));
    }
    save(&book)
}

/// Adds a device reached for the first time, if it is not in the book yet.
pub fn auto_add(device_id: &str, name: &str, password: &str) -> Result<bool, String> {
    if load().entries.iter().any(|e| e.device_id == device_id) {
        return Ok(false);
    }
    let entry = EntryUi {
        device_id: device_id.to_string(),
        name: name.to_string(),
        password: password.to_string(),
        ..Default::default()
    };
    put(entry).map(|()| true)
}

#[cfg(windows)]
fn protect(plain: &str) -> Result<String, String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };
    use windows::core::PCWSTR;

    let input = CRYPT_INTEGER_BLOB { cbData: plain.len() as u32, pbData: plain.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points to `plain`, alive for the call; output is freed below.
    unsafe {
        CryptProtectData(&input, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)
            .map_err(|e| e.to_string())?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let text = format!("dpapi:{}", hex(bytes));
        LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(text)
    }
}

/// Empty if the password cannot be read (another user or computer).
#[cfg(windows)]
fn unprotect(stored: &str) -> String {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let Some(bytes) = stored.strip_prefix("dpapi:").and_then(unhex) else {
        return String::new();
    };
    let input = CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points to `bytes`, alive for the call; output is freed below.
    unsafe {
        if CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output).is_err()
        {
            return String::new();
        }
        let plain =
            String::from_utf8_lossy(std::slice::from_raw_parts(output.pbData, output.cbData as usize))
                .into_owned();
        LocalFree(Some(HLOCAL(output.pbData.cast())));
        plain
    }
}

#[cfg(not(windows))]
fn protect(_plain: &str) -> Result<String, String> {
    Err(t("err.unexpected"))
}

#[cfg(not(windows))]
fn unprotect(_stored: &str) -> String {
    String::new()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let bytes = [0u8, 1, 0x7f, 0xff];
        assert_eq!(unhex(&hex(&bytes)).unwrap(), bytes);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip() {
        let stored = protect("pàssword").unwrap();
        assert!(stored.starts_with("dpapi:") && !stored.contains("pàssword"));
        assert_eq!(unprotect(&stored), "pàssword");
        assert_eq!(unprotect("dpapi:00"), "");
        assert_eq!(unprotect(""), "");
    }
}
