//! TCP tunnel (NuviraHub Pro): a bidirectional stream of the viewer for each TCP
//! connection, towards an address of this computer's local network.
//!
//! The stream starts with `StreamOpen.tunnel` (host and port, read by the caller);
//! the agent connects, answers `TunnelResult` and then copies the raw bytes both
//! ways until either side closes. Only local destinations are allowed (loopback,
//! private and link-local addresses): the PC must not become a proxy to the internet.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use nuvira_proto::v1::{TunnelOpen, TunnelResult};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{info, warn};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    open: TunnelOpen,
    refused: Option<&'static str>,
) -> Result<()> {
    let connected = match (refused, u16::try_from(open.port)) {
        (Some(reason), _) => Err(reason),
        (None, Ok(port)) if port != 0 => connect(&open.host, port).await,
        (None, _) => Err("@agent.tunnel_unreachable"),
    };
    let tcp = match connected {
        Ok(tcp) => tcp,
        Err(reason) => {
            info!(host = open.host, port = open.port, reason, "tunnel refused");
            let result = TunnelResult { ok: false, error: reason.into() };
            send.write_all(&nuvira_proto::framing::encode(&result)).await?;
            let _ = send.finish();
            return Ok(());
        }
    };
    let peer = tcp.peer_addr().ok();
    send.write_all(&nuvira_proto::framing::encode(&TunnelResult { ok: true, error: String::new() })).await?;
    info!(host = open.host, port = open.port, ?peer, "tunnel open");

    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();
    let to_tcp = async {
        let copied = tokio::io::copy(&mut recv, &mut tcp_tx).await;
        let _ = tcp_tx.shutdown().await;
        copied
    };
    let to_viewer = async {
        let copied = tokio::io::copy(&mut tcp_rx, &mut send).await;
        let _ = send.finish();
        copied
    };
    let (up, down) = tokio::join!(to_tcp, to_viewer);
    info!(
        host = open.host,
        port = open.port,
        sent = up.as_ref().ok(),
        received = down.as_ref().ok(),
        "tunnel closed"
    );
    Ok(())
}

/// Connects to the first local address of `host`; `Err` with the reason ("@key").
async fn connect(host: &str, port: u16) -> Result<TcpStream, &'static str> {
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            warn!(host, "tunnel: name not resolved: {e}");
            return Err("@agent.tunnel_unreachable");
        }
    };
    let local: Vec<SocketAddr> = addrs.iter().copied().filter(|a| is_local(a.ip())).collect();
    if local.is_empty() {
        warn!(host, ?addrs, "tunnel: destination outside the local network");
        return Err("@agent.tunnel_not_local");
    }
    for addr in local {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(tcp)) => {
                let _ = tcp.set_nodelay(true);
                return Ok(tcp);
            }
            Ok(Err(e)) => warn!(%addr, "tunnel: {e}"),
            Err(_) => warn!(%addr, "tunnel: no answer"),
        }
    }
    Err("@agent.tunnel_unreachable")
}

/// Loopback, private (RFC 1918, IPv6 unique local) and link-local addresses.
fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_destinations() {
        for local in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.1",
            "192.168.178.1",
            "169.254.1.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(is_local(local.parse().unwrap()), "{local}");
        }
        for public in ["8.8.8.8", "172.32.0.1", "100.64.0.1", "1.1.1.1", "2001:4860::8888", "::ffff:8.8.8.8"]
        {
            assert!(!is_local(public.parse().unwrap()), "{public}");
        }
    }
}
