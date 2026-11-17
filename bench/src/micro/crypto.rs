//! B-MICRO-14 password hash cost (`nuvira_secret`) and B-MICRO-16 QUIC/TLS stream
//! throughput in loopback (`nuvira_transport::server_endpoint` / `connect`, the
//! production transport configuration).
//!
//! 14 is gated both ways (`security_both_ways`): a faster hash is a weaker one.
//!
//! 16: agent side and viewer side each run a current-thread tokio runtime on a
//! thread of their own, so the CPU time and allocations of each thread are those of
//! that side (quinn's endpoint driver runs on the runtime that created it). Each
//! rep sends either 256 MiB of seeded data on one uni stream in 256 KiB writes, or
//! 10,000 `FileChunk` messages of 1 KiB through `FramedSend` / `FramedRecv`. A first
//! untimed transfer checks the received bytes' SHA-256 (gate `exact`).

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use nuvira_proto::v1::FileChunk;
use nuvira_transport::{FramedRecv, FramedSend};
use sha2::{Digest, Sha256};

use super::{MicroCase, RepLoop, metric, single};
use crate::alloc::ThreadAllocs;
use crate::measure::{Better, Gate, cpu_time_thread};
use crate::registry::{CaseResult, FULL_ONLY, Registry, RunCtx};
use crate::rng::Xoshiro256;

const PASSWORD: &str = "correct horse battery staple 42";
const STREAM_BYTES: usize = 256 << 20;
const CHUNK: usize = 256 << 10;
const MESSAGES: usize = 10_000;
const MESSAGE_DATA: usize = 1 << 10;
const QUIC_SEED: u64 = 0x5155_4943_5345_4544; // "QUICSEED"

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stream,
    Framed,
}

pub fn register(reg: &mut Registry) {
    reg.add_case(MicroCase::new("B-MICRO-14", "hash/pbkdf2-sha256", FULL_ONLY, |ctx, r| {
        run_hash(ctx, r, false)
    }));
    reg.add_case(MicroCase::new("B-MICRO-14", "verify/pbkdf2-sha256", FULL_ONLY, |ctx, r| {
        run_hash(ctx, r, true)
    }));
    reg.add_case(MicroCase::new("B-MICRO-16", "quic/stream/256MiB", FULL_ONLY, |ctx, r| {
        run_quic(ctx, r, Mode::Stream)
    }));
    reg.add_case(MicroCase::new("B-MICRO-16", "quic/framed/10000x1KiB", FULL_ONLY, |ctx, r| {
        run_quic(ctx, r, Mode::Framed)
    }));
}

fn run_hash(ctx: &mut RunCtx, mut r: CaseResult, verify: bool) -> Result<CaseResult> {
    let stored = nuvira_secret::hash(PASSWORD);
    anyhow::ensure!(nuvira_secret::verify(PASSWORD, &stored), "the hash does not verify its password");
    anyhow::ensure!(!nuvira_secret::verify("wrong", &stored), "the hash verifies a wrong password");
    let reps = super::sampler(ctx).run(|| {
        if verify {
            std::hint::black_box(nuvira_secret::verify(PASSWORD, &stored));
        } else {
            std::hint::black_box(nuvira_secret::hash(PASSWORD));
        }
    });
    r.reps(&reps);
    r.metric("ms_op", reps.ms_per_iter(Gate::SecurityBothWays));
    r.metric("cpu_ms_op", reps.cpu_ms_per_iter(Gate::Info));
    // `pbkdf2-sha256$<iterations>$<salt>$<hash>`: the parameter itself, recorded.
    if let Some(it) = stored.split('$').nth(1) {
        r.param("kdf_iterations", it);
    }
    r.param("format", stored.split('$').next().unwrap_or(""));
    Ok(r)
}

/// What one side measured during one transfer.
#[derive(Clone, Copy, Debug, Default)]
struct SideRep {
    cpu: Duration,
    allocs: u64,
    alloc_bytes: u64,
    bytes: u64,
    /// Client: start of the transfer; server: end of the stream.
    at: Option<Instant>,
    sha: Option<[u8; 32]>,
}

