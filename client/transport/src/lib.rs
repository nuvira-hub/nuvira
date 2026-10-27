//! Session transport over QUIC.
//!
//! Every device has an Ed25519 key (see [`identity`]) that signs its TLS
//! certificate; the other side checks the key it was told about by nexodesk-server
//! ([`p2p`]). The LAN prototype mode ([`connect`] without the server) can check
//! the agent's SHA-256 certificate fingerprint instead (`--fingerprint`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nexodesk_proto::v1::FileChunk;
use prost::Message;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, RecvStream, SendStream, ServerConfig, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tracing::warn;

pub mod api;
pub mod identity;
pub mod p2p;
pub mod signal;
pub mod ws;

pub use quinn::{Connection, Endpoint};

/// Protocol identifier negotiated in TLS (ALPN).
pub(crate) const ALPN: &[u8] = b"nexodesk/0";

/// How nexodesk-server (and its relay) is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerTransport {
    /// QUIC over UDP, the normal path.
    Udp,
    /// QUIC inside a WebSocket (ws:// or wss://) for networks that block UDP.
    WebSocket(String),
}

/// QUIC endpoint towards `server` with the given transport. With UDP the socket is
/// ready (also bound to the relay); with WebSocket the tunnel is opened.
pub(crate) async fn endpoint_for(
    transport: &ServerTransport,
    server: SocketAddr,
    udp: Option<std::net::UdpSocket>,
    server_config: Option<ServerConfig>,
) -> Result<quinn::Endpoint> {
    let runtime = Arc::new(quinn::TokioRuntime);
    match transport {
        ServerTransport::Udp => {
            let socket = match udp {
                Some(s) => s,
                None => std::net::UdpSocket::bind(if server.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" })?,
            };
            Ok(quinn::Endpoint::new(quinn::EndpointConfig::default(), server_config, socket, runtime)?)
        }
        ServerTransport::WebSocket(url) => {
            let socket = ws::WsSocket::connect(url, server).await?;
            Ok(quinn::Endpoint::new_with_abstract_socket(
                quinn::EndpointConfig::default(),
                server_config,
                socket,
                runtime,
            )?)
        }
    }
}

/// Unacknowledged data per connection (see `transport_config`).
const SEND_WINDOW: u64 = 256 * 1024;

/// Default UDP port of the LAN prototype.
pub const DEFAULT_PORT: u16 = 21500;

pub type Fingerprint = [u8; 32];

/// `host[:port]` address (IPv6 also in brackets); without a port `default_port` is used.
pub fn resolve_addr(host: &str, default_port: u16) -> Result<SocketAddr> {
    use std::net::{IpAddr, ToSocketAddrs};
    if let Ok(addr) = host.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((ip, default_port).into());
    }
    let mut addrs =
        if host.contains(':') { host.to_socket_addrs()? } else { (host, default_port).to_socket_addrs()? };
    addrs.next().with_context(|| format!("invalid address: {host}"))
}

pub fn fingerprint_hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_fingerprint(hex: &str) -> Result<Fingerprint> {
    let hex: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 64 {
        bail!("the fingerprint must have 64 hexadecimal digits");
    }
    let mut fp = [0u8; 32];
    for (i, b) in fp.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)?;
    }
    Ok(fp)
}

pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

pub(crate) fn transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    // If the other side disappears without closing (viewer killed, network down) the
    // agent stays "busy" until the timeout: 10 s, with a packet every 2 s so that a
    // live connection is never silent for that long.
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("valid duration")));
    // GSO (several datagrams in a single send call) loses packets on loopback on
    // Windows, and after the loss the connection receives nothing more: disabled.
    // TODO: try again on a real LAN and enable it where it works.
    t.enable_segmentation_offload(false);
    // Limited send buffer: if the network does not keep up, sending blocks early and
    // the agent skips frames instead of queueing seconds of them (adaptive bitrate does the rest).
    t.send_window(SEND_WINDOW);
    // BBR estimates bandwidth instead of halving the window at every loss: with random
    // losses (Wi-Fi, mobile networks) NewReno/Cubic drop well below the real capacity.
    if std::env::var_os("NEXODESK_CC_CUBIC").is_none() {
        t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    }
    Arc::new(t)
}

/// Agent endpoint: accepts connections on `bind`. Also returns the certificate fingerprint.
pub fn server_endpoint(bind: SocketAddr) -> Result<(Endpoint, Fingerprint)> {
    let cert = rcgen::generate_simple_self_signed(vec!["nexodesk-agent".to_string()])?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let fingerprint: Fingerprint = Sha256::digest(&cert_der).into();

    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(transport_config());
    let endpoint = Endpoint::server(config, bind).with_context(|| format!("listening on {bind}"))?;
    Ok((endpoint, fingerprint))
}

