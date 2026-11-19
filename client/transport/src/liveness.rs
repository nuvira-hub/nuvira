//! Whether the other side of a connection is still there, from the packets
//! received on it, independently of QUIC's idle timeout (review NET-1).
//!
//! QUIC's idle timer of a side that has data to send can be held off by the
//! congestion gate after an outage, so session connections use a long idle
//! timeout ([`crate::SESSION_IDLE_TIMEOUT`]). The agent still wants to free the
//! computer soon when the viewer vanished without closing (killed, powered off,
//! network gone): a live viewer always sends something (a Ping every second, its
//! ACKs, QUIC keep-alives every 2 s), so a connection on which no authenticated
//! packet arrived for [`PEER_SILENCE_LIMIT`] is closed, as the 10 s idle timeout
//! did before.
//!
//! The check runs in a task of its own that reads `Connection::stats()` once a
//! second: it does not depend on the session's loops, which can wait for a write
//! behind the send window (NET-11).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use quinn::{Connection, VarInt};
use tokio::time::Instant;

/// No authenticated packet from the other side for this long: it is gone.
pub const PEER_SILENCE_LIMIT: Duration = Duration::from_secs(10);

/// Application close code of a connection closed by [`watch`] (0 is a normal end).
pub const SILENT_PEER_CODE: u32 = 2;

/// How often the counters are read.
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// Frames received on `conn` after decryption, of every kind: it grows with every
/// authenticated packet. (UDP datagrams are counted before decryption, so a
/// datagram with the right connection ID but a bad tag would count there.)
pub fn frames_received(conn: &Connection) -> u64 {
    let f = conn.stats().frame_rx;
    [
        f.acks,
        f.ack_frequency,
        f.crypto,
        f.connection_close,
        f.data_blocked,
        f.datagram,
        u64::from(f.handshake_done),
        f.immediate_ack,
        f.max_data,
        f.max_stream_data,
        f.max_streams_bidi,
        f.max_streams_uni,
        f.new_connection_id,
        f.new_token,
        f.path_challenge,
        f.path_response,
        f.ping,
        f.reset_stream,
        f.retire_connection_id,
        f.stream_data_blocked,
        f.streams_blocked_bidi,
        f.streams_blocked_uni,
        f.stop_sending,
        f.stream,
    ]
    .iter()
    .fold(0u64, |a, b| a.wrapping_add(*b))
}

/// The watch on one connection, see [`watch`]. Dropping it stops the watch.
pub struct Watch {
    task: tokio::task::JoinHandle<()>,
    fired: Arc<AtomicBool>,
    limit: Duration,
}

