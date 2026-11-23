//! A viewer that opens again by itself a session it lost: the web page back from
//! the background (iOS suspends a page it does not show, Chrome for Android stops
//! one hidden for about a minute). Two rules (review Phase 7, MT-P2-11 F2 case 1
//! and MT-P2-12 F1):
//!
//! - [`Ends`]: how each viewer's last session here ended. A viewer that says in its
//!   `Hello` that it is reopening (`Hello.reopen`) a session that ended normally
//!   (the person at the PC ended it, or the agent did) is refused before its
//!   password with `@agent.ended_at_computer`: the end never reached the page
//!   while it was not running, and the person at the PC must not see the session
//!   come back by itself. After a lost connection, or with no record (the agent
//!   restarted since: a known limit), it opens as before.
//! - [`Hosts`]: the viewers whose session is in progress here, and since when
//!   nothing has arrived from them. A new request from a viewer whose own session
//!   is still in progress but silent is that viewer coming back before the agent
//!   noticed the loss (10 s, `liveness`): it is refused as busy (the page tries
//!   again every 2 s) instead of joining its own dead session as a guest.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nuvira_transport::Connection;

/// Records older than this are forgotten.
const KEEP: Duration = Duration::from_secs(24 * 3600);
/// At most this many viewers are remembered (the oldest go first).
const MAX_VIEWERS: usize = 64;
/// How often [`Hosts`] looks at a connection's counters.
const SAMPLE: Duration = Duration::from_millis(250);
/// A host silent this long is gone: a live viewer sends a ping every second and
/// QUIC keep-alives every 2 s (and acknowledges the video).
pub const HOST_SILENT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Ended on purpose: by the person at the PC, the agent (a time limit, its
    /// rules) or the viewer itself.
    Normal,
    /// The connection was lost (nothing received for 10 s, an idle timeout, a reset).
    Lost,
}

impl Ended {
    /// How a session that gave `result` ended (after [`crate::normal_end`]): `Ok`
    /// and an "@key" of the agent are a normal end, any other error a loss.
    pub fn of(result: &anyhow::Result<()>) -> Self {
        match result {
            Ok(()) => Ended::Normal,
            Err(e) if e.chain().any(|c| c.to_string().starts_with('@')) => Ended::Normal,
            Err(_) => Ended::Lost,
        }
    }
}

/// How each viewer's last session here ended.
#[derive(Default)]
pub struct Ends {
    map: Mutex<HashMap<String, (Ended, Instant)>>,
}

impl Ends {
    pub fn record(&self, viewer: &str, ended: Ended) {
        self.record_at(viewer, ended, Instant::now());
    }

    fn record_at(&self, viewer: &str, ended: Ended, at: Instant) {
        let mut map = self.map.lock().expect("ends");
        map.retain(|_, (_, t)| at.saturating_duration_since(*t) < KEEP);
        map.insert(viewer.to_string(), (ended, at));
        while map.len() > MAX_VIEWERS {
            let Some(oldest) = map.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone()) else { break };
            map.remove(&oldest);
        }
    }

    /// Whether an automatic reopen from `viewer` is refused: its last session here
    /// ended normally, less than a day ago.
    pub fn refuses_reopen(&self, viewer: &str) -> bool {
        self.refuses_reopen_at(viewer, Instant::now())
    }

    fn refuses_reopen_at(&self, viewer: &str, now: Instant) -> bool {
        let map = self.map.lock().expect("ends");
        map.get(viewer).is_some_and(|(e, t)| *e == Ended::Normal && now.saturating_duration_since(*t) < KEEP)
    }
}

struct Host {
    id: u64,
    viewer: String,
    heard: Arc<Mutex<Instant>>,
    task: tokio::task::JoinHandle<()>,
}

/// The viewers whose (first-viewer) session runs here, with the last time
/// something arrived from each.
#[derive(Default)]
pub struct Hosts {
    list: Arc<Mutex<Vec<Host>>>,
    next: Mutex<u64>,
}

/// Removes its host from [`Hosts`] when dropped.
pub struct HostGuard {
    list: Arc<Mutex<Vec<Host>>>,
    id: u64,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        let mut list = self.list.lock().expect("hosts");
        if let Some(i) = list.iter().position(|h| h.id == self.id) {
            list.remove(i).task.abort();
        }
    }
}

