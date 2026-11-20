//! The permanent password of unattended access is stored only as a PBKDF2-SHA256
//! hash with a random salt: whoever reads the service configuration (administrators,
//! a backup) does not learn it. Format: `pbkdf2-sha256$<iterations>$<salt>$<hash>`
//! (hexadecimal).
//!
//! Also the time-based codes (TOTP, RFC 6238) of the two-factor check of
//! unattended access: 6 digits, 30 seconds, HMAC-SHA1, as every authenticator app
//! expects.

use std::num::NonZeroU32;

use ring::{hmac, pbkdf2};
use ring::rand::{SecureRandom, SystemRandom};

/// Each check costs a fraction of a second on an old PC: fine for a sign-in, slow
/// for whoever tries passwords against a copy of the file.
const ITERATIONS: u32 = 300_000;
const ALGORITHM: pbkdf2::Algorithm = pbkdf2::PBKDF2_HMAC_SHA256;

/// Hash to store for `password`.
pub fn hash(password: &str) -> String {
    let mut salt = [0u8; 16];
    SystemRandom::new().fill(&mut salt).expect("system random generator");
    let mut out = [0u8; 32];
    let iterations = NonZeroU32::new(ITERATIONS).expect("non-zero");
    pbkdf2::derive(ALGORITHM, iterations, &salt, password.as_bytes(), &mut out);
    format!("pbkdf2-sha256${ITERATIONS}${}${}", hex(&salt), hex(&out))
}

/// The iterations a stored hash may ask for: every hash made by [`hash`] has
/// [`ITERATIONS`]; fewer would make a copied hash cheap to attack, more would
/// make each check take seconds (a typo with a few more digits, minutes).
const ITERATIONS_ACCEPTED: std::ops::RangeInclusive<u32> = 100_000..=10_000_000;
/// The shortest salt accepted (16 bytes in every hash made by [`hash`]).
const SALT_MIN: usize = 16;
/// The length of the derived key: 32 bytes, as made by [`hash`]. A shorter one
/// (a hash cut in a copy and paste) would match many passwords.
const OUTPUT_LEN: usize = 32;

