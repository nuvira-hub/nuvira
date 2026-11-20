//! Automatic updates.
//!
//! Each nuvira-server distributes the version its devices should run, from the
//! `updates` folder of its data, on the TCP port of the WebSocket fallback:
//!
//! ```text
//! GET /nuvira/update/windows-x64.json                 manifest (this module's `Manifest`)
//! GET /nuvira/update/windows-x64/nuvira-0.1.0.exe   the program
//! ```
//!
//! Channels: `windows-x64` (nuvira.exe), `linux-x64` (the `nuvira` program, for
//! copies the user can replace), `linux-x64-deb` and `linux-x64-rpm` (the packages,
//! installed by root's timer) and `macos` (the NuviraHub.app bundle in a zip).
//!
//! The manifest is signed with the NuviraHub release key (Ed25519, kept by whoever
//! builds the releases; `nuvira-release` makes and uses it) and the program is
//! checked against the size and SHA-256 in it. The public keys are built into the
//! program: a tampered server or download cannot get anything installed. There are
//! two: the one in use and a backup kept offline, so that losing the first does not
//! leave the installed copies without updates (sign with the backup, then ship a
//! program with a new pair).
//!
//! Since 0.29 the manifest also carries the time it was signed (`published`), with
//! a second signature of its own, so that programs of before, which check only the
//! first one, still accept it. A server whose newest version has no signed time,
//! or one older than [`STALE_AFTER`], makes the app say that updates look held
//! back (review APP-1): someone on the network, or a server nobody keeps up to
//! date, cannot hide newer releases without it being seen.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Public halves of the NuviraHub release keys (see `nuvira-release keygen`): an
/// update signed by either is accepted.
#[rustfmt::skip]
pub const PUBLIC_KEYS: [[u8; 32]; 2] = [
    // In use: 59fbcd29526e30ef3b4ad1b44eea9c4393eab3f5affa45baf3d0375da95df7a8
    [
        0x59, 0xfb, 0xcd, 0x29, 0x52, 0x6e, 0x30, 0xef, 0x3b, 0x4a, 0xd1, 0xb4, 0x4e, 0xea, 0x9c, 0x43,
        0x93, 0xea, 0xb3, 0xf5, 0xaf, 0xfa, 0x45, 0xba, 0xf3, 0xd0, 0x37, 0x5d, 0xa9, 0x5d, 0xf7, 0xa8,
    ],
    // Backup, kept offline (2026-11-09):
    // ce51cc85140e851c6c2971afcdae190220a29cfdc592dcb429062cadfa890f73
    [
        0xce, 0x51, 0xcc, 0x85, 0x14, 0x0e, 0x85, 0x1c, 0x6c, 0x29, 0x71, 0xaf, 0xcd, 0xae, 0x19, 0x02,
        0x20, 0xa2, 0x9c, 0xfd, 0xc5, 0x92, 0xdc, 0xb4, 0x29, 0x06, 0x2c, 0xad, 0xfa, 0x89, 0x0f, 0x73,
    ],
];

/// Port of nuvira-server when the address has none.
pub const DEFAULT_PORT: u16 = 21116;

/// Host of NuviraHub's public server (`nuvira_transport::signal::PUBLIC_SERVER`).
pub const PUBLIC_HOST: &str = "connect.nuvirahub.dev";

/// The public server's updates over HTTPS (Caddy in front of the hub on the VPS,
/// review APP-1). The plain `http://connect.nuvirahub.dev:21116/nuvira/update` stays
/// for the programs installed before; this one never falls back to it.
pub const PUBLIC_UPDATES: &str = "https://connect.nuvirahub.dev/nuvira/update";

/// Version of this program.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// File in the service's data folder (Windows) holding the version whose
/// installation keeps failing, as text: something on the PC (an antivirus or
/// anti-ransomware program) stops it. Written by the service, read by its agent,
/// which tells the app.
pub const BLOCKED_FILE: &str = "update-blocked";

#[cfg(unix)]
pub mod unix;

/// File in the agent's data folder (the service's on Windows) holding the
/// [`Stale`] of the last check, as JSON: written by the updater, read by the agent,
/// which tells the app. No file: updates look fine.
pub const STALE_FILE: &str = "update-stale";

