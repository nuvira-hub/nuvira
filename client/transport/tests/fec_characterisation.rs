//! Characterisation tests (Phase 2 of the full review) of the video datagrams with
//! forward error correction (`nuvira_transport::fec`): `encode` and
//! `Receiver::push`. They pin the CURRENT behaviour, edge cases included.

use std::collections::BTreeSet;

use nuvira_transport::fec::{HEADER, Receiver, encode};
use proptest::prelude::*;

fn frame(len: usize, salt: u8) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8 ^ salt).collect()
}

/// A shard built by hand: the header of `fec.rs`, then `payload`.
#[allow(clippy::too_many_arguments)]
fn shard(
    seq: u64,
    block: u8,
    blocks: u8,
    index: u8,
    data: u8,
    parity: u8,
    frame_len: u32,
    shard_len: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut p = vec![1u8];
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(&[block, blocks, index, data, parity]);
    p.extend_from_slice(&frame_len.to_le_bytes());
    p.extend_from_slice(&shard_len.to_le_bytes());
    p.extend_from_slice(payload);
    p
}

/// Header fields of an encoded packet: (block, blocks, index, data, parity).
fn fields(p: &[u8]) -> (usize, usize, usize, usize, usize) {
    (p[9] as usize, p[10] as usize, p[11] as usize, p[12] as usize, p[13] as usize)
}

fn push_all<'a>(rx: &mut Receiver, packets: impl IntoIterator<Item = &'a Vec<u8>>) -> Vec<(u64, Vec<u8>)> {
    packets.into_iter().filter_map(|p| rx.push(p).ok().flatten()).collect()
}

#[test]
fn header_layout() {
    let packets = encode(0x0102_0304_0506_0708, &[9; 10], 1200, 0.0).unwrap();
    // 10 bytes: one data shard of 10 bytes and one parity (the minimum without losses).
    assert_eq!(packets.len(), 2);
    let p = &packets[0];
    assert_eq!(p.len(), HEADER + 10);
    assert_eq!(p[0], 1, "version");
    assert_eq!(u64::from_le_bytes(p[1..9].try_into().unwrap()), 0x0102_0304_0506_0708);
    assert_eq!(fields(p), (0, 1, 0, 1, 1));
    assert_eq!(u32::from_le_bytes(p[14..18].try_into().unwrap()), 10);
    assert_eq!(u16::from_le_bytes(p[18..20].try_into().unwrap()), 10);
}

#[test]
fn parity_per_block() {
    let parity = |len: usize, ratio: f64| fields(&encode(1, &frame(len, 0), 1200, ratio).unwrap()[0]).4;
    // Below 0.12 (no losses measured) at least one parity shard, from 0.12 at least two.
    assert_eq!(parity(100, 0.0), 1);
    assert_eq!(parity(100, 0.11), 1);
    assert_eq!(parity(100, 0.12), 2);
    // 32 data shards (one full block) at 0.5: 16.
    assert_eq!(parity(32 * 1180, 0.5), 16);
    // A ratio above 1 is capped so that data + parity <= 255.
    assert_eq!(parity(32 * 1180, 100.0), 255 - 32);
}

#[test]
fn encode_limits() {
    // Datagrams must be larger than HEADER + 16.
    assert!(encode(1, &[1], HEADER + 16, 0.2).is_err());
    assert!(encode(1, &[1], HEADER + 17, 0.2).is_ok());
    // At most 255 blocks of 32 shards: with 17 bytes of payload, 255 * 32 * 17 bytes.
    let max = 255 * 32 * 17;
    assert!(encode(1, &vec![0; max], HEADER + 17, 0.0).is_ok());
    assert!(encode(1, &vec![0; max + 1], HEADER + 17, 0.0).is_err());
    // An empty frame is one shard of one (padding) byte.
    let empty = encode(1, &[], 1200, 0.0).unwrap();
    assert_eq!(empty.len(), 2);
    assert_eq!(empty[0].len(), HEADER + 1);
    let mut rx = Receiver::default();
    assert_eq!(push_all(&mut rx, &empty), vec![(1, vec![])]);
}

