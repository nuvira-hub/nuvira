//! Where private keys live on disk: protected by the system whenever it can.
//!
//! A key file is one of:
//! - the PKCS#8 DER itself (starts with `0x30`): a plain file, as written before
//!   0.21. It is protected at the next load when the system can;
//! - `NVK1`, a kind byte and its payload:
//!   - `D` DPAPI (Windows): only the same account on this computer (a person, or
//!     SYSTEM for the service) can read it;
//!   - `K` macOS Keychain: AES-256-GCM, with a wrapping key in the person's login
//!     keychain, or in the System keychain for root;
//!   - `S` Secret Service (Linux, a person): the same, with the wrapping key in the
//!     person's default collection (GNOME Keyring, KWallet);
//!   - `C` systemd-creds (Linux, root): encrypted with the TPM 2.0 when there is
//!     one, and with the host key in `/var/lib/systemd` (root only);
//!   - `F` a file protected by its permissions only (0600, or the folder's ACL),
//!     when the system cannot do better;
//!   - `H` handed over: a plain copy root gives to another account (a person's
//!     session, the sign-in screen) for as long as it needs it. Never protected
//!     in place: the one who handed it compares it with its own.
//!
//! The private key never leaves the computer in a readable form, except through
//! `H` copies, which stay in folders of the receiving account only.
//!
//! `NUVIRA_KEY_STORE=file` keeps new keys as plain files (tests, headless runs);
//! another name (`secret-service`, `keychain`, ...) forces that one, for tests.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

const MAGIC: &[u8; 4] = b"NVK1";

/// How a key file is protected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    /// The file's permissions only.
    File,
    /// A copy handed over by root.
    Handed,
    Dpapi,
    Keychain,
    SecretService,
    SystemdCreds,
}

impl Protection {
    fn tag(self) -> Option<u8> {
        Some(match self {
            Self::File => b'F',
            Self::Handed => b'H',
            Self::Dpapi => b'D',
            Self::Keychain => b'K',
            Self::SecretService => b'S',
            Self::SystemdCreds => b'C',
        })
    }

    fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            b'F' => Self::File,
            b'H' => Self::Handed,
            b'D' => Self::Dpapi,
            b'K' => Self::Keychain,
            b'S' => Self::SecretService,
            b'C' => Self::SystemdCreds,
            _ => return None,
        })
    }

    /// Protected by the system, not just by the file's permissions.
    pub fn by_the_system(self) -> bool {
        !matches!(self, Self::File | Self::Handed)
    }

    /// Short name for logs and the app ("dpapi", "keychain", ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Handed => "handed",
            Self::Dpapi => "dpapi",
            Self::Keychain => "keychain",
            Self::SecretService => "secret-service",
            Self::SystemdCreds => "systemd-creds",
        }
    }
}

/// Why a key file could not be read.
#[derive(Debug)]
pub enum ReadError {
    /// The system cannot open it now (keyring locked, no session bus): try later.
    Unavailable(anyhow::Error),
    /// It will never open here (another account or computer, wrapping key gone).
    Unreadable(anyhow::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(e) => write!(f, "not available now: {e:#}"),
            Self::Unreadable(e) => write!(f, "cannot be read here: {e:#}"),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ReadError {}

/// Reads a key file: the DER and how it was protected. `Ok(None)` if missing.
pub fn read(path: &Path) -> Result<Option<(Vec<u8>, Protection)>, ReadError> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ReadError::Io(e)),
    };
    decode(&data).map(Some)
}

fn decode(data: &[u8]) -> Result<(Vec<u8>, Protection), ReadError> {
    let Some(rest) = data.strip_prefix(MAGIC) else {
        if data.first() == Some(&0x30) {
            return Ok((data.to_vec(), Protection::File));
        }
        return Err(ReadError::Unreadable(anyhow!("not a key file")));
    };
    let (&tag, payload) =
        rest.split_first().ok_or_else(|| ReadError::Unreadable(anyhow!("truncated key file")))?;
    let protection = Protection::from_tag(tag)
        .ok_or_else(|| ReadError::Unreadable(anyhow!("unknown protection {tag}")))?;
    let der = match protection {
        Protection::File | Protection::Handed => payload.to_vec(),
        Protection::Dpapi => sys::dpapi_open(payload)?,
        Protection::Keychain | Protection::SecretService => {
            let key = sys::wrapping_key(protection, false)?
                .ok_or_else(|| ReadError::Unreadable(anyhow!("the wrapping key is gone")))?;
            unwrap(&key, payload).map_err(ReadError::Unreadable)?
        }
        Protection::SystemdCreds => sys::creds_decrypt(payload)?,
    };
    Ok((der, protection))
}

