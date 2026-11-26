//! Signalling connection with nuvira-server: device registration (proof of
//! the key), then exchange of requests and routes.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::{ClientMessage, Proof, Register, ServerMessage, client_message, server_message};
use quinn::ClientConfig;
use quinn::crypto::rustls::QuicClientConfig;

use crate::identity::Identity;
use crate::{
    Fingerprint, FramedRecv, FramedSend, PinnedCert, ServerTransport, provider, signal_transport_config,
};

/// Signalling ALPN (must match the server's).
const ALPN: &[u8] = b"nuvira-signal/1";
/// Signalling protocol version: 2 = the agent applies the limits of each session
/// (`Incoming.limits`).
pub const PROTOCOL_VERSION: u32 = 2;
/// Prefix signed together with the nonce (as in the server).
const REGISTER_CONTEXT: &[u8] = b"nuvira-register-v1";
/// Default UDP port of nuvira-server.
pub const DEFAULT_PORT: u16 = 21116;

/// NuviraHub's public server, used when no server is set.
pub const PUBLIC_SERVER: &str = "connect.nuvirahub.dev";
/// Its certificate fingerprint, built in: even the first connection is checked.
const PUBLIC_SERVER_KEY: &str = "9e6828e460bf9fa4745babf35878ae68ee728f0d8ac266e0ad097874e0e64052";

/// The server to use: `server`, or the public one when it is empty.
pub fn server_or_public(server: &str) -> &str {
    match server.trim() {
        "" => PUBLIC_SERVER,
        s => s,
    }
}

pub struct Signal {
    pub device_id: String,
    /// Rules of the server for its devices (all off from servers without them).
    pub policy: nuvira_proto::v1::Policy,
    /// Optional modules of the server (NuviraHub Pro), e.g. "pro.address_book".
    pub features: Vec<String>,
    /// The device's alias on the server, and why the one it asked for is not it.
    pub alias: String,
    pub alias_error: String,
    /// Server address: it is also the relay address.
    pub server: SocketAddr,
    /// Fingerprint of the server certificate seen in this connection.
    pub server_fingerprint: Fingerprint,
    /// Transport the server was reached with: sessions will use the same one.
    pub transport: ServerTransport,
    pub tx: SignalSender,
    pub rx: SignalReceiver,
    /// The QUIC connection: further streams carry files to the server (Upload).
    pub conn: quinn::Connection,
}

pub struct SignalSender(FramedSend);
pub struct SignalReceiver(FramedRecv);

impl SignalSender {
    pub async fn send(&mut self, msg: client_message::Msg) -> Result<()> {
        self.0.send(&ClientMessage { msg: Some(msg) }).await
    }
}

impl SignalReceiver {
    /// Next message from the server; `None` if it closed the connection.
    pub async fn recv(&mut self) -> Result<Option<server_message::Msg>> {
        Ok(self.0.recv::<ServerMessage>().await?.and_then(|m| m.msg))
    }
}

/// Which transport to use towards the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportMode {
    /// UDP; if it does not answer within a few seconds, WebSocket.
    #[default]
    Auto,
    Udp,
    WebSocket,
}

/// Time given to UDP before trying the WebSocket.
const UDP_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(4);

/// Time a UDP handshake with the server gets before it is started again, once,
/// from a new socket (review NET-13): a handshake answered in time takes one
/// round trip. A socket can be the wrong one: the server's relay forwards
/// datagrams by source address before its QUIC listener sees them, so a new
/// socket that gets the port of a recent session's (a link the relay still
/// keeps, on Linux and macOS) has its Initials forwarded to the old peer. With
/// [`TransportMode::Auto`] the second try has what is left of [`UDP_ATTEMPT`].
const UDP_HANDSHAKE_RETRY: std::time::Duration = std::time::Duration::from_secs(3);