#[test]
fn malformed_shards_are_errors() {
    let good = shard(1, 0, 1, 0, 1, 1, 4, 4, &[1, 2, 3, 4]);
    let mut rx = Receiver::default();
    assert!(rx.push(&good[..HEADER]).is_err(), "header only");
    assert!(rx.push(&[]).is_err());
    let mut v2 = good.clone();
    v2[0] = 2;
    assert!(rx.push(&v2).is_err(), "other version");
    for bad in [
        shard(1, 1, 1, 0, 1, 1, 4, 4, &[0; 4]), // block >= blocks
        shard(1, 0, 0, 0, 1, 1, 4, 4, &[0; 4]), // blocks 0
        shard(1, 0, 1, 2, 1, 1, 4, 4, &[0; 4]), // index >= data + parity
        shard(1, 0, 1, 0, 0, 1, 4, 4, &[0; 4]), // data 0
        shard(1, 0, 1, 0, 1, 1, 4, 5, &[0; 4]), // shard_len != payload
    ] {
        let err = rx.push(&bad).unwrap_err().to_string();
        assert_eq!(err, "malformed video shard");
    }
    // Shards of one frame that disagree.
    let mut rx = Receiver::default();
    assert_eq!(rx.push(&shard(2, 0, 2, 0, 1, 1, 8, 4, &[0; 4])).unwrap(), None);
    let err = rx.push(&shard(2, 1, 2, 0, 1, 1, 9, 4, &[0; 4])).unwrap_err().to_string();
    assert_eq!(err, "inconsistent video shards");
    let err = rx.push(&shard(2, 0, 2, 1, 2, 1, 8, 4, &[0; 4])).unwrap_err().to_string();
    assert_eq!(err, "inconsistent video block");
}

#[test]
fn seq_zero_is_reserved_and_never_delivered() {
    // Review lot 18 (NET-6, K3): seq 0 is reserved, documented in `fec.rs`: the
    // receiver starts as if frame 0 had been delivered, so a frame with seq 0 is late.
    // The agent numbers frames from 1.
    let mut rx = Receiver::default();
    let got = push_all(&mut rx, &encode(0, &frame(100, 0), 1200, 0.2).unwrap());
    assert!(got.is_empty());
    let got = push_all(&mut rx, &encode(1, &frame(100, 0), 1200, 0.2).unwrap());
    assert_eq!(got, vec![(1, frame(100, 0))]);
}

#[test]
fn frame_len_larger_than_the_shards_is_refused() {
    // Fixed in review lot 18 (NET-6, K2): `frame_len` comes from the packet; a frame
    // that announces more bytes than its data shards hold is dropped when it completes,
    // before anything is reserved (before: accepted, returned shorter than announced,
    // with the announced capacity reserved, up to 4 GiB + 64 KiB per frame).
    let mut rx = Receiver::default();
    let err = rx.push(&shard(5, 0, 1, 0, 1, 1, 1_000_000, 4, &[1, 2, 3, 4])).unwrap_err().to_string();
    assert_eq!(err, "video frame longer than its shards");
    // The rest of that frame is late now, the next frame comes through.
    assert_eq!(rx.push(&shard(5, 0, 1, 1, 1, 1, 1_000_000, 4, &[1, 2, 3, 4])).unwrap(), None);
    // As many bytes as the shards hold: accepted.
    let got = rx.push(&shard(6, 0, 1, 0, 1, 1, 4, 4, &[1, 2, 3, 4])).unwrap();
    assert_eq!(got, Some((6, vec![1, 2, 3, 4])));
    // A frame_len smaller than the data truncates.
    let got = rx.push(&shard(7, 0, 1, 0, 1, 1, 2, 4, &[1, 2, 3, 4])).unwrap();
    assert_eq!(got, Some((7, vec![1, 2])));
    // frame_len 0: an empty frame.
    let got = rx.push(&shard(8, 0, 1, 0, 1, 1, 0, 4, &[1, 2, 3, 4])).unwrap();
    assert_eq!(got, Some((8, vec![])));
    // Two blocks: the bound is the sum of their data shards (3 + 2) x shard_len.
    let mut rx = Receiver::default();
    for (block, index, data) in [(0u8, 0u8, 3u8), (0, 1, 3), (0, 2, 3), (1, 0, 2), (1, 1, 2)] {
        let r = rx.push(&shard(9, block, 2, index, data, 1, 21, 4, &[7; 4]));
        if (block, index) == (1, 1) {
            assert_eq!(r.unwrap_err().to_string(), "video frame longer than its shards");
        } else {
            assert_eq!(r.unwrap(), None);
        }
    }
    let mut rx = Receiver::default();
    let got = push_all(&mut rx, &[0u8, 1, 2].map(|i| shard(9, 0, 2, i, 3, 1, 20, 4, &[7; 4])).to_vec());
    assert!(got.is_empty());
    let got = push_all(&mut rx, &[0u8, 1].map(|i| shard(9, 1, 2, i, 2, 1, 20, 4, &[7; 4])).to_vec());
    assert_eq!(got, vec![(9, vec![7; 20])]);
}