/// How the key file at `path` is protected, from its header only (nothing is
/// opened). `None` if missing or not readable by this account.
pub fn protection_of(path: &Path) -> Option<Protection> {
    use std::io::Read;
    let mut head = [0u8; 5];
    let mut file = std::fs::File::open(path).ok()?;
    let n = file.read(&mut head).ok()?;
    match &head[..n] {
        [0x30, ..] => Some(Protection::File),
        [b'N', b'V', b'K', b'1', tag] => Protection::from_tag(*tag),
        _ => None,
    }
}

/// What the system offers in this context for new or plain keys.
pub fn preferred() -> Protection {
    let forced = std::env::var("NUVIRA_KEY_STORE").unwrap_or_default();
    [
        Protection::File,
        Protection::Dpapi,
        Protection::Keychain,
        Protection::SecretService,
        Protection::SystemdCreds,
    ]
    .into_iter()
    .find(|p| forced.eq_ignore_ascii_case(p.name()))
    .unwrap_or_else(sys::preferred)
}

/// Writes `der` to `path` with the best protection available here (falls back to
/// the file's permissions). Returns the protection used.
pub fn write(path: &Path, der: &[u8]) -> Result<Protection> {
    let wanted = preferred();
    let (protection, data) = match seal(wanted, der) {
        Ok(data) => (wanted, data),
        Err(e) => {
            tracing::warn!(
                "key {}: {} not available, protected by the file's permissions only: {e:#}",
                path.display(),
                wanted.name()
            );
            (Protection::File, seal(Protection::File, der)?)
        }
    };
    write_private(path, &data)?;
    Ok(protection)
}

/// A plain copy for another account, which uses it as it is.
pub fn write_handed(path: &Path, der: &[u8]) -> Result<()> {
    write_private(path, &seal(Protection::Handed, der)?)
}

fn seal(protection: Protection, der: &[u8]) -> Result<Vec<u8>> {
    let payload = match protection {
        Protection::File | Protection::Handed => der.to_vec(),
        Protection::Dpapi => sys::dpapi_seal(der)?,
        Protection::Keychain | Protection::SecretService => {
            let key = match sys::wrapping_key(protection, true) {
                Ok(Some(key)) => key,
                Ok(None) => bail!("no wrapping key"),
                Err(e) => bail!("{e}"),
            };
            wrap(&key, der)?
        }
        Protection::SystemdCreds => sys::creds_encrypt(der)?,
    };
    let mut data = MAGIC.to_vec();
    data.push(protection.tag().expect("tag"));
    data.extend_from_slice(&payload);
    // What was written must read back (a keyring that took the key but cannot
    // give it back would lose the ID at the next start).
    match decode(&data) {
        Ok((back, _)) if back == der => Ok(data),
        Ok(_) => bail!("the protected key does not read back the same"),
        Err(e) => bail!("the protected key does not read back: {e}"),
    }
}

/// Atomic write, readable by this account only.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("key-new");
    let _ = std::fs::remove_file(&partial);
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&partial).with_context(|| format!("writing {}", partial.display()))?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    // Keep the owner and mode of the file in place (root may write for another).
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        use std::os::unix::fs::MetadataExt;
        let _ = std::os::unix::fs::chown(&partial, Some(meta.uid()), Some(meta.gid()));
    }
    std::fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))
}

fn wrap(key: &[u8; 32], der: &[u8]) -> Result<Vec<u8>> {
    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).map_err(|_| anyhow!("wrapping key"))?);
    let mut nonce = [0u8; NONCE_LEN];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
        .map_err(|_| anyhow!("random"))?;
    let mut data = der.to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(MAGIC), &mut data)
        .map_err(|_| anyhow!("sealing the key"))?;
    Ok([nonce.as_slice(), &data].concat())
}

fn unwrap(key: &[u8; 32], payload: &[u8]) -> Result<Vec<u8>> {
    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).map_err(|_| anyhow!("wrapping key"))?);
    if payload.len() < NONCE_LEN {
        bail!("truncated key file");
    }
    let (nonce, sealed) = payload.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| anyhow!("nonce"))?;
    let mut data = sealed.to_vec();
    let plain = key
        .open_in_place(nonce, Aad::from(MAGIC), &mut data)
        .map_err(|_| anyhow!("the wrapping key does not open this file (another one replaced it)"))?;
    Ok(plain.to_vec())
}

