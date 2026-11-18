//! An update manifest as the server (or anyone on the path, the channel is plain
//! HTTP) can send it: JSON parsing, signature check against the built-in keys,
//! version comparison and the program path. Never a panic, and never accepted
//! (nobody but the release keys can sign).
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuvira_update::{CURRENT, Manifest, PUBLIC_KEYS, newer};

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = serde_json::from_slice::<Manifest>(data) {
        assert!(m.verify_any(&PUBLIC_KEYS).is_err(), "a forged manifest was accepted: {m:?}");
        let _ = m.program_path();
        let _ = newer(&m.version, CURRENT);
        let _ = newer(CURRENT, &m.version);
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let mid = (0..=text.len() / 2).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
        let (a, b) = text.split_at(mid);
        // Antisymmetric: never both newer than the other.
        assert!(!(newer(a, b) && newer(b, a)));
        let _ = nuvira_update::base_url(a, Some(b).filter(|b| b.starts_with("ws")), 21116);
    }
});