/// Viewer connection to an agent. Without an expected fingerprint any certificate is accepted.
pub async fn connect(addr: SocketAddr, expected: Option<Fingerprint>) -> Result<Connection> {
    if expected.is_none() {
        warn!("agent fingerprint not given: connection NOT verified (LAN tests only)");
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { expected, provider: provider(), seen: None }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport_config());

    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(config);
    let conn =
        endpoint.connect(addr, "nexodesk-agent")?.await.with_context(|| format!("connecting to {addr}"))?;
    Ok(conn)
}

/// Checks the agent's certificate by comparing its SHA-256 fingerprint.
#[derive(Debug)]
pub(crate) struct PinnedCert {
    pub(crate) expected: Option<Fingerprint>,
    pub(crate) provider: Arc<CryptoProvider>,
    /// If present, the fingerprint seen is stored here (to remember it at the first connection).
    pub(crate) seen: Option<Arc<std::sync::Mutex<Option<Fingerprint>>>>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint: Fingerprint = Sha256::digest(end_entity).into();
        if let Some(seen) = &self.seen {
            *seen.lock().expect("fingerprint") = Some(fingerprint);
        }
        match self.expected {
            Some(expected) if fingerprint != expected => {
                Err(rustls::Error::General("certificate fingerprint differs from the expected one".into()))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Sending Protobuf messages on a QUIC stream.
pub struct FramedSend {
    stream: SendStream,
}

impl FramedSend {
    pub fn new(stream: SendStream) -> Self {
        Self { stream }
    }

    pub async fn send<M: Message>(&mut self, msg: &M) -> Result<()> {
        self.stream.write_all(&nexodesk_proto::framing::encode(msg)).await?;
        Ok(())
    }

    /// Closes the stream: the other side receives the end after the last message.
    pub fn finish(mut self) -> Result<()> {
        self.stream.finish()?;
        Ok(())
    }
}

/// Receiving Protobuf messages from a QUIC stream.
pub struct FramedRecv {
    stream: RecvStream,
    decoder: nexodesk_proto::framing::Decoder,
    buf: Vec<u8>,
}

impl FramedRecv {
    pub fn new(stream: RecvStream) -> Self {
        Self { stream, decoder: Default::default(), buf: vec![0; 64 * 1024] }
    }

    /// Next message; `None` when the other side closes the stream.
    pub async fn recv<M: Message + Default>(&mut self) -> Result<Option<M>> {
        loop {
            if let Some(msg) = self.decoder.next_message()? {
                return Ok(Some(msg));
            }
            match self.stream.read(&mut self.buf).await? {
                Some(n) => self.decoder.push(&self.buf[..n]),
                None => return Ok(None),
            }
        }
    }
}

/// Sends `data` on a new unidirectional stream: `header` (name, size, kind) then
/// the data in 256 KB chunks, as for files (`FileChunk`).
pub async fn send_stream(conn: &quinn::Connection, header: FileChunk, data: &[u8]) -> Result<()> {
    let mut tx = FramedSend::new(conn.open_uni().await?);
    tx.send(&FileChunk { size: data.len() as u64, data: Vec::new(), ..header }).await?;
    for chunk in data.chunks(256 * 1024) {
        tx.send(&FileChunk { data: chunk.to_vec(), ..Default::default() }).await?;
    }
    tx.finish()
}

/// Reads the rest of a stream whose `header` was already read: at most `limit`
/// bytes, and exactly the announced size.
pub async fn read_stream(rx: &mut FramedRecv, header: &FileChunk, limit: u64) -> Result<Vec<u8>> {
    if header.size > limit {
        bail!("{} bytes, more than the {limit} accepted", header.size);
    }
    let mut data = header.data.clone();
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        data.extend_from_slice(&chunk.data);
        if data.len() as u64 > header.size {
            bail!("more data than announced");
        }
    }
    if data.len() as u64 != header.size {
        bail!("incomplete: received {} bytes out of {}", data.len(), header.size);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_adds_default_port() {
        assert_eq!(resolve_addr("127.0.0.1", 9).unwrap().port(), 9);
        assert_eq!(resolve_addr("127.0.0.1:9000", 9).unwrap().port(), 9000);
        assert_eq!(resolve_addr("[::1]:9000", 9).unwrap().port(), 9000);
        assert_eq!(resolve_addr("::1", 9).unwrap().port(), 9);
    }

    #[test]
    fn fingerprint_hex_round_trip() {
        let fp: Fingerprint = core::array::from_fn(|i| i as u8 * 7);
        let hex = fingerprint_hex(&fp);
        assert_eq!(parse_fingerprint(&hex).unwrap(), fp);
        // Separators are allowed, as in fingerprints copied by hand.
        let spaced: String =
            hex.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(":");
        assert_eq!(parse_fingerprint(&spaced).unwrap(), fp);
        assert!(parse_fingerprint("abc").is_err());
    }
}