struct Cmd {
    hash: bool,
    done: tokio::sync::oneshot::Receiver<()>,
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

fn server_side(
    mode: Mode,
    ready: mpsc::Sender<Result<(std::net::SocketAddr, [u8; 32])>>,
    hashes: mpsc::Receiver<bool>,
    out: mpsc::Sender<Result<SideRep>>,
) {
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let result: Result<()> = rt.block_on(async {
        let (ep, fp) = nuvira_transport::server_endpoint("127.0.0.1:0".parse()?)?;
        let _ = ready.send(Ok((ep.local_addr()?, fp)));
        let conn = ep.accept().await.ok_or_else(|| anyhow!("no connection"))?.await?;
        loop {
            let rx = match conn.accept_uni().await {
                Ok(rx) => rx,
                Err(_) => break,
            };
            let hash = hashes.recv().unwrap_or(false);
            let c0 = cpu_time_thread();
            let a0 = ThreadAllocs::take();
            let mut h = Sha256::new();
            let mut bytes = 0u64;
            match mode {
                Mode::Stream => {
                    let mut rx = rx;
                    let mut buf = vec![0u8; CHUNK];
                    while let Some(n) = rx.read(&mut buf).await? {
                        bytes += n as u64;
                        if hash {
                            h.update(&buf[..n]);
                        }
                    }
                }
                Mode::Framed => {
                    let mut rx = FramedRecv::new(rx);
                    while let Some(m) = rx.recv::<FileChunk>().await? {
                        bytes += m.data.len() as u64;
                        if hash {
                            h.update(&m.data);
                        }
                    }
                }
            }
            let at = Instant::now();
            let a = ThreadAllocs::take().since(&a0);
            let rep = SideRep {
                cpu: cpu_time_thread().saturating_sub(c0),
                allocs: a.allocs,
                alloc_bytes: a.bytes,
                bytes,
                at: Some(at),
                sha: hash.then(|| h.finalize().into()),
            };
            if out.send(Ok(rep)).is_err() {
                break;
            }
        }
        ep.wait_idle().await;
        Ok(())
    });
    if let Err(e) = result {
        let _ = out.send(Err(e));
    }
}

fn client_side(
    mode: Mode,
    addr: std::net::SocketAddr,
    fp: [u8; 32],
    data: Arc<Vec<u8>>,
    mut cmds: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::Sender<Result<SideRep>>,
) {
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = out.send(Err(e));
            return;
        }
    };
    let result: Result<()> = rt.block_on(async {
        let conn = nuvira_transport::connect(addr, Some(fp)).await?;
        while let Some(cmd) = cmds.recv().await {
            let _ = cmd.hash;
            let c0 = cpu_time_thread();
            let a0 = ThreadAllocs::take();
            let at = Instant::now();
            let tx = conn.open_uni().await?;
            match mode {
                Mode::Stream => {
                    let mut tx = tx;
                    for chunk in data.chunks(CHUNK) {
                        tx.write_all(chunk).await?;
                    }
                    tx.finish()?;
                    // Keep driving the connection until the agent side has it all.
                    let _ = cmd.done.await;
                    let _ = tx.stopped().await;
                }
                Mode::Framed => {
                    let mut tx = FramedSend::new(tx);
                    for chunk in data.chunks(MESSAGE_DATA).take(MESSAGES) {
                        tx.send(&FileChunk { data: chunk.to_vec(), ..Default::default() }).await?;
                    }
                    tx.finish()?;
                    let _ = cmd.done.await;
                }
            }
            let a = ThreadAllocs::take().since(&a0);
            let rep = SideRep {
                cpu: cpu_time_thread().saturating_sub(c0),
                allocs: a.allocs,
                alloc_bytes: a.bytes,
                bytes: 0,
                at: Some(at),
                sha: None,
            };
            if out.send(Ok(rep)).is_err() {
                break;
            }
        }
        conn.close(0u32.into(), b"done");
        Ok(())
    });
    if let Err(e) = result {
        let _ = out.send(Err(e));
    }
}

