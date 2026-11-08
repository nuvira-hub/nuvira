//! Connection between two devices, directly or through nuvira-server's relay.
//!
//! Each side opens a UDP socket for the session, binds it to the relay with the
//! token received from the server (RelayBind) and then runs QUIC on it. TLS is
//! end-to-end between the devices: each presents the certificate signed with its
//! own key and checks that the other's uses the key announced by the server.
//!
//! IPv6: the session socket is dual-stack where the system allows it (`[::]:0`
//! with `IPV6_V6ONLY` off), so the same socket reaches IPv4 addresses too (as
//! IPv4-mapped ones). Each side announces a local address of both families and,
//! when the server's name has an address of the other family as well, announces
//! itself to the relay on that one too: the other side then learns this side's
//! public address of both families and tries them all. IPv6 has no NAT, so the
//! direct path succeeds there much more often; the relay stays the fallback.

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

/// Role in the relay (same values as `nuvira.v1.Role`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer = 1,
    Agent = 2,
}

/// Route of a session received from the server.
#[derive(Debug, Clone)]
pub struct Route {
    pub relay: SocketAddr,
    /// How the relay is reached (the same transport as signalling).
    pub transport: crate::ServerTransport,
    pub token: [u8; 16],
    pub peer_public_key: PublicKey,
    pub role: Role,
    /// The relay's address of the other IP family, when the server's name has one
    /// ([`crate::other_family`]): this side announces itself there too, so the
    /// other side learns its public address of that family.
    pub relay_alt: Option<SocketAddr>,
}

/// The same address in its plain form: IPv4-mapped IPv6 (`::ffff:a.b.c.d`, as a
/// dual-stack socket reports IPv4 peers) becomes IPv4.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(a) => match a.ip().to_ipv4_mapped() {
            Some(ip) => SocketAddr::new(ip.into(), a.port()),
            None => addr,
        },
        v4 => v4,
    }
}

/// `addr` as a socket of this family takes it: an IPv4 address becomes
/// IPv4-mapped on an IPv6 (dual-stack) socket.
fn for_socket(addr: SocketAddr, ipv6_socket: bool) -> SocketAddr {
    match addr {
        SocketAddr::V4(a) if ipv6_socket => SocketAddr::new(a.ip().to_ipv6_mapped().into(), a.port()),
        other => other,
    }
}

/// Whether this socket can send to `addr` (an IPv4-only socket cannot reach IPv6).
fn reachable(addr: SocketAddr, ipv6_socket: bool) -> bool {
    ipv6_socket || addr.is_ipv4()
}

/// The session's UDP socket: dual-stack if the system has IPv6, otherwise of the
/// relay's family (IPv6 off, or a system without dual-stack sockets).
fn session_udp(relay: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let dual = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).and_then(|s| {
        s.set_only_v6(false)?;
        s.bind(&SocketAddr::from(([0u16; 8], 0)).into())?;
        Ok(s)
    });
    match dual {
        Ok(s) => Ok(s.into()),
        Err(e) => {
            debug!("no dual-stack socket ({e}): only the relay's family");
            UdpSocket::bind(if relay.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" })
        }
    }
}

const RELAY_MAGIC: [u8; 4] = [0x00, b'N', b'X', b'R'];
const RELAY_BIND: u8 = 0x01;
const RELAY_BOUND: u8 = 0x02;
/// Set on the role of a bind sent to the relay's address of the other family: the
/// relay only notes the address it came from as one more of this side's
/// candidates (servers older than 0.21 ignore it).
const ROLE_ANNOUNCE: u8 = 0x80;

/// The bind that only announces this side's address (see [`ROLE_ANNOUNCE`]).
fn announce_message(route: &Route) -> Vec<u8> {
    let mut msg = bind_message(route, &[]);
    msg[5 + 16] |= ROLE_ANNOUNCE;
    msg
}

/// Binds `socket` to the relay and waits until the other side is bound too.
/// Returns the other side's address as the relay sees it (for the direct path).
fn bind_message(route: &Route, locals: &[SocketAddr]) -> Vec<u8> {
    let mut bind = Vec::with_capacity(64);
    bind.extend_from_slice(&RELAY_MAGIC);
    bind.push(RELAY_BIND);
    bind.extend_from_slice(&route.token);
    bind.push(route.role as u8);
    // At most 4 (the relay's limit).
    let locals = &locals[..locals.len().min(4)];
    bind.push(locals.len() as u8);
    for addr in locals {
        push_addr(&mut bind, *addr);
    }
    bind
}

