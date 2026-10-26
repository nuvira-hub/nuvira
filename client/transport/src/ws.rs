//! Ripiego per le reti che bloccano UDP: i datagrammi QUIC viaggiano dentro una
//! connessione WebSocket verso nexo-server (ws:// o wss:// dietro un reverse proxy).
//!
//! [`WsSocket`] implementa il socket astratto di quinn: QUIC, la crittografia
//! end-to-end e il relay restano identici, cambia solo come escono i datagrammi.
//! Il server tratta ogni connessione WebSocket come un indirizzo UDP virtuale.
//! Tutto il traffico va al server: i tentativi di via diretta verso l'altro
//! dispositivo vengono scartati (su queste reti UDP non passa comunque).

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

/// Percorso del ripiego WebSocket su nexo-server.
pub const PATH: &str = "/nexo";

/// URL WebSocket predefinito per un server raggiunto in UDP su `server`
/// (stessa porta, in TCP).
pub fn default_url(server_name: &str, server: SocketAddr) -> String {
    // Host di `server_name` senza porta (anche IPv6, con o senza parentesi).
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
    /// Indirizzo con cui QUIC conosce il server (e il relay): i datagrammi
    /// ricevuti risultano provenire da qui, quelli inviati qui vanno nel tunnel.
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
    /// Apre la connessione WebSocket. `server` è l'indirizzo con cui QUIC
    /// si rivolgerà al server attraverso il tunnel.
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
        .with_context(|| format!("{url}: tempo scaduto"))?
        .with_context(|| format!("collegamento WebSocket a {url}"))?;
        debug!(url, "tunnel WebSocket aperto");
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
                    // Coda piena: si scarta, come farebbe un socket UDP (QUIC ritrasmette).
                    let _ = in_tx.try_send(data.to_vec());
                }
            }
        });
        Ok(Arc::new(Self { server, tx, rx: Mutex::new(rx) }))
    }

    /// Invia un datagramma al server fuori da QUIC (aggancio al relay).
    pub fn send_raw(&self, data: &[u8]) -> Result<()> {
        if self.tx.send(data.to_vec()).is_err() {
            bail!("tunnel WebSocket chiuso");
        }
        Ok(())
    }

    /// Prossimo datagramma dal server fuori da QUIC, entro `timeout`.
    pub async fn recv_raw(&self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let rx = &self.rx;
        let next = std::future::poll_fn(|cx| rx.lock().expect("tunnel").poll_recv(cx));
        match tokio::time::timeout(timeout, next).await {
            Ok(Some(data)) => Ok(Some(data)),
            Ok(None) => bail!("tunnel WebSocket chiuso"),
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
        // La coda verso il tunnel non ha limite: l'invio non deve mai attendere
        // (il controllo di congestione di QUIC regola il ritmo).
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        if transmit.destination != self.server {
            // Via diretta verso l'altro dispositivo: non raggiungibile dal tunnel.
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
        assert_eq!(default_url("192.168.1.10", a), "ws://192.168.1.10:21116/nexo");
        assert_eq!(default_url("nexo.example.it:21116", a), "ws://nexo.example.it:21116/nexo");
        let b: SocketAddr = "[::1]:9000".parse().unwrap();
        assert_eq!(default_url("[::1]:9000", b), "ws://[::1]:9000/nexo");
        assert_eq!(default_url("::1", b), "ws://[::1]:9000/nexo");
    }
}
