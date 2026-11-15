//! A QUIC connection inside a stream of another one (switch sides).
//!
//! When the technician lets the person at the controlled computer see their own
//! screen, the roles swap inside the session already open: no new route, relay
//! or password through the server. The technician's side serves a session of its
//! own on an endpoint whose "socket" is a bidirectional stream of the session
//! ([`StreamSocket`]); at the controlled computer the agent relays that stream to
//! a UDP socket on the loopback ([`bridge_to_udp`]), where a viewer started for
//! the person signed in connects as to any agent, with the certificate pinned.
//!
//! Each QUIC datagram travels as a big-endian u16 length and its bytes. The
//! stream is reliable and ordered, so the inner connection sees no loss; QUIC
//! still needs its datagrams to be at least 1200 bytes, more than a datagram of
//! the outer connection can carry, hence a stream and not datagrams.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, Endpoint, EndpointConfig, RecvStream, SendStream, TokioRuntime, UdpPoller};
use tokio::sync::mpsc;
use tracing::debug;

/// The address the other end of the stream appears to have.
pub const PEER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2)), 1);

/// Writes one datagram as a frame.
pub async fn write_frame(send: &mut SendStream, data: &[u8]) -> Result<()> {
    let len = u16::try_from(data.len()).context("datagram too large")?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(data).await?;
    Ok(())
}

/// Reads one frame: `None` at the end of the stream.
pub async fn read_frame(recv: &mut RecvStream) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 2];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let mut data = vec![0u8; u16::from_be_bytes(len) as usize];
    recv.read_exact(&mut data).await?;
    Ok(Some(data))
}

/// A socket for quinn whose datagrams travel as frames on a stream.
pub struct StreamSocket {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl fmt::Debug for StreamSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StreamSocket")
    }
}

impl StreamSocket {
    /// The socket over `send`/`recv`, with the tasks that move the frames; they
    /// end with the stream (the socket then reports an error to quinn).
    pub fn new(mut send: SendStream, mut recv: RecvStream) -> Arc<Self> {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = out_rx.recv().await {
                if let Err(e) = write_frame(&mut send, &data).await {
                    debug!("nested connection: stream closed for writing: {e:#}");
                    return;
                }
            }
            let _ = send.finish();
        });
        tokio::spawn(async move {
            loop {
                match read_frame(&mut recv).await {
                    Ok(Some(data)) => {
                        if in_tx.send(data).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => return debug!("nested connection: stream closed for reading: {e:#}"),
                }
            }
        });
        Arc::new(Self { tx: out_tx, rx: Mutex::new(in_rx) })
    }
}

#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for StreamSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // As WsSocket: the queue has no limit, QUIC's congestion control sets the pace.
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
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
        let mut rx = self.rx.lock().expect("stream socket");
        match rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(bufs[0].len());
                bufs[0][..n].copy_from_slice(&data[..n]);
                meta[0] = RecvMeta { addr: PEER, len: n, stride: n, ecn: None, dst_ip: None };
                Poll::Ready(Ok(1))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionAborted))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    fn max_transmit_segments(&self) -> usize {
        1
    }
}

/// An agent's endpoint on the stream `send`/`recv`, with `config` (from
/// [`crate::server_config`], whose fingerprint the other side pins).
pub fn server_over_stream(config: quinn::ServerConfig, send: SendStream, recv: RecvStream) -> Result<Endpoint> {
    let socket = StreamSocket::new(send, recv);
    Ok(Endpoint::new_with_abstract_socket(EndpointConfig::default(), Some(config), socket, Arc::new(TokioRuntime))?)
}

/// Relays the stream to `udp` (bound on the loopback): the datagrams of the first
/// program that writes to it go into the stream, the stream's go back to it.
/// Ends when the stream does, or the program stays silent for `idle`.
pub async fn bridge_to_udp(
    mut send: SendStream,
    mut recv: RecvStream,
    udp: tokio::net::UdpSocket,
    idle: std::time::Duration,
) -> Result<()> {
    let udp = Arc::new(udp);
    let peer: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    let to_stream = {
        let (udp, peer) = (udp.clone(), peer.clone());
        async move {
            let mut buf = vec![0u8; 65535];
            loop {
                let (n, from) = match tokio::time::timeout(idle, udp.recv_from(&mut buf)).await {
                    Ok(r) => r?,
                    Err(_) => anyhow::bail!("nothing from the local viewer for {} s", idle.as_secs()),
                };
                // Only the first one: another local program cannot join in.
                let first = *peer.lock().expect("peer").get_or_insert(from);
                if first == from {
                    write_frame(&mut send, &buf[..n]).await?;
                }
            }
        }
    };
    let from_stream = async move {
        while let Some(data) = read_frame(&mut recv).await? {
            let to = *peer.lock().expect("peer");
            if let Some(to) = to {
                let _ = udp.send_to(&data, to).await;
            }
        }
        anyhow::Ok(())
    };
    tokio::select! {
        r = to_stream => r,
        r = from_stream => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection inside a stream of another: the outer one on the loopback, the
    /// inner one served on the stream at one end and reached through the UDP
    /// bridge at the other, with the certificate pinned; a message goes and comes back.
    #[tokio::test]
    async fn connection_inside_a_stream() {
        let (outer_server, outer_fp) = crate::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = outer_server.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let conn = outer_server.accept().await.unwrap().await.unwrap();
            let (send, mut recv) = conn.accept_bi().await.unwrap();
            let mut marker = [0u8; 1];
            recv.read_exact(&mut marker).await.unwrap();
            // The controlled computer's side: the stream to a UDP socket on the loopback.
            let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let local = udp.local_addr().unwrap();
            tokio::spawn(bridge_to_udp(send, recv, udp, std::time::Duration::from_secs(10)));
            (conn, local)
        });
        let outer = crate::connect(addr, Some(outer_fp)).await.unwrap();
        let (mut send, recv) = outer.open_bi().await.unwrap();
        send.write_all(b"R").await.unwrap();
        // The technician's side: an agent's endpoint on the stream.
        let (config, inner_fp) = crate::server_config().unwrap();
        let inner_server = server_over_stream(config, send, recv).unwrap();
        let echo = tokio::spawn(async move {
            let conn = inner_server.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let got = recv.read_to_end(1 << 20).await.unwrap();
            send.write_all(&got).await.unwrap();
            send.finish().unwrap();
            conn.closed().await;
        });
        let (_outer_conn, local) = accept.await.unwrap();
        let inner = crate::connect(local, Some(inner_fp)).await.unwrap();
        let (mut send, mut recv) = inner.open_bi().await.unwrap();
        let message: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        send.write_all(&message).await.unwrap();
        send.finish().unwrap();
        let back = recv.read_to_end(1 << 20).await.unwrap();
        assert_eq!(back, message);
        inner.close(0u32.into(), b"done");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), echo).await;
    }
}
