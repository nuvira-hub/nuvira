//! Trasporto delle sessioni su QUIC.
//!
//! Fase 0: connessione diretta in LAN. L'host presenta un certificato
//! autofirmato e il viewer può verificarne l'impronta SHA-256 (`--fingerprint`).
//! Questa NON è ancora l'autenticazione definitiva: nella fase 1 arrivano
//! l'handshake Noise con le chiavi dei dispositivi e il server di rendezvous.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use prost::Message;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tracing::warn;

pub use quinn::Connection;

/// Identificativo del protocollo negoziato in TLS (ALPN).
const ALPN: &[u8] = b"nexo/0";

/// Porta UDP predefinita del prototipo in LAN.
pub const DEFAULT_PORT: u16 = 21500;

pub type Fingerprint = [u8; 32];

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

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

fn transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(Duration::from_secs(20).try_into().expect("durata valida")));
    Arc::new(t)
}

/// Endpoint dell'host: accetta connessioni su `bind`. Restituisce anche l'impronta del certificato.
pub fn server_endpoint(bind: SocketAddr) -> Result<(Endpoint, Fingerprint)> {
    let cert = rcgen::generate_simple_self_signed(vec!["nexo-host".to_string()])?;
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

/// Connessione del viewer verso un host. Senza impronta attesa accetta qualunque certificato.
pub async fn connect(addr: SocketAddr, expected: Option<Fingerprint>) -> Result<Connection> {
    if expected.is_none() {
        warn!("impronta dell'host non indicata: connessione NON verificata (solo per prove in LAN)");
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { expected, provider: provider() }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport_config());

    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(config);
    let conn = endpoint.connect(addr, "nexo-host")?.await.with_context(|| format!("connessione a {addr}"))?;
    Ok(conn)
}

/// Verifica il certificato dell'host confrontandone l'impronta SHA-256.
#[derive(Debug)]
struct PinnedCert {
    expected: Option<Fingerprint>,
    provider: Arc<CryptoProvider>,
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
        match self.expected {
            Some(expected) if <[u8; 32]>::from(Sha256::digest(end_entity)) != expected => {
                Err(rustls::Error::General("impronta dell'host diversa da quella attesa".into()))
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
        self.stream.write_all(&nexo_proto::framing::encode(msg)).await?;
        Ok(())
    }
}

/// Ricezione di messaggi Protobuf da uno stream QUIC.
pub struct FramedRecv {
    stream: RecvStream,
    decoder: nexo_proto::framing::Decoder,
    buf: Vec<u8>,
}

impl FramedRecv {
    pub fn new(stream: RecvStream) -> Self {
        Self { stream, decoder: Default::default(), buf: vec![0; 64 * 1024] }
    }

    /// Prossimo messaggio; `None` quando l'altro lato chiude lo stream.
    pub async fn recv<M: Message + Default>(&mut self) -> Result<Option<M>> {
        loop {
            if let Some(msg) = self.decoder.next()? {
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
    fn fingerprint_hex_round_trip() {
        let fp: Fingerprint = core::array::from_fn(|i| i as u8 * 7);
        let hex = fingerprint_hex(&fp);
        assert_eq!(parse_fingerprint(&hex).unwrap(), fp);
        // Separatori ammessi, come nelle impronte copiate a mano.
        let spaced: String = hex.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(":");
        assert_eq!(parse_fingerprint(&spaced).unwrap(), fp);
        assert!(parse_fingerprint("abc").is_err());
    }
}
