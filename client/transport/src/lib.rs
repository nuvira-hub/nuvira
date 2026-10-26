//! Trasporto delle sessioni su QUIC.
//!
//! Fase 0: connessione diretta in LAN. L'agent presenta un certificato
//! autofirmato e il viewer può verificarne l'impronta SHA-256 (`--fingerprint`).
//! Questa NON è ancora l'autenticazione definitiva: nella fase 1 arrivano
//! l'handshake Noise con le chiavi dei dispositivi e il server di rendezvous.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use prost::Message;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, RecvStream, SendStream, ServerConfig, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tracing::warn;

pub mod identity;
pub mod p2p;
pub mod signal;
pub mod ws;

pub use quinn::{Connection, Endpoint};

/// Identificativo del protocollo negoziato in TLS (ALPN).
pub(crate) const ALPN: &[u8] = b"nexodesk/0";

/// Come si raggiunge nexodesk-server (e il suo relay).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerTransport {
    /// QUIC su UDP, il percorso normale.
    Udp,
    /// QUIC dentro un WebSocket (ws:// o wss://) per le reti che bloccano UDP.
    WebSocket(String),
}

/// Endpoint QUIC verso `server` con il trasporto indicato. Con UDP il socket è
/// già pronto (anche agganciato al relay); con WebSocket si apre il tunnel.
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

/// Dati non ancora confermati per connessione (vedi `transport_config`).
const SEND_WINDOW: u64 = 256 * 1024;

/// Porta UDP predefinita del prototipo in LAN.
pub const DEFAULT_PORT: u16 = 21500;

pub type Fingerprint = [u8; 32];

/// Indirizzo `host[:porta]` (anche IPv6 tra parentesi); senza porta si usa `default_port`.
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
    addrs.next().with_context(|| format!("indirizzo non valido: {host}"))
}

pub fn fingerprint_hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_fingerprint(hex: &str) -> Result<Fingerprint> {
    let hex: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 64 {
        bail!("l'impronta deve avere 64 cifre esadecimali");
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
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(Duration::from_secs(20).try_into().expect("durata valida")));
    // GSO (più datagrammi in un'unica chiamata di invio) su Windows perde i pacchetti
    // in loopback, e dopo la perdita la connessione non riceve più nulla: disattivato.
    // TODO: riprovare su LAN reale e riattivarlo dove funziona.
    t.enable_segmentation_offload(false);
    // Buffer di invio limitato: se la rete non smaltisce, l'invio si blocca presto e
    // l'agent salta frame invece di accodarne secondi (il bitrate adattivo fa il resto).
    t.send_window(SEND_WINDOW);
    // BBR stima la banda invece di dimezzare la finestra a ogni perdita: con perdite
    // casuali (Wi-Fi, reti mobili) NewReno/Cubic scendono ben sotto la capacità reale.
    if std::env::var_os("NEXODESK_CC_CUBIC").is_none() {
        t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    }
    Arc::new(t)
}

/// Endpoint dell'agent: accetta connessioni su `bind`. Restituisce anche l'impronta del certificato.
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
    let endpoint = Endpoint::server(config, bind).with_context(|| format!("ascolto su {bind}"))?;
    Ok((endpoint, fingerprint))
}

/// Connessione del viewer verso un agent. Senza impronta attesa accetta qualunque certificato.
pub async fn connect(addr: SocketAddr, expected: Option<Fingerprint>) -> Result<Connection> {
    if expected.is_none() {
        warn!("impronta dell'agent non indicata: connessione NON verificata (solo per prove in LAN)");
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
        endpoint.connect(addr, "nexodesk-agent")?.await.with_context(|| format!("connessione a {addr}"))?;
    Ok(conn)
}

/// Verifica il certificato dell'agent confrontandone l'impronta SHA-256.
#[derive(Debug)]
pub(crate) struct PinnedCert {
    pub(crate) expected: Option<Fingerprint>,
    pub(crate) provider: Arc<CryptoProvider>,
    /// Se presente, vi si registra l'impronta vista (per memorizzarla al primo collegamento).
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
            *seen.lock().expect("impronta") = Some(fingerprint);
        }
        match self.expected {
            Some(expected) if fingerprint != expected => {
                Err(rustls::Error::General("impronta del certificato diversa da quella attesa".into()))
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
        Err(rustls::Error::General("TLS 1.2 non supportato".into()))
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

/// Invio di messaggi Protobuf su uno stream QUIC.
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
}

/// Ricezione di messaggi Protobuf da uno stream QUIC.
pub struct FramedRecv {
    stream: RecvStream,
    decoder: nexodesk_proto::framing::Decoder,
    buf: Vec<u8>,
}

impl FramedRecv {
    pub fn new(stream: RecvStream) -> Self {
        Self { stream, decoder: Default::default(), buf: vec![0; 64 * 1024] }
    }

    /// Prossimo messaggio; `None` quando l'altro lato chiude lo stream.
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
        // Separatori ammessi, come nelle impronte copiate a mano.
        let spaced: String =
            hex.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(":");
        assert_eq!(parse_fingerprint(&spaced).unwrap(), fp);
        assert!(parse_fingerprint("abc").is_err());
    }
}