#[test]
fn frame_len_u32_max_with_one_data_shard_reserves_nothing_large() {
    // Review NET-6: the largest frame_len a peer can announce, with one data shard of
    // 16 bytes. Refused, nothing reserved (before: Vec::with_capacity(4 GiB + 16),
    // which aborts the viewer where the allocation fails).
    let mut rx = Receiver::default();
    let err = rx.push(&shard(1, 0, 1, 0, 1, 1, u32::MAX, 16, &[0; 16])).unwrap_err().to_string();
    assert_eq!(err, "video frame longer than its shards");
    let got = push_all(&mut rx, &encode(2, &frame(100, 0), 1200, 0.2).unwrap());
    assert_eq!(got, vec![(2, frame(100, 0))]);
}

#[test]
fn parity_zero_or_too_many_shards_fail_in_reed_solomon() {
    // data 1, parity 0: the shard passes the checks, Reed-Solomon refuses the block.
    let mut rx = Receiver::default();
    assert!(rx.push(&shard(1, 0, 1, 0, 1, 0, 4, 4, &[0; 4])).is_err());
    // The same shard again: already there, nothing happens.
    assert_eq!(rx.push(&shard(1, 0, 1, 0, 1, 0, 4, 4, &[0; 4])).unwrap(), None);
    // 200 + 100 shards: more than Reed-Solomon over GF(2^8) can do (256).
    let mut rx = Receiver::default();
    let mut err = None;
    for index in 0..200u8 {
        if let Err(e) = rx.push(&shard(1, 0, 1, index, 200, 100, 800, 4, &[0; 4])) {
            err = Some(e);
            break;
        }
    }
    assert!(err.is_some(), "the 200th data shard tries to rebuild and fails");
}

#[test]
fn duplicates_and_late_shards_change_nothing() {
    let f = frame(5000, 1);
    let packets = encode(3, &f, 1200, 0.25).unwrap();
    let mut rx = Receiver::default();
    // Every shard twice.
    let doubled: Vec<&Vec<u8>> = packets.iter().flat_map(|p| [p, p]).collect();
    assert_eq!(push_all(&mut rx, doubled), vec![(3, f)]);
    // The whole frame again, once completed: late.
    assert!(push_all(&mut rx, &packets).is_empty());
}

#[test]
fn older_incomplete_frames_are_dropped_when_a_newer_one_completes() {
    let (a, b) = (frame(5000, 2), frame(300, 3));
    let pa = encode(10, &a, 1200, 0.25).unwrap();
    let pb = encode(11, &b, 1200, 0.25).unwrap();
    let mut rx = Receiver::default();
    assert!(push_all(&mut rx, &pa[..2]).is_empty());
    assert_eq!(push_all(&mut rx, &pb), vec![(11, b)]);
    // The rest of frame 10 is late now.
    assert!(push_all(&mut rx, &pa[2..]).is_empty());
}

#[test]
fn loss_is_counted_only_for_frames_two_behind_and_within_keep_behind() {
    let mut rx = Receiver::default();
    // Frame 1: 7 shards, 3 arrive.
    let p1 = encode(1, &frame(5000, 0), 1200, 0.25).unwrap();
    assert_eq!(p1.len(), 7);
    push_all(&mut rx, &p1[..3]);
    assert_eq!(rx.take_loss(), (0, 0));
    // Frame 2 complete: frame 1 is only one behind, still open.
    push_all(&mut rx, &encode(2, &frame(10, 0), 1200, 0.0).unwrap());
    assert_eq!(rx.take_loss(), (0, 0));
    // Frame 3: frame 1 closes (7 expected, 4 lost).
    push_all(&mut rx, &encode(3, &frame(10, 0), 1200, 0.0).unwrap());
    assert_eq!(rx.take_loss(), (7, 4));
    // A jump of 100 frames: 2 and 3 close (2 + 2 shards, none lost).
    push_all(&mut rx, &encode(103, &frame(10, 0), 1200, 0.0).unwrap());
    assert_eq!(rx.take_loss(), (4, 0));
    // A shard of a frame 64 or more behind the newest is not counted at all.
    rx.push(&encode(39, &frame(10, 0), 1200, 0.0).unwrap()[0]).unwrap();
    push_all(&mut rx, &encode(200, &frame(10, 0), 1200, 0.0).unwrap());
    assert_eq!(rx.take_loss(), (2, 0), "only frame 103 closed");
}

