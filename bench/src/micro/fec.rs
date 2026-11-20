//! B-MICRO-12 FEC encode, B-MICRO-13 FEC receive and rebuild
//! (`nuvira_transport::fec`, datagrams of 1,200 bytes as the agent sends them).
//!
//! 12: one `fec::Encoder` kept over the iterations, as the agent's video sender keeps
//! one for its session (review NET-9: before, `fec::encode` built its Reed-Solomon
//! codecs for every block); the `shards` checksum is the same as `fec::encode`'s.
//!
//! 13: a sequence of [`SEQ_FRAMES`] frames of 12 data shards each, ratio 0.25 (3
//! parity shards), with a seeded loss pattern (random 0/1/5/10%, or Gilbert-Elliott
//! bursts as profile N5: 5% loss, mean burst 4) applied once; every iteration feeds
//! the surviving shards, in order, to a new `Receiver`. Outside the timing the
//! rebuilt frames are compared byte by byte with the input (gate `exact`) and the
//! fraction of frames with lost shards that parity recovered is recorded (gate
//! `quality`).

use anyhow::Result;
use nuvira_transport::fec;

use super::{MicroCase, metric, record_sampler, single};
use crate::measure::{Better, Gate};
use crate::registry::{BOTH_TIERS, CaseResult, Registry, RunCtx};
use crate::rng::Xoshiro256;

const MAX_DATAGRAM: usize = 1200;
const SEQ_FRAMES: usize = 64;
const FEC_SEED: u64 = 0x4645_4353_4545_4431; // "FECSEED1"

const SIZES: [(&str, usize); 4] = [("2k", 2 << 10), ("50k", 50 << 10), ("500k", 500 << 10), ("2m", 2 << 20)];
const RATIOS: [f64; 3] = [0.1, 0.25, 0.5];

#[derive(Clone, Copy)]
enum Loss {
    Random(f64),
    /// Gilbert-Elliott: stationary loss and mean burst length (in shards).
    Bursts(f64, f64),
}

pub fn register(reg: &mut Registry) {
    for (label, size) in SIZES {
        for ratio in RATIOS {
            reg.add_case(MicroCase::new(
                "B-MICRO-12",
                format!("encode/{label}/r{ratio}"),
                BOTH_TIERS,
                move |ctx, r| run_encode(ctx, r, size, ratio),
            ));
        }
    }
    for (label, loss) in [
        ("loss0", Loss::Random(0.0)),
        ("loss1", Loss::Random(0.01)),
        ("loss5", Loss::Random(0.05)),
        ("loss10", Loss::Random(0.10)),
        ("n5_bursts", Loss::Bursts(0.05, 4.0)),
    ] {
        reg.add_case(MicroCase::new(
            "B-MICRO-13",
            format!("receive/12shards/{label}"),
            BOTH_TIERS,
            move |ctx, r| run_receive(ctx, r, loss),
        ));
    }
}

fn frame_data(seed: u64, len: usize) -> Vec<u8> {
    Xoshiro256::new(FEC_SEED ^ seed).bytes(len)
}