/// The newest version a server offers is older than this: updates look held back.
pub const STALE_AFTER: Duration = Duration::from_secs(90 * 24 * 3600);

/// Largest program accepted (the Windows one is ~28 MB).
const MAX_PROGRAM: u64 = 256 << 20;
const MAX_MANIFEST: u64 = 64 << 10;
const TIMEOUT: Duration = Duration::from_secs(20);

/// Name of this platform in the update paths (the program alone; the Linux
/// packages have channels of their own, see [`unix::PackageKind`]).
pub fn platform() -> &'static str {
    if cfg!(all(windows, target_arch = "x86_64")) {
        "windows-x64"
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        "windows-arm64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x64"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "unsupported"
    }
}

/// Extension of the published file of a channel.
pub fn extension(platform: &str) -> &'static str {
    match platform {
        p if p.starts_with("windows-") => ".exe",
        p if p.ends_with("-deb") => ".deb",
        p if p.ends_with("-rpm") => ".rpm",
        "macos" => ".zip",
        _ => "",
    }
}

/// A version offered by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub platform: String,
    pub size: u64,
    /// SHA-256 of the program, hexadecimal.
    pub sha256: String,
    /// Ed25519 signature of `signed_message()`, hexadecimal.
    pub signature: String,
    /// When `nuvira-release` signed it, in seconds since 1970 (UTC). Since 0.29;
    /// older programs ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<u64>,
    /// Ed25519 signature of `published_message()`, hexadecimal: a signature of its
    /// own, so that `signature` stays what the older programs check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_signature: Option<String>,
}

impl Manifest {
    /// Path of the program under the update URL.
    pub fn program_path(&self) -> String {
        format!("{}/nuvira-{}{}", self.platform, self.version, extension(&self.platform))
    }

    fn signed_message(&self) -> Vec<u8> {
        format!("nuvira-update\n{}\n{}\n{}\n{}", self.version, self.platform, self.size, self.sha256)
            .into_bytes()
    }

    /// The publication time with everything the first signature covers: a time
    /// cannot be moved to another manifest.
    fn published_message(&self, published: u64) -> Vec<u8> {
        format!(
            "nuvira-update-published\n{}\n{}\n{}\n{}\n{published}",
            self.version, self.platform, self.size, self.sha256
        )
        .into_bytes()
    }

    /// Makes a signed manifest for `program`, without a publication time (as the
    /// programs before 0.29 made them).
    pub fn sign(key: &Ed25519KeyPair, version: &str, platform: &str, program: &[u8]) -> Self {
        let mut manifest = Self {
            version: version.to_string(),
            platform: platform.to_string(),
            size: program.len() as u64,
            sha256: hex(&Sha256::digest(program)),
            signature: String::new(),
            published: None,
            published_signature: None,
        };
        manifest.signature = hex(key.sign(&manifest.signed_message()).as_ref());
        manifest
    }

    /// Like [`Manifest::sign`], with the signed publication time `published`
    /// (seconds since 1970): what `nuvira-release sign` writes.
    pub fn sign_published(
        key: &Ed25519KeyPair,
        version: &str,
        platform: &str,
        program: &[u8],
        published: u64,
    ) -> Self {
        let mut manifest = Self::sign(key, version, platform, program);
        manifest.published = Some(published);
        manifest.published_signature = Some(hex(key.sign(&manifest.published_message(published)).as_ref()));
        manifest
    }

    /// The publication time, if there is one and it is signed by one of `keys`.
    pub fn verified_published(&self, keys: &[[u8; 32]]) -> Option<u64> {
        let published = self.published?;
        let signature = unhex(self.published_signature.as_deref()?)?;
        let message = self.published_message(published);
        keys.iter()
            .any(|key| UnparsedPublicKey::new(&ED25519, key).verify(&message, &signature).is_ok())
            .then_some(published)
    }

    /// Checks the signature against `public_key`.
    pub fn verify(&self, public_key: &[u8]) -> Result<()> {
        let signature = unhex(&self.signature).context("signature is not hexadecimal")?;
        UnparsedPublicKey::new(&ED25519, public_key)
            .verify(&self.signed_message(), &signature)
            .map_err(|_| anyhow::anyhow!("the update signature is not valid"))
    }