/// Like [`connect`], choosing the transport: with `Auto` it tries UDP and, if the
/// server does not answer (network blocking UDP), moves to the WebSocket `ws_url`.
pub async fn connect_auto(
    server: SocketAddr,
    ws_url: &str,
    mode: TransportMode,
    expected: Option<Fingerprint>,
    identity: &Identity,
    device: &DeviceInfo<'_>,
) -> Result<Signal> {
    let ws = ServerTransport::WebSocket(ws_url.to_string());
    match mode {
        TransportMode::Udp => connect(server, &ServerTransport::Udp, expected, identity, device).await,
        TransportMode::WebSocket => connect(server, &ws, expected, identity, device).await,
        TransportMode::Auto => {
            let udp = tokio::time::timeout(
                UDP_ATTEMPT,
                connect(server, &ServerTransport::Udp, expected, identity, device),
            )
            .await;
            match udp {
                Ok(Ok(signal)) => Ok(signal),
                // The server answered but refuses (fingerprint, registration): the WebSocket would not change anything.
                Ok(Err(e)) if !is_unreachable(&e) => Err(e),
                other => {
                    let why = match other {
                        Ok(Err(e)) => format!("{e:#}"),
                        _ => "no answer".into(),
                    };
                    tracing::info!(
                        "nuvira-server not reachable over UDP ({why}): trying the WebSocket {ws_url}"
                    );
                    connect(server, &ws, expected, identity, device).await
                }
            }
        }
    }
}

/// Does the error mean the server was not reached (rather than a refusal)?
fn is_unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<quinn::ConnectionError>(),
            Some(quinn::ConnectionError::TimedOut | quinn::ConnectionError::Reset)
        ) || c.downcast_ref::<std::io::Error>().is_some()
    })
}

/// What to ask the server to connect to: the ID's digits (spaces and dashes
/// typed with it ignored), or an alias as typed, trimmed and in lower case.
pub fn target(typed: &str) -> String {
    let typed = typed.trim();
    if typed.chars().all(|c| c.is_ascii_digit() || matches!(c, ' ' | '-' | '.')) {
        typed.chars().filter(char::is_ascii_digit).collect()
    } else {
        typed.to_lowercase()
    }
}

/// A `target` the server can resolve: 9 digits, or an alias (3 to 32 of a-z,
/// 0-9 and "-", at least one letter, no "-" at the ends; docs/46-aliases.md).
pub fn valid_target(target: &str) -> bool {
    let b = target.as_bytes();
    if b.iter().all(u8::is_ascii_digit) {
        return b.len() == 9;
    }
    (3..=32).contains(&b.len())
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b.iter().any(u8::is_ascii_lowercase)
}

/// How the device introduces itself to the server.
pub struct DeviceInfo<'a> {
    pub name: &'a str,
    /// Not reachable: it receives no connection requests (viewer, app using the address book).
    pub reachable: bool,
    /// The alias it keeps (agents; `None`: the server's stays).
    pub alias: Option<String>,
}

/// Random bytes of this process, sent in every registration (`Register.instance`):
/// the server tells a reconnection of the same process from a new one.
pub fn instance() -> &'static [u8; 16] {
    static INSTANCE: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| {
        let mut bytes = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("random generator");
        bytes
    })
}