fn run_encode(ctx: &mut RunCtx, mut r: CaseResult, size: usize, ratio: f64) -> Result<CaseResult> {
    let frame = frame_data(size as u64, size);
    let mut seq = 0u64;
    let mut err = None;
    let mut encoder = fec::Encoder::default();
    let reps = super::sampler(ctx).run(|| {
        seq += 1;
        match encoder.encode(seq, &frame, MAX_DATAGRAM, ratio) {
            Ok(v) => {
                std::hint::black_box(v);
            }
            Err(e) => err = Some(e),
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    record_sampler(&mut r, &reps, "ns_frame", true, Gate::Micro);
    let ns = r.metrics["ns_frame"].values.clone();
    r.metric(
        "mb_s",
        metric("MB/s", Better::Higher, Gate::Micro, ns.iter().map(|v| size as f64 / v * 1e3).collect()),
    );
    let shards = fec::encode(1, &frame, MAX_DATAGRAM, ratio)?;
    let wire: usize = shards.iter().map(|s| s.len()).sum();
    r.metric("shards", single("1", Better::Lower, Gate::Info, shards.len() as f64));
    r.metric("wire_bytes", single("B", Better::Lower, Gate::Info, wire as f64));
    r.metric("overhead", single("1", Better::Lower, Gate::Info, wire as f64 / size as f64 - 1.0));
    r.param("frame_bytes", size).param("ratio", ratio).param("max_datagram", MAX_DATAGRAM);
    r.checksum("shards", super::sha_hex(&shards.concat()));
    Ok(r)
}

/// Shard length of 12 data shards in a datagram.
fn twelve_shard_frame() -> usize {
    12 * (MAX_DATAGRAM - fec::HEADER)
}

/// Which shards (in send order) are lost.
fn loss_mask(n: usize, loss: Loss, rng: &mut Xoshiro256) -> Vec<bool> {
    match loss {
        Loss::Random(p) => (0..n).map(|_| rng.chance(p)).collect(),
        Loss::Bursts(p, burst) => {
            // Bad state: every shard lost, left with probability 1/burst; good state
            // entered with q such that the stationary bad share is p.
            let leave = 1.0 / burst;
            let enter = p * leave / (1.0 - p);
            let mut bad = false;
            (0..n)
                .map(|_| {
                    bad = if bad { !rng.chance(leave) } else { rng.chance(enter) };
                    bad
                })
                .collect()
        }
    }
}

fn run_receive(ctx: &mut RunCtx, mut r: CaseResult, loss: Loss) -> Result<CaseResult> {
    let ratio = 0.25;
    let len = twelve_shard_frame();
    let frames: Vec<Vec<u8>> = (0..SEQ_FRAMES).map(|i| frame_data(1000 + i as u64, len)).collect();
    let mut rng = Xoshiro256::new(ctx.rng_seed ^ FEC_SEED);
    let mut packets: Vec<Vec<u8>> = Vec::new();
    // Per frame: (shards sent, shards lost, data shards lost).
    let mut per_frame: Vec<(usize, usize, usize)> = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let shards = fec::encode(i as u64 + 1, f, MAX_DATAGRAM, ratio)?;
        let mask = loss_mask(shards.len(), loss, &mut rng);
        let data_shards = f.len().div_ceil(MAX_DATAGRAM - fec::HEADER);
        let mut lost = (0, 0);
        for (k, (s, l)) in shards.into_iter().zip(&mask).enumerate() {
            if *l {
                lost.0 += 1;
                if k < data_shards {
                    lost.1 += 1;
                }
            } else {
                packets.push(s);
            }
        }
        per_frame.push((mask.len(), lost.0, lost.1));
    }
    let mut err = None;
    let reps = super::sampler(ctx).run(|| {
        let mut rx = fec::Receiver::default();
        let mut done = 0usize;
        for p in &packets {
            match rx.push(p) {
                Ok(Some(f)) => {
                    done += 1;
                    std::hint::black_box(f);
                }
                Ok(None) => {}
                Err(e) => err = Some(e),
            }
        }
        std::hint::black_box((done, rx.take_loss()));
    });
    if let Some(e) = err {
        return Err(e);
    }
    record_sampler(&mut r, &reps, "ns_sequence", true, Gate::Micro);
    let per_seq = r.metrics["ns_sequence"].values.clone();
    r.metric(
        "ns_frame",
        metric("ns", Better::Lower, Gate::Micro, per_seq.iter().map(|v| v / SEQ_FRAMES as f64).collect()),
    );
    // Correctness and recovery, once.
    let mut rx = fec::Receiver::default();
    let mut rebuilt = vec![None; SEQ_FRAMES];
    for p in &packets {
        if let Some((seq, f)) = rx.push(p)? {
            rebuilt[(seq - 1) as usize] = Some(f);
        }
    }
    let (expected, lost_counted) = rx.take_loss();
    let identical = rebuilt.iter().zip(&frames).all(|(b, f)| b.as_ref().is_none_or(|b| b == f));
    let complete = rebuilt.iter().filter(|b| b.is_some()).count();
    let damaged: Vec<usize> = (0..SEQ_FRAMES).filter(|&i| per_frame[i].1 > 0).collect();
    let recovered = damaged.iter().filter(|&&i| rebuilt[i].is_some()).count();
    let by_parity = (0..SEQ_FRAMES).filter(|&i| per_frame[i].2 > 0 && rebuilt[i].is_some()).count();
    let shards_lost: usize = per_frame.iter().map(|p| p.1).sum();
    let shards_sent: usize = per_frame.iter().map(|p| p.0).sum();
    r.metric("frames_identical", single("1", Better::Higher, Gate::Exact, f64::from(u8::from(identical))));
    r.metric(
        "recovered_fraction",
        single(
            "1",
            Better::Higher,
            Gate::Quality,
            if damaged.is_empty() { 1.0 } else { recovered as f64 / damaged.len() as f64 },
        ),
    );
    r.metric("frames_rebuilt_by_parity", single("1", Better::Higher, Gate::Info, by_parity as f64));
    r.metric("frames_lost", single("1", Better::Lower, Gate::Info, (SEQ_FRAMES - complete) as f64));
    r.metric("shards_lost", single("1", Better::Lower, Gate::Info, shards_lost as f64));
    r.metric("receiver_shards_expected", single("1", Better::Lower, Gate::Info, expected as f64));
    r.metric("receiver_shards_lost", single("1", Better::Lower, Gate::Info, lost_counted as f64));
    r.param("frames", SEQ_FRAMES)
        .param("frame_bytes", len)
        .param("ratio", ratio)
        .param("shards_sent", shards_sent)
        .param("max_datagram", MAX_DATAGRAM);
    anyhow::ensure!(identical, "a rebuilt frame differs from its input");
    Ok(r)
}