impl Watch {
    /// The watch closed the connection because the other side was silent.
    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }

    /// `result` of the work on the connection, or a "timed out" error
    /// (`quinn::ConnectionError::TimedOut`) when the watch closed it: the work then
    /// failed with "closed locally" errors of its own.
    pub fn result<T>(&self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        if self.fired() {
            // quinn's own "timed out", as the idle timeout gave: the session log
            // and the callers treat both the same.
            let secs = self.limit.as_secs();
            return Err(anyhow::Error::new(quinn::ConnectionError::TimedOut)
                .context(format!("connection lost: nothing received from the other side for {secs} s")));
        }
        result
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Closes `conn` (code [`SILENT_PEER_CODE`], reason "timed out") once no
/// authenticated packet has arrived on it for `limit`. Must be called inside a
/// Tokio runtime.
pub fn watch(conn: &Connection, limit: Duration) -> Watch {
    let fired = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn({
        let conn = conn.clone();
        let fired = fired.clone();
        async move {
            let mut seen = frames_received(&conn);
            let mut last = Instant::now();
            let mut tick = tokio::time::interval(CHECK_EVERY.min(limit));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = conn.closed() => return,
                }
                let now = frames_received(&conn);
                if now != seen {
                    seen = now;
                    last = Instant::now();
                } else if last.elapsed() >= limit {
                    tracing::info!(
                        "nothing received from {} for {} s: closing the connection",
                        conn.remote_address(),
                        last.elapsed().as_secs()
                    );
                    fired.store(true, Ordering::Release);
                    conn.close(VarInt::from_u32(SILENT_PEER_CODE), b"timed out");
                    return;
                }
            }
        }
    });
    Watch { task, fired, limit }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, UdpSocket};

    /// A UDP forwarder on the loopback between one client and `to`: drops every
    /// packet both ways while `cut` is set.
    fn forwarder(to: SocketAddr, cut: Arc<AtomicBool>) -> SocketAddr {
        let front = UdpSocket::bind("127.0.0.1:0").unwrap();
        let back = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = front.local_addr().unwrap();
        let client = Arc::new(std::sync::Mutex::new(None::<SocketAddr>));
        {
            let (front, back, cut, client) =
                (front.try_clone().unwrap(), back.try_clone().unwrap(), cut.clone(), client.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 65536];
                while let Ok((n, from)) = front.recv_from(&mut buf) {
                    *client.lock().unwrap() = Some(from);
                    if !cut.load(Ordering::Relaxed) {
                        let _ = back.send_to(&buf[..n], to);
                    }
                }
            });
        }
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok((n, _)) = back.recv_from(&mut buf) {
                let c = *client.lock().unwrap();
                if let (Some(c), false) = (c, cut.load(Ordering::Relaxed)) {
                    let _ = front.send_to(&buf[..n], c);
                }
            }
        });
        addr
    }

    /// A live connection (keep-alives only) is never closed by the watch; once
    /// every packet is dropped, the watch closes it after the limit (plus at most
    /// one check), and the result is quinn's "timed out".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closes_a_silent_connection_only() {
        let (server, fp) = crate::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let cut = Arc::new(AtomicBool::new(false));
        let front = forwarder(server.local_addr().unwrap(), cut.clone());
        let accepted = tokio::spawn(async move { server.accept().await.unwrap().await.unwrap() });
        let client = crate::connect(front, Some(fp)).await.unwrap();
        let conn = accepted.await.unwrap();
        let limit = Duration::from_secs(3);
        let watch = watch(&conn, limit);

        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!watch.fired(), "fired on a live connection");
        assert!(conn.close_reason().is_none());

        cut.store(true, Ordering::Relaxed);
        let start = std::time::Instant::now();
        let closed =
            tokio::time::timeout(Duration::from_secs(10), conn.closed()).await.expect("closed by the watch");
        let after = start.elapsed();
        assert!(matches!(closed, quinn::ConnectionError::LocallyClosed), "{closed:?}");
        assert!(watch.fired());
        assert!(after >= limit - Duration::from_millis(200) && after <= limit + CHECK_EVERY * 2, "{after:?}");
        let e = watch.result::<()>(Ok(())).unwrap_err();
        assert!(format!("{e:#}").contains("timed out"), "{e:#}");
        assert!(e.chain().any(|c| matches!(c.downcast_ref(), Some(quinn::ConnectionError::TimedOut))));
        drop(client);
    }

    /// Dropping the watch stops it: the connection stays open past the limit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_watch_does_nothing() {
        let (server, fp) = crate::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let cut = Arc::new(AtomicBool::new(false));
        let front = forwarder(server.local_addr().unwrap(), cut.clone());
        let accepted = tokio::spawn(async move { server.accept().await.unwrap().await.unwrap() });
        let _client = crate::connect(front, Some(fp)).await.unwrap();
        let conn = accepted.await.unwrap();
        drop(watch(&conn, Duration::from_secs(1)));
        cut.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(conn.close_reason().is_none());
    }

    #[tokio::test]
    async fn frames_received_grows() {
        let (server, fp) = crate::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let accepted = tokio::spawn(async move { server.accept().await.unwrap().await.unwrap() });
        let client = crate::connect(addr, Some(fp)).await.unwrap();
        let conn = accepted.await.unwrap();
        let before = frames_received(&conn);
        let mut s = client.open_uni().await.unwrap();
        s.write_all(b"x").await.unwrap();
        s.finish().unwrap();
        let mut r = conn.accept_uni().await.unwrap();
        r.read_to_end(10).await.unwrap();
        assert!(frames_received(&conn) > before);
    }
}
