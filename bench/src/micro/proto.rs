//! B-MICRO-11 message framing (`nuvira_proto::framing`) and the datagram path
//! (`prost::Message::encode_to_vec` / `decode`) on the protocol samples (plan 5.3).
//!
//! Per sample: `encode/<sample>` (framing::encode, with the wire check: the SHA-256
//! of the encoding must equal the manifest's, gate `exact`), `decode/<sample>`
//! (Decoder::push of the whole message + next_message), `decode_1200/<sample>` and
//! `decode_64k/<sample>` (the bytes pushed in QUIC-sized pieces, for messages longer
//! than the piece), and for video frames `raw_encode/` and `raw_decode/` (datagram
//! payloads). Allocations are the calling thread's, exact.

use anyhow::Result;
use nuvira_proto::framing;
use prost::Message;

use super::{MicroCase, record_sampler, sha_hex, single};
use crate::fixtures::proto_samples::{self, Sample};
use crate::measure::{Better, Gate};
use crate::registry::{BOTH_TIERS, CaseResult, Registry, RunCtx};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Encode,
    Decode(usize),
    RawEncode,
    RawDecode,
}

pub fn register(reg: &mut Registry) {
    for (name, sample) in proto_samples::samples() {
        let len = sample.encode().len();
        let mut ops = vec![("encode", Op::Encode), ("decode", Op::Decode(usize::MAX))];
        if len > 1200 {
            ops.push(("decode_1200", Op::Decode(1200)));
        }
        if len > 64 << 10 {
            ops.push(("decode_64k", Op::Decode(64 << 10)));
        }
        if matches!(sample, Sample::Video(_)) {
            ops.push(("raw_encode", Op::RawEncode));
            ops.push(("raw_decode", Op::RawDecode));
        }
        for (op_name, op) in ops {
            reg.add_case(MicroCase::new(
                "B-MICRO-11",
                format!("{op_name}/{name}"),
                BOTH_TIERS,
                move |ctx, r| run(ctx, r, name, op),
            ));
        }
    }
}

/// Pushes `bytes` into a framing decoder in `piece`-byte parts and takes the message.
fn decode_framed<M: Message + Default>(bytes: &[u8], piece: usize) -> Option<M> {
    let mut d = framing::Decoder::default();
    let mut got = None;
    for part in bytes.chunks(piece.max(1)) {
        d.push(part);
        if let Ok(Some(m)) = d.next_message::<M>() {
            got = Some(m);
        }
    }
    got
}

fn decode_sample(sample: &Sample, bytes: &[u8], piece: usize) -> bool {
    match sample {
        Sample::Video(_) => {
            decode_framed::<nuvira_proto::v1::VideoFrame>(bytes, piece).map(std::hint::black_box).is_some()
        }
        Sample::Input(_) => {
            decode_framed::<nuvira_proto::v1::InputEvent>(bytes, piece).map(std::hint::black_box).is_some()
        }
        Sample::Control(_) => decode_framed::<nuvira_proto::v1::ControlMessage>(bytes, piece)
            .map(std::hint::black_box)
            .is_some(),
        Sample::Chunk(_) => {
            decode_framed::<nuvira_proto::v1::FileChunk>(bytes, piece).map(std::hint::black_box).is_some()
        }
    }
}

fn run(ctx: &mut RunCtx, mut r: CaseResult, name: &'static str, op: Op) -> Result<CaseResult> {
    let sample = proto_samples::samples()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| s)
        .ok_or_else(|| anyhow::anyhow!("no protocol sample {name}"))?;
    let framed = sample.encode();
    let raw = sample.encode_raw();
    let mut ok = true;
    let reps = match op {
        Op::Encode => super::sampler(ctx).run(|| {
            std::hint::black_box(match &sample {
                Sample::Video(m) => framing::encode(m),
                Sample::Input(m) => framing::encode(m),
                Sample::Control(m) => framing::encode(m),
                Sample::Chunk(m) => framing::encode(m),
            });
        }),
        Op::Decode(piece) => super::sampler(ctx).run(|| ok &= decode_sample(&sample, &framed, piece)),
        Op::RawEncode => super::sampler(ctx).run(|| {
            std::hint::black_box(sample.encode_raw());
        }),
        Op::RawDecode => super::sampler(ctx).run(|| {
            let m = nuvira_proto::v1::VideoFrame::decode(raw.as_slice());
            ok &= m.is_ok();
            std::hint::black_box(m.ok());
        }),
    };
    anyhow::ensure!(ok, "{name}: a decode failed");
    record_sampler(&mut r, &reps, "ns_msg", true, Gate::Micro);
    let bytes = if matches!(op, Op::RawEncode | Op::RawDecode) { &raw } else { &framed };
    r.param("message_bytes", bytes.len());
    if let Op::Decode(piece) = op
        && piece != usize::MAX
    {
        r.param("piece_bytes", piece);
    }
    let mbs = |ns: f64| bytes.len() as f64 / ns * 1e3;
    if let Some(m) = r.metrics.get("ns_msg").cloned() {
        r.metric(
            "mb_s",
            super::metric("MB/s", Better::Higher, Gate::Info, m.values.iter().map(|&v| mbs(v)).collect()),
        );
    }
    // Wire check: the decoded message must also re-encode to the same bytes.
    let back = sample.decode_same(&framed)?;
    anyhow::ensure!(back == sample, "{name}: decoding does not give the same message");
    let sha = sha_hex(&framed);
    r.checksum("encoding", sha.as_str());
    if op == Op::Encode {
        let manifest = crate::fixtures::Manifest::embedded()?;
        let expected = manifest.blob_sha256(name).map(String::from);
        let matches = expected.as_deref() == Some(sha.as_str());
        r.metric(
            "wire_matches_manifest",
            single("1", Better::Higher, Gate::Exact, f64::from(u8::from(matches))),
        );
        if !matches {
            r.note(format!(
                "wire encoding of {name} differs from the manifest ({sha} vs {}): compatibility break",
                expected.as_deref().unwrap_or("missing")
            ));
        }
    }
    Ok(r)
}
