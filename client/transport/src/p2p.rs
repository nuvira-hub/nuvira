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

/// Aggancia `socket` al relay e attende che anche l'altro lato sia collegato.
/// Restituisce l'indirizzo dell'altro lato come lo vede il relay (per la via diretta).
fn bind_relay(socket: &UdpSocket, route: &Route) -> Result<Option<SocketAddr>> {
    let mut bind = Vec::with_capacity(22);
    bind.extend_from_slice(&RELAY_MAGIC);
    bind.push(RELAY_BIND);
    bind.extend_from_slice(&route.token);
    bind.push(route.role as u8);

    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    // Grande quanto un datagramma QUIC: se l'altro lato è già partito, i suoi pacchetti
    // arrivano qui prima della fine dell'aggancio (e su Windows un buffer piccolo
    // non tronca, ma fa fallire recv_from con WSAEMSGSIZE).
    let mut buf = [0u8; 2048];
    let mut acked = false;
    // ~15 s: l'altro lato può arrivare qualche secondo dopo.
    for _ in 0..60 {
        socket.send_to(&bind, route.relay)?;
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from))
                    if from == route.relay && n >= 22 && buf[..5] == [0, b'N', b'X', b'R', RELAY_BOUND] =>
                {
                    if buf[5..21] != route.token {
                        continue;
                    }
                    if !acked {
                        debug!("agganciato al relay, in attesa dell'altro lato");
                        acked = true;
                    }
                    if buf[21] == 1 {
                        socket.set_read_timeout(None)?;
                        return Ok(parse_addr(&buf[22..n]));
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
    if acked {
        bail!("l'altro dispositivo non si è agganciato al relay")
    } else {
        bail!("il relay {} non risponde", route.relay)
    }
}

/// Indirizzo nel formato del relay: famiglia (4/6) | IP | porta big endian.
fn parse_addr(b: &[u8]) -> Option<SocketAddr> {
    let (ip, rest): (std::net::IpAddr, &[u8]) = match b.first()? {
        4 if b.len() >= 7 => (<[u8; 4]>::try_from(&b[1..5]).ok()?.into(), &b[5..]),
        6 if b.len() >= 19 => (<[u8; 16]>::try_from(&b[1..17]).ok()?.into(), &b[17..]),
        _ => return None,
    };
    Some(SocketAddr::new(ip, u16::from_be_bytes([rest[0], rest[1]])))
}

/// Pacchetti di apertura verso l'altro lato: aprono la mappatura del NAT e lo stato
/// del firewall locale, così i pacchetti QUIC diretti dell'altro lato possono entrare.
/// QUIC li scarta (primo byte senza il bit 0x40).
fn punch(socket: &UdpSocket, peer: SocketAddr, token: &[u8; 16]) {
    let mut packet = vec![0x00, b'N', b'X', b'P'];
    packet.extend_from_slice(token);
    for _ in 0..3 {
        let _ = socket.send_to(&packet, peer);
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Socket della sessione: agganciato al relay e con la via diretta già aperta verso l'altro lato.
async fn session_socket(route: &Route) -> Result<(UdpSocket, Option<SocketAddr>)> {
    let bind: SocketAddr = if route.relay.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let socket = UdpSocket::bind(bind)?;
    let route = route.clone();
    tokio::task::spawn_blocking(move || {
        let peer = bind_relay(&socket, &route)?;
        if let Some(peer) = peer {
            debug!(%peer, "indirizzo dell'altro lato");
            punch(&socket, peer, &route.token);
        }
        socket.set_nonblocking(true)?;
        Ok((socket, peer))
    })
    .await?
}

/// Lato agent: endpoint QUIC che accetta solo il viewer atteso, sia attraverso il
/// relay sia in diretta.
pub async fn agent_endpoint(identity: &Identity, route: &Route) -> Result<Endpoint> {
    let (socket, _peer) = session_socket(route).await?;
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

/// Come è stata raggiunta l'altra parte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Direct,
    Relay,
}

/// Quanto attendere la via diretta dopo che il relay è già pronto.
const DIRECT_GRACE: Duration = Duration::from_millis(1500);

/// Lato viewer: prova insieme la via diretta e il relay e preferisce la diretta.
pub async fn viewer_connect(identity: &Identity, route: &Route) -> Result<(Connection, Path)> {
    let (socket, peer) = session_socket(route).await?;
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

    let relay = endpoint.connect(route.relay, "nexo-device")?;
    // NEXO_FORCE_RELAY=1: solo relay, per misurarne il costo nei test.
    let force_relay = std::env::var_os("NEXO_FORCE_RELAY").is_some();
    let Some(peer) = peer.filter(|p| *p != route.relay && !force_relay) else {
        let conn = relay.await.context("collegamento attraverso il relay")?;
        return Ok((conn, Path::Relay));
    };
    let direct = endpoint.connect(peer, "nexo-device")?;
    let (mut relay, mut direct) = (Box::pin(relay), Box::pin(direct));

    // Si attende la prima delle due; se è il relay si concede ancora un po' di tempo alla diretta.
    let (conn, path, loser) = tokio::select! {
        r = &mut direct => match r {
            Ok(conn) => (conn, Path::Direct, Some(relay)),
            Err(e) => {
                debug!("via diretta non riuscita: {e}");
                (relay.await.context("collegamento attraverso il relay")?, Path::Relay, None)
            }
        },
        r = &mut relay => {
            let relayed = r.context("collegamento attraverso il relay");
            match tokio::time::timeout(DIRECT_GRACE, &mut direct).await {
                Ok(Ok(conn)) => {
                    if let Ok(c) = relayed {
                        c.close(0u32.into(), b"percorso diretto");
                    }
                    (conn, Path::Direct, None)
                }
                _ => (relayed?, Path::Relay, None),
            }
        }
    };
    // La connessione perdente, se arriva, viene chiusa: l'agent usa quella su cui si apre la sessione.
    if let Some(loser) = loser {
        tokio::spawn(async move {
            if let Ok(c) = loser.await {
                c.close(0u32.into(), b"percorso diretto");
            }
        });
    }
    debug!(?path, remote = %conn.remote_address(), "collegato");
    Ok((conn, path))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relay_addresses() {
        assert_eq!(parse_addr(&[4, 10, 0, 0, 1, 0x03, 0xe8]), Some("10.0.0.1:1000".parse().unwrap()));
        let mut v6 = vec![6];
        v6.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        v6.extend_from_slice(&[0, 80]);
        assert_eq!(parse_addr(&v6), Some("[::1]:80".parse().unwrap()));
        assert_eq!(parse_addr(&[4, 1, 2]), None);
        assert_eq!(parse_addr(&[]), None);
    }
}
