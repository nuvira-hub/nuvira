//! Text typed by people or read from files: the device to connect to
//! (`signal::target` / `valid_target`), fingerprints, two-factor
//! secrets and codes, stored password hashes (bounded cost: only small iteration
//! counts reach PBKDF2). Never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let t = nuvira_transport::signal::target(text);
    if nuvira_transport::signal::valid_target(&t) {
        assert!(t.is_ascii());
    }
    let _ = nuvira_transport::parse_fingerprint(text);
    let mid = (0..=text.len() / 2).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let (secret, code) = text.split_at(mid);
    let _ = nuvira_secret::totp_code(secret, 1);
    let _ = nuvira_secret::totp_verify(secret, code, 1_000_000_000);
    let cheap = text.split('$').nth(1).and_then(|n| n.parse::<u32>().ok()).is_none_or(|n| n <= 64);
    if cheap {
        let _ = nuvira_secret::verify(code, text);
    }
});
