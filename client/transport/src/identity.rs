//! Device identity: an Ed25519 key created at first start.
//!
//! The server binds the public key to the 9-digit ID; in sessions the same key
//! signs the device's TLS certificate, which the other side checks.
//! The private key is kept protected by the system (module `keystore`): DPAPI,
//! the macOS Keychain, the Secret Service or systemd-creds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519, SigningKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::keystore::{self, Protection, ReadError};

pub type PublicKey = [u8; 32];

pub struct Identity {
    key: KeyPair,
    public_key: PublicKey,
    cert: CertificateDer<'static>,
    protection: Protection,
}

/// The system keeps the key locked (a keyring not opened), past the time given
/// to [`Identity::load_or_create_within`].
#[derive(Debug)]
pub struct KeyringLocked(pub String);

impl std::fmt::Display for KeyringLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeyringLocked {}

impl Identity {
    /// Reads the key from `path` or creates it (with its folders) if missing.
    /// A plain file is protected in place when the system can. While the system
    /// cannot open it (a locked keyring) this waits. One that will never open
    /// here (another account or computer) is set aside as `*.unreadable` and a
    /// new key, hence a new ID, is made.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        Self::load_or_create_within(path, None)
    }

    /// As [`Self::load_or_create`], but gives up after `limit` while the system
    /// keeps the key locked (a program started by hand, such as the viewer,
    /// says so instead of waiting without end).
    pub fn load_or_create_within(path: &Path, limit: Option<Duration>) -> Result<Self> {
        let started = std::time::Instant::now();
        let mut waited = false;
        let (der, protection) = loop {
            match keystore::read(path) {
                Ok(Some((der, protection))) => break (der, Some(protection)),
                Ok(None) => break (KeyPair::generate_for(&PKCS_ED25519)?.serialize_der(), None),
                Err(ReadError::Unavailable(e)) => {
                    if limit.is_some_and(|limit| started.elapsed() >= limit) {
                        return Err(KeyringLocked(format!(
                            "device key {}: the system keeps it locked ({e:#}); open the keyring (KWallet, GNOME Keyring) and try again",
                            path.display()
                        ))
                        .into());
                    }
                    if !waited {
                        tracing::warn!(
                            "device key {}: waiting for the system to open it: {e:#}",
                            path.display()
                        );
                        waited = true;
                    }
                    std::thread::sleep(Duration::from_secs(5));
                }
                Err(ReadError::Unreadable(e)) => {
                    // Another account's key (root opening a person's, or the
                    // other way round) is not this one's to replace.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        // SAFETY: geteuid cannot fail.
                        let me = unsafe { libc::geteuid() };
                        if let Ok(meta) = std::fs::metadata(path)
                            && meta.uid() != me
                        {
                            anyhow::bail!("device key {} belongs to another account and {e:#}", path.display());
                        }
                    }
                    let aside = path.with_extension("unreadable");
                    tracing::error!(
                        "device key {} {e:#}: kept as {} and a new one made (new ID)",
                        path.display(),
                        aside.display()
                    );
                    std::fs::rename(path, &aside).with_context(|| format!("moving {}", path.display()))?;
                    break (KeyPair::generate_for(&PKCS_ED25519)?.serialize_der(), None);
                }
                Err(ReadError::Io(e)) => {
                    return Err(e).with_context(|| format!("reading {}", path.display()));
                }
            }
        };
        if waited {
            tracing::info!("device key {}: opened", path.display());
        }
        let key = KeyPair::try_from(der.as_slice()).context("invalid device key")?;
        let protection = match protection {
            None => keystore::write(path, &der)?,
            Some(Protection::File) if keystore::preferred().by_the_system() => {
                match keystore::write(path, &der) {
                    Ok(protection) => {
                        if protection.by_the_system() {
                            tracing::info!(
                                "device key {}: now protected by the system ({})",
                                path.display(),
                                protection.name()
                            );
                        }
                        protection
                    }
                    Err(e) => {
                        tracing::warn!("device key {}: left as it is: {e:#}", path.display());
                        Protection::File
                    }
                }
            }
            // The computer now signs in by itself: the keyring would stay locked
            // after a restart, and the agent with it.
            Some(Protection::SecretService) if keystore::preferred() == Protection::File => {
                match keystore::write(path, &der) {
                    Ok(protection) => {
                        tracing::info!("device key {}: back to the file (automatic sign-in)", path.display());
                        protection
                    }
                    Err(e) => {
                        tracing::warn!("device key {}: left in the Secret Service: {e:#}", path.display());
                        Protection::SecretService
                    }
                }
            }
            Some(protection) => protection,
        };
        Self::new(key, protection)
    }

    fn new(key: KeyPair, protection: Protection) -> Result<Self> {
        let public_key: PublicKey =
            key.public_key_raw().try_into().map_err(|_| anyhow!("the device key is not Ed25519"))?;
        let cert = CertificateParams::new(vec!["nuvira-device".to_string()])?.self_signed(&key)?;
        Ok(Self { cert: cert.der().clone(), key, public_key, protection })
    }

    /// How the private key is kept on disk.
    pub fn protection(&self) -> Protection {
        self.protection
    }

    /// The private key (PKCS#8 DER), to hand it to another account
    /// (`keystore::write_handed`) or to protect it there (`keystore::write`).
    pub fn export_der(&self) -> Vec<u8> {
        self.key.serialize_der()
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

/// The public key in a key file, if this account can open it now (no waiting).
pub fn public_key_of(path: &Path) -> Option<PublicKey> {
    let (der, _) = keystore::read(path).ok()??;
    KeyPair::try_from(der.as_slice()).ok()?.public_key_raw().try_into().ok()
}

/// Copies the key in `from` (opened by this account) to `to` for another account,
/// as a plain handed-over copy.
pub fn hand_over(from: &Path, to: &Path) -> Result<()> {
    let (der, _) = keystore::read(from)
        .map_err(|e| anyhow!("{e}"))?
        .with_context(|| format!("{} is missing", from.display()))?;
    keystore::write_handed(to, &der)
}

/// Writes the key in `from` (a key file of any kind this account can open, such
/// as a handed-over copy) to `to`, protected for this account.
pub fn import(from: &Path, to: &Path) -> Result<Protection> {
    let (der, _) = keystore::read(from)
        .map_err(|e| anyhow!("{e}"))?
        .with_context(|| format!("{} is missing", from.display()))?;
    KeyPair::try_from(der.as_slice()).context("invalid device key")?;
    keystore::write(to, &der)
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

    #[test]
    fn plain_key_is_protected_in_place_with_the_same_id() {
        let dir = std::env::temp_dir().join(format!("nuvira-id-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("device.key");
        let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        std::fs::write(&path, key.serialize_der()).unwrap();
        let a = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public_key().as_slice(), key.public_key_raw());
        if a.protection().by_the_system() {
            assert!(std::fs::read(&path).unwrap().starts_with(b"NVK1"));
        }
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        assert_eq!(public_key_of(&path), Some(*a.public_key()));

        let handed = dir.join("handed.key");
        hand_over(&path, &handed).unwrap();
        let c = Identity::load_or_create(&handed).unwrap();
        assert_eq!((c.public_key(), c.protection()), (a.public_key(), Protection::Handed));
        let imported = dir.join("imported.key");
        import(&handed, &imported).unwrap();
        assert_eq!(public_key_of(&imported), Some(*a.public_key()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unreadable_key_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("nuvira-id-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("device.key");
        std::fs::write(&path, b"NVK1Dnot a dpapi blob").unwrap();
        Identity::load_or_create(&path).unwrap();
        assert!(dir.join("device.unreadable").exists());
        assert!(public_key_of(&path).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