#[test]
fn seq_near_u64_max_does_not_overflow() {
    // Fixed in review lot 18 (NET-6, K1): `seq + KEEP_BEHIND` in `count` and in the
    // retain is saturating (before: a panic in debug builds, a wrapping sum in release
    // builds for seq > u64::MAX - 64). The agent numbers frames from 1, so only a
    // malicious or broken peer can send such a seq.
    let packets = encode(u64::MAX - 10, &frame(10, 0), 1200, 0.0).unwrap();
    let mut rx = Receiver::default();
    assert_eq!(push_all(&mut rx, &packets), vec![(u64::MAX - 10, frame(10, 0))]);
    // The very last seq, then older frames: late, ignored.
    let got = push_all(&mut rx, &encode(u64::MAX, &frame(10, 1), 1200, 0.0).unwrap());
    assert_eq!(got, vec![(u64::MAX, frame(10, 1))]);
    assert!(push_all(&mut rx, &encode(u64::MAX - 5, &frame(10, 0), 1200, 0.0).unwrap()).is_empty());
    assert!(push_all(&mut rx, &encode(5, &frame(10, 0), 1200, 0.0).unwrap()).is_empty());
    // Up to u64::MAX - 64, as before.
    let mut rx = Receiver::default();
    let got = push_all(&mut rx, &encode(u64::MAX - 64, &frame(10, 0), 1200, 0.0).unwrap());
    assert_eq!(got, vec![(u64::MAX - 64, frame(10, 0))]);
    // An incomplete frame near the end, then a complete one: the loss is counted.
    let mut rx = Receiver::default();
    let p = encode(u64::MAX - 3, &frame(5000, 0), 1200, 0.25).unwrap();
    push_all(&mut rx, &p[..3]);
    for seq in [u64::MAX - 2, u64::MAX - 1] {
        push_all(&mut rx, &encode(seq, &frame(10, 0), 1200, 0.0).unwrap());
    }
    assert_eq!(rx.take_loss(), (7, 4));
}

#[test]
fn fuzz_reproducer_fec_receiver_seq_overflow() {
    // The crash cargo-fuzz found in `fuzz/fuzz_targets/fec_receiver.rs` (MSI-UBUNTU,
    // 2026-11-18, after 73 682 runs): one datagram of seq u64::MAX, "attempt to add
    // with overflow" at fec.rs:214 (`seq + KEEP_BEHIND`). Fixed with K1 (review lot
    // 18): block 0 of 81, waiting for the rest, in debug and release builds alike.
    let packet: [u8; 21] = [
        0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x51, 0x00, 0x11, 0xff, 0xff, 0xff, 0xff,
        0x3f, 0x01, 0x00, 0x10,
    ];
    assert!(matches!(Receiver::default().push(&packet), Ok(None)));
}

#[test]
fn take_rebuilt_counts_frames_repaired_by_parity() {
    let mut rx = Receiver::default();
    for seq in 1..=4u64 {
        let packets = encode(seq, &frame(5000, seq as u8), 1200, 0.25).unwrap();
        // Odd frames lose their first data shard.
        let skip = usize::from(seq % 2 == 1);
        assert_eq!(push_all(&mut rx, &packets[skip..]).len(), 1);
    }
    assert_eq!(rx.take_rebuilt(), 2);
    assert_eq!(rx.take_rebuilt(), 0);
}

#[test]
fn reordering_counts_as_rebuilt() {
    // A parity shard first: the block is complete with it (data 1), the data shard
    // that comes after is ignored, and the frame counts as rebuilt by parity
    // although nothing was lost (statistics `frames_rebuilt` only).
    let f = frame(10, 0);
    let packets = encode(1, &f, 1200, 0.3).unwrap();
    assert_eq!(packets.len(), 3);
    let mut rx = Receiver::default();
    assert_eq!(push_all(&mut rx, [&packets[2], &packets[0], &packets[1]]), vec![(1, f)]);
    assert_eq!(rx.take_rebuilt(), 1);
    assert_eq!(rx.take_loss(), (0, 0));
}

