//! Repeated network outages on one session connection, at the transport level
//! (review NET-1, K38; the Phase 3 probe of `docs/review/analysis/transport.md`
//! made into a test). Slow (about 2 minutes), so ignored by default:
//!
//!   cargo test --release -p nuvira-transport --test outage_probe -- --ignored --nocapture
//!
//! The agent side writes 8 Mbit/s on a uni stream (60 chunks a second, a chunk
//! skipped while the previous one is still being written, as the agent's video
//! does) and answers 8-byte pings on a bidi stream; the viewer side pings every
//! second, as the viewer does. A UDP proxy on the loopback drops every packet both
//! ways during each outage. With the 10 s idle timeout of before NET-1, the
//! sender's congestion window full of lost packets kept it silent (no ACK either)
//! past the viewer's idle timeout in most runs; with the session configuration
//! (30 s) each of 6 outages of 8 s, 1 s apart, must be recovered: the viewer
//! receives video data again within 15 s of the end of each outage.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A UDP proxy between one client and `to` that drops everything while `cut`.
fn proxy(to: SocketAddr, cut: Arc<AtomicBool>) -> SocketAddr {
    let front = UdpSocket::bind("127.0.0.1:0").unwrap();
    let back = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = front.local_addr().unwrap();
    let client = Arc::new(Mutex::new(None::<SocketAddr>));
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

fn now_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

#[test]
#[ignore = "about 2 minutes: run with --ignored"]
fn repeated_8s_outages_are_recovered() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async {
        let (server, fp) = nuvira_transport::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let cut = Arc::new(AtomicBool::new(false));
        let front = proxy(server.local_addr().unwrap(), cut.clone());
        let start = Instant::now();

        // Agent side: video and ping answers.
        let agent = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let (mut psend, mut precv) = conn.accept_bi().await.unwrap();
            tokio::spawn(async move {
                let mut ping = [0u8; 8];
                while precv.read_exact(&mut ping).await.is_ok() {
                    if psend.write_all(&ping).await.is_err() {
                        return;
                    }
                }
            });
            let mut video = conn.open_uni().await.unwrap();
            let chunk = vec![0x5au8; 8_000_000 / 8 / 60];
            let writing = Arc::new(AtomicBool::new(false));
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
            let w = writing.clone();
            let writer = tokio::spawn(async move {
                while rx.recv().await.is_some() {
                    if video.write_all(&chunk).await.is_err() {
                        return;
                    }
                    w.store(false, Ordering::Relaxed);
                }
            });
            let mut tick = tokio::time::interval(Duration::from_micros(16_667));
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = conn.closed() => break,
                }
                // A frame is skipped while the previous one is still being written.
                if !writing.swap(true, Ordering::Relaxed) && tx.try_send(()).is_err() {
                    writing.store(false, Ordering::Relaxed);
                }
            }
            writer.abort();
            conn.close_reason()
        });

        // Viewer side.
        let conn = nuvira_transport::connect(front, Some(fp)).await.unwrap();
        let (mut psend, mut precv) = conn.open_bi().await.unwrap();
        psend.write_all(&0u64.to_be_bytes()).await.unwrap();
        let last_video = Arc::new(AtomicU64::new(0));
        {
            let conn = conn.clone();
            let last = last_video.clone();
            tokio::spawn(async move {
                let mut video = conn.accept_uni().await.unwrap();
                let mut buf = vec![0u8; 65536];
                while let Ok(Some(_)) = video.read(&mut buf).await {
                    last.store(now_ms(start), Ordering::Relaxed);
                }
            });
        }
        tokio::spawn(async move {
            let mut ping = [0u8; 8];
            while precv.read_exact(&mut ping).await.is_ok() {}
        });
        {
            let conn = conn.clone();
            tokio::spawn(async move {
                let mut n = 1u64;
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if psend.write_all(&n.to_be_bytes()).await.is_err() || conn.close_reason().is_some() {
                        return;
                    }
                    n += 1;
                }
            });
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(last_video.load(Ordering::Relaxed) > 0, "no video before the outages");

        let mut recovered = Vec::new();
        for i in 1..=6 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cut.store(true, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(8)).await;
            cut.store(false, Ordering::Relaxed);
            let end = now_ms(start);
            let back = loop {
                if last_video.load(Ordering::Relaxed) > end {
                    break Some(now_ms(start) - end);
                }
                if let Some(e) = conn.close_reason() {
                    panic!("outage {i}: session lost: {e} (recovered before: {recovered:?})");
                }
                if now_ms(start) - end > 15_000 {
                    break None;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            let stats = conn.stats();
            eprintln!("outage {i} of 8 s: video again after {back:?} ms (viewer rtt {:?})", stats.path.rtt);
            let Some(ms) = back else {
                panic!("outage {i}: no video within 15 s (recovered before: {recovered:?})")
            };
            recovered.push(ms);
        }
        conn.close(0u32.into(), b"done");
        let agent_end = tokio::time::timeout(Duration::from_secs(5), agent).await.unwrap().unwrap();
        eprintln!("recovered after {recovered:?} ms; agent's connection ended: {agent_end:?}");
    });
}