/// Relay answer to the bind for this token: `Some(true)` if the other side is bound too.
fn bound_state(data: &[u8], token: &[u8; 16]) -> Option<bool> {
    (data.len() >= 22 && data[..5] == [0, b'N', b'X', b'R', RELAY_BOUND] && data[5..21] == token[..])
        .then(|| data[21] == 1)
}

/// Binding to the relay through the WebSocket tunnel (no direct path).
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
    bail!("the other device did not bind to the relay")
}

fn bind_relay(socket: &UdpSocket, route: &Route, locals: &[SocketAddr]) -> Result<Candidates> {
    let bind = bind_message(route, locals);
    let ipv6_socket = socket.local_addr()?.is_ipv6();
    let relay = for_socket(route.relay, ipv6_socket);
    // To the other family's address of the relay, before every bind: the announce
    // must be there when the other side gets its answer.
    let announce = route
        .relay_alt
        .filter(|a| reachable(*a, ipv6_socket))
        .map(|a| (for_socket(a, ipv6_socket), announce_message(route)));

    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    // As large as a QUIC datagram: if the other side has already started, its packets
    // arrive here before binding is over (and on Windows a small buffer does not
    // truncate, it makes recv_from fail with WSAEMSGSIZE).
    let mut buf = [0u8; 2048];
    let mut acked = false;
    // ~15 s: the other side may arrive a few seconds later.
    for _ in 0..60 {
        if let Some((to, msg)) = &announce {
            // Errors ignored: the other family may have no route at all.
            let _ = socket.send_to(msg, *to);
        }
        socket.send_to(&bind, relay)?;
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if canonical(from) == route.relay => match bound_state(&buf[..n], &route.token) {
                    Some(true) => {
                        socket.set_read_timeout(None)?;
                        return Ok(parse_candidates(&buf[22..n]));
                    }
                    Some(false) if !acked => {
                        debug!("bound to the relay, waiting for the other side");
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
        bail!("the other device did not bind to the relay")
    } else {
        bail!("the relay {} does not answer", route.relay)
    }
}

/// Addresses to try to reach the other side directly.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Candidates {
    /// As the relay sees it, through any NATs.
    reflexive: Option<SocketAddr>,
    /// Addresses in its local network (useful when behind the same router).
    locals: Vec<SocketAddr>,
}

impl Candidates {
    /// All addresses, local ones first (on a LAN they are the fastest), without duplicates.
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

/// Address in the relay's format: family (4/6) | IP | big-endian port.
/// Returns the address and the bytes that follow.
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

/// "Ready" answer of the relay: address seen by the relay, then n local addresses.
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

/// Local addresses this PC uses to reach the relay or the Internet (the main
/// network interface), one per family the socket has, with the session socket's
/// port. On IPv6 the local address is also the public one (no NAT).
fn local_candidates(relay: SocketAddr, relay_alt: Option<SocketAddr>, port: u16, ipv6_socket: bool) -> Vec<SocketAddr> {
    // Towards the relay where it has an address of that family, otherwise a public
    // one (connect on UDP sends nothing: it only asks the system which interface
    // it would use).
    let towards = |v6: bool| {
        [Some(relay), relay_alt].into_iter().flatten().find(|a| a.is_ipv6() == v6).unwrap_or_else(|| {
            if v6 { "[2001:4860:4860::8888]:53" } else { "8.8.8.8:53" }.parse().expect("address")
        })
    };
    let probe = |target: SocketAddr| {
        let bind = if target.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        UdpSocket::bind(bind).and_then(|s| s.connect(target).and_then(|()| s.local_addr())).ok()
    };
    let mut out = Vec::new();
    if let Some(a) = probe(towards(false))
        && !a.ip().is_unspecified()
    {
        out.push(SocketAddr::new(a.ip(), port));
    }
    if ipv6_socket
        && let Some(a) = probe(towards(true))
        && let std::net::IpAddr::V6(ip) = a.ip()
        && usable_ipv6(ip, relay)
    {
        out.push(SocketAddr::new(a.ip(), port));
    }
    out
}

/// An IPv6 address worth giving to the other side: not unspecified, not
/// link-local (`fe80::/10`, valid only on one link and needing a scope), not
/// IPv4-mapped, and loopback only towards a relay on loopback (tests).
fn usable_ipv6(ip: std::net::Ipv6Addr, relay: SocketAddr) -> bool {
    !ip.is_unspecified()
        && (ip.segments()[0] & 0xffc0) != 0xfe80
        && ip.to_ipv4_mapped().is_none()
        && (!ip.is_loopback() || relay.ip().is_loopback())
}

/// Opening packets towards the other side: they open the NAT mapping and the local
/// firewall state, so the other side's direct QUIC packets can get in.
/// QUIC drops them (first byte without the 0x40 bit).
fn punch(socket: &UdpSocket, peer: SocketAddr, token: &[u8; 16]) {
    let Ok(local) = socket.local_addr() else { return };
    if !reachable(peer, local.is_ipv6()) {
        return;
    }
    let peer = for_socket(peer, local.is_ipv6());
    let mut packet = vec![0x00, b'N', b'X', b'P'];
    packet.extend_from_slice(token);
    for _ in 0..3 {
        let _ = socket.send_to(&packet, peer);
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Session QUIC endpoint, bound to the relay, and the other side's direct addresses.
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
    let socket = session_udp(route.relay)?;
    let route = route.clone();
    tokio::task::spawn_blocking(move || {
        let local = socket.local_addr()?;
        let locals = local_candidates(route.relay, route.relay_alt, local.port(), local.is_ipv6());
        debug!(?locals, "this side's addresses");
        let candidates = bind_relay(&socket, &route, &locals)?;
        debug!(?candidates, "addresses of the other side");
        let relays = [Some(route.relay), route.relay_alt];
        let peers: Vec<SocketAddr> = candidates
            .all()
            .into_iter()
            .map(canonical)
            .filter(|a| !relays.contains(&Some(*a)) && reachable(*a, local.is_ipv6()))
            .collect();
        for peer in &peers {
            punch(&socket, *peer, &route.token);
        }
        socket.set_nonblocking(true)?;
        Ok((socket, peers))
    })
    .await?
}

/// Agent side: QUIC endpoint accepting only the expected viewer, both through the
/// relay and directly.
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

/// How the other side was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Direct,
    Relay,
}

/// How long to wait for the direct path once the relay is ready.
const DIRECT_GRACE: Duration = Duration::from_millis(1500);

/// Viewer side: tries the direct path and the relay together and prefers the direct one.
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

    let mut relay = Box::pin(endpoint.connect(route.relay, "nuvira-device")?);
    // NUVIRA_FORCE_RELAY=1: relay only, to measure its cost in tests.
    let force_relay = std::env::var_os("NUVIRA_FORCE_RELAY").is_some();
    // Direct attempts towards all the other side's addresses, in parallel.
    let mut direct = tokio::task::JoinSet::new();
    if !force_relay {
        for peer in peers {
            // An address this socket cannot reach (an IPv6 one from an IPv4-only
            // socket) is skipped: the others and the relay are still tried.
            let connecting = match endpoint.connect(peer, "nuvira-device") {
                Ok(connecting) => connecting,
                Err(e) => {
                    debug!("direct attempt to {peer} skipped: {e}");
                    continue;
                }
            };
            direct.spawn(async move { connecting.await.map_err(|e| (peer, e)) });
        }
    }

    // The first direct connection wins; the relay is used only if no direct one
    // succeeds within DIRECT_GRACE of the relay being ready.
    let mut relayed: Option<Result<Connection>> = None;
    let mut deadline = None;
    let chosen = loop {
        tokio::select! {
            Some(r) = direct.join_next(), if !direct.is_empty() => match r {
                Ok(Ok(conn)) => break (conn, Path::Direct),
                Ok(Err((peer, e))) => debug!(%peer, "direct path failed: {e}"),
                Err(e) => debug!("direct attempt interrupted: {e}"),
            },
            r = &mut relay, if relayed.is_none() => {
                relayed = Some(r.context("connecting through the relay"));
                deadline = Some(tokio::time::Instant::now() + DIRECT_GRACE);
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                break (relayed.take().expect("relay ready")?, Path::Relay);
            }
        }
        // All direct attempts failed and the relay is ready: no point waiting.
        if direct.is_empty() && relayed.is_some() {
            break (relayed.take().expect("relay ready")?, Path::Relay);
        }
    };
    let (conn, path) = chosen;
    // The other attempts are closed: the agent uses the connection the session opens on.
    direct.abort_all();
    match relayed {
        Some(Ok(c)) if path == Path::Direct => c.close(0u32.into(), b"direct path"),
        Some(_) => {}
        None if path == Path::Direct => {
            tokio::spawn(async move {
                if let Ok(c) = relay.await {
                    c.close(0u32.into(), b"direct path");
                }
            });
        }
        None => {}
    }
    debug!(?path, remote = %canonical(conn.remote_address()), "connected");
    Ok((conn, path))
}

/// Does the certificate use this Ed25519 public key?
pub fn cert_has_key(cert: &CertificateDer<'_>, key: &PublicKey) -> bool {
    // DER SubjectPublicKeyInfo of an Ed25519 key (RFC 8410): fixed prefix + 32 bytes.
    const ED25519_SPKI_PREFIX: [u8; 12] =
        [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    let Ok(parsed) = webpki::EndEntityCert::try_from(cert) else { return false };
    let spki = parsed.subject_public_key_info();
    spki.as_ref().len() == 44 && spki.as_ref()[..12] == ED25519_SPKI_PREFIX && spki.as_ref()[12..] == key[..]
}

/// Checks that the other device's certificate uses the expected key.
/// The handshake signature (verify_tls13_signature) then proves that the other
/// side holds the private key.
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
            Err(rustls::Error::General("the device does not have the key given by the server".into()))
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
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
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
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
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
        // Local ones first, without duplicates.
        assert_eq!(c.all(), vec![b, a]);
        assert_eq!(parse_addr(&[4, 1, 2]), None);
        assert_eq!(parse_candidates(&[]), Candidates::default());
    }

    #[test]
    fn finds_local_address() {
        let locals = local_candidates("127.0.0.1:9".parse().unwrap(), None, 1234, true);
        assert_eq!(locals.first(), Some(&"127.0.0.1:1234".parse().unwrap()));
        // At most one of each family; an IPv6 one only if global or ULA.
        assert!(locals.len() <= 2);
        assert!(locals.iter().skip(1).all(|a| a.is_ipv6() && a.port() == 1234));
        // IPv4-only socket: no IPv6 address.
        assert!(local_candidates("127.0.0.1:9".parse().unwrap(), None, 1, false).iter().all(|a| a.is_ipv4()));
    }

    #[test]
    fn ipv6_candidates_filtered() {
        let relay: SocketAddr = "[2001:db8::1]:21116".parse().unwrap();
        let ok = |s: &str| usable_ipv6(s.parse().unwrap(), relay);
        assert!(ok("2001:db8::5"));
        assert!(ok("fd12:3456::1"), "ULA: same network");
        assert!(!ok("fe80::1"));
        assert!(!ok("febf::1"));
        assert!(!ok("::"));
        assert!(!ok("::1"));
        assert!(!ok("::ffff:192.168.1.2"));
        assert!(usable_ipv6("::1".parse().unwrap(), "[::1]:9".parse().unwrap()));
    }

    #[test]
    fn mapped_addresses() {
        let v4: SocketAddr = "192.0.2.7:5000".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:192.0.2.7]:5000".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::7]:5000".parse().unwrap();
        assert_eq!(canonical(mapped), v4);
        assert_eq!(canonical(v4), v4);
        assert_eq!(canonical(v6), v6);
        assert_eq!(for_socket(v4, true), mapped);
        assert_eq!(for_socket(v4, false), v4);
        assert_eq!(for_socket(v6, true), v6);
        assert!(!reachable(v6, false));
        assert!(reachable(v4, true));
    }

    #[test]
    fn announce_sets_the_role_flag() {
        let route = Route {
            relay: "192.0.2.1:21116".parse().unwrap(),
            transport: crate::ServerTransport::Udp,
            token: [7; 16],
            peer_public_key: [0; 32],
            role: Role::Agent,
            relay_alt: None,
        };
        let bind = bind_message(&route, &[]);
        let announce = announce_message(&route);
        assert_eq!(bind[21], 2);
        assert_eq!(announce[21], 0x82);
        assert_eq!(announce[..21], bind[..21]);
        // No more than the relay accepts.
        let many: Vec<SocketAddr> = (1..=6).map(|i| SocketAddr::from(([10, 0, 0, i], 1))).collect();
        assert_eq!(bind_message(&route, &many)[22], 4);
    }
}