    /// Checks the signature against any of `keys`.
    pub fn verify_any(&self, keys: &[[u8; 32]]) -> Result<()> {
        if keys.iter().any(|key| self.verify(key).is_ok()) {
            Ok(())
        } else {
            bail!("the update signature is not valid")
        }
    }
}

/// Public key of a key pair in PKCS#8 (as written by `nuvira-release keygen`).
pub fn public_key_of(pkcs8: &[u8]) -> Result<[u8; 32]> {
    let key = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| anyhow::anyhow!("not an Ed25519 PKCS#8 key"))?;
    Ok(key.public_key().as_ref().try_into()?)
}

/// The parts of a version: one or more dot-separated runs of digits (`0.28.0`, and
/// the four parts of a Windows file version). Anything else is not a version: a
/// leading `v`, an empty part (`1..0`), a suffix (`1.0.0-rc.1`, never published:
/// the release candidates are 0.99.x), a part beyond u64, a path.
fn version_parts(v: &str) -> Option<Vec<u64>> {
    v.split('.')
        .map(|p| if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) { p.parse().ok() } else { None })
        .collect()
}

/// `v` is a version as [`newer`] understands it.
pub fn is_version(v: &str) -> bool {
    version_parts(v).is_some()
}

/// `a` is newer than `b` (versions `x.y.z`; missing parts count as 0). Never when
/// either is not a version (see [`is_version`]).
pub fn newer(a: &str, b: &str) -> bool {
    let (Some(mut a), Some(mut b)) = (version_parts(a), version_parts(b)) else { return false };
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a > b
}

/// Base URL of the updates on a server: next to the WebSocket fallback.
/// `server` is host[:port]; `server_ws`, if set, the published WebSocket URL
/// (e.g. `wss://nuvira.example.com/nuvira` behind a reverse proxy).
pub fn base_url(server: &str, server_ws: Option<&str>, default_port: u16) -> String {
    if let Some(ws) = server_ws {
        let http = ws.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);
        return format!("{}/update", http.trim_end_matches('/'));
    }
    let has_port =
        server.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !server.ends_with(']');
    if has_port {
        format!("http://{server}/nuvira/update")
    } else {
        format!("http://{server}:{default_port}/nuvira/update")
    }
}

/// Base URL of the updates of a device on `server` (host[:port], the public server
/// already filled in for an empty setting): the published `server_ws` if set, the
/// HTTPS address for NuviraHub's public server, otherwise [`base_url`]. Only for
/// updates: `base_url` also builds the server's other addresses (invitations).
pub fn update_base(server: &str, server_ws: Option<&str>) -> String {
    let server = server.trim();
    let public = server.eq_ignore_ascii_case(PUBLIC_HOST)
        || server.eq_ignore_ascii_case(&format!("{PUBLIC_HOST}:{DEFAULT_PORT}"));
    if server_ws.is_none() && public {
        return PUBLIC_UPDATES.to_string();
    }
    base_url(server, server_ws, DEFAULT_PORT)
}

/// Updates look held back (see [`STALE_AFTER`]): the newest version a server
/// offers, and when it was published (`None`: no signed time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stale {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<u64>,
}

impl Stale {
    /// Text for the logs.
    pub fn describe(&self, base: &str) -> String {
        match self.published {
            Some(p) => format!(
                "updates may be held back: the newest version on {base} is {}, published {} days ago",
                self.version,
                now().saturating_sub(p) / 86_400
            ),
            None => format!(
                "updates may be held back: the newest version on {base} is {}, with no signed publication time",
                self.version
            ),
        }
    }
}

/// What a server offers on a channel, checked (signature, platform, version): its
/// newest version, whatever it is compared with this program.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub manifest: Manifest,
    /// The signed publication time, if any.
    pub published: Option<u64>,
}

