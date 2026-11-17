//! B-MICRO-15 Opus encode / decode (`nuvira_audio::opus`, 48 kHz stereo, 20 ms
//! frames) on 10 s of the seeded multi-tone + noise fixture.
//!
//! Encode and decode cycle over the 500 frames (codec state carries on, as in a
//! session). The decode-with-loss case drops a seeded 5% of the packets
//! (`decode(None, ..)`, libopus concealment). Quality: SNR of a full decode of the
//! sequence against the source, time-aligned by the codec delay found by
//! cross-correlation (searched once, recorded).

use anyhow::Result;
use nuvira_audio::opus::{Decoder, Encoder};

use super::{MicroCase, metric, record_sampler, single};
use crate::fixtures::audio::{CHANNELS, FRAME_SAMPLES, pcm_stereo};
use crate::measure::{Better, Gate};
use crate::registry::{CaseResult, FULL_ONLY, Registry, RunCtx};
use crate::rng::Xoshiro256;

const SECONDS: u32 = 10;
const LOSS: f64 = 0.05;
const LOSS_SEED: u64 = 0x4f50_5553_4c4f_5353; // "OPUSLOSS"
/// Largest codec delay searched, in samples per channel.
const MAX_DELAY: usize = 2000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Encode,
    Decode,
    DecodeLoss,
}

pub fn register(reg: &mut Registry) {
    for (name, op) in [("encode", Op::Encode), ("decode", Op::Decode), ("decode_loss5", Op::DecodeLoss)] {
        reg.add_case(MicroCase::new("B-MICRO-15", format!("opus/{name}"), FULL_ONLY, move |ctx, r| {
            run(ctx, r, op)
        }));
    }
}

fn frames(pcm: &[f32]) -> Vec<&[f32]> {
    pcm.as_chunks::<{ FRAME_SAMPLES * CHANNELS }>().0.iter().map(|f| f.as_slice()).collect()
}

fn encode_all(pcm: &[f32]) -> Result<Vec<Vec<u8>>> {
    let mut enc = Encoder::new()?;
    frames(pcm).into_iter().map(|f| enc.encode(f)).collect()
}

fn loss_mask(n: usize) -> Vec<bool> {
    let mut rng = Xoshiro256::new(LOSS_SEED);
    (0..n).map(|_| rng.chance(LOSS)).collect()
}

fn decode_all(packets: &[Vec<u8>], lost: Option<&[bool]>) -> Result<Vec<f32>> {
    let mut dec = Decoder::new()?;
    let mut out = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        let gone = lost.is_some_and(|l| l[i]);
        out.extend(dec.decode(if gone { None } else { Some(p) }, false)?);
    }
    Ok(out)
}

/// Delay (samples per channel) maximising the correlation of the left channels.
fn find_delay(src: &[f32], out: &[f32]) -> usize {
    let n = (src.len() / CHANNELS).min(out.len() / CHANNELS).saturating_sub(MAX_DELAY).min(48_000);
    let mut best = (0usize, f64::NEG_INFINITY);
    for d in 0..MAX_DELAY {
        let mut c = 0f64;
        for i in (0..n).step_by(4) {
            c += src[i * CHANNELS] as f64 * out[(i + d) * CHANNELS] as f64;
        }
        if c > best.1 {
            best = (d, c);
        }
    }
    best.0
}

fn snr_db(src: &[f32], out: &[f32], delay: usize) -> f64 {
    let n = (src.len() / CHANNELS).min(out.len() / CHANNELS - delay.min(out.len() / CHANNELS));
    let (mut s, mut e) = (0f64, 0f64);
    for i in 0..n * CHANNELS {
        let a = src[i] as f64;
        let b = out[i + delay * CHANNELS] as f64;
        s += a * a;
        e += (a - b) * (a - b);
    }
    10.0 * (s / e.max(1e-30)).log10()
}

fn run(ctx: &mut RunCtx, mut r: CaseResult, op: Op) -> Result<CaseResult> {
    let pcm = pcm_stereo(SECONDS);
    let input = frames(&pcm);
    let packets = encode_all(&pcm)?;
    let lost = loss_mask(packets.len());
    let mut err = None;
    let mut k = 0usize;
    let reps = match op {
        Op::Encode => {
            let mut enc = Encoder::new()?;
            super::sampler(ctx).run(|| {
                match enc.encode(input[k % input.len()]) {
                    Ok(p) => {
                        std::hint::black_box(p);
                    }
                    Err(e) => err = Some(e),
                }
                k += 1;
            })
        }
        Op::Decode | Op::DecodeLoss => {
            let mut dec = Decoder::new()?;
            super::sampler(ctx).run(|| {
                let i = k % packets.len();
                let gone = op == Op::DecodeLoss && lost[i];
                match dec.decode(if gone { None } else { Some(&packets[i]) }, false) {
                    Ok(p) => {
                        std::hint::black_box(p);
                    }
                    Err(e) => err = Some(e),
                }
                k += 1;
            })
        }
    };
    if let Some(e) = err {
        return Err(e);
    }
    record_sampler(&mut r, &reps, "ns_frame", true, Gate::Micro);
    let us: Vec<f64> = r.metrics["ns_frame"].values.iter().map(|v| v / 1e3).collect();
    r.metric("us_frame", metric("us", Better::Lower, Gate::Info, us));
    let bytes: usize = packets.iter().map(|p| p.len()).sum();
    r.metric("bytes_frame", single("B", Better::Lower, Gate::Info, bytes as f64 / packets.len() as f64));
    let clean = decode_all(&packets, None)?;
    let delay = find_delay(&pcm, &clean);
    r.param("codec_delay_samples", delay);
    match op {
        Op::Encode => {
            r.metric("snr_db", single("dB", Better::Higher, Gate::Quality, snr_db(&pcm, &clean, delay)));
        }
        Op::Decode => {
            r.metric("snr_db", single("dB", Better::Higher, Gate::Quality, snr_db(&pcm, &clean, delay)));
        }
        Op::DecodeLoss => {
            let concealed = decode_all(&packets, Some(&lost))?;
            r.metric("snr_db", single("dB", Better::Higher, Gate::Info, snr_db(&pcm, &concealed, delay)));
            r.param("packets_lost", lost.iter().filter(|l| **l).count());
        }
    }
    r.param("frames", packets.len()).param("seconds", SECONDS).param("frame_samples", FRAME_SAMPLES);
    r.checksum("packets", super::sha_hex(&packets.concat()));
    Ok(r)
}
