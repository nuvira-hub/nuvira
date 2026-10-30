//! Device identity: an Ed25519 key created at first start.
//!
//! The server binds the public key to the 9-digit ID; in sessions the same key
//! signs the device's TLS certificate, which the other side checks.
//! TODO (phase 3): protect the private key with DPAPI/TPM, Keychain, Secret Service.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519, SigningKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub type PublicKey = [u8; 32];

pub struct Identity {
    key: KeyPair,
    public_key: PublicKey,
    cert: CertificateDer<'static>,
}

impl Identity {
    /// Reads the key from `path` or creates it (with its folders) if missing.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let key = match std::fs::read(path) {
            Ok(der) => KeyPair::try_from(der.as_slice()).context("invalid device key")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = KeyPair::generate_for(&PKCS_ED25519)?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(path, key.serialize_der())
                    .with_context(|| format!("writing {}", path.display()))?;
                key
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let public_key: PublicKey =
            key.public_key_raw().try_into().map_err(|_| anyhow!("the device key is not Ed25519"))?;
        let cert = CertificateParams::new(vec!["nuvira-device".to_string()])?.self_signed(&key)?;
        Ok(Self { cert: cert.der().clone(), key, public_key })
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        Ok(self.key.sign(message)?)
    }

    /// Certificate and key for the sessions' TLS.
    pub fn tls_cert(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (vec![self.cert.clone()], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.serialize_der())))
    }
}

/// NuviraHub data folder of the user (`%APPDATA%\nuvira` on Windows).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NUVIRA_DATA_DIR") {
        return dir.into();
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.unwrap_or_else(|| ".".into()).join("nuvira")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_signs() {
        let dir = std::env::temp_dir().join(format!("nuvira-id-{}", std::process::id()));
        let path = dir.join("device.key");
        let a = Identity::load_or_create(&path).unwrap();
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public_key(), b.public_key());

        let sig = a.sign(b"ciao").unwrap();
        let pk = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, a.public_key());
        assert!(pk.verify(b"ciao", &sig).is_ok());
        assert!(crate::p2p::cert_has_key(&a.tls_cert().0[0], a.public_key()));
        assert!(!crate::p2p::cert_has_key(&a.tls_cert().0[0], &[0u8; 32]));
        let _ = std::fs::remove_dir_all(dir);
    }
}