impl Offer {
    /// Updates look held back at `now` (seconds since 1970): no signed publication
    /// time, or one older than [`STALE_AFTER`]. A time ahead of the clock is fresh.
    pub fn stale(&self, now: u64) -> Option<Stale> {
        let fresh = self.published.is_some_and(|p| now.saturating_sub(p) <= STALE_AFTER.as_secs());
        (!fresh).then(|| Stale { version: self.manifest.version.clone(), published: self.published })
    }
}

/// Seconds since 1970.
pub fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Writes `stale` to the [`STALE_FILE`] of `dir`, or removes the file when updates look fine.
pub fn record_stale(dir: &Path, stale: Option<&Stale>) {
    let path = dir.join(STALE_FILE);
    match stale.and_then(|s| serde_json::to_vec(s).ok()) {
        Some(json) => {
            let _ = std::fs::write(path, json);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The [`Stale`] written by [`record_stale`], if any.
pub fn read_stale(path: &Path) -> Option<Stale> {
    use std::io::Read;
    let mut text = Vec::new();
    std::fs::File::open(path).ok()?.take(4096).read_to_end(&mut text).ok()?;
    serde_json::from_slice(&text).ok()
}

/// The version offered by the server, if newer than this program and correctly
/// signed with `public_key`. `Ok(None)`: nothing to install.
pub async fn check(base: &str, keys: &[[u8; 32]]) -> Result<Option<Manifest>> {
    check_channel(base, keys, platform()).await
}

/// Like [`check`], for the channel `platform` (e.g. a Linux package's). Updates
/// that look held back are logged.
pub async fn check_channel(base: &str, keys: &[[u8; 32]], platform: &str) -> Result<Option<Manifest>> {
    Ok(check_offer(base, keys, platform).await?.0)
}

/// Like [`check_channel`], also telling whether updates look held back (logged
/// too). No manifest on the server (404): nothing to install and nothing to say.
pub async fn check_offer(
    base: &str,
    keys: &[[u8; 32]],
    platform: &str,
) -> Result<(Option<Manifest>, Option<Stale>)> {
    let Some(offer) = offer(base, keys, platform).await? else { return Ok((None, None)) };
    let stale = offer.stale(now());
    if let Some(stale) = &stale {
        tracing::warn!("{}", stale.describe(base));
    }
    Ok((newer(&offer.manifest.version, CURRENT).then_some(offer.manifest), stale))
}

/// The newest version on the server for `platform`, checked, whatever it is
/// compared with this program. `Ok(None)`: no manifest (404).
pub async fn offer(base: &str, keys: &[[u8; 32]], platform: &str) -> Result<Option<Offer>> {
    let url = format!("{base}/{platform}.json");
    let Some(body) = get(&url, MAX_MANIFEST, None).await? else { return Ok(None) };
    let manifest: Manifest = serde_json::from_slice(&body).with_context(|| format!("{url} is not valid"))?;
    if manifest.platform != platform {
        bail!("{url} is for {}", manifest.platform);
    }
    manifest.verify_any(keys)?;
    // Signed, but used in the download path: only digits and dots.
    if !is_version(&manifest.version) {
        bail!("{url}: {:?} is not a version", manifest.version);
    }
    let published = manifest.verified_published(keys);
    Ok(Some(Offer { manifest, published }))
}

/// Downloads the program of `manifest` to `dest`, checking size and SHA-256. On any
/// mismatch the file is deleted. The partial file (`dest` with the extension
/// `part`) of an earlier attempt is deleted first and a new one is always made: a
/// file or link someone else put there is never written through (APP-9).
pub async fn download(base: &str, manifest: &Manifest, dest: &Path) -> Result<()> {
    if !is_version(&manifest.version) {
        bail!("{:?} is not a version", manifest.version);
    }
    let url = format!("{base}/{}", manifest.program_path());
    let partial = dest.with_extension("part");
    let _ = tokio::fs::remove_file(&partial).await;
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await
            .with_context(|| format!("creating {}", partial.display()))?;
        let mut hash = Sha256::new();
        get(&url, MAX_PROGRAM.min(manifest.size), Some((&mut file, &mut hash)))
            .await?
            .context("the program is not on the server")?;
        file.flush().await?;
        drop(file);
        let written = tokio::fs::metadata(&partial).await?.len();
        if written != manifest.size {
            bail!("downloaded {written} bytes instead of {}", manifest.size);
        }
        if hex(&hash.finalize()) != manifest.sha256 {
            bail!("the program does not match the manifest (SHA-256)");
        }
        tokio::fs::rename(&partial, dest).await.with_context(|| format!("renaming to {}", dest.display()))
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
    }
    result
}

/// Downloads the program of `manifest` to `dest` unless `dest` already is that
/// program ([`file_matches`]): whatever else is there under the same name (a file
/// of an earlier attempt, another build of the same version, a file put there by
/// someone who can write the folder) is deleted and downloaded again, never
/// installed as it is. True when it was downloaded.
pub async fn download_unless_cached(base: &str, manifest: &Manifest, dest: &Path) -> Result<bool> {
    if file_matches(dest, manifest) {
        return Ok(false);
    }
    let _ = tokio::fs::remove_file(dest).await;
    download(base, manifest, dest).await?;
    Ok(true)
}

/// Minimal HTTP/1.1 GET (http and https). `Ok(None)` for 404. The body goes to
/// `sink` (written and hashed) if given, otherwise it is returned. At most `limit` bytes.
async fn get(
    url: &str,
    limit: u64,
    sink: Option<(&mut tokio::fs::File, &mut Sha256)>,
) -> Result<Option<Vec<u8>>> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("unsupported URL {url}");
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h).trim_matches(['[', ']']);
    let address = if authority.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        authority.to_string()
    } else {
        format!("{authority}:{}", if tls { 443 } else { 80 })
    };
    let tcp = tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect(&address))
        .await
        .with_context(|| format!("{url}: timed out"))?
        .with_context(|| format!("connecting to {address}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: nuvira/{CURRENT}\r\nConnection: close\r\n\r\n"
    );
    if tls {
        let config =
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(rustls::RootCertStore::from_iter(
                    webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                ))
                .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await?;
        exchange(stream, &request, url, limit, sink).await
    } else {
        exchange(tcp, &request, url, limit, sink).await
    }
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &str,
    url: &str,
    limit: u64,
    mut sink: Option<(&mut tokio::fs::File, &mut Sha256)>,
) -> Result<Option<Vec<u8>>> {
    stream.write_all(request.as_bytes()).await?;
    // Headers.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 << 10 {
            bail!("{url}: headers too long");
        }
        let n = tokio::time::timeout(TIMEOUT, stream.read(&mut byte)).await.context("timed out")??;
        if n == 0 {
            bail!("{url}: connection closed before the response");
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status: u16 =
        head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).context("not an HTTP response")?;
    if status == 404 {
        return Ok(None);
    }
    if status != 200 {
        bail!("{url}: HTTP {status}");
    }
    let header = |name: &str| {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    if header("Transfer-Encoding").is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
        bail!("{url}: chunked responses are not supported");
    }
    let length: Option<u64> = header("Content-Length").and_then(|v| v.parse().ok());
    if length.is_some_and(|l| l > limit) {
        bail!("{url}: {} bytes, more than the {limit} expected", length.unwrap_or_default());
    }
    // Body: up to Content-Length, or to the end (Connection: close).
    let mut body = Vec::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        let want = length.map_or(buffer.len() as u64, |l| (l - total).min(buffer.len() as u64)) as usize;
        if want == 0 {
            break;
        }
        let n =
            tokio::time::timeout(TIMEOUT, stream.read(&mut buffer[..want])).await.context("timed out")??;
        if n == 0 {
            if length.is_some() {
                bail!("{url}: connection closed after {total} bytes");
            }
            break;
        }
        total += n as u64;
        if total > limit {
            bail!("{url}: more than the {limit} bytes expected");
        }
        match sink.as_mut() {
            Some((file, hash)) => {
                hash.update(&buffer[..n]);
                file.write_all(&buffer[..n]).await?;
            }
            None => body.extend_from_slice(&buffer[..n]),
        }
    }
    Ok(Some(body))
}