/// Connects to nuvira-server and registers the device. With `expected` the server
/// certificate must have that fingerprint; without it, any fingerprint is accepted
/// and returned in `server_fingerprint` (to be remembered: see [`known_server`]).
pub async fn connect(
    server: SocketAddr,
    transport: &ServerTransport,
    expected: Option<Fingerprint>,
    identity: &Identity,
    device: &DeviceInfo<'_>,
) -> Result<Signal> {
    let seen = Arc::new(Mutex::new(None));
    let verifier = PinnedCert { expected, provider: provider(), seen: Some(seen.clone()) };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(signal_transport_config());

    let handshake = async || -> Result<quinn::Connection> {
        let mut endpoint = crate::endpoint_for(transport, server, None, None).await?;
        endpoint.set_default_client_config(config.clone());
        endpoint
            .connect(server, "nuvira-server")?
            .await
            .with_context(|| format!("connecting to nuvira-server {server}"))
    };
    let conn = match transport {
        ServerTransport::Udp => match tokio::time::timeout(UDP_HANDSHAKE_RETRY, handshake()).await {
            Ok(conn) => conn?,
            Err(_) => {
                tracing::info!(
                    "no answer from nuvira-server {server} over UDP in {} s: trying again from a new socket",
                    UDP_HANDSHAKE_RETRY.as_secs()
                );
                handshake().await?
            }
        },
        ServerTransport::WebSocket(_) => handshake().await?,
    };
    let server_fingerprint =
        seen.lock().expect("fingerprint").expect("fingerprint stored during the handshake");

    let (send, recv) = conn.open_bi().await?;
    let (mut tx, mut rx) = (SignalSender(FramedSend::new(send)), SignalReceiver(FramedRecv::new(recv)));
    tx.send(client_message::Msg::Register(Register {
        public_key: identity.public_key().to_vec(),
        device_name: device.name.to_string(),
        os: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION,
        api_only: !device.reachable,
        alias: device.alias.clone(),
        instance: instance().to_vec(),
    }))
    .await?;

    let nonce = match rx.recv().await? {
        Some(server_message::Msg::Challenge(c)) => c.nonce,
        Some(server_message::Msg::Error(e)) => bail!("the server refuses the registration: {}", e.message),
        other => bail!("unexpected answer to the registration: {other:?}"),
    };
    let signature = identity.sign(&[REGISTER_CONTEXT, &nonce].concat())?;
    tx.send(client_message::Msg::Proof(Proof { signature })).await?;
    let (device_id, policy, features, alias, alias_error) = match rx.recv().await? {
        Some(server_message::Msg::Registered(r)) => {
            (r.device_id, r.policy.unwrap_or_default(), r.features, r.alias, r.alias_error)
        }
        Some(server_message::Msg::Error(e)) => bail!("the server refuses the registration: {}", e.message),
        other => bail!("unexpected answer to the registration: {other:?}"),
    };
    Ok(Signal {
        device_id,
        policy,
        features,
        alias,
        alias_error,
        server,
        server_fingerprint,
        transport: transport.clone(),
        tx,
        rx,
        conn,
    })
}

/// Fingerprints of the servers already seen (`servers.txt` in the data folder): at
/// the first connection the fingerprint is remembered, afterwards it must not change.
///
/// One line per server, `server fingerprint`, with the server as it was given.
/// Lookups compare servers by [`key`](known_server::key) (review NET-7): "NAS",
/// "nas" and "nas:21116" are one server, also in lines written by older versions.
pub mod known_server {
    use std::path::{Path, PathBuf};

    use super::*;

    fn path() -> PathBuf {
        crate::identity::data_dir().join("servers.txt")
    }

