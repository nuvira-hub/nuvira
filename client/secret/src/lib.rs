//! The permanent password of unattended access is stored only as a PBKDF2-SHA256
//! hash with a random salt: whoever reads the service configuration (administrators,
//! a backup) does not learn it. Format: `pbkdf2-sha256$<iterations>$<salt>$<hash>`
//! (hexadecimal).

use std::num::NonZeroU32;

use ring::pbkdf2;
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

/// `password` matches the stored `hash` (constant-time comparison). A malformed hash
/// matches nothing.
pub fn verify(password: &str, hash: &str) -> bool {
    let mut parts = hash.split('$');
    let (Some("pbkdf2-sha256"), Some(iterations), Some(salt), Some(expected), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let (Some(iterations), Some(salt), Some(expected)) =
        (iterations.parse().ok().and_then(NonZeroU32::new), unhex(salt), unhex(expected))
    else {
        return false;
    };
    pbkdf2::verify(ALGORITHM, iterations, &salt, password.as_bytes(), &expected).is_ok()
}

/// The text is a stored hash (and not a password in clear text).
pub fn is_hash(text: &str) -> bool {
    text.starts_with("pbkdf2-sha256$")
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
        }
    }
}
