//! Characterisation tests (Phase 2 of the full review) of the permanent password
//! hash (PBKDF2) and of the two-factor codes (TOTP): edge cases of the formats
//! accepted, pinned as they are now (the stored hash's parameters since the
//! review's fix of S15: see `verify_refuses_parameters_outside_the_ranges`).

use std::num::NonZeroU32;

use nuvira_secret::{
    TOTP_STEP, hash, is_hash, is_valid_hash, totp_code, totp_secret, totp_uri, totp_verify, verify,
};
use proptest::prelude::*;
use ring::pbkdf2;

/// RFC 6238 SHA-1 key "12345678901234567890" in base32.
const RFC: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// A stored hash made by hand, with any iterations, salt and output length.
fn stored(password: &str, iterations: u32, salt: &[u8], len: usize) -> String {
    let mut out = vec![0u8; len];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(iterations).unwrap(),
        salt,
        password.as_bytes(),
        &mut out,
    );
    format!("pbkdf2-sha256${iterations}${}${}", hex(salt), hex(&out))
}

#[test]
fn hash_format() {
    let h = hash("pässwörd ✓");
    let parts: Vec<&str> = h.split('$').collect();
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0], "pbkdf2-sha256");
    assert_eq!(parts[1], "300000");
    assert_eq!(parts[2].len(), 32, "16 bytes of salt");
    assert_eq!(parts[3].len(), 64, "32 bytes of hash");
    assert!(parts[2..].iter().all(|p| p.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())));
    assert!(verify("pässwörd ✓", &h));
    assert!(!verify("pässwörd", &h));
    assert!(is_hash(&h) && !is_hash("pbkdf2-sha256") && is_hash("pbkdf2-sha256$garbage"));
    assert!(is_valid_hash(&h) && !is_valid_hash("pbkdf2-sha256$garbage"));
}

#[test]
fn verify_refuses_parameters_outside_the_ranges() {
    // Fixed in the review (S15, APP-7): the stored text no longer decides the
    // cost or the strength. Iterations 100,000..=10,000,000 in plain digits, a
    // salt of at least 16 bytes and exactly 32 bytes of output; everything that
    // `hash()` makes is inside.
    let salt = [7u8; 16];
    let good = stored("p", 100_000, &salt, 32);
    assert!(verify("p", &good) && is_valid_hash(&good));
    assert!(!verify("q", &good));
    // Upper-case hex is still read.
    let upper = good.to_uppercase().replace("PBKDF2-SHA256", "pbkdf2-sha256");
    assert!(verify("p", &upper));
    // A longer salt is fine.
    assert!(verify("p", &stored("p", 100_000, &[1u8; 32], 32)));
    // What was accepted before: one iteration, an empty or short salt, a short
    // or long output, "+N" as the count. All refused now, whatever the password.
    for bad in [
        stored("p", 1, b"", 32),
        stored("p", 1, b"salt", 4),
        stored("p", 2, b"salt", 100),
        stored("p", 99_999, &salt, 32),
        stored("p", 100_000, &salt[..15], 32),
        stored("p", 100_000, &salt, 31),
        stored("p", 100_000, &salt, 33),
        stored("p", 100_000, &salt, 32).replacen("$100000$", "$+100000$", 1),
        stored("p", 100_000, &salt, 32).replacen("$100000$", "$ 100000$", 1),
        // Above the range: refused before any work is done.
        "pbkdf2-sha256$10000001$07070707070707070707070707070707$0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        "pbkdf2-sha256$4294967295$07070707070707070707070707070707$0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        "pbkdf2-sha256$1$00$".to_string(),
    ] {
        assert!(!verify("p", &bad), "{bad}");
        assert!(!is_valid_hash(&bad), "{bad}");
        // Still recognised as a hash (not a password in clear text).
        assert!(is_hash(&bad));
    }
    // A count far above the range is refused at once.
    let started = std::time::Instant::now();
    assert!(!verify(
        "p",
        "pbkdf2-sha256$4294967295$07070707070707070707070707070707$0000000000000000000000000000000000000000000000000000000000000000"
    ));
    assert!(started.elapsed() < std::time::Duration::from_millis(50));
    // Five parts, odd hex, non-ASCII in the hex, a zero or negative count,
    // the prefix in capitals: refused, as before.
    for bad in [
        format!("{good}$x"),
        "pbkdf2-sha256$100000$0$00".to_string(),
        "pbkdf2-sha256$100000$é0$00".to_string(),
        "pbkdf2-sha256$0$00$00".to_string(),
        "pbkdf2-sha256$-1$00$00".to_string(),
        good.replace("pbkdf2-sha256", "PBKDF2-SHA256"),
    ] {
        assert!(!verify("p", &bad), "{bad}");
        assert!(!is_valid_hash(&bad), "{bad}");
    }
}

