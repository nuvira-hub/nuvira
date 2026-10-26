//! Collegamento tra due dispositivi passando dal relay di nexodesk-server.
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

/// Ruolo nel relay (stessi valori di `nexodesk.v1.Role`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer = 1,
    Agent = 2,
}

/// Percorso di una sessione ricevuto dal server.
#[derive(Debug, Clone)]
pub struct Route {
    pub relay: SocketAddr,
    /// Come si raggiunge il relay (lo stesso trasporto della segnalazione).
    pub transport: crate::ServerTransport,
    pub token: [u8; 16],
    pub peer_public_key: PublicKey,
    pub role: Role,
}

const RELAY_MAGIC: [u8; 4] = [0x00, b'N', b'X', b'R'];
const RELAY_BIND: u8 = 0x01;
const RELAY_BOUND: u8 = 0x02;

/// Aggancia `socket` al relay e attende che anche l'altro lato sia collegato.
/// Restituisce l'indirizzo dell'altro lato come lo vede il relay (per la via diretta).
fn bind_message(route: &Route, locals: &[SocketAddr]) -> Vec<u8> {
    let mut bind = Vec::with_capacity(64);
    bind.extend_from_slice(&RELAY_MAGIC);
    bind.push(RELAY_BIND);
    bind.extend_from_slice(&route.token);
    bind.push(route.role as u8);
    bind.push(locals.len() as u8);
    for addr in locals {
        push_addr(&mut bind, *addr);
    }
    bind
}

/// Risposta del relay al bind per questo gettone: `Some(true)` se anche l'altro lato è agganciato.
fn bound_state(data: &[u8], token: &[u8; 16]) -> Option<bool> {
    (data.len() >= 22 && data[..5] == [0, b'N', b'X', b'R', RELAY_BOUND] && data[5..21] == token[..])
        .then(|| data[21] == 1)
}

/// Aggancio al relay attraverso il tunnel WebSocket (niente via diretta).
async fn bind_relay_ws(socket: &crate::ws::WsSocket, route: &Route) -> Result<()> {
    let bind = bind_message(route, &[]);
    for _ in 0..60 {
        socket.send_raw(&bind)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        while let Some(data) =
            socket.recv_raw(deadline.saturating_duration_since(tokio::time::Instant::now())).await?
        {
            if bound_state(&data, &route.token) == Some(true) {
                return Ok(());
            }
        }
    }
    bail!("l'altro dispositivo non si è agganciato al relay")
}

fn bind_relay(socket: &UdpSocket, route: &Route, locals: &[SocketAddr]) -> Result<Candidates> {
    let bind = bind_message(route, locals);

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
                Ok((n, from)) if from == route.relay => match bound_state(&buf[..n], &route.token) {
                    Some(true) => {
                        socket.set_read_timeout(None)?;
                        return Ok(parse_candidates(&buf[22..n]));
                    }
                    Some(false) if !acked => {
                        debug!("agganciato al relay, in attesa dell'altro lato");
                        acked = true;
                    }
                    _ => {}
                },
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

/// Indirizzi da cui provare a raggiungere direttamente l'altro lato.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Candidates {
    /// Come lo vede il relay, attraverso gli eventuali NAT.
    reflexive: Option<SocketAddr>,
    /// Indirizzi nella sua rete locale (utili se si è dietro lo stesso router).
    locals: Vec<SocketAddr>,
}

impl Candidates {
    /// Tutti gli indirizzi, prima i locali (in LAN sono i più veloci), senza doppioni.
    fn all(&self) -> Vec<SocketAddr> {
        let mut all = self.locals.clone();
        all.extend(self.reflexive);
        all.dedup();
        let mut seen = Vec::new();
        all.retain(|a| {
            let new = !seen.contains(a);
            seen.push(*a);
            new
        });
        all
    }
}

/// Indirizzo nel formato del relay: famiglia (4/6) | IP | porta big endian.
/// Restituisce l'indirizzo e i byte che seguono.
fn parse_addr(b: &[u8]) -> Option<(SocketAddr, &[u8])> {
    let (ip, rest): (std::net::IpAddr, &[u8]) = match b.first()? {
        4 if b.len() >= 7 => (<[u8; 4]>::try_from(&b[1..5]).ok()?.into(), &b[5..]),
        6 if b.len() >= 19 => (<[u8; 16]>::try_from(&b[1..17]).ok()?.into(), &b[17..]),
        _ => return None,
    };
    Some((SocketAddr::new(ip, u16::from_be_bytes([rest[0], rest[1]])), &rest[2..]))
}

