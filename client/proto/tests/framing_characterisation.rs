//! Characterisation tests (Phase 2 of the full review) of the stream framing:
//! `framing::encode` and `framing::Decoder`. They pin the CURRENT behaviour,
//! including the edge cases a peer can send, before any refactoring.

use nuvira_proto::framing::{Decoder, FrameError, MAX_MESSAGE_LEN, encode};
use nuvira_proto::v1::{ControlMessage, FileChunk, Ping, VideoFrame};
use proptest::prelude::*;
use prost::Message;

fn varint(n: u64) -> Vec<u8> {
    let mut v = Vec::new();
    prost::encoding::encode_varint(n, &mut v);
    v
}

/// Drains the decoder: messages until `Ok(None)`, or the first error.
fn drain<M: Message + Default>(dec: &mut Decoder) -> (Vec<M>, Option<FrameError>) {
    let mut out = Vec::new();
    loop {
        match dec.next_message::<M>() {
            Ok(Some(m)) => out.push(m),
            Ok(None) => return (out, None),
            Err(e) => return (out, Some(e)),
        }
    }
}

#[test]
fn empty_decoder_has_nothing() {
    let mut dec = Decoder::default();
    assert!(matches!(dec.next_message::<Ping>(), Ok(None)));
}

#[test]
fn zero_length_frame_is_the_default_message() {
    let mut dec = Decoder::default();
    dec.push(&[0]);
    assert_eq!(dec.next_message::<Ping>().unwrap(), Some(Ping::default()));
    assert!(dec.next_message::<Ping>().unwrap().is_none());
    // encode() of a default message is exactly that one byte.
    assert_eq!(encode(&Ping::default()), vec![0]);
}

#[test]
fn incomplete_varint_below_ten_bytes_waits() {
    // 1..=9 continuation bytes: not enough to know the length, more bytes awaited.
    for n in 1..=9 {
        let mut dec = Decoder::default();
        dec.push(&vec![0x80; n]);
        assert!(matches!(dec.next_message::<Ping>(), Ok(None)), "{n} bytes");
    }
}

#[test]
fn ten_continuation_bytes_are_an_error() {
    let mut dec = Decoder::default();
    dec.push(&[0x80; 10]);
    assert!(matches!(dec.next_message::<Ping>(), Err(FrameError::Decode(_))));
    // The error is sticky: the bytes are not consumed, the same error comes again.
    assert!(matches!(dec.next_message::<Ping>(), Err(FrameError::Decode(_))));
}

#[test]
fn varint_with_a_tenth_byte_above_one_is_an_error() {
    // A 64-bit varint can only have 0 or 1 in its 10th byte.
    let mut wire = vec![0xff; 9];
    wire.push(0x02);
    let mut dec = Decoder::default();
    dec.push(&wire);
    assert!(matches!(dec.next_message::<Ping>(), Err(FrameError::Decode(_))));
}

#[test]
fn overlong_varint_for_a_small_length_is_accepted() {
    // Length 2 written in 3 bytes (0x82 0x80 0x00): accepted (prost accepts it too).
    let body = Ping { sent_us: 5 }.encode_to_vec();
    assert_eq!(body.len(), 2);
    let mut wire = vec![0x82, 0x80, 0x00];
    wire.extend(&body);
    let mut dec = Decoder::default();
    dec.push(&wire);
    assert_eq!(dec.next_message::<Ping>().unwrap(), Some(Ping { sent_us: 5 }));
}

#[test]
fn length_exactly_at_the_limit_waits_for_the_body() {
    let mut dec = Decoder::default();
    dec.push(&varint(MAX_MESSAGE_LEN as u64));
    // Accepted: the decoder waits for 16 MiB of body.
    assert!(matches!(dec.next_message::<VideoFrame>(), Ok(None)));
    dec.push(&vec![0u8; 1000]);
    assert!(matches!(dec.next_message::<VideoFrame>(), Ok(None)));
}

#[test]
fn length_above_the_limit_is_refused_at_once_and_sticky() {
    for len in [MAX_MESSAGE_LEN as u64 + 1, u32::MAX as u64, u64::MAX] {
        let mut dec = Decoder::default();
        dec.push(&varint(len));
        match dec.next_message::<Ping>() {
            // On 64-bit targets `len as usize` keeps the value.
            Err(FrameError::TooLarge(n)) => assert_eq!(n as u64, len),
            other => panic!("{len}: {:?}", other.map(|_| ())),
        }
        // Not consumed: every later call returns the same error.
        dec.push(&encode(&Ping { sent_us: 1 }));
        assert!(matches!(dec.next_message::<Ping>(), Err(FrameError::TooLarge(_))));
    }
}