    /// The server compared by lookups: lower case, the default port added, an IP
    /// address in its canonical form (`[::1]:21116`).
    pub fn key(server: &str) -> String {
        let server = server.trim().to_ascii_lowercase();
        if let Ok(addr) = server.parse::<SocketAddr>() {
            return addr.to_string();
        }
        if let Ok(ip) = server.parse::<std::net::IpAddr>() {
            return SocketAddr::new(ip, DEFAULT_PORT).to_string();
        }
        match server.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => server,
            _ => format!("{server}:{DEFAULT_PORT}"),
        }
    }

    pub fn get(server: &str) -> Option<Fingerprint> {
        if key(server) == key(PUBLIC_SERVER) {
            return crate::parse_fingerprint(PUBLIC_SERVER_KEY).ok();
        }
        get_in(&path(), server)
    }

    fn get_in(path: &Path, server: &str) -> Option<Fingerprint> {
        let text = std::fs::read_to_string(path).ok()?;
        find(&text, &key(server))
    }

    /// The fingerprint of the first line for `key` (as before: a later line for the
    /// same server never replaces it).
    fn find(text: &str, key: &str) -> Option<Fingerprint> {
        text.lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(s, _)| self::key(s) == key)
            .and_then(|(_, fp)| crate::parse_fingerprint(fp).ok())
    }

    pub fn remember(server: &str, fp: &Fingerprint) -> Result<()> {
        remember_in(&path(), server, fp)
    }

    /// Adds `server` unless a line for it exists (another program may have
    /// remembered it since this one looked), through a new file renamed over the
    /// old one: a reader never sees the file empty or half written (review NET-7;
    /// it used to be truncated and rewritten in place). Two programs remembering
    /// at the same moment can still lose one line: that server is then simply
    /// remembered again at its next connection.
    fn remember_in(path: &Path, server: &str, fp: &Fingerprint) -> Result<()> {
        static WRITING: Mutex<()> = Mutex::new(());
        let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let mut text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if find(&text, &key(server)).is_some() {
            return Ok(());
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("{} {}\n", server.trim(), crate::fingerprint_hex(fp)));
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let partial = path.with_extension(format!("txt-new-{}", std::process::id()));
        let written = std::fs::write(&partial, &text).and_then(|()| std::fs::rename(&partial, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        written.with_context(|| format!("writing {}", path.display()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp_dir(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("nuvira-known-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn keys() {
            assert_eq!(key("NAS"), "nas:21116");
            assert_eq!(key(" nas:21116 "), "nas:21116");
            assert_eq!(key("Nas.Example:21117"), "nas.example:21117");
            assert_eq!(key("192.168.1.5"), "192.168.1.5:21116");
            assert_eq!(key("192.168.1.5:21116"), "192.168.1.5:21116");
            assert_eq!(key("::1"), "[::1]:21116");
            assert_eq!(key("[0:0::1]:21116"), "[::1]:21116");
            assert_eq!(key("FE80::1"), "[fe80::1]:21116");
            assert_ne!(key("nas:21117"), key("nas"));
        }

        #[test]
        fn one_line_per_server_and_any_spelling_finds_it() {
            let dir = temp_dir("spelling");
            let path = dir.join("servers.txt");
            let (a, b) = ([1u8; 32], [2u8; 32]);
            remember_in(&path, "NAS", &a).unwrap();
            remember_in(&path, "nas:21116", &b).unwrap();
            remember_in(&path, "nas", &b).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text, format!("NAS {}\n", crate::fingerprint_hex(&a)));
            for spelling in ["nas", "NAS", "nas:21116", " Nas:21116"] {
                assert_eq!(get_in(&path, spelling), Some(a), "{spelling}");
            }
            assert_eq!(get_in(&path, "nas:21117"), None);
            // Another port is another server.
            remember_in(&path, "nas:21117", &b).unwrap();
            assert_eq!(get_in(&path, "NAS:21117"), Some(b));
            assert_eq!(get_in(&path, "nas"), Some(a));
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn lines_of_older_versions_stay_readable() {
            let dir = temp_dir("old");
            let path = dir.join("servers.txt");
            let (a, b) = ([3u8; 32], [4u8; 32]);
            // Older versions appended one line per spelling, and the last line may
            // lack its newline.
            let old = format!("Nas {}\nnas:21116 {}", crate::fingerprint_hex(&a), crate::fingerprint_hex(&b));
            std::fs::write(&path, &old).unwrap();
            assert_eq!(get_in(&path, "nas:21116"), Some(a));
            remember_in(&path, "other", &b).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text, format!("{old}\nother {}\n", crate::fingerprint_hex(&b)));
            assert_eq!(get_in(&path, "OTHER:21116"), Some(b));
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn readers_never_see_the_file_empty_while_it_is_rewritten() {
            use std::sync::atomic::{AtomicBool, Ordering};
            let dir = temp_dir("race");
            let path = dir.join("servers.txt");
            let first = [9u8; 32];
            remember_in(&path, "first", &first).unwrap();
            let done = Arc::new(AtomicBool::new(false));
            let writers: Vec<_> = (0..2u8)
                .map(|w| {
                    let path = path.clone();
                    std::thread::spawn(move || {
                        for i in 0..200u32 {
                            remember_in(&path, &format!("s{w}-{i}"), &[w; 32]).unwrap();
                        }
                    })
                })
                .collect();
            let reader = {
                let (path, done) = (path.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut reads = 0u32;
                    while !done.load(Ordering::Relaxed) || reads < 100 {
                        assert_eq!(get_in(&path, "first"), Some(first), "read {reads}");
                        reads += 1;
                    }
                    reads
                })
            };
            for w in writers {
                w.join().unwrap();
            }
            done.store(true, Ordering::Relaxed);
            assert!(reader.join().unwrap() >= 100);
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text.lines().count(), 401);
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temporary file left");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review NET-13 (b): the first socket's packets go nowhere (as when the relay
    /// forwards them to a stale link); the handshake is started again from a new
    /// socket after 3 s and reaches the server, which here answers the
    /// registration with an error.
    #[tokio::test]
    async fn handshake_tried_again_from_a_new_socket() {
        use std::net::UdpSocket;
        use std::time::{Duration, Instant};

        // A signalling server that refuses every registration.
        let cert = rcgen::generate_simple_self_signed(vec!["nuvira-server".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            cert.signing_key.serialize_der(),
        ));
        let mut tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key)
            .unwrap();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap(),
        ));
        let server = quinn::Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            while let Some(incoming) = server.accept().await {
                tokio::spawn(async move {
                    let conn = incoming.await?;
                    let (send, recv) = conn.accept_bi().await?;
                    let (mut tx, mut rx) = (FramedSend::new(send), FramedRecv::new(recv));
                    rx.recv::<ClientMessage>().await?;
                    let error =
                        nuvira_proto::v1::Error { message: "test refusal".into(), ..Default::default() };
                    tx.send(&ServerMessage { msg: Some(server_message::Msg::Error(error)) }).await?;
                    conn.closed().await;
                    anyhow::Ok(())
                });
            }
        });

        // In front of it: drops everything from the first client address.
        let front = UdpSocket::bind("127.0.0.1:0").unwrap();
        let front_addr = front.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut first = None;
            let mut backs: Vec<(SocketAddr, Arc<UdpSocket>)> = Vec::new();
            let mut buf = [0u8; 2048];
            while let Ok((n, from)) = front.recv_from(&mut buf) {
                if *first.get_or_insert(from) == from {
                    continue;
                }
                let back = match backs.iter().find(|(c, _)| *c == from) {
                    Some((_, b)) => b.clone(),
                    None => {
                        let b = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
                        b.connect(server_addr).unwrap();
                        let (b2, front2) = (b.clone(), front.try_clone().unwrap());
                        std::thread::spawn(move || {
                            let mut buf = [0u8; 2048];
                            while let Ok(n) = b2.recv(&mut buf) {
                                let _ = front2.send_to(&buf[..n], from);
                            }
                        });
                        backs.push((from, b.clone()));
                        b
                    }
                };
                let _ = back.send(&buf[..n]);
            }
        });

        let dir = std::env::temp_dir().join(format!("nuvira-signal-retry-{}", std::process::id()));
        let identity = Identity::load_or_create(&dir.join("device.key")).unwrap();
        let device = DeviceInfo { name: "test", reachable: false, alias: None };
        let start = Instant::now();
        let err = connect(front_addr, &ServerTransport::Udp, None, &identity, &device)
            .await
            .err()
            .expect("refused")
            .to_string();
        let took = start.elapsed();
        // The refusal proves the retry: without it the first handshake would end in
        // a timeout error, never reaching the server.
        assert_eq!(err, "the server refuses the registration: test refusal");
        // The retry waited its 3 s first. The second handshake takes one round trip
        // on 127.0.0.1, but a heavily loaded machine has taken 5 s for it (8 s in
        // all, lot 19 on the NUC): generous upper bound, only against a hang.
        assert!(
            took >= UDP_HANDSHAKE_RETRY && took < UDP_HANDSHAKE_RETRY + Duration::from_secs(15),
            "{took:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn targets() {
        assert_eq!(target(" 123 456 789 "), "123456789");
        assert_eq!(target("123-456-789"), "123456789");
        assert_eq!(target(" Ufficio-Mario "), "ufficio-mario");
        assert_eq!(target("pc1"), "pc1");
        assert!(valid_target("123456789"));
        assert!(valid_target("ufficio-mario"));
        assert!(!valid_target("12345678"));
        assert!(!valid_target("-abc"));
        assert!(!valid_target("ab"));
        assert!(!valid_target("123-456"));
        assert!(!valid_target("caffè"));
    }

    #[test]
    fn public_server() {
        assert_eq!(server_or_public(""), PUBLIC_SERVER);
        assert_eq!(server_or_public("  "), PUBLIC_SERVER);
        assert_eq!(server_or_public("nas.example:21117"), "nas.example:21117");
        let key = known_server::get(PUBLIC_SERVER).expect("built-in fingerprint");
        assert_eq!(known_server::get("CONNECT.nuvirahub.dev:21116"), Some(key));
        assert_eq!(crate::fingerprint_hex(&key), PUBLIC_SERVER_KEY);
    }
}