/// The parameters of a stored hash, if it is well formed and within the ranges.
fn parse(hash: &str) -> Option<(NonZeroU32, Vec<u8>, Vec<u8>)> {
    let mut parts = hash.split('$');
    let (Some("pbkdf2-sha256"), Some(iterations), Some(salt), Some(expected), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    // Digits only: no sign, no spaces.
    if iterations.is_empty() || !iterations.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let iterations: u32 = iterations.parse().ok().filter(|n| ITERATIONS_ACCEPTED.contains(n))?;
    let (salt, expected) = (unhex(salt)?, unhex(expected)?);
    if salt.len() < SALT_MIN || expected.len() != OUTPUT_LEN {
        return None;
    }
    Some((NonZeroU32::new(iterations)?, salt, expected))
}

/// `password` matches the stored `hash` (constant-time comparison). A malformed
/// hash, or one with parameters outside the accepted ranges (iterations
/// 100,000 to 10,000,000, salt of at least 16 bytes, 32 bytes of output),
/// matches nothing.
pub fn verify(password: &str, hash: &str) -> bool {
    let Some((iterations, salt, expected)) = parse(hash) else {
        return false;
    };
    pbkdf2::verify(ALGORITHM, iterations, &salt, password.as_bytes(), &expected).is_ok()
}

/// The text is a stored hash (and not a password in clear text): only the
/// prefix, so that a malformed hash is never taken for a password.
pub fn is_hash(text: &str) -> bool {
    text.starts_with("pbkdf2-sha256$")
}

/// The text is a hash [`verify`] can match: well formed, with parameters in the
/// accepted ranges. What a policy or `deploy --password-hash` gives is checked
/// with this.
pub fn is_valid_hash(text: &str) -> bool {
    parse(text).is_some()
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

/// Seconds each code lasts.
pub const TOTP_STEP: u64 = 30;

/// A new TOTP secret: 160 random bits, base32 (what authenticator apps take).
pub fn totp_secret() -> String {
    let mut key = [0u8; 20];
    SystemRandom::new().fill(&mut key).expect("system random generator");
    base32_encode(&key)
}

/// The `otpauth://` URI of a secret, the content of the QR code.
pub fn totp_uri(secret: &str, issuer: &str, account: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let (issuer, account) = (enc(issuer), enc(account));
    format!("otpauth://totp/{issuer}:{account}?secret={secret}&issuer={issuer}&digits=6&period={TOTP_STEP}")
}

/// The 6-digit code of `secret` for the time step `step` (Unix time / 30).
pub fn totp_code(secret: &str, step: u64) -> Option<String> {
    let key = base32_decode(secret)?;
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &key), &step.to_be_bytes());
    let mac = tag.as_ref();
    let offset = (mac[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([mac[offset] & 0x7f, mac[offset + 1], mac[offset + 2], mac[offset + 3]]);
    Some(format!("{:06}", bin % 1_000_000))
}

/// The time step `code` belongs to, if it is right for `now` (Unix seconds) or one
/// step before or after (clocks a little apart). The caller refuses a step already
/// used, so a code works once.
pub fn totp_verify(secret: &str, code: &str, now: u64) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let now = now / TOTP_STEP;
    let mut found = None;
    // Every step checked, also after a match: the time taken does not tell which.
    for step in [now.saturating_sub(1), now, now + 1] {
        if let Some(expected) = totp_code(secret, step)
            && expected.bytes().zip(code.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
        {
            found.get_or_insert(step);
        }
    }
    found
}

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for &b in bytes {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(BASE32[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|c| !matches!(c, b' ' | b'-' | b'=')) {
        let v = BASE32.iter().position(|&a| a == c.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buffer >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totp() {
        // RFC 6238, appendix B (SHA-1 key "12345678901234567890"), last 6 digits.
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(secret, "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        let vectors = [(59, "287082"), (1111111109, "081804"), (1234567890, "005924"), (2000000000, "279037")];
        for (time, code) in vectors {
            assert_eq!(totp_code(&secret, time / TOTP_STEP).as_deref(), Some(code), "{time}");
            assert_eq!(totp_verify(&secret, code, time), Some(time / TOTP_STEP));
        }
        // One step either side is accepted, two are not.
        assert!(totp_verify(&secret, "287082", 59 + 30).is_some());
        assert!(totp_verify(&secret, "287082", 59 + 60).is_none());
        assert!(totp_verify(&secret, "287 082", 59).is_some());
        for wrong in ["", "28708", "2870822", "28708a", "000000"] {
            assert!(totp_verify(&secret, wrong, 59).is_none(), "{wrong}");
        }
        assert!(totp_code("not base32!", 1).is_none());
        let fresh = totp_secret();
        assert_eq!(fresh.len(), 32);
        assert_eq!(base32_decode(&fresh).map(|k| k.len()), Some(20));
        assert_eq!(
            totp_uri("ABC", "NuviraHub", "PC 1"),
            "otpauth://totp/NuviraHub:PC%201?secret=ABC&issuer=NuviraHub&digits=6&period=30"
        );
    }

    #[test]
    fn hash_and_verify() {
        let stored = hash("kgtmrejq");
        assert!(is_hash(&stored) && !stored.contains("kgtmrejq"));
        assert!(verify("kgtmrejq", &stored));
        assert!(!verify("kgtmrejQ", &stored));
        assert!(!verify("", &stored));
        // Same password, different salt.
        assert_ne!(stored, hash("kgtmrejq"));
        for broken in ["", "kgtmrejq", "pbkdf2-sha256$0$00$00", "pbkdf2-sha256$1$zz$00", "md5$1$00$00"] {
            assert!(!verify("kgtmrejq", broken), "{broken}");
            assert!(!is_valid_hash(broken), "{broken}");
        }
        assert!(is_valid_hash(&stored));
    }
}