/// True if `path` is already the program of `manifest` (same size and SHA-256): a
/// download kept under a fixed name is used again only when it is the right version.
pub fn file_matches(path: &Path, manifest: &Manifest) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    if file.metadata().map(|m| m.len()).ok() != Some(manifest.size) {
        return false;
    }
    let mut hash = Sha256::new();
    let mut reader = std::io::BufReader::new(file);
    if std::io::copy(&mut reader, &mut hash).is_err() {
        return false;
    }
    hex(&hash.finalize()) == manifest.sha256
}

pub fn hex(bytes: &[u8]) -> String {
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

    fn test_key() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).expect("key");
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("key")
    }

    #[test]
    fn file_matches_only_the_same_program() {
        let path = std::env::temp_dir().join(format!("nuvira-file-matches-{}", std::process::id()));
        let manifest = Manifest::sign(&test_key(), "0.2.0", "windows-x64", b"program");
        assert!(!file_matches(&path, &manifest), "missing file");
        std::fs::write(&path, b"program").expect("write");
        assert!(file_matches(&path, &manifest));
        std::fs::write(&path, b"programs").expect("write");
        assert!(!file_matches(&path, &manifest), "other size");
        std::fs::write(&path, b"PROGRAM").expect("write");
        assert!(!file_matches(&path, &manifest), "same size, other content");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn either_release_key() {
        let (first, second) = (test_key(), test_key());
        let keys: [[u8; 32]; 2] = [
            first.public_key().as_ref().try_into().expect("32"),
            second.public_key().as_ref().try_into().expect("32"),
        ];
        for key in [&first, &second] {
            Manifest::sign(key, "0.2.0", "windows-x64", b"program").verify_any(&keys).expect("trusted key");
        }
        assert!(Manifest::sign(&test_key(), "0.2.0", "windows-x64", b"program").verify_any(&keys).is_err());
        assert_ne!(PUBLIC_KEYS[0], PUBLIC_KEYS[1]);
    }

    #[test]
    fn signed_manifest() {
        let key = test_key();
        let m = Manifest::sign(&key, "0.2.0", "windows-x64", b"program");
        m.verify(key.public_key().as_ref()).expect("valid");
        // Another key, or any field changed: rejected.
        assert!(m.verify(test_key().public_key().as_ref()).is_err());
        for tamper in [
            |m: &mut Manifest| m.version = "9.9.9".into(),
            |m: &mut Manifest| m.size += 1,
            |m: &mut Manifest| m.sha256 = hex(&Sha256::digest(b"other")),
            |m: &mut Manifest| m.platform = "windows-arm64".into(),
        ] {
            let mut t = m.clone();
            tamper(&mut t);
            assert!(t.verify(key.public_key().as_ref()).is_err());
        }
        assert_eq!(m.program_path(), "windows-x64/nuvira-0.2.0.exe");
        let path = |platform: &str| Manifest::sign(&key, "0.17.0", platform, b"p").program_path();
        assert_eq!(path("linux-x64"), "linux-x64/nuvira-0.17.0");
        assert_eq!(path("linux-x64-deb"), "linux-x64-deb/nuvira-0.17.0.deb");
        assert_eq!(path("linux-x64-rpm"), "linux-x64-rpm/nuvira-0.17.0.rpm");
        assert_eq!(path("macos"), "macos/nuvira-0.17.0.zip");
    }

    /// APP-1: the signed publication time. Older programs check only `signature`,
    /// which stays the same; the time cannot be changed, moved to another manifest
    /// or signed by another key; a manifest without it, as before, still verifies.
    #[test]
    fn published_time() {
        let key = test_key();
        let keys: [[u8; 32]; 1] = [key.public_key().as_ref().try_into().expect("32")];
        let plain = Manifest::sign(&key, "0.29.0", "linux-x64", b"program");
        let dated = Manifest::sign_published(&key, "0.29.0", "linux-x64", b"program", 1_800_000_000);
        assert_eq!(dated.signature, plain.signature);
        dated.verify_any(&keys).expect("first signature");
        assert_eq!(dated.verified_published(&keys), Some(1_800_000_000));
        assert_eq!(plain.verified_published(&keys), None);
        let mut moved = dated.clone();
        moved.published = Some(1_900_000_000);
        assert_eq!(moved.verified_published(&keys), None, "time changed");
        let mut other = Manifest::sign(&key, "0.29.1", "linux-x64", b"program");
        other.published = dated.published;
        other.published_signature = dated.published_signature.clone();
        assert_eq!(other.verified_published(&keys), None, "time of another manifest");
        let foreign = Manifest::sign_published(&test_key(), "0.29.0", "linux-x64", b"program", 1_800_000_000);
        assert_eq!(foreign.verified_published(&keys), None, "another key");
        let mut garbage = dated.clone();
        garbage.published_signature = Some("zz".into());
        assert_eq!(garbage.verified_published(&keys), None);

        // In JSON: two more fields, read by a manifest of before as unknown ones.
        let json = serde_json::to_value(&dated).expect("json");
        assert_eq!(json["published"], 1_800_000_000u64);
        assert!(json["published_signature"].is_string());
        assert!(!serde_json::to_string(&plain).expect("json").contains("published"));
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Before {
            version: String,
            platform: String,
            size: u64,
            sha256: String,
            signature: String,
        }
        let before: Before = serde_json::from_value(json).expect("old reader");
        assert_eq!(before.signature, plain.signature);
    }

    /// APP-1: when updates look held back.
    #[test]
    fn stale_offers() {
        let key = test_key();
        let day = 86_400;
        let now = 1_800_000_000;
        let offer = |published: Option<u64>| Offer {
            manifest: match published {
                Some(p) => Manifest::sign_published(&key, "0.29.0", "linux-x64", b"p", p),
                None => Manifest::sign(&key, "0.29.0", "linux-x64", b"p"),
            },
            published,
        };
        assert_eq!(offer(Some(now - day)).stale(now), None);
        assert_eq!(offer(Some(now - 90 * day)).stale(now), None);
        assert_eq!(offer(Some(now + day)).stale(now), None, "ahead of the clock");
        assert_eq!(
            offer(Some(now - 91 * day)).stale(now),
            Some(Stale { version: "0.29.0".into(), published: Some(now - 91 * day) })
        );
        assert_eq!(offer(None).stale(now), Some(Stale { version: "0.29.0".into(), published: None }));

        let dir = std::env::temp_dir().join(format!("nuvira-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let stale = Stale { version: "0.29.0".into(), published: Some(now) };
        record_stale(&dir, Some(&stale));
        assert_eq!(read_stale(&dir.join(STALE_FILE)), Some(stale));
        record_stale(&dir, None);
        assert!(!dir.join(STALE_FILE).exists());
        assert_eq!(read_stale(&dir.join(STALE_FILE)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn versions() {
        assert!(newer("0.0.2", "0.0.1"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.99.99"));
        assert!(!newer("0.0.1", "0.0.1"));
        assert!(!newer("0.0.1", "0.0.2"));
        assert!(!newer("1.0", "1.0.0"));
    }

    /// APP-2: every pair of plain numeric versions keeps the order of before the
    /// strict parser (the installed programs still use the old function).
    #[test]
    fn numeric_versions_keep_their_order() {
        let old = |a: &str, b: &str| {
            let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
            let (mut a, mut b) = (parse(a), parse(b));
            let n = a.len().max(b.len());
            a.resize(n, 0);
            b.resize(n, 0);
            a > b
        };
        let all = [
            "0", "0.0.1", "0.0.2", "0.9.9", "0.10.0", "0.22.4", "0.28.0", "0.28.0.0", "0.28.1", "0.99.0",
            "0.99.12", "1", "1.0", "1.0.0", "1.0.0.1", "1.2.3", "10.0.0", "2.0.0",
        ];
        for a in all {
            assert!(is_version(a), "{a}");
            for b in all {
                assert_eq!(newer(a, b), old(a, b), "{a} vs {b}");
            }
        }
        for not_a_version in
            ["", "v1.0.0", "1..0", ".1", "1.", "1.0.0-rc.1", "1.0.0-rc1", "../../x", "1.x", " 1.0"]
        {
            assert!(!is_version(not_a_version), "{not_a_version:?}");
        }
    }

    #[test]
    fn urls() {
        assert_eq!(base_url("alexbn.ddns.net", None, 21116), "http://alexbn.ddns.net:21116/nuvira/update");
        assert_eq!(base_url("10.0.0.5:21117", None, 21116), "http://10.0.0.5:21117/nuvira/update");
        assert_eq!(
            base_url("x", Some("wss://nuvira.example.com/nuvira"), 21116),
            "https://nuvira.example.com/nuvira/update"
        );
    }

    /// APP-1: the public server's updates over HTTPS, without a port; any other
    /// server, or a published WebSocket address, as before. base_url itself is
    /// unchanged for the public server (it also builds the invitation addresses).
    #[test]
    fn update_urls() {
        for public in ["connect.nuvirahub.dev", "CONNECT.nuvirahub.dev", "connect.nuvirahub.dev:21116"] {
            assert_eq!(update_base(public, None), "https://connect.nuvirahub.dev/nuvira/update", "{public}");
        }
        assert_eq!(
            update_base("connect.nuvirahub.dev:21117", None),
            "http://connect.nuvirahub.dev:21117/nuvira/update"
        );
        assert_eq!(update_base("alexbn.ddns.net", None), "http://alexbn.ddns.net:21116/nuvira/update");
        assert_eq!(update_base("10.0.0.5:21117", None), "http://10.0.0.5:21117/nuvira/update");
        assert_eq!(
            update_base("connect.nuvirahub.dev", Some("wss://nuvira.example.com/nuvira")),
            "https://nuvira.example.com/nuvira/update"
        );
        assert_eq!(
            base_url("connect.nuvirahub.dev", None, DEFAULT_PORT),
            "http://connect.nuvirahub.dev:21116/nuvira/update"
        );
    }

    /// Download from a local HTTP server: wrong hash or size are rejected and leave nothing.
    #[test]
    fn download_checks_the_program() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(async {
            let program = b"the new program".to_vec();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let served = program.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut s, _)) = listener.accept().await else { return };
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", served.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&served).await;
                }
            });
            let base = format!("http://127.0.0.1:{port}/nuvira/update");
            let dir = std::env::temp_dir().join(format!("nuvira-update-test-{port}"));
            std::fs::create_dir_all(&dir).expect("dir");
            let dest = dir.join("new.exe");
            let key = test_key();

            let good = Manifest::sign(&key, "0.2.0", platform(), &program);
            download(&base, &good, &dest).await.expect("download");
            assert_eq!(std::fs::read(&dest).expect("read"), program);
            std::fs::remove_file(&dest).expect("remove");

            let mut wrong_hash = good.clone();
            wrong_hash.sha256 = hex(&Sha256::digest(b"something else"));
            assert!(download(&base, &wrong_hash, &dest).await.is_err());
            let mut wrong_size = good.clone();
            wrong_size.size += 1;
            assert!(download(&base, &wrong_size, &dest).await.is_err());
            assert!(!dest.exists() && !dest.with_extension("part").exists());

            // APP-9: a partial file left by an earlier attempt does not stop the
            // download; a link there is replaced, not written through.
            std::fs::write(dest.with_extension("part"), b"left over").expect("write");
            download(&base, &good, &dest).await.expect("download over a partial file");
            assert_eq!(std::fs::read(&dest).expect("read"), program);
            std::fs::remove_file(&dest).expect("remove");
            #[cfg(unix)]
            {
                let victim = dir.join("victim");
                std::fs::write(&victim, b"keep me").expect("write");
                std::os::unix::fs::symlink(&victim, dest.with_extension("part")).expect("link");
                download(&base, &good, &dest).await.expect("download over a link");
                assert_eq!(std::fs::read(&victim).expect("read"), b"keep me");
                assert_eq!(std::fs::read(&dest).expect("read"), program);
                std::fs::remove_file(&dest).expect("remove");
            }

            // APP-8: a cached file with the right name and other content is
            // downloaded again; the right program is used as it is.
            std::fs::write(&dest, b"the old program").expect("write");
            assert!(download_unless_cached(&base, &good, &dest).await.expect("download"));
            assert_eq!(std::fs::read(&dest).expect("read"), program);
            assert!(!download_unless_cached(&base, &good, &dest).await.expect("cached"));
            assert_eq!(std::fs::read(&dest).expect("read"), program);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
