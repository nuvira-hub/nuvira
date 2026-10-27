//! Signalling connection with nexodesk-server: device registration (proof of
//! the key), then exchange of requests and routes.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use nexodesk_proto::v1::{ClientMessage, Proof, Register, ServerMessage, client_message, server_message};
use quinn::ClientConfig;
use quinn::crypto::rustls::QuicClientConfig;

use crate::identity::Identity;
use crate::{Fingerprint, FramedRecv, FramedSend, PinnedCert, ServerTransport, provider, transport_config};

/// Signalling ALPN (must match the server's).
const ALPN: &[u8] = b"nexodesk-signal/1";
/// Signalling protocol version.
pub const PROTOCOL_VERSION: u32 = 1;
/// Prefix signed together with the nonce (as in the server).
const REGISTER_CONTEXT: &[u8] = b"nexodesk-register-v1";
/// Default UDP port of nexodesk-server.
pub const DEFAULT_PORT: u16 = 21116;

pub struct Signal {
    pub device_id: String,
    /// Rules of the server for its devices (all off from servers without them).
    pub policy: nexodesk_proto::v1::Policy,
    /// Server address: it is also the relay address.
    pub server: SocketAddr,
    /// Fingerprint of the server certificate seen in this connection.
    pub server_fingerprint: Fingerprint,
    /// Transport the server was reached with: sessions will use the same one.
    pub transport: ServerTransport,
    pub tx: SignalSender,
    pub rx: SignalReceiver,
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
                        "nexodesk-server not reachable over UDP ({why}): trying the WebSocket {ws_url}"
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

/// How the device introduces itself to the server.
pub struct DeviceInfo<'a> {
    pub name: &'a str,
    /// Not reachable: it receives no connection requests (viewer, app using the address book).
    pub reachable: bool,
}

/// Connects to nexodesk-server and registers the device. With `expected` the server
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
    config.transport_config(transport_config());

    let mut endpoint = crate::endpoint_for(transport, server, None, None).await?;
    endpoint.set_default_client_config(config);
    let conn = endpoint
        .connect(server, "nexodesk-server")?
        .await
        .with_context(|| format!("connecting to nexodesk-server {server}"))?;
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
    }))
    .await?;

    let nonce = match rx.recv().await? {
        Some(server_message::Msg::Challenge(c)) => c.nonce,
        Some(server_message::Msg::Error(e)) => bail!("the server refuses the registration: {}", e.message),
        other => bail!("unexpected answer to the registration: {other:?}"),
    };
    let signature = identity.sign(&[REGISTER_CONTEXT, &nonce].concat())?;
    tx.send(client_message::Msg::Proof(Proof { signature })).await?;
    let (device_id, policy) = match rx.recv().await? {
        Some(server_message::Msg::Registered(r)) => (r.device_id, r.policy.unwrap_or_default()),
        Some(server_message::Msg::Error(e)) => bail!("the server refuses the registration: {}", e.message),
        other => bail!("unexpected answer to the registration: {other:?}"),
    };
    Ok(Signal { device_id, policy, server, server_fingerprint, transport: transport.clone(), tx, rx })
}

/// Fingerprints of the servers already seen (`servers.txt` in the data folder): at
/// the first connection the fingerprint is remembered, afterwards it must not change.
pub mod known_server {
    use super::*;

    fn path() -> std::path::PathBuf {
        crate::identity::data_dir().join("servers.txt")
    }

    pub fn get(server: &str) -> Option<Fingerprint> {
        let text = std::fs::read_to_string(path()).ok()?;
        text.lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(s, _)| *s == server)
            .and_then(|(_, fp)| crate::parse_fingerprint(fp).ok())
    }

    pub fn remember(server: &str, fp: &Fingerprint) -> Result<()> {
        let path = path();
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&format!("{server} {}\n", crate::fingerprint_hex(fp)));
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text)?;
        Ok(())
    }
}
