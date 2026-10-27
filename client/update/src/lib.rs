//! Automatic updates.
//!
//! Each nexodesk-server distributes the version its devices should run, from the
//! `updates` folder of its data, on the TCP port of the WebSocket fallback:
//!
//! ```text
//! GET /nexodesk/update/windows-x64.json                 manifest (this module's `Manifest`)
//! GET /nexodesk/update/windows-x64/nexodesk-0.1.0.exe   the program
//! ```
//!
//! The manifest is signed with the NexoDesk release key (Ed25519, kept by whoever
//! builds the releases; `nexodesk-release` makes and uses it) and the program is
//! checked against the size and SHA-256 in it. The public key is built into the
//! program: a tampered server or download cannot get anything installed.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Public half of the NexoDesk release key (see `nexodesk-release keygen`).
/// 59fbcd29526e30ef3b4ad1b44eea9c4393eab3f5affa45baf3d0375da95df7a8
pub const PUBLIC_KEY: [u8; 32] = [
    0x59, 0xfb, 0xcd, 0x29, 0x52, 0x6e, 0x30, 0xef, 0x3b, 0x4a, 0xd1, 0xb4, 0x4e, 0xea, 0x9c, 0x43, //
    0x93, 0xea, 0xb3, 0xf5, 0xaf, 0xfa, 0x45, 0xba, 0xf3, 0xd0, 0x37, 0x5d, 0xa9, 0x5d, 0xf7, 0xa8,
];

/// Port of nexodesk-server when the address has none.
pub const DEFAULT_PORT: u16 = 21116;

/// Version of this program.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Largest program accepted (the Windows one is ~28 MB).
const MAX_PROGRAM: u64 = 256 << 20;
const MAX_MANIFEST: u64 = 64 << 10;
const TIMEOUT: Duration = Duration::from_secs(20);

/// Name of this platform in the update paths.
pub fn platform() -> &'static str {
    if cfg!(all(windows, target_arch = "x86_64")) {
        "windows-x64"
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        "windows-arm64"
    } else {
        "unsupported"
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
}

impl Manifest {
    /// Path of the program under the update URL.
    pub fn program_path(&self) -> String {
        format!("{}/nexodesk-{}.exe", self.platform, self.version)
    }

    fn signed_message(&self) -> Vec<u8> {
        format!("nexodesk-update\n{}\n{}\n{}\n{}", self.version, self.platform, self.size, self.sha256)
            .into_bytes()
    }

    /// Makes a signed manifest for `program` (used by `nexodesk-release`).
    pub fn sign(key: &Ed25519KeyPair, version: &str, platform: &str, program: &[u8]) -> Self {
        let mut manifest = Self {
            version: version.to_string(),
            platform: platform.to_string(),
            size: program.len() as u64,
            sha256: hex(&Sha256::digest(program)),
            signature: String::new(),
        };
        manifest.signature = hex(key.sign(&manifest.signed_message()).as_ref());
        manifest
    }

    /// Checks the signature against `public_key`.
    pub fn verify(&self, public_key: &[u8]) -> Result<()> {
        let signature = unhex(&self.signature).context("signature is not hexadecimal")?;
        UnparsedPublicKey::new(&ED25519, public_key)
            .verify(&self.signed_message(), &signature)
            .map_err(|_| anyhow::anyhow!("the update signature is not valid"))
    }
}

/// Public key of a key pair in PKCS#8 (as written by `nexodesk-release keygen`).
pub fn public_key_of(pkcs8: &[u8]) -> Result<[u8; 32]> {
    let key = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| anyhow::anyhow!("not an Ed25519 PKCS#8 key"))?;
    Ok(key.public_key().as_ref().try_into()?)
}

/// `a` is newer than `b` (versions `x.y.z`; missing parts count as 0).
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (mut a, mut b) = (parse(a), parse(b));
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a > b
}

/// Base URL of the updates on a server: next to the WebSocket fallback.
/// `server` is host[:port]; `server_ws`, if set, the published WebSocket URL
/// (e.g. `wss://nexodesk.example.com/nexodesk` behind a reverse proxy).
pub fn base_url(server: &str, server_ws: Option<&str>, default_port: u16) -> String {
    if let Some(ws) = server_ws {
        let http = ws.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);
        return format!("{}/update", http.trim_end_matches('/'));
    }
    let has_port =
        server.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !server.ends_with(']');
    if has_port {
        format!("http://{server}/nexodesk/update")
    } else {
        format!("http://{server}:{default_port}/nexodesk/update")
    }
}

/// The version offered by the server, if newer than this program and correctly
/// signed with `public_key`. `Ok(None)`: nothing to install.
pub async fn check(base: &str, public_key: &[u8]) -> Result<Option<Manifest>> {
    let url = format!("{base}/{}.json", platform());
    let Some(body) = get(&url, MAX_MANIFEST, None).await? else { return Ok(None) };
    let manifest: Manifest = serde_json::from_slice(&body).with_context(|| format!("{url} is not valid"))?;
    if manifest.platform != platform() {
        bail!("{url} is for {}", manifest.platform);
    }
    manifest.verify(public_key)?;
    Ok(newer(&manifest.version, CURRENT).then_some(manifest))
}

/// Downloads the program of `manifest` to `dest`, checking size and SHA-256. On any
/// mismatch the file is deleted.
pub async fn download(base: &str, manifest: &Manifest, dest: &Path) -> Result<()> {
    let url = format!("{base}/{}", manifest.program_path());
    let partial = dest.with_extension("part");
    let result = async {
        let mut file = tokio::fs::File::create(&partial)
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
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: NexoDesk/{CURRENT}\r\nConnection: close\r\n\r\n"
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
        assert_eq!(m.program_path(), "windows-x64/nexodesk-0.2.0.exe");
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

    #[test]
    fn urls() {
        assert_eq!(base_url("alexbn.ddns.net", None, 21116), "http://alexbn.ddns.net:21116/nexodesk/update");
        assert_eq!(base_url("10.0.0.5:21117", None, 21116), "http://10.0.0.5:21117/nexodesk/update");
        assert_eq!(
            base_url("x", Some("wss://nexodesk.example.com/nexodesk"), 21116),
            "https://nexodesk.example.com/nexodesk/update"
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
            let base = format!("http://127.0.0.1:{port}/nexodesk/update");
            let dir = std::env::temp_dir().join(format!("nexodesk-update-test-{port}"));
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
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