/// `encode` succeeds exactly when the frame fits in 255 blocks of 32 shards.
fn fits(len: usize, max_datagram: usize) -> bool {
    let payload = (max_datagram - HEADER).min(u16::MAX as usize);
    len.div_ceil(payload).max(1).div_ceil(32) <= 255
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn round_trip_any_size(len in 0usize..200_000, max_datagram in (HEADER + 17)..1500, ratio in 0.0f64..1.0, seq in 1u64..u64::MAX / 2) {
        let f = frame(len, 5);
        let encoded = encode(seq, &f, max_datagram, ratio);
        prop_assert_eq!(encoded.is_ok(), fits(len, max_datagram));
        let Ok(packets) = encoded else { return Ok(()) };
        prop_assert!(packets.iter().all(|p| p.len() <= max_datagram));
        let mut rx = Receiver::default();
        prop_assert_eq!(push_all(&mut rx, &packets), vec![(seq, f)]);
        prop_assert_eq!(rx.take_rebuilt(), 0);
    }

    /// Any loss of up to `parity` shards per block, in any order: rebuilt; a data
    /// shard lost counts one frame rebuilt.
    #[test]
    fn loss_up_to_parity_in_any_order(len in 1usize..60_000, ratio in 0.0f64..0.6, drop_seed in any::<u64>(), order in any::<u64>()) {
        let f = frame(len, 9);
        let packets = encode(1, &f, 1200, ratio).unwrap();
        // Drop, per block, a pseudo-random number (<= parity) of pseudo-random shards.
        let mut kept = Vec::new();
        let mut data_lost = false;
        let mut state = drop_seed | 1;
        let mut next = || { state ^= state << 13; state ^= state >> 7; state ^= state << 17; state };
        let mut by_block: std::collections::BTreeMap<usize, Vec<&Vec<u8>>> = Default::default();
        for p in &packets {
            by_block.entry(fields(p).0).or_default().push(p);
        }
        for shards in by_block.values() {
            let (_, _, _, data, parity) = fields(shards[0]);
            let n = (next() % (parity as u64 + 1)) as usize;
            let mut lost = BTreeSet::new();
            while lost.len() < n {
                lost.insert((next() % (data + parity) as u64) as usize);
            }
            data_lost |= lost.iter().any(|&i| i < data);
            kept.extend(shards.iter().enumerate().filter(|(i, _)| !lost.contains(i)).map(|(_, p)| *p));
        }
        // Shuffle what is kept.
        let mut s = order | 1;
        for i in (1..kept.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            kept.swap(i, (s >> 33) as usize % (i + 1));
        }
        let mut rx = Receiver::default();
        prop_assert_eq!(push_all(&mut rx, kept), vec![(1, f)]);
        // A lost data shard always counts the frame as rebuilt; with the shards out of
        // order a parity shard arriving before a data shard counts too (see
        // `reordering_counts_as_rebuilt`).
        let rebuilt = rx.take_rebuilt();
        prop_assert!(rebuilt <= 1);
        if data_lost {
            prop_assert_eq!(rebuilt, 1);
        }
    }

    /// Whatever the interleaving, duplicates and losses of several frames, the frames
    /// returned have strictly increasing seq numbers above 0, each one whole. (The
    /// viewer's `seq - last - 1` in `net.rs` relies on this.)
    #[test]
    fn returned_seq_numbers_strictly_increase(
        lens in prop::collection::vec(1usize..8000, 1..10),
        picks in prop::collection::vec(any::<u16>(), 0..400),
    ) {
        let frames: Vec<Vec<u8>> = lens.iter().enumerate().map(|(i, &l)| frame(l, i as u8)).collect();
        let all: Vec<Vec<u8>> = frames
            .iter()
            .enumerate()
            .flat_map(|(i, f)| encode(i as u64 + 1, f, 1200, 0.3).unwrap())
            .collect();
        let mut rx = Receiver::default();
        let mut last = 0u64;
        for pick in picks {
            let p = &all[pick as usize % all.len()];
            if let Some((seq, data)) = rx.push(p).unwrap() {
                prop_assert!(seq > last);
                prop_assert_eq!(&data, &frames[seq as usize - 1]);
                last = seq;
            }
        }
    }

    /// Arbitrary datagrams never panic (any seq and frame_len since NET-6).
    #[test]
    fn arbitrary_datagrams_never_panic(packets in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..80), 0..64)) {
        let mut rx = Receiver::default();
        for p in packets {
            let _ = rx.push(&p);
            let _ = rx.take_loss();
        }
    }

    /// Well-formed headers with arbitrary fields: never a panic.
    #[test]
    fn arbitrary_headers_never_panic(
        shards in prop::collection::vec((1u64..50, any::<u8>(), 1u8..4, any::<u8>(), 1u8..40, 0u8..40, 0u32..5000, 1u16..40), 0..200)
    ) {
        let mut rx = Receiver::default();
        for (seq, block, blocks, index, data, parity, frame_len, shard_len) in shards {
            let p = shard(seq, block % blocks, blocks, index, data, parity, frame_len, shard_len, &vec![7; shard_len as usize]);
            let _ = rx.push(&p);
        }
        let _ = rx.take_loss();
        let _ = rx.take_rebuilt();
    }
}