#[cfg_attr(windows, allow(dead_code))]
fn random_key() -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key)
        .map_err(|_| anyhow!("random"))?;
    Ok(key)
}

/// `CryptUnprotectData` errors that mean the blob will never open for this
/// account (review NET-8; measured on Windows 11, Phase 4 and lot 19 on
/// PC-LABORATORIO): `NTE_BAD_KEY_STATE` (a person's account without the blob's
/// master key: another account or computer), `NTE_BAD_DATA` and
/// `ERROR_INVALID_DATA` (damaged or not a DPAPI blob) and `E_INVALIDARG` (a
/// truncated blob). LocalSystem (the service and its agents) answers a master key
/// it does not have with `ERROR_PATH_NOT_FOUND` (or `ERROR_FILE_NOT_FOUND`): its
/// master keys are always on the system disk, so for it these are permanent too.
/// Any other error (the DPAPI or RPC service not reachable yet, a person's
/// profile not loaded, out of memory) may pass: the key is waited for instead of
/// being replaced by a new one.
#[cfg_attr(not(windows), allow(dead_code))]
fn dpapi_error_is_permanent(hresult: u32, local_system: bool) -> bool {
    const NTE_BAD_KEY_STATE: u32 = 0x8009_000B;
    const NTE_BAD_DATA: u32 = 0x8009_0005;
    const ERROR_INVALID_DATA: u32 = 0x8007_000D;
    const E_INVALIDARG: u32 = 0x8007_0057;
    const ERROR_FILE_NOT_FOUND: u32 = 0x8007_0002;
    const ERROR_PATH_NOT_FOUND: u32 = 0x8007_0003;
    matches!(hresult, NTE_BAD_KEY_STATE | NTE_BAD_DATA | ERROR_INVALID_DATA | E_INVALIDARG)
        || (local_system && matches!(hresult, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND))
}

#[cfg(windows)]
mod sys {
    //! DPAPI with the account running (a person, or SYSTEM for the service and
    //! its agents). TPM: Ed25519 is not a TPM 2.0 algorithm, so the key itself
    //! cannot live there; DPAPI's master key is what the system protects.

    use super::{Protection, ReadError};
    use anyhow::{Result, anyhow};
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::w;

    pub fn preferred() -> Protection {
        Protection::Dpapi
    }

    pub fn dpapi_seal(der: &[u8]) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: der.len() as u32, pbData: der.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input points to `der`, alive for the call; output is freed below.
        unsafe {
            CryptProtectData(
                &input,
                w!("NuviraHub device key"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|e| anyhow!("CryptProtectData: {e}"))?;
            let blob = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(Some(HLOCAL(output.pbData.cast())));
            Ok(blob)
        }
    }

    pub fn dpapi_open(blob: &[u8]) -> Result<Vec<u8>, ReadError> {
        let input = CRYPT_INTEGER_BLOB { cbData: blob.len() as u32, pbData: blob.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `dpapi_seal`.
        unsafe {
            CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)
                .map_err(|e| {
                    let code = e.code().0 as u32;
                    if super::dpapi_error_is_permanent(code, running_as_system()) {
                        ReadError::Unreadable(anyhow!(
                            "CryptUnprotectData (another account or computer?): {e}"
                        ))
                    } else {
                        ReadError::Unavailable(anyhow!("CryptUnprotectData: {e}"))
                    }
                })?;
            let der = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(Some(HLOCAL(output.pbData.cast())));
            Ok(der)
        }
    }

    /// The process runs as LocalSystem.
    fn running_as_system() -> bool {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: the process's own token, queried into an aligned buffer and closed.
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut buf = [0u64; 64];
            let mut len = 0;
            let size = std::mem::size_of_val(&buf) as u32;
            let ok =
                GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), size, &mut len).is_ok();
            let _ = CloseHandle(token);
            ok && IsWellKnownSid((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, WinLocalSystemSid).as_bool()
        }
    }

    pub fn wrapping_key(_: Protection, _: bool) -> Result<Option<[u8; 32]>, ReadError> {
        Err(ReadError::Unreadable(anyhow!("not on Windows")))
    }

    pub fn creds_encrypt(_: &[u8]) -> Result<Vec<u8>> {
        Err(anyhow!("not on Windows"))
    }

    pub fn creds_decrypt(_: &[u8]) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::Unreadable(anyhow!("not on Windows")))
    }
}

