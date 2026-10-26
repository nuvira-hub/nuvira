//! Collegamento tra due dispositivi passando dal relay di nexo-server.
//!
//! Ciascun lato apre un socket UDP per la sessione, lo aggancia al relay con il
//! gettone ricevuto dal server (RelayBind) e poi ci fa girare QUIC. Il TLS è
//! end-to-end tra i dispositivi: ognuno presenta il certificato firmato con la
//! propria chiave e verifica che quello dell'altro usi la chiave comunicata dal server.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tracing::debug;

use crate::identity::{Identity, PublicKey};
use crate::{ALPN, provider, transport_config};

/// Ruolo nel relay (stessi valori di `nexo.v1.Role`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer = 1,
    Agent = 2,
}

/// Percorso di una sessione ricevuto dal server.
#[derive(Debug, Clone)]
pub struct Route {
    pub relay: SocketAddr,
    pub token: [u8; 16],
    pub peer_public_key: PublicKey,
    pub role: Role,
}

const RELAY_MAGIC: [u8; 4] = [0x00, b'N', b'X', b'R'];
const RELAY_BIND: u8 = 0x01;
const RELAY_BOUND: u8 = 0x02;

/// Aggancia `socket` al relay: invia il bind finché il relay non conferma.
fn bind_relay(socket: &UdpSocket, route: &Route) -> Result<()> {
    let mut bind = Vec::with_capacity(22);
    bind.extend_from_slice(&RELAY_MAGIC);
    bind.push(RELAY_BIND);
    bind.extend_from_slice(&route.token);
    bind.push(route.role as u8);

    socket.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut buf = [0u8; 64];
    for _ in 0..20 {
        socket.send_to(&bind, route.relay)?;
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from == route.relay && n == 22 && buf[..4] == RELAY_MAGIC => {
                    if buf[4] == RELAY_BOUND && buf[5..21] == route.token {
                        debug!(pronto = buf[21] == 1, "agganciato al relay");
                        socket.set_read_timeout(None)?;
                        return Ok(());
                    }
                }
                Ok(_) => {}
                Err(e)
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    bail!("il relay {} non risponde", route.relay)
}

async fn relay_socket(route: &Route) -> Result<UdpSocket> {
    let bind: SocketAddr = if route.relay.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let socket = UdpSocket::bind(bind)?;
    let route = route.clone();
    tokio::task::spawn_blocking(move || {
        bind_relay(&socket, &route)?;
        socket.set_nonblocking(true)?;
        Ok(socket)
    })
    .await?
}

/// Lato agent: endpoint QUIC agganciato al relay che accetta solo il viewer atteso.
pub async fn agent_endpoint(identity: &Identity, route: &Route) -> Result<Endpoint> {
    let socket = relay_socket(route).await?;
    let (certs, key) = identity.tls_cert();
    let verifier = PeerKey { expected: route.peer_public_key, provider: provider() };
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(certs, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(transport_config());
    Ok(Endpoint::new(EndpointConfig::default(), Some(config), socket, Arc::new(TokioRuntime))?)
}

/// Lato viewer: connessione QUIC all'agent attraverso il relay.
pub async fn viewer_connect(identity: &Identity, route: &Route) -> Result<Connection> {
    let socket = relay_socket(route).await?;
    let (certs, key) = identity.tls_cert();
    let verifier = PeerKey { expected: route.peer_public_key, provider: provider() };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(certs, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport_config());
    let mut endpoint = Endpoint::new(EndpointConfig::default(), None, socket, Arc::new(TokioRuntime))?;
    endpoint.set_default_client_config(config);
    endpoint
        .connect(route.relay, "nexo-device")?
        .await
        .context("collegamento al dispositivo attraverso il relay")
}

/// Il certificato usa questa chiave pubblica Ed25519?
pub fn cert_has_key(cert: &CertificateDer<'_>, key: &PublicKey) -> bool {
    // SubjectPublicKeyInfo DER di una chiave Ed25519 (RFC 8410): prefisso fisso + 32 byte.
    const ED25519_SPKI_PREFIX: [u8; 12] =
        [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    let Ok(parsed) = webpki::EndEntityCert::try_from(cert) else { return false };
    let spki = parsed.subject_public_key_info();
    spki.as_ref().len() == 44 && spki.as_ref()[..12] == ED25519_SPKI_PREFIX && spki.as_ref()[12..] == key[..]
}

/// Verifica che il certificato dell'altro dispositivo usi la chiave attesa.
/// La firma dell'handshake (verify_tls13_signature) dimostra poi che l'altro
/// possiede la chiave privata.
#[derive(Debug)]
struct PeerKey {
    expected: PublicKey,
    provider: Arc<CryptoProvider>,
}

impl PeerKey {
    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if cert_has_key(cert, &self.expected) {
            Ok(())
        } else {
            Err(rustls::Error::General("il dispositivo non ha la chiave indicata dal server".into()))
        }
    }

    fn tls13(
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
}

impl ServerCertVerifier for PeerKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ServerCertVerified::assertion())
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
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for PeerKey {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ClientCertVerified::assertion())
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
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
