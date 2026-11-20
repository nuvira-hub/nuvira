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
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::{ALPN, provider, session_transport_config};

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

/// `cancel` stops it between two binds (the session it was for is gone).
fn bind_relay(
    socket: &UdpSocket,
    route: &Route,
    locals: &[SocketAddr],
    cancel: &AtomicBool,
) -> Result<Candidates> {
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
        if cancel.load(Ordering::Relaxed) {
            bail!("binding to the relay stopped: the session is gone");
        }
        if let Some((to, msg)) = &announce {
            // Errors ignored: the other family may have no route at all.
            let _ = socket.send_to(msg, *to);
        }
        socket.send_to(&bind, relay)?;
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if canonical(from) == route.relay => match bound_state(&buf[..n], &route.token)
                {
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
fn local_candidates(
    relay: SocketAddr,
    relay_alt: Option<SocketAddr>,
    port: u16,
    ipv6_socket: bool,
) -> Vec<SocketAddr> {
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

/// An opening packet: the magic, then 16 zero bytes. Nobody reads it, in any
/// version (QUIC drops it: first byte without the 0x40 bit). It used to carry
/// the relay token, and the other side's private addresses can be unrelated
/// hosts on this side's network, which could then re-point the relayed session
/// (review NET-2); the length stays 20 bytes so that NATs and firewalls see what
/// they saw before.
const PUNCH: [u8; 20] = [0x00, b'N', b'X', b'P', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Opening packets towards the other side: they open the NAT mapping and the local
/// firewall state, so the other side's direct QUIC packets can get in.
fn punch(socket: &UdpSocket, peer: SocketAddr) {
    let Ok(local) = socket.local_addr() else { return };
    if !reachable(peer, local.is_ipv6()) {
        return;
    }
    let peer = for_socket(peer, local.is_ipv6());
    for _ in 0..3 {
        let _ = socket.send_to(&PUNCH, peer);
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

/// Sets the flag when dropped: with the future of [`session_socket`] (a viewer
/// window closed, an agent session cancelled), so that its blocking task stops
/// binding to the relay within ~250 ms instead of ~15 s (review NET-14).
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn session_socket(route: &Route) -> Result<(UdpSocket, Vec<SocketAddr>)> {
    let socket = session_udp(route.relay)?;
    let route = route.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    tokio::task::spawn_blocking(move || {
        let local = socket.local_addr()?;
        let locals = local_candidates(route.relay, route.relay_alt, local.port(), local.is_ipv6());
        debug!(?locals, "this side's addresses");
        let candidates = bind_relay(&socket, &route, &locals, &cancel)?;
        debug!(?candidates, "addresses of the other side");
        let relays = [Some(route.relay), route.relay_alt];
        // Only families this PC has a route for (an IPv4-only network has no IPv6
        // address of its own: sending there fails with "network unreachable").
        let has_family = |a: &SocketAddr| locals.iter().any(|l| l.is_ipv6() == a.is_ipv6());
        let peers: Vec<SocketAddr> = candidates
            .all()
            .into_iter()
            .map(canonical)
            .filter(|a| !relays.contains(&Some(*a)) && reachable(*a, local.is_ipv6()) && has_family(a))
            .collect();
        for peer in &peers {
            punch(&socket, *peer);
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
    config.transport_config(session_transport_config());
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
    config.transport_config(session_transport_config());
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
    fn punch_packets_do_not_carry_the_token() {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        punch(&socket, peer.local_addr().unwrap());
        let mut buf = [0u8; 64];
        for _ in 0..3 {
            let (n, from) = peer.recv_from(&mut buf).unwrap();
            assert_eq!(from, socket.local_addr().unwrap());
            // Same length and magic as before review NET-2, no token after it.
            assert_eq!(n, 20);
            assert_eq!(buf[..4], [0x00, b'N', b'X', b'P']);
            assert_eq!(buf[4..20], [0; 16]);
        }
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

/// Characterisation tests (Phase 2 of the full review): the relay's wire format as
/// parsed here, and the bind handshake against a fake relay on the loopback.
#[cfg(test)]
mod characterisation {
    use super::*;
    use proptest::prelude::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn route(relay: SocketAddr) -> Route {
        Route {
            relay,
            transport: crate::ServerTransport::Udp,
            token: [9; 16],
            peer_public_key: [0; 32],
            role: Role::Viewer,
            relay_alt: None,
        }
    }

    fn bound(token: &[u8; 16], state: u8, tail: &[u8]) -> Vec<u8> {
        let mut m = vec![0, b'N', b'X', b'R', RELAY_BOUND];
        m.extend_from_slice(token);
        m.push(state);
        m.extend_from_slice(tail);
        m
    }

    #[test]
    fn parse_addr_lengths() {
        assert_eq!(parse_addr(&[]), None);
        // IPv4: family + 4 + 2 bytes, no less.
        assert_eq!(parse_addr(&[4, 1, 2, 3, 4, 0]), None);
        let (a, rest) = parse_addr(&[4, 1, 2, 3, 4, 0x12, 0x34, 0xee]).unwrap();
        assert_eq!(a, SocketAddr::from(([1, 2, 3, 4], 0x1234)));
        assert_eq!(rest, [0xee]);
        // IPv6: family + 16 + 2.
        let mut v6 = vec![6];
        v6.extend_from_slice(&[0; 15]);
        v6.push(1);
        assert_eq!(parse_addr(&v6), None, "no port");
        v6.extend_from_slice(&[0, 80]);
        assert_eq!(parse_addr(&v6), Some(("[::1]:80".parse().unwrap(), &[][..])));
        // Any other family byte.
        for family in [0u8, 5, 46, 255] {
            assert_eq!(parse_addr(&[family; 20]), None);
        }
    }

    #[test]
    fn parse_candidates_edge_cases() {
        let a: SocketAddr = "192.0.2.1:1".parse().unwrap();
        let b: SocketAddr = "192.0.2.2:2".parse().unwrap();
        let mut wire = Vec::new();
        push_addr(&mut wire, a);
        // Reflexive only, no count byte.
        assert_eq!(parse_candidates(&wire), Candidates { reflexive: Some(a), locals: vec![] });
        // A count larger than the addresses there: the ones there are kept.
        wire.push(200);
        push_addr(&mut wire, b);
        wire.extend_from_slice(&[4, 1]);
        assert_eq!(parse_candidates(&wire), Candidates { reflexive: Some(a), locals: vec![b] });
        // A reflexive address that does not parse: nothing at all.
        assert_eq!(parse_candidates(&[9, 9, 9]), Candidates::default());
        // IPv4-mapped IPv6 addresses are kept as given (no canonical()).
        let mapped: SocketAddr = "[::ffff:192.0.2.1]:1".parse().unwrap();
        let mut wire = Vec::new();
        push_addr(&mut wire, mapped);
        assert_eq!(parse_candidates(&wire).reflexive, Some(mapped));
        // all(): the reflexive address equal to a local one appears once, locals first.
        let c = Candidates { reflexive: Some(a), locals: vec![b, a, b] };
        assert_eq!(c.all(), vec![b, a]);
    }

    #[test]
    fn bound_state_cases() {
        let token = [9u8; 16];
        assert_eq!(bound_state(&bound(&token, 1, &[]), &token), Some(true));
        assert_eq!(bound_state(&bound(&token, 0, &[]), &token), Some(false));
        // Any value other than 1 means "not yet".
        assert_eq!(bound_state(&bound(&token, 2, &[]), &token), Some(false));
        // Candidates may follow.
        assert_eq!(bound_state(&bound(&token, 1, &[4, 1, 2, 3, 4, 0, 1]), &token), Some(true));
        // Another token, another message type, or too short: not an answer.
        assert_eq!(bound_state(&bound(&[8; 16], 1, &[]), &token), None);
        let mut bind = bound(&token, 1, &[]);
        bind[4] = RELAY_BIND;
        assert_eq!(bound_state(&bind, &token), None);
        assert_eq!(bound_state(&bound(&token, 1, &[])[..21], &token), None);
        assert_eq!(bound_state(&[], &token), None);
    }

    #[test]
    fn bind_message_layout() {
        let r = route("192.0.2.1:21116".parse().unwrap());
        let locals: Vec<SocketAddr> = vec!["10.0.0.1:5".parse().unwrap(), "[2001:db8::1]:6".parse().unwrap()];
        let m = bind_message(&r, &locals);
        assert_eq!(m[..5], [0, b'N', b'X', b'R', RELAY_BIND]);
        assert_eq!(m[5..21], [9; 16]);
        assert_eq!(m[21], Role::Viewer as u8);
        assert_eq!(m[22], 2);
        let (first, rest) = parse_addr(&m[23..]).unwrap();
        let (second, rest) = parse_addr(rest).unwrap();
        assert_eq!((first, second, rest.len()), (locals[0], locals[1], 0));
    }

    /// A fake relay on the loopback: answers each bind with `answers` in turn (the
    /// last one again and again), from its own address; stops after the answer
    /// "both bound" or 3 s without binds.
    fn fake_relay(answers: Vec<Vec<u8>>) -> (SocketAddr, std::thread::JoinHandle<Vec<Vec<u8>>>) {
        let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
        relay.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let addr = relay.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut buf = [0u8; 2048];
            let mut i = 0;
            while let Ok((n, from)) = relay.recv_from(&mut buf) {
                got.push(buf[..n].to_vec());
                let answer = &answers[i.min(answers.len() - 1)];
                i += 1;
                relay.send_to(answer, from).unwrap();
                if answer.get(21) == Some(&1) {
                    break;
                }
            }
            got
        });
        (addr, handle)
    }

    #[test]
    fn bind_relay_waits_for_the_other_side_and_returns_its_candidates() {
        let token = [9u8; 16];
        let peer: SocketAddr = "198.51.100.7:4000".parse().unwrap();
        let local: SocketAddr = "10.1.1.1:4001".parse().unwrap();
        let mut tail = Vec::new();
        push_addr(&mut tail, peer);
        tail.push(1);
        push_addr(&mut tail, local);
        let (relay, handle) = fake_relay(vec![bound(&token, 0, &[]), bound(&token, 1, &tail)]);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let candidates = bind_relay(&socket, &route(relay), &[], &AtomicBool::new(false)).unwrap();
        assert_eq!(candidates, Candidates { reflexive: Some(peer), locals: vec![local] });
        let binds = handle.join().unwrap();
        // The first answer ("bound, alone") is noted; the next round's bind gets "both".
        assert_eq!(binds.len(), 2);
        assert!(binds.iter().all(|b| b[4] == RELAY_BIND && b[5..21] == token));
        // The socket is back to blocking without a timeout.
        assert_eq!(socket.read_timeout().unwrap(), None);
    }

    #[test]
    fn bind_relay_ignores_answers_from_other_addresses() {
        // KNOWN LEAD (features.md, UNVERIFIED, p2p.rs:180): the only check on the
        // answer's origin is its source address (and the token it echoes). An answer
        // from another loopback port is ignored; the one from the relay's address
        // is taken.
        let token = [9u8; 16];
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let me = socket.local_addr().unwrap();
        let (relay, handle) = fake_relay(vec![bound(&token, 1, &[])]);
        let impostor = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut fake = Vec::new();
        push_addr(&mut fake, "203.0.113.66:1".parse().unwrap());
        impostor.send_to(&bound(&token, 1, &fake), me).unwrap();
        // The impostor's answer is queued first; the relay's comes after.
        let candidates = bind_relay(&socket, &route(relay), &[], &AtomicBool::new(false)).unwrap();
        assert_eq!(candidates, Candidates::default(), "the relay's empty answer, not the impostor's");
        handle.join().unwrap();
    }

    #[test]
    fn bind_relay_gives_up_after_about_15_seconds() {
        // A relay that never answers.
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let start = std::time::Instant::now();
        let err = bind_relay(&socket, &route(silent.local_addr().unwrap()), &[], &AtomicBool::new(false))
            .unwrap_err()
            .to_string();
        let took = start.elapsed();
        assert_eq!(err, format!("the relay {} does not answer", silent.local_addr().unwrap()));
        assert!(took >= Duration::from_secs(14) && took < Duration::from_secs(30), "{took:?}");
        // 60 binds were sent.
        silent.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut n = 0;
        let mut buf = [0u8; 256];
        while silent.recv_from(&mut buf).is_ok() {
            n += 1;
        }
        assert_eq!(n, 60);
    }

    #[tokio::test]
    async fn a_dropped_session_socket_stops_binding() {
        // Review NET-14: a relay that never answers, and a session given up after
        // 600 ms. The blocking task used to bind on for ~15 s (60 binds).
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let r = route(silent.local_addr().unwrap());
        assert!(tokio::time::timeout(Duration::from_millis(600), session_socket(&r)).await.is_err());
        // One bind may still be on its way when the flag is set.
        std::thread::sleep(Duration::from_millis(400));
        silent.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut buf = [0u8; 256];
        let mut before = 0;
        while silent.recv_from(&mut buf).is_ok() {
            before += 1;
        }
        assert!((2..=5).contains(&before), "{before} binds in the first second");
        std::thread::sleep(Duration::from_millis(1000));
        let mut after = 0;
        while silent.recv_from(&mut buf).is_ok() {
            after += 1;
        }
        assert_eq!(after, 0, "binds after the session was dropped");
    }

    #[test]
    fn bind_relay_acked_but_alone() {
        let token = [9u8; 16];
        let (relay, _handle) = fake_relay(vec![bound(&token, 0, &[])]);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let err = bind_relay(&socket, &route(relay), &[], &AtomicBool::new(false)).unwrap_err().to_string();
        assert_eq!(err, "the other device did not bind to the relay");
    }

    fn any_addr() -> impl Strategy<Value = SocketAddr> {
        prop_oneof![
            (any::<[u8; 4]>(), any::<u16>())
                .prop_map(|(ip, p)| SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), p)),
            (any::<[u8; 16]>(), any::<u16>())
                .prop_map(|(ip, p)| SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), p)),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

        #[test]
        fn parsers_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..200), token in any::<[u8; 16]>()) {
            let _ = parse_addr(&bytes);
            let c = parse_candidates(&bytes);
            prop_assert!(c.locals.len() <= 255);
            let _ = c.all();
            let _ = bound_state(&bytes, &token);
        }

        #[test]
        fn push_and_parse_round_trip(
            reflexive in any_addr(),
            locals in prop::collection::vec(any_addr(), 0..10),
            tail in prop::collection::vec(any::<u8>(), 0..8),
        ) {
            let mut wire = Vec::new();
            push_addr(&mut wire, reflexive);
            wire.push(locals.len() as u8);
            for a in &locals {
                push_addr(&mut wire, *a);
            }
            wire.extend(&tail);
            let c = parse_candidates(&wire);
            prop_assert_eq!(c.reflexive, Some(reflexive));
            prop_assert_eq!(c.locals, locals);
        }

        #[test]
        fn bind_messages_keep_at_most_four_locals(locals in prop::collection::vec(any_addr(), 0..10)) {
            let m = bind_message(&route("192.0.2.1:1".parse().unwrap()), &locals);
            prop_assert_eq!(m[22] as usize, locals.len().min(4));
            let mut rest = &m[23..];
            for a in locals.iter().take(4) {
                let (got, tail) = parse_addr(rest).unwrap();
                prop_assert_eq!(got, *a);
                rest = tail;
            }
            prop_assert!(rest.is_empty());
        }
    }
}