#[cfg(target_os = "macos")]
mod sys {
    //! The wrapping key is a generic password: in the person's login keychain
    //! (unlocked at sign-in, automatic login included), or in the System keychain
    //! for root (the sign-in screen's agent, `service deploy`). Only the signed
    //! NuviraHub program reads it without a prompt.

    use super::{Protection, ReadError, random_key};
    use anyhow::{Result, anyhow};
    use security_framework::os::macos::keychain::SecKeychain;

    const SERVICE: &str = "io.github.nuvira-hub.nuvira";
    const ACCOUNT: &str = "device-keys";
    const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";
    const NOT_FOUND: i32 = -25300;
    /// errSecInteractionNotAllowed, errSecNoSuchKeychain, errSecNotAvailable,
    /// errSecAuthFailed (what an item answers with interaction disabled when it
    /// would have asked to allow this program: another build or signature) and
    /// errSecUserCanceled (Cancel in the keychain window). Never "unreadable":
    /// that would set the key aside and change the computer's ID.
    const LATER: [i32; 5] = [-25308, -25294, -25291, -25293, -128];

    pub fn preferred() -> Protection {
        Protection::Keychain
    }

    fn keychain() -> Result<SecKeychain, security_framework::base::Error> {
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            SecKeychain::open(SYSTEM_KEYCHAIN)
        } else {
            SecKeychain::default()
        }
    }

    pub fn wrapping_key(_: Protection, create: bool) -> Result<Option<[u8; 32]>, ReadError> {
        let classify = |e: security_framework::base::Error| {
            let err = anyhow!("keychain: {e} ({})", e.code());
            if LATER.contains(&e.code()) { ReadError::Unavailable(err) } else { ReadError::Unreadable(err) }
        };
        // A new key never waits for a window: a locked keychain, or an item that
        // would ask whether this program may use it, answers at once and the key
        // goes to the file (an agent with nobody at the Mac, a CI runner). An
        // existing key keeps the window, as before: someone can unlock or allow
        // it, and meanwhile the key is only waited for, never replaced.
        // Process-wide while this call lasts.
        let _no_window = create.then(SecKeychain::disable_user_interaction);
        let keychain = keychain().map_err(classify)?;
        match keychain.find_generic_password(SERVICE, ACCOUNT) {
            Ok((password, _)) => {
                let key: [u8; 32] = password
                    .as_ref()
                    .try_into()
                    .map_err(|_| ReadError::Unreadable(anyhow!("keychain: wrong key size")))?;
                Ok(Some(key))
            }
            Err(e) if e.code() == NOT_FOUND && create => {
                let key = random_key().map_err(ReadError::Unreadable)?;
                keychain.add_generic_password(SERVICE, ACCOUNT, &key).map_err(classify)?;
                Ok(Some(key))
            }
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(classify(e)),
        }
    }

    pub fn dpapi_seal(_: &[u8]) -> Result<Vec<u8>> {
        Err(anyhow!("not on macOS"))
    }

    pub fn dpapi_open(_: &[u8]) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::Unreadable(anyhow!("not on macOS")))
    }

    pub fn creds_encrypt(_: &[u8]) -> Result<Vec<u8>> {
        Err(anyhow!("not on macOS"))
    }

    pub fn creds_decrypt(_: &[u8]) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::Unreadable(anyhow!("not on macOS")))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod sys {
    //! Root: `systemd-creds` (TPM 2.0 when present, with no PCRs so firmware and
    //! Secure Boot updates do not lose the key; plus the host key).
    //! A person: the Secret Service, only when its default collection is unlocked
    //! (or opens with no window, as KWallet does after a sign-in with the
    //! password) and the computer does not sign in automatically (the keyring
    //! would stay locked after a restart, and the agent with it); otherwise the
    //! file.

    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{Protection, ReadError};
    use anyhow::{Context, Result, anyhow};

    const CREDENTIAL: &str = "nuvira-key";

    fn root() -> bool {
        // SAFETY: geteuid cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    pub fn preferred() -> Protection {
        if root() {
            if creds_available() { Protection::SystemdCreds } else { Protection::File }
        } else if !autologin() && secret_service::usable() {
            Protection::SecretService
        } else {
            Protection::File
        }
    }

    fn creds_available() -> bool {
        Command::new("systemd-creds")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// How long `systemd-creds` may take (a TPM answers in well under a second).
    const CREDS_TIMEOUT: Duration = Duration::from_secs(30);

    /// Runs `systemd-creds`. The error says whether it ran and refused
    /// ([`ReadError::Unreadable`]: its messages are not a stable API, so any
    /// refusal counts as one) or could not run or answer now
    /// ([`ReadError::Unavailable`]: missing during an upgrade, or hung at early
    /// boot), review NET-8.
    fn creds(args: &[&str], input: &[u8]) -> Result<Vec<u8>, ReadError> {
        let later =
            |e: anyhow::Error| ReadError::Unavailable(e.context(format!("systemd-creds {}", args[0])));
        let mut child = Command::new("systemd-creds")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| later(e.into()))?;
        // The key and the messages are a few kilobytes: they fit in the pipes,
        // so the child never waits for its output to be read.
        let written = child.stdin.take().context("stdin").and_then(|mut i| Ok(i.write_all(input)?));
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < CREDS_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(later(anyhow!("no answer in {} s", CREDS_TIMEOUT.as_secs())));
                }
                Err(e) => return Err(later(e.into())),
            }
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout.read_to_end(&mut out).map_err(|e| later(e.into()))?;
        }
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_end(&mut err);
        }
        if !status.success() {
            return Err(ReadError::Unreadable(anyhow!(
                "systemd-creds {}: {}",
                args[0],
                String::from_utf8_lossy(&err).trim()
            )));
        }
        written.map_err(later)?;
        Ok(out)
    }

    pub fn creds_encrypt(der: &[u8]) -> Result<Vec<u8>> {
        let name = format!("--name={CREDENTIAL}");
        creds(&["encrypt", &name, "--with-key=auto", "--tpm2-pcrs=", "-", "-"], der)
            .map_err(|e| anyhow!("{e}"))
    }

    pub fn creds_decrypt(blob: &[u8]) -> Result<Vec<u8>, ReadError> {
        if !root() {
            return Err(ReadError::Unreadable(anyhow!("a key of root's")));
        }
        let name = format!("--name={CREDENTIAL}");
        creds(&["decrypt", &name, "-", "-"], blob)
    }

    pub fn wrapping_key(_: Protection, create: bool) -> Result<Option<[u8; 32]>, ReadError> {
        secret_service::wrapping_key(create)
    }

    pub fn dpapi_seal(_: &[u8]) -> Result<Vec<u8>> {
        Err(anyhow!("not on Linux"))
    }

    pub fn dpapi_open(_: &[u8]) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::Unreadable(anyhow!("not on Linux")))
    }

    /// The display manager signs someone in by itself (GDM, LightDM, SDDM).
    pub fn autologin() -> bool {
        let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
        let dir = |path: &str| -> Vec<String> {
            let mut files: Vec<_> = std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "conf"))
                .collect();
            files.sort();
            files.iter().map(|p| std::fs::read_to_string(p).unwrap_or_default()).collect()
        };
        let lines = |texts: Vec<String>| -> Vec<(String, String, String)> {
            let mut out = Vec::new();
            for text in texts {
                let mut section = String::new();
                for line in text.lines().map(str::trim) {
                    if line.starts_with('#') || line.starts_with(';') {
                        continue;
                    }
                    if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                        section = name.trim().to_ascii_lowercase();
                    } else if let Some((key, value)) = line.split_once('=') {
                        out.push((
                            section.clone(),
                            key.trim().to_ascii_lowercase(),
                            value.trim().to_string(),
                        ));
                    }
                }
            }
            out
        };
        let gdm = lines(vec![
            read("/etc/gdm3/custom.conf"),
            read("/etc/gdm/custom.conf"),
            read("/etc/gdm3/daemon.conf"),
        ]);
        let gdm_on = gdm.iter().any(|(s, k, v)| {
            s == "daemon"
                && (k == "automaticloginenable" || k == "timedloginenable")
                && v.eq_ignore_ascii_case("true")
        });
        let mut lightdm = dir("/usr/share/lightdm/lightdm.conf.d");
        lightdm.extend(dir("/etc/lightdm/lightdm.conf.d"));
        lightdm.push(read("/etc/lightdm/lightdm.conf"));
        let lightdm_on = lines(lightdm).iter().any(|(_, k, v)| k == "autologin-user" && !v.is_empty());
        let mut sddm = dir("/usr/lib/sddm/sddm.conf.d");
        sddm.extend(dir("/etc/sddm.conf.d"));
        sddm.push(read("/etc/sddm.conf"));
        let sddm_on = lines(sddm).iter().any(|(s, k, v)| s == "autologin" && k == "user" && !v.is_empty());
        gdm_on || lightdm_on || sddm_on
    }

    mod secret_service {
        //! The org.freedesktop.secrets D-Bus API, plain transfer (the session
        //! bus is local to the person). On its own thread and runtime: callers
        //! may be inside tokio already.

        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        use super::super::{ReadError, random_key};
        use anyhow::anyhow;
        use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

        const DEST: &str = "org.freedesktop.secrets";
        const PATH: &str = "/org/freedesktop/secrets";
        const SERVICE: &str = "org.freedesktop.Secret.Service";
        const LABEL: &str = "NuviraHub device keys";

        type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

        /// The person was asked to open the keyring (once per program).
        static ASKED: AtomicBool = AtomicBool::new(false);

        fn attributes() -> HashMap<&'static str, &'static str> {
            HashMap::from([("application", "nuvira"), ("purpose", "device-keys")])
        }

        fn run<T: Send + 'static>(
            f: impl AsyncFnOnce(zbus::Connection) -> Result<T, ReadError> + Send + 'static,
        ) -> Result<T, ReadError> {
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| ReadError::Unavailable(e.into()))?;
                rt.block_on(async move {
                    let conn = tokio::time::timeout(Duration::from_secs(5), zbus::Connection::session())
                        .await
                        .map_err(|_| ReadError::Unavailable(anyhow!("session bus: timed out")))?
                        .map_err(|e| ReadError::Unavailable(anyhow!("session bus: {e}")))?;
                    tokio::time::timeout(Duration::from_secs(10), f(conn))
                        .await
                        .map_err(|_| ReadError::Unavailable(anyhow!("Secret Service: timed out")))?
                })
            })
            .join()
            .map_err(|_| ReadError::Unavailable(anyhow!("Secret Service thread")))?
        }

        fn later(e: zbus::Error) -> ReadError {
            ReadError::Unavailable(anyhow!("Secret Service: {e}"))
        }

        async fn proxy<'a>(
            conn: &zbus::Connection,
            path: &'a str,
            iface: &'a str,
        ) -> Result<zbus::Proxy<'a>, ReadError> {
            zbus::Proxy::new(conn, DEST, path.to_string(), iface).await.map_err(later)
        }

        async fn default_collection(conn: &zbus::Connection) -> Result<Option<OwnedObjectPath>, ReadError> {
            let service = proxy(conn, PATH, SERVICE).await?;
            let path: OwnedObjectPath = service.call("ReadAlias", &("default",)).await.map_err(later)?;
            Ok((path.as_str() != "/").then_some(path))
        }

        async fn locked(conn: &zbus::Connection, collection: &OwnedObjectPath) -> Result<bool, ReadError> {
            let collection = proxy(conn, collection.as_str(), "org.freedesktop.Secret.Collection").await?;
            collection.get_property::<bool>("Locked").await.map_err(later)
        }

        /// KWallet's service (ksecretd, kwalletd). After a sign-in with the
        /// password it opens its wallet with no window, but only when a program
        /// asks: until then the wallet stays locked.
        async fn kwallet(conn: &zbus::Connection) -> bool {
            let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else { return false };
            let Ok(name) = zbus::names::BusName::try_from(DEST) else { return false };
            let Ok(pid) = dbus.get_connection_unix_process_id(name).await else { return false };
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            matches!(comm.trim(), "ksecretd" | "kwalletd6" | "kwalletd5")
        }

        /// Asks the service to open `objects`; whether they are open now. Quietly
        /// (`window` false) the system's prompt is run only for KWallet, which
        /// needs no window after a sign-in with the password, and dismissed if
        /// it is not done in 3 s (a wallet with a password of its own). With
        /// `window` the prompt stays on screen for the person to answer later.
        async fn open(
            conn: &zbus::Connection,
            objects: Vec<OwnedObjectPath>,
            window: bool,
        ) -> Result<bool, ReadError> {
            use futures_util::StreamExt;

            // Quietly only KWallet is asked: another service would only get an unlock
            // dismissed at once, and GNOME Keyring aborts (assertion `!self->current` in
            // gkd-secret-unlock.c, a known upstream bug) when an unlock is dismissed while
            // the window of another one is on screen: the person's password then goes
            // nowhere. So for them only whether everything is open already.
            if !window && !kwallet(conn).await {
                for object in &objects {
                    // A collection (…/collection/NAME) or an item of one (one level deeper).
                    let interface = match object.as_str().matches('/').count() {
                        5 => "org.freedesktop.Secret.Collection",
                        _ => "org.freedesktop.Secret.Item",
                    };
                    let object = proxy(conn, object.as_str(), interface).await?;
                    if object.get_property::<bool>("Locked").await.map_err(later)? {
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            let service = proxy(conn, PATH, SERVICE).await?;
            let (_, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
                service.call("Unlock", &(objects,)).await.map_err(later)?;
            if prompt.as_str() == "/" {
                return Ok(true);
            }
            let prompt = proxy(conn, prompt.as_str(), "org.freedesktop.Secret.Prompt").await?;
            let mut completed = prompt.receive_signal("Completed").await.map_err(later)?;
            prompt.call::<_, _, ()>("Prompt", &("",)).await.map_err(later)?;
            if window {
                return Ok(false);
            }
            match tokio::time::timeout(Duration::from_secs(3), completed.next()).await {
                Ok(Some(message)) => {
                    let (dismissed, _): (bool, OwnedValue) = message.body().deserialize().map_err(later)?;
                    Ok(!dismissed)
                }
                _ => {
                    let _ = prompt.call::<_, _, ()>("Dismiss", &()).await;
                    Ok(false)
                }
            }
        }

        /// The default collection exists and is unlocked now, or opens with no
        /// window when asked (KWallet after a sign-in with the password).
        pub fn usable() -> bool {
            run(async |conn| match default_collection(&conn).await? {
                Some(collection) => {
                    if !locked(&conn, &collection).await? {
                        return Ok(true);
                    }
                    Ok(open(&conn, vec![collection.clone()], false).await?
                        && !locked(&conn, &collection).await?)
                }
                None => Ok(false),
            })
            .unwrap_or(false)
        }

        pub fn wrapping_key(create: bool) -> Result<Option<[u8; 32]>, ReadError> {
            run(async move |conn| {
                let service = proxy(&conn, PATH, SERVICE).await?;
                let search = async || -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>), ReadError> {
                    service.call("SearchItems", &(attributes(),)).await.map_err(later)
                };
                let (mut unlocked, locked_items) = search().await?;
                if unlocked.is_empty() && !locked_items.is_empty() {
                    if open(&conn, locked_items.clone(), false).await? {
                        unlocked = search().await?.0;
                    }
                    if unlocked.is_empty() {
                        // Asks the person (the system's prompt), once for this
                        // program: the caller tries again later.
                        if !ASKED.swap(true, Ordering::Relaxed) {
                            let _ = open(&conn, locked_items, true).await;
                        }
                        return Err(ReadError::Unavailable(anyhow!("the keyring is locked")));
                    }
                }
                let (_, session): (OwnedValue, OwnedObjectPath) =
                    service.call("OpenSession", &("plain", Value::from(""))).await.map_err(later)?;
                if let Some(item) = unlocked.first() {
                    let secrets: HashMap<OwnedObjectPath, Secret> =
                        service.call("GetSecrets", &(vec![item.clone()], &session)).await.map_err(later)?;
                    let (_, _, value, _) = secrets
                        .into_values()
                        .next()
                        .ok_or_else(|| ReadError::Unavailable(anyhow!("no secret")))?;
                    let key: [u8; 32] = value
                        .as_slice()
                        .try_into()
                        .map_err(|_| ReadError::Unreadable(anyhow!("Secret Service: wrong key size")))?;
                    return Ok(Some(key));
                }
                // A locked collection may hide its items: not "gone" then.
                let collection = default_collection(&conn)
                    .await?
                    .ok_or_else(|| ReadError::Unavailable(anyhow!("no default collection")))?;
                if locked(&conn, &collection).await? {
                    return Err(ReadError::Unavailable(anyhow!("the keyring is locked")));
                }
                if !create {
                    return Ok(None);
                }
                let key = random_key().map_err(ReadError::Unreadable)?;
                let collection_proxy =
                    proxy(&conn, collection.as_str(), "org.freedesktop.Secret.Collection").await?;
                let properties: HashMap<&str, Value> = HashMap::from([
                    ("org.freedesktop.Secret.Item.Label", Value::from(LABEL)),
                    ("org.freedesktop.Secret.Item.Attributes", Value::from(attributes())),
                ]);
                let secret = (&session, Vec::<u8>::new(), key.to_vec(), "application/octet-stream");
                let (item, prompt): (OwnedObjectPath, OwnedObjectPath) =
                    collection_proxy.call("CreateItem", &(properties, secret, true)).await.map_err(later)?;
                if item.as_str() == "/" || prompt.as_str() != "/" {
                    return Err(ReadError::Unavailable(anyhow!("the keyring asked for a prompt")));
                }
                let _ = proxy(&conn, session.as_str(), "org.freedesktop.Secret.Session")
                    .await?
                    .call::<_, _, ()>("Close", &())
                    .await;
                Ok(Some(key))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_file_reads_as_file() {
        let (der, protection) = decode(&[0x30, 1, 2]).unwrap();
        assert_eq!((der, protection), (vec![0x30, 1, 2], Protection::File));
    }

    #[test]
    fn handed_and_file_round_trip() {
        for protection in [Protection::Handed, Protection::File] {
            let data = seal(protection, &[0x30, 9, 9]).unwrap();
            assert!(data.starts_with(MAGIC));
            assert_eq!(decode(&data).unwrap(), (vec![0x30, 9, 9], protection));
        }
    }

    #[test]
    fn wrapped_needs_the_same_key() {
        let key = random_key().unwrap();
        let sealed = wrap(&key, b"secret").unwrap();
        assert_eq!(unwrap(&key, &sealed).unwrap(), b"secret");
        assert!(unwrap(&random_key().unwrap(), &sealed).is_err());
    }

    #[test]
    fn garbage_is_unreadable() {
        assert!(matches!(decode(b"hello"), Err(ReadError::Unreadable(_))));
        assert!(matches!(decode(b"NVK1Z"), Err(ReadError::Unreadable(_))));
    }

    #[test]
    fn dpapi_errors_that_never_pass() {
        // Review NET-8: these set the key aside (new ID); anything else waits.
        for system in [false, true] {
            for code in [0x8009_000Bu32, 0x8009_0005, 0x8007_000D, 0x8007_0057] {
                assert!(dpapi_error_is_permanent(code, system), "{code:#x}");
            }
            // RPC server unavailable, call failed, out of memory, access denied.
            for code in [0x8007_06BAu32, 0x8007_06BE, 0x8007_000E, 0x8007_0005] {
                assert!(!dpapi_error_is_permanent(code, system), "{code:#x}");
            }
        }
        // File / path not found: a person's profile not loaded yet, but a master
        // key LocalSystem does not have (its own are always there).
        for code in [0x8007_0002u32, 0x8007_0003] {
            assert!(!dpapi_error_is_permanent(code, false), "{code:#x}");
            assert!(dpapi_error_is_permanent(code, true), "{code:#x}");
        }
    }

    /// Blobs that will never open here are `Unreadable` at once: the codes
    /// Windows really gives (Phase 4 E2), not just the mapping.
    #[cfg(windows)]
    #[test]
    fn damaged_dpapi_blobs_are_unreadable() {
        let data = seal(Protection::Dpapi, &[0x30, 5, 5]).unwrap();
        let header = MAGIC.len() + 1;
        // DPAPI blob: version (4), provider GUID (16), master key version (4),
        // master key GUID (16): another GUID is a master key this account lacks.
        let mut other_master_key = data.clone();
        other_master_key[header + 24] ^= 0x55;
        let mut damaged = data.clone();
        let last = damaged.len() - 30;
        damaged[last] ^= 0x55;
        let truncated = data[..data.len() - 40].to_vec();
        let short = data[..header + 30].to_vec();
        let mut random = data[..header].to_vec();
        random.extend((0..200u32).map(|i| (i * 37 % 251) as u8));
        for (what, blob) in [
            ("master key", other_master_key),
            ("damaged", damaged),
            ("truncated", truncated),
            ("short", short),
            ("random", random),
        ] {
            match decode(&blob) {
                Err(ReadError::Unreadable(e)) => eprintln!("{what}: {e:#}"),
                other => panic!("{what}: {other:?}"),
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip() {
        let data = seal(Protection::Dpapi, &[0x30, 7, 7]).unwrap();
        assert_eq!(decode(&data).unwrap(), (vec![0x30, 7, 7], Protection::Dpapi));
        assert!(!data.windows(3).any(|w| w == [0x30, 7, 7]));
    }
}
