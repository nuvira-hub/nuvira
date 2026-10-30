//! Fallback for networks that block UDP: QUIC datagrams travel inside a
//! WebSocket connection to nuvira-server (ws://, or wss:// behind a reverse proxy).
//!
//! [`WsSocket`] implements quinn's abstract socket: QUIC, end-to-end encryption
//! and the relay stay the same, only the way datagrams leave changes.
//! The server treats each WebSocket connection as a virtual UDP address.
//! All traffic goes to the server: direct-path attempts towards the other device
//! are dropped (UDP does not get through on these networks anyway).

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures_util::{SinkExt, StreamExt};
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;

/// Path of the WebSocket fallback on nuvira-server.
pub const PATH: &str = "/nuvira";

/// Default WebSocket URL for a server reached over UDP at `server`
/// (same port, over TCP).
pub fn default_url(server_name: &str, server: SocketAddr) -> String {
    // Host of `server_name` without the port (IPv6 too, with or without brackets).
    let host = match server_name.parse::<SocketAddr>() {
        Ok(addr) => addr.ip().to_string(),
        Err(_) => match server_name.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => h.to_string(),
            _ => server_name.trim_matches(['[', ']']).to_string(),
        },
    };
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    format!("ws://{host}:{}{PATH}", server.port())
}

pub struct WsSocket {
    /// Address QUIC knows the server (and the relay) by: received datagrams
    /// appear to come from here, those sent here go into the tunnel.
    server: SocketAddr,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl fmt::Debug for WsSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsSocket").field("server", &self.server).finish()
    }
}

impl WsSocket {
    /// Opens the WebSocket connection. `server` is the address QUIC will use
    /// for the server through the tunnel.
    pub async fn connect(url: &str, server: SocketAddr) -> Result<Arc<Self>> {
        let tls = Arc::new(
            rustls::ClientConfig::builder_with_provider(crate::provider())
                .with_safe_default_protocol_versions()?
                .with_root_certificates(rustls::RootCertStore::from_iter(
                    webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                ))
                .with_no_client_auth(),
        );
        let connector = tokio_tungstenite::Connector::Rustls(tls);
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_tls_with_config(url, None, true, Some(connector)),
        )
        .await
        .with_context(|| format!("{url}: timed out"))?
        .with_context(|| format!("WebSocket connection to {url}"))?;
        debug!(url, "WebSocket tunnel open");
        let (mut sink, mut stream) = ws.split();

        let (tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = out_rx.recv().await {
                if sink.send(Message::Binary(data.into())).await.is_err() {
                    return;
                }
            }
            let _ = sink.close().await;
        });
        let (in_tx, rx) = mpsc::channel::<Vec<u8>>(1024);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Binary(data) = msg {
                    // Queue full: dropped, as a UDP socket would do (QUIC retransmits).
                    let _ = in_tx.try_send(data.to_vec());
                }
            }
        });
        Ok(Arc::new(Self { server, tx, rx: Mutex::new(rx) }))
    }

    /// Sends a datagram to the server outside QUIC (binding to the relay).
    pub fn send_raw(&self, data: &[u8]) -> Result<()> {
        if self.tx.send(data.to_vec()).is_err() {
            bail!("WebSocket tunnel closed");
        }
        Ok(())
    }

    /// Next datagram from the server outside QUIC, within `timeout`.
    pub async fn recv_raw(&self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let rx = &self.rx;
        let next = std::future::poll_fn(|cx| rx.lock().expect("tunnel").poll_recv(cx));
        match tokio::time::timeout(timeout, next).await {
            Ok(Some(data)) => Ok(Some(data)),
            Ok(None) => bail!("WebSocket tunnel closed"),
            Err(_) => Ok(None),
        }
    }
}

#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for WsSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // The queue towards the tunnel has no limit: sending must never wait
        // (QUIC congestion control sets the pace).
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        if transmit.destination != self.server {
            // Direct path towards the other device: not reachable through the tunnel.
            return Ok(());
        }
        let segment = transmit.segment_size.unwrap_or(transmit.contents.len()).max(1);
        for chunk in transmit.contents.chunks(segment) {
            self.tx.send(chunk.to_vec()).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut rx = self.rx.lock().expect("tunnel");
        match rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(bufs[0].len());
                bufs[0][..n].copy_from_slice(&data[..n]);
                meta[0] = RecvMeta { addr: self.server, len: n, stride: n, ecn: None, dst_ip: None };
                Poll::Ready(Ok(1))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionAborted))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    fn max_transmit_segments(&self) -> usize {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_urls() {
        let a: SocketAddr = "192.168.1.10:21116".parse().unwrap();
        assert_eq!(default_url("192.168.1.10", a), "ws://192.168.1.10:21116/nuvira");
        assert_eq!(default_url("nuvira.example.it:21116", a), "ws://nuvira.example.it:21116/nuvira");
        let b: SocketAddr = "[::1]:9000".parse().unwrap();
        assert_eq!(default_url("[::1]:9000", b), "ws://[::1]:9000/nuvira");
        assert_eq!(default_url("::1", b), "ws://[::1]:9000/nuvira");
    }
}