fn push_addr(out: &mut Vec<u8>, addr: SocketAddr) {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        std::net::IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

/// Risposta "pronto" del relay: indirizzo visto dal relay, poi n indirizzi locali.
fn parse_candidates(b: &[u8]) -> Candidates {
    let Some((reflexive, mut rest)) = parse_addr(b) else { return Candidates::default() };
    let mut locals = Vec::new();
    if let Some((&n, tail)) = rest.split_first() {
        rest = tail;
        for _ in 0..n {
            let Some((addr, tail)) = parse_addr(rest) else { break };
            locals.push(addr);
            rest = tail;
        }
    }
    Candidates { reflexive: Some(reflexive), locals }
}

/// Indirizzo locale con cui questo PC raggiunge il relay (l'interfaccia di rete
/// principale), con la porta del socket della sessione.
fn local_candidates(relay: SocketAddr, port: u16) -> Vec<SocketAddr> {
    let probe_bind: SocketAddr =
        if relay.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse().expect("indirizzo");
    // connect su UDP non invia nulla: chiede solo al sistema quale interfaccia userebbe.
    let ip = UdpSocket::bind(probe_bind).and_then(|s| s.connect(relay).and_then(|()| s.local_addr()));
    match ip {
        Ok(addr) if !addr.ip().is_unspecified() => vec![SocketAddr::new(addr.ip(), port)],
        _ => Vec::new(),
    }
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
/// Endpoint QUIC della sessione, agganciato al relay, e indirizzi diretti dell'altro lato.
async fn session_endpoint(
    route: &Route,
    server_config: Option<ServerConfig>,
) -> Result<(Endpoint, Vec<SocketAddr>)> {
    match &route.transport {
        crate::ServerTransport::Udp => {
            let (socket, peers) = session_socket(route).await?;
            Ok((
                crate::endpoint_for(&route.transport, route.relay, Some(socket), server_config).await?,
                peers,
            ))
        }
        crate::ServerTransport::WebSocket(url) => {
            let socket = crate::ws::WsSocket::connect(url, route.relay).await?;
            bind_relay_ws(&socket, route).await?;
            let endpoint = Endpoint::new_with_abstract_socket(
                EndpointConfig::default(),
                server_config,
                socket,
                Arc::new(TokioRuntime),
            )?;
            Ok((endpoint, Vec::new()))
        }
    }
}

async fn session_socket(route: &Route) -> Result<(UdpSocket, Vec<SocketAddr>)> {
    let bind: SocketAddr = if route.relay.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let socket = UdpSocket::bind(bind)?;
    let route = route.clone();
    tokio::task::spawn_blocking(move || {
        let locals = local_candidates(route.relay, socket.local_addr()?.port());
        let candidates = bind_relay(&socket, &route, &locals)?;
        debug!(?candidates, "indirizzi dell'altro lato");
        let peers: Vec<SocketAddr> = candidates.all().into_iter().filter(|a| *a != route.relay).collect();
        for peer in &peers {
            punch(&socket, *peer, &route.token);
        }
        socket.set_nonblocking(true)?;
        Ok((socket, peers))
    })
    .await?
}

/// Lato agent: endpoint QUIC che accetta solo il viewer atteso, sia attraverso il
/// relay sia in diretta.
pub async fn agent_endpoint(identity: &Identity, route: &Route) -> Result<Endpoint> {
    let (certs, key) = identity.tls_cert();
    let verifier = PeerKey { expected: route.peer_public_key, provider: provider() };
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(certs, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(transport_config());
    Ok(session_endpoint(route, Some(config)).await?.0)
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
    let (mut endpoint, peers) = session_endpoint(route, None).await?;
    endpoint.set_default_client_config(config);

    let mut relay = Box::pin(endpoint.connect(route.relay, "nexodesk-device")?);
    // NEXODESK_FORCE_RELAY=1: solo relay, per misurarne il costo nei test.
    let force_relay = std::env::var_os("NEXODESK_FORCE_RELAY").is_some();
    // Tentativi diretti verso tutti gli indirizzi dell'altro lato, in parallelo.
    let mut direct = tokio::task::JoinSet::new();
    if !force_relay {
        for peer in peers {
            let connecting = endpoint.connect(peer, "nexodesk-device")?;
            direct.spawn(async move { connecting.await.map_err(|e| (peer, e)) });
        }
    }

    // Vince la prima connessione diretta; il relay serve solo se nessuna diretta
    // riesce entro DIRECT_GRACE dal momento in cui il relay è pronto.
    let mut relayed: Option<Result<Connection>> = None;
    let mut deadline = None;
    let chosen = loop {
        tokio::select! {
            Some(r) = direct.join_next(), if !direct.is_empty() => match r {
                Ok(Ok(conn)) => break (conn, Path::Direct),
                Ok(Err((peer, e))) => debug!(%peer, "via diretta non riuscita: {e}"),
                Err(e) => debug!("tentativo diretto interrotto: {e}"),
            },
            r = &mut relay, if relayed.is_none() => {
                relayed = Some(r.context("collegamento attraverso il relay"));
                deadline = Some(tokio::time::Instant::now() + DIRECT_GRACE);
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                break (relayed.take().expect("relay pronto")?, Path::Relay);
            }
        }
        // Tutti i tentativi diretti falliti e relay pronto: inutile aspettare.
        if direct.is_empty() && relayed.is_some() {
            break (relayed.take().expect("relay pronto")?, Path::Relay);
        }
    };
    let (conn, path) = chosen;
    // Gli altri tentativi si chiudono: l'agent usa la connessione su cui si apre la sessione.
    direct.abort_all();
    match relayed {
        Some(Ok(c)) if path == Path::Direct => c.close(0u32.into(), b"percorso diretto"),
        Some(_) => {}
        None if path == Path::Direct => {
            tokio::spawn(async move {
                if let Ok(c) = relay.await {
                    c.close(0u32.into(), b"percorso diretto");
                }
            });
        }
        None => {}
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
        let a: SocketAddr = "10.0.0.1:1000".parse().unwrap();
        let b: SocketAddr = "[::1]:80".parse().unwrap();
        let mut wire = Vec::new();
        push_addr(&mut wire, a);
        wire.push(2);
        push_addr(&mut wire, b);
        push_addr(&mut wire, a);
        let c = parse_candidates(&wire);
        assert_eq!(c.reflexive, Some(a));
        assert_eq!(c.locals, vec![b, a]);
        // Locali prima, senza doppioni.
        assert_eq!(c.all(), vec![b, a]);
        assert_eq!(parse_addr(&[4, 1, 2]), None);
        assert_eq!(parse_candidates(&[]), Candidates::default());
    }

    #[test]
    fn finds_local_address() {
        let locals = local_candidates("127.0.0.1:9".parse().unwrap(), 1234);
        assert_eq!(locals, vec!["127.0.0.1:1234".parse().unwrap()]);
    }
}