fn run_quic(ctx: &mut RunCtx, mut r: CaseResult, mode: Mode) -> Result<CaseResult> {
    let total = match mode {
        Mode::Stream => STREAM_BYTES,
        Mode::Framed => MESSAGES * MESSAGE_DATA,
    };
    let data = Arc::new(Xoshiro256::new(QUIC_SEED).bytes(total));
    let expected: [u8; 32] = Sha256::digest(data.as_slice()).into();

    let (ready_tx, ready_rx) = mpsc::channel();
    let (hash_tx, hash_rx) = mpsc::channel::<bool>();
    let (srv_tx, srv_rx) = mpsc::channel();
    let server = std::thread::Builder::new()
        .name("bench-quic-agent".into())
        .spawn(move || server_side(mode, ready_tx, hash_rx, srv_tx))?;
    let (addr, fp) = ready_rx.recv().context("server thread")??;
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cli_tx, cli_rx) = mpsc::channel();
    let client_data = data.clone();
    let client = std::thread::Builder::new()
        .name("bench-quic-viewer".into())
        .spawn(move || client_side(mode, addr, fp, client_data, cmd_rx, cli_tx))?;

    let transfer = |hash: bool| -> Result<(SideRep, SideRep)> {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        hash_tx.send(hash).map_err(|_| anyhow!("server side gone"))?;
        cmd_tx.send(Cmd { hash, done: done_rx }).map_err(|_| anyhow!("client side gone"))?;
        let s = srv_rx.recv_timeout(Duration::from_secs(120)).context("server side result")??;
        let _ = done_tx.send(());
        let c = cli_rx.recv_timeout(Duration::from_secs(30)).context("client side result")??;
        Ok((c, s))
    };
    let outcome = (|| -> Result<CaseResult> {
        let (_, check) = transfer(true)?;
        let sha_ok = check.sha == Some(expected) && check.bytes == total as u64;
        let mut lp = RepLoop::new(ctx);
        let reps = lp.run(|_, _| transfer(false))?;
        lp.record(&mut r, 1);
        let mb = total as f64 / (1 << 20) as f64;
        let wall: Vec<f64> = reps
            .iter()
            .map(|(c, s)| s.at.zip(c.at).map(|(e, b)| (e - b).as_secs_f64()).unwrap_or(f64::NAN))
            .collect();
        r.metric("mb_s", metric("MB/s", Better::Higher, Gate::Micro, wall.iter().map(|w| mb / w).collect()));
        r.metric(
            "transfer_ms",
            metric("ms", Better::Lower, Gate::Micro, wall.iter().map(|w| w * 1e3).collect()),
        );
        let side = |f: &dyn Fn(&(SideRep, SideRep)) -> f64| reps.iter().map(f).collect::<Vec<_>>();
        r.metric(
            "cpu_ms_mb_viewer",
            metric("ms", Better::Lower, Gate::Micro, side(&|(c, _)| c.cpu.as_secs_f64() * 1e3 / mb)),
        );
        r.metric(
            "cpu_ms_mb_agent",
            metric("ms", Better::Lower, Gate::Micro, side(&|(_, s)| s.cpu.as_secs_f64() * 1e3 / mb)),
        );
        r.metric(
            "allocs_mb_viewer",
            metric("1", Better::Lower, Gate::Micro, side(&|(c, _)| c.allocs as f64 / mb)),
        );
        r.metric(
            "allocs_mb_agent",
            metric("1", Better::Lower, Gate::Micro, side(&|(_, s)| s.allocs as f64 / mb)),
        );
        r.metric(
            "alloc_bytes_mb_viewer",
            metric("B", Better::Lower, Gate::Micro, side(&|(c, _)| c.alloc_bytes as f64 / mb)),
        );
        r.metric(
            "alloc_bytes_mb_agent",
            metric("B", Better::Lower, Gate::Micro, side(&|(_, s)| s.alloc_bytes as f64 / mb)),
        );
        r.metric(
            "received_sha256_matches",
            single("1", Better::Higher, Gate::Exact, f64::from(u8::from(sha_ok))),
        );
        r.checksum("data", crate::fixtures::hex(&expected));
        r.param("bytes", total);
        match mode {
            Mode::Stream => r.param("write_bytes", CHUNK),
            Mode::Framed => r.param("messages", MESSAGES).param("message_data_bytes", MESSAGE_DATA),
        };
        if mode == Mode::Framed {
            let per_msg: Vec<f64> = wall.iter().map(|w| w * 1e9 / MESSAGES as f64).collect();
            r.metric("ns_msg", metric("ns", Better::Lower, Gate::Micro, per_msg));
        }
        anyhow::ensure!(sha_ok, "received data differs from what was sent");
        Ok(r)
    })();
    drop(cmd_tx);
    drop(hash_tx);
    let _ = client.join();
    let _ = server.join();
    outcome
}