#[test]
fn totp_secret_formats() {
    let code = totp_code(RFC, 1).unwrap();
    // Lower case, spaces, dashes and '=' padding are accepted.
    assert_eq!(totp_code(&RFC.to_lowercase(), 1).unwrap(), code);
    assert_eq!(totp_code("GEZD GNBV-GY3T QOJQ GEZD GNBV GY3T QOJQ====", 1).unwrap(), code);
    // Not base32 (0, 1, 8, 9 and symbols), or too short for one byte: no code.
    for bad in ["", "A", "====", "GEZ1", "GEZ0", "GE_Z", "GEZ\u{e9}"] {
        assert_eq!(totp_code(bad, 1), None, "{bad:?}");
    }
    // Two characters = one byte of key: a code (no minimum key length).
    assert!(totp_code("AB", 1).is_some());
    let fresh = totp_secret();
    assert!(fresh.bytes().all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b)));
}

#[test]
fn totp_window_edges() {
    let code0 = totp_code(RFC, 0).unwrap();
    // Unix time 0..29 is step 0: steps "0, 0, 1" are checked (no underflow).
    assert_eq!(totp_verify(RFC, &code0, 0), Some(0));
    assert_eq!(totp_verify(RFC, &code0, 29), Some(0));
    assert_eq!(totp_verify(RFC, &code0, 59), Some(0), "one step after");
    assert_eq!(totp_verify(RFC, &code0, 60), None, "two steps after");
    // The far future: no overflow.
    let last = u64::MAX / TOTP_STEP;
    let code = totp_code(RFC, last).unwrap();
    assert_eq!(totp_verify(RFC, &code, u64::MAX), Some(last));
    // One step before is accepted too.
    let c = totp_code(RFC, 1000).unwrap();
    assert_eq!(totp_verify(RFC, &c, 1001 * TOTP_STEP), Some(1000));
    assert_eq!(totp_verify(RFC, &c, 999 * TOTP_STEP), Some(1000));
}

#[test]
fn totp_code_formats() {
    let code = totp_code(RFC, 1).unwrap();
    let spaced = format!("{} {}", &code[..3], &code[3..]);
    assert!(totp_verify(RFC, &spaced, TOTP_STEP).is_some());
    let tabbed = format!("\t{}\n{}\r\n", &code[..3], &code[3..]);
    assert!(totp_verify(RFC, &tabbed, TOTP_STEP).is_some(), "any whitespace is removed");
    // Full-width digits, a sign, a dash: refused.
    let wide: String =
        code.chars().map(|c| char::from_u32(c as u32 - '0' as u32 + 0xff10).unwrap()).collect();
    assert_eq!(totp_verify(RFC, &wide, TOTP_STEP), None);
    assert_eq!(totp_verify(RFC, &format!("+{}", &code[1..]), TOTP_STEP), None);
    assert_eq!(totp_verify(RFC, &format!("{}-{}", &code[..3], &code[3..]), TOTP_STEP), None);
    // A secret that is not base32: never a match.
    assert_eq!(totp_verify("!!", "000000", 0), None);
}

#[test]
fn totp_uri_escaping() {
    assert_eq!(
        totp_uri("S", "Nuvira Hub: ü", "a@b/c?d"),
        "otpauth://totp/Nuvira%20Hub%3A%20%C3%BC:a%40b%2Fc%3Fd?secret=S&issuer=Nuvira%20Hub%3A%20%C3%BC&digits=6&period=30"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 500, ..ProptestConfig::default() })]

    #[test]
    fn verify_never_panics_on_arbitrary_text(text in ".{0,80}", password in ".{0,20}") {
        // Bounded cost: only texts whose count is small get as far as PBKDF2.
        let cheap = text.split('$').nth(1).and_then(|n| n.parse::<u32>().ok()).is_none_or(|n| n <= 1000);
        if cheap {
            let _ = verify(&password, &text);
        }
        let _ = is_hash(&text);
    }

    #[test]
    fn totp_never_panics(secret in ".{0,40}", code in ".{0,10}", now in any::<u64>(), step in any::<u64>()) {
        let _ = totp_code(&secret, step);
        let _ = totp_verify(&secret, &code, now);
    }

    #[test]
    fn totp_codes_are_six_digits_and_verify(step in 1u64..u64::MAX / TOTP_STEP) {
        let code = totp_code(RFC, step).unwrap();
        prop_assert_eq!(code.len(), 6);
        prop_assert!(code.bytes().all(|b| b.is_ascii_digit()));
        // The earliest matching step of (step - 1, step, step + 1) is returned.
        let earliest = if totp_code(RFC, step - 1).as_deref() == Some(code.as_str()) { step - 1 } else { step };
        prop_assert_eq!(totp_verify(RFC, &code, step * TOTP_STEP), Some(earliest));
    }
}