impl Hosts {
    /// `viewer`'s session on `conn` runs from now until the guard is dropped.
    /// Must be called inside a Tokio runtime.
    pub fn add(&self, viewer: &str, conn: &Connection) -> HostGuard {
        let heard = Arc::new(Mutex::new(Instant::now()));
        let task = tokio::spawn({
            let (conn, heard) = (conn.clone(), heard.clone());
            async move {
                let mut seen = nuvira_transport::liveness::frames_received(&conn);
                let mut tick = tokio::time::interval(SAMPLE);
                loop {
                    tokio::select! {
                        _ = tick.tick() => {}
                        _ = conn.closed() => return,
                    }
                    let now = nuvira_transport::liveness::frames_received(&conn);
                    if now != seen {
                        seen = now;
                        *heard.lock().expect("heard") = Instant::now();
                    }
                }
            }
        });
        let id = {
            let mut next = self.next.lock().expect("hosts");
            *next += 1;
            *next
        };
        self.list.lock().expect("hosts").push(Host { id, viewer: viewer.to_string(), heard, task });
        HostGuard { list: self.list.clone(), id }
    }

    /// Whether `viewer` has a session here from which nothing has arrived for
    /// `silent` or more.
    pub fn silent(&self, viewer: &str, silent: Duration) -> bool {
        self.list
            .lock()
            .expect("hosts")
            .iter()
            .any(|h| h.viewer == viewer && h.heard.lock().expect("heard").elapsed() >= silent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_end_refuses_a_reopen_a_loss_does_not() {
        let ends = Ends::default();
        assert!(!ends.refuses_reopen("v"), "no record: as before");
        ends.record("v", Ended::Normal);
        assert!(ends.refuses_reopen("v"));
        assert!(!ends.refuses_reopen("other"), "only that viewer");
        ends.record("v", Ended::Lost);
        assert!(!ends.refuses_reopen("v"), "the last end counts");
        ends.record("v", Ended::Normal);
        assert!(ends.refuses_reopen("v"));
    }

    #[test]
    fn records_expire_and_are_bounded() {
        let ends = Ends::default();
        let t0 = Instant::now();
        ends.record_at("old", Ended::Normal, t0);
        assert!(ends.refuses_reopen_at("old", t0 + KEEP - Duration::from_secs(1)));
        assert!(!ends.refuses_reopen_at("old", t0 + KEEP), "a day later: forgotten");
        for i in 0..MAX_VIEWERS + 10 {
            ends.record_at(&format!("v{i}"), Ended::Normal, t0 + Duration::from_millis(i as u64));
        }
        let map = ends.map.lock().unwrap();
        assert_eq!(map.len(), MAX_VIEWERS);
        assert!(
            !map.contains_key("v0") && map.contains_key(&format!("v{}", MAX_VIEWERS + 9)),
            "the oldest go"
        );
    }

    #[test]
    fn how_a_session_ended() {
        assert_eq!(Ended::of(&Ok(())), Ended::Normal);
        assert_eq!(Ended::of(&Err(anyhow::anyhow!("@agent.idle_timeout"))), Ended::Normal);
        assert_eq!(Ended::of(&Err(anyhow::anyhow!("@agent.idle_timeout").context("control"))), Ended::Normal);
        let lost = anyhow::Error::new(quinn::ConnectionError::TimedOut)
            .context("connection lost: nothing received from the other side for 10 s");
        assert_eq!(Ended::of(&Err(lost)), Ended::Lost);
        assert_eq!(Ended::of(&Err(anyhow::anyhow!("sending video: connection lost"))), Ended::Lost);
    }

    /// A UDP forwarder on the loopback between one client and `to`: drops every
    /// packet both ways while `cut` is set (as in nuvira_transport::liveness).
    fn forwarder(to: std::net::SocketAddr, cut: Arc<std::sync::atomic::AtomicBool>) -> std::net::SocketAddr {
        use std::net::UdpSocket;
        use std::sync::atomic::Ordering;
        let front = UdpSocket::bind("127.0.0.1:0").unwrap();
        let back = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = front.local_addr().unwrap();
        let client = Arc::new(Mutex::new(None::<std::net::SocketAddr>));
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

    /// A host connection with keep-alives only is not silent; once every packet is
    /// dropped it is, after HOST_SILENT; only for its own viewer, and not once its
    /// session ended (guard dropped).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_host_is_silent_only_when_nothing_arrives() {
        let (server, fp) = nuvira_transport::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let cut = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let front = forwarder(server.local_addr().unwrap(), cut.clone());
        let accepted = tokio::spawn(async move { server.accept().await.unwrap().await.unwrap() });
        let _client = nuvira_transport::connect(front, Some(fp)).await.unwrap();
        let conn = accepted.await.unwrap();
        let hosts = Hosts::default();
        let guard = hosts.add("v", &conn);
        tokio::time::sleep(HOST_SILENT + Duration::from_secs(2)).await;
        assert!(!hosts.silent("v", HOST_SILENT), "keep-alives arrive: not silent");
        cut.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(HOST_SILENT + Duration::from_millis(800)).await;
        assert!(hosts.silent("v", HOST_SILENT), "nothing for HOST_SILENT: silent");
        assert!(!hosts.silent("other", HOST_SILENT), "only its own viewer");
        drop(guard);
        assert!(!hosts.silent("v", HOST_SILENT), "its session ended");
    }
}