#[test]
fn a_smaller_limit_can_be_set_per_stream() {
    // Was KNOWN LEAD K7 (Phase 2): every stream had the same 16 MiB limit, with no
    // way to ask for less (review NET-3). The default stays 16 MiB, for any type.
    let mut dec = Decoder::default();
    dec.push(&varint(MAX_MESSAGE_LEN as u64));
    assert!(matches!(dec.next_message::<ControlMessage>(), Ok(None)));
    // A decoder with its own limit refuses above it, from the length alone.
    const LIMIT: usize = 64 * 1024;
    let mut dec = Decoder::with_limit(LIMIT);
    dec.push(&varint(LIMIT as u64 + 1));
    assert!(matches!(dec.next_message::<ControlMessage>(), Err(FrameError::TooLarge(n)) if n == LIMIT + 1));
    // Exactly the limit: waits for the body.
    let mut dec = Decoder::with_limit(LIMIT);
    dec.push(&varint(LIMIT as u64));
    assert!(matches!(dec.next_message::<ControlMessage>(), Ok(None)));
    // Raised later (after authentication): the waiting length is accepted then.
    let mut dec = Decoder::with_limit(LIMIT);
    let big = encode(&FileChunk { data: vec![7; LIMIT], ..Default::default() });
    dec.push(&big);
    assert!(matches!(dec.next_message::<FileChunk>(), Err(FrameError::TooLarge(_))));
    dec.set_limit(MAX_MESSAGE_LEN);
    assert_eq!(dec.next_message::<FileChunk>().unwrap().map(|c| c.data.len()), Some(LIMIT));
}

#[test]
fn a_body_that_does_not_decode_is_consumed() {
    // A frame with an invalid body (field 1 with wire type 7), then a valid one.
    let mut wire = vec![1u8, 0x0f];
    wire.extend(encode(&Ping { sent_us: 9 }));
    let mut dec = Decoder::default();
    dec.push(&wire);
    assert!(matches!(dec.next_message::<Ping>(), Err(FrameError::Decode(_))));
    // The bad frame is gone: the next one decodes.
    assert_eq!(dec.next_message::<Ping>().unwrap(), Some(Ping { sent_us: 9 }));
}

#[test]
fn a_frame_of_another_type_decodes_as_whatever_matches() {
    // Protobuf has no type tag: a VideoFrame read as a Ping keeps the fields that
    // fit (field 1 = seq = sent_us) and skips the others.
    let frame = VideoFrame { seq: 77, data: vec![1, 2, 3], keyframe: true, ..Default::default() };
    let mut dec = Decoder::default();
    dec.push(&encode(&frame));
    assert_eq!(dec.next_message::<Ping>().unwrap(), Some(Ping { sent_us: 77 }));
}

#[test]
fn display_of_errors() {
    assert_eq!(FrameError::TooLarge(5).to_string(), "message too large: 5 bytes");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Any sequence of messages, cut anywhere, comes out whole and in order.
    #[test]
    fn round_trip_with_arbitrary_cuts(
        frames in prop::collection::vec(
            (any::<u64>(), prop::collection::vec(any::<u8>(), 0..3000), any::<bool>()), 1..8),
        cuts in prop::collection::vec(1usize..700, 1..40),
    ) {
        let msgs: Vec<VideoFrame> = frames
            .into_iter()
            .map(|(seq, data, keyframe)| VideoFrame { seq, data, keyframe, ..Default::default() })
            .collect();
        let wire: Vec<u8> = msgs.iter().flat_map(encode).collect();
        let mut dec = Decoder::default();
        let mut got = Vec::new();
        let (mut at, mut i) = (0, 0);
        while at < wire.len() {
            let n = cuts[i % cuts.len()].min(wire.len() - at);
            dec.push(&wire[at..at + n]);
            at += n;
            i += 1;
            let (mut some, err) = drain::<VideoFrame>(&mut dec);
            prop_assert!(err.is_none());
            got.append(&mut some);
        }
        prop_assert_eq!(got, msgs);
    }

    /// Arbitrary bytes never panic, and draining always ends (no endless loop).
    #[test]
    fn arbitrary_bytes_never_panic(chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..16)) {
        let mut dec = Decoder::default();
        for chunk in &chunks {
            dec.push(chunk);
            let _ = drain::<ControlMessage>(&mut dec);
        }
    }

    /// Same as the decoder of a single length-delimited message in prost.
    #[test]
    fn same_as_prost_length_delimited(sent_us in any::<u64>()) {
        let msg = Ping { sent_us };
        prop_assert_eq!(encode(&msg), msg.encode_length_delimited_to_vec());
        let mut dec = Decoder::default();
        dec.push(&encode(&msg));
        prop_assert_eq!(dec.next_message::<Ping>().unwrap(), Some(msg));
    }

    /// A file chunk with arbitrary bytes survives the framing.
    #[test]
    fn file_chunks(name in ".{0,40}", data in prop::collection::vec(any::<u8>(), 0..5000), size in any::<u64>()) {
        let chunk = FileChunk { name, data, size, ..Default::default() };
        let mut dec = Decoder::default();
        dec.push(&encode(&chunk));
        prop_assert_eq!(dec.next_message::<FileChunk>().unwrap(), Some(chunk));
    }
}
