//! Video over QUIC datagrams with forward error correction (prototype, opt-in).
//!
//! On the reliable video stream a lost packet holds every frame after it until it is
//! retransmitted, at least one round trip: on a link with 2% loss and 100 ms of RTT
//! that was most of the remaining 95th percentile (2026-11-01). Here each frame is
//! cut into shards that each fit a datagram, and every block of up to [`MAX_DATA`]
//! shards gets Reed-Solomon parity shards: any `k` of the `k + m` shards of a block
//! rebuild it, with no retransmission. A frame that cannot be rebuilt is lost (the
//! viewer then asks for a keyframe).
//!
//! Shard: [`HEADER`] bytes, then `shard_len` bytes of data or parity.
//! `version u8, seq u64, block u8, blocks u8, index u8, data u8, parity u8, frame_len u32, shard_len u16`
//! (little endian). The frame is laid out shard after shard, the last one padded.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use reed_solomon_erasure::galois_8::ReedSolomon;

const VERSION: u8 = 1;
pub const HEADER: usize = 20;
/// Data shards per block: a 1080p keyframe (~150 kB) is a few blocks.
const MAX_DATA: usize = 32;
/// Frames still incomplete this far behind the newest one are dropped.
const KEEP_BEHIND: u64 = 64;

/// Parity shards for `data` data shards: at least two. With one, a small frame (2–4
/// shards) was lost whenever two of its shards were: with 2% random loss, about one
/// frame in 400, a freeze every 8 s at 50 fps.
fn parity_for(data: usize, ratio: f64) -> usize {
    ((data as f64 * ratio).ceil() as usize).clamp(MIN_PARITY, 255 - data)
}

const MIN_PARITY: usize = 2;

/// The datagrams carrying frame `seq`, each at most `max_datagram` bytes, with
/// `ratio` parity shards per data shard (0.2 = 20% more).
pub fn encode(seq: u64, frame: &[u8], max_datagram: usize, ratio: f64) -> Result<Vec<Vec<u8>>> {
    ensure!(max_datagram > HEADER + 16, "datagrams too small: {max_datagram} bytes");
    ensure!(frame.len() <= u32::MAX as usize, "frame too large");
    let max_payload = (max_datagram - HEADER).min(u16::MAX as usize);
    let shards = frame.len().div_ceil(max_payload).max(1);
    let blocks = shards.div_ceil(MAX_DATA);
    if blocks > 255 {
        bail!("frame of {} bytes too large for the datagrams", frame.len());
    }
    // Shards of equal size, as small as the frame allows (a small frame is not padded
    // to a full datagram).
    let shard_len = frame.len().div_ceil(shards).max(1);
    let mut out = Vec::new();
    let mut offset = 0;
    for block in 0..blocks {
        // Shards spread evenly over the blocks.
        let data = shards / blocks + usize::from(block < shards % blocks);
        let parity = parity_for(data, ratio);
        let mut set = vec![vec![0u8; shard_len]; data + parity];
        for shard in set.iter_mut().take(data) {
            let end = (offset + shard_len).min(frame.len());
            if offset < end {
                shard[..end - offset].copy_from_slice(&frame[offset..end]);
            }
            offset += shard_len;
        }
        ReedSolomon::new(data, parity)?.encode(&mut set)?;
        for (index, shard) in set.into_iter().enumerate() {
            let mut packet = Vec::with_capacity(HEADER + shard_len);
            packet.push(VERSION);
            packet.extend_from_slice(&seq.to_le_bytes());
            packet.extend_from_slice(&[block as u8, blocks as u8, index as u8, data as u8, parity as u8]);
            packet.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            packet.extend_from_slice(&(shard_len as u16).to_le_bytes());
            packet.extend_from_slice(&shard);
            out.push(packet);
        }
    }
    Ok(out)
}

struct Block {
    data: usize,
    parity: usize,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    /// The data shards, once rebuilt.
    done: Option<Vec<Vec<u8>>>,
}

struct Partial {
    frame_len: usize,
    shard_len: usize,
    blocks: Vec<Option<Block>>,
}

/// Shards seen of a recent frame, to count the lost ones.
struct Count {
    /// Shards of each block (data + parity), 0 while none of the block has arrived.
    blocks: Vec<usize>,
    got: usize,
}

/// Rebuilds frames from their datagrams, and counts the shards lost.
#[derive(Default)]
pub struct Receiver {
    frames: BTreeMap<u64, Partial>,
    /// Last frame returned: older shards are late and ignored.
    last: u64,
    counts: BTreeMap<u64, Count>,
    newest: u64,
    shards: u64,
    shards_lost: u64,
}

impl Receiver {
    /// A datagram received: returns `(seq, frame)` when it completes a frame. Frames
    /// older than the one completed that are still incomplete are lost (shards arrive
    /// in order and are never retransmitted).
    pub fn push(&mut self, packet: &[u8]) -> Result<Option<(u64, Vec<u8>)>> {
        ensure!(packet.len() > HEADER && packet[0] == VERSION, "not a video shard");
        let seq = u64::from_le_bytes(packet[1..9].try_into()?);
        let (block, blocks, index, data, parity) = (
            packet[9] as usize,
            packet[10] as usize,
            packet[11] as usize,
            packet[12] as usize,
            packet[13] as usize,
        );
        let frame_len = u32::from_le_bytes(packet[14..18].try_into()?) as usize;
        let shard_len = u16::from_le_bytes(packet[18..20].try_into()?) as usize;
        let shard = &packet[HEADER..];
        ensure!(
            block < blocks && index < data + parity && data > 0 && shard.len() == shard_len,
            "malformed video shard"
        );
        self.count(seq, block, blocks, data + parity);
        if seq <= self.last {
            return Ok(None);
        }
        let partial = self.frames.entry(seq).or_insert_with(|| Partial {
            frame_len,
            shard_len,
            blocks: (0..blocks).map(|_| None).collect(),
        });
        ensure!(
            partial.frame_len == frame_len
                && partial.shard_len == shard_len
                && partial.blocks.len() == blocks,
            "inconsistent video shards"
        );
        let state = partial.blocks[block].get_or_insert_with(|| Block {
            data,
            parity,
            shards: vec![None; data + parity],
            received: 0,
            done: None,
        });
        ensure!(state.data == data && state.parity == parity, "inconsistent video block");
        if state.done.is_some() || state.shards[index].is_some() {
            return Ok(None);
        }
        state.shards[index] = Some(shard.to_vec());
        state.received += 1;
        if state.received < data {
            return Ok(None);
        }
        ReedSolomon::new(data, parity)?
            .reconstruct_data(&mut state.shards)
            .context("rebuilding a video block")?;
        state.done = Some(state.shards.drain(..data).map(|s| s.expect("rebuilt")).collect());
        state.shards.clear();

        if !partial.blocks.iter().all(|b| b.as_ref().is_some_and(|b| b.done.is_some())) {
            return Ok(None);
        }
        let partial = self.frames.remove(&seq).expect("present");
        let mut frame = Vec::with_capacity(partial.frame_len + partial.shard_len);
        for block in partial.blocks.into_iter().flatten() {
            for shard in block.done.into_iter().flatten() {
                frame.extend_from_slice(&shard);
            }
        }
        frame.truncate(partial.frame_len);
        self.last = seq;
        self.frames.retain(|&s, _| s > seq);
        Ok(Some((seq, frame)))
    }

    /// Shards expected and lost since the previous call, for the frames that can get no
    /// more of them.
    pub fn take_loss(&mut self) -> (u64, u64) {
        (std::mem::take(&mut self.shards), std::mem::take(&mut self.shards_lost))
    }

    fn count(&mut self, seq: u64, block: usize, blocks: usize, total: usize) {
        if seq + KEEP_BEHIND <= self.newest {
            return;
        }
        let count = self.counts.entry(seq).or_insert_with(|| Count { blocks: vec![0; blocks], got: 0 });
        if let Some(n) = count.blocks.get_mut(block) {
            *n = total;
        }
        count.got += 1;
        if seq <= self.newest {
            return;
        }
        self.newest = seq;
        // Shards arrive in order: a frame two behind the newest has all it will get.
        let closed: Vec<u64> = self.counts.range(..seq.saturating_sub(1)).map(|(s, _)| *s).collect();
        for s in closed {
            let count = self.counts.remove(&s).expect("present");
            // Blocks of which nothing arrived: as large as the others.
            let known: Vec<usize> = count.blocks.iter().copied().filter(|&n| n > 0).collect();
            let typical = known.iter().sum::<usize>() / known.len().max(1);
            let expected: usize = count.blocks.iter().map(|&n| if n > 0 { n } else { typical }).sum();
            self.shards += expected as u64;
            self.shards_lost += expected.saturating_sub(count.got) as u64;
        }
        // Memory bound when many frames are lost.
        self.frames.retain(|&s, _| s + KEEP_BEHIND > seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn round_trip_without_losses() {
        let mut rx = Receiver::default();
        for (seq, len) in [(1, 10), (2, 1200), (3, 150_000)] {
            let f = frame(len);
            let mut got = None;
            for p in encode(seq, &f, 1200, 0.2).unwrap() {
                assert!(p.len() <= 1200);
                if let Some(done) = rx.push(&p).unwrap() {
                    got = Some(done);
                }
            }
            assert_eq!(got, Some((seq, f)));
        }
    }

    #[test]
    fn rebuilds_with_lost_shards_up_to_the_parity() {
        let f = frame(5000);
        let packets = encode(7, &f, 1200, 0.25).unwrap();
        // 5 data shards, 2 parity (25%, and the minimum): lose two data shards.
        assert_eq!(packets.len(), 7);
        let mut rx = Receiver::default();
        let mut got = None;
        for (i, p) in packets.iter().enumerate() {
            if i == 0 || i == 3 {
                continue;
            }
            if let Some(done) = rx.push(p).unwrap() {
                got = Some(done);
            }
        }
        assert_eq!(got, Some((7, f)));
    }

    #[test]
    fn counts_the_lost_shards() {
        let mut rx = Receiver::default();
        // 5 data + 2 parity, two lost: the frame is rebuilt, the loss is counted.
        for (i, p) in encode(1, &frame(5000), 1200, 0.25).unwrap().iter().enumerate() {
            if i != 1 && i != 4 {
                rx.push(p).unwrap();
            }
        }
        assert_eq!(rx.take_loss(), (0, 0), "frame 1 may still get shards");
        for seq in [2, 3] {
            for p in encode(seq, &frame(100), 1200, 0.25).unwrap() {
                rx.push(&p).unwrap();
            }
        }
        assert_eq!(rx.take_loss(), (7, 2));
        assert_eq!(rx.take_loss(), (0, 0));
    }

    #[test]
    fn a_frame_that_cannot_be_rebuilt_is_skipped() {
        let mut rx = Receiver::default();
        let lost = encode(1, &frame(3000), 1200, 0.2).unwrap();
        // Three shards, two parity: three lost.
        for p in &lost[3..] {
            assert_eq!(rx.push(p).unwrap(), None);
        }
        let next = frame(800);
        let mut got = None;
        for p in encode(2, &next, 1200, 0.2).unwrap() {
            if let Some(done) = rx.push(&p).unwrap() {
                got = Some(done);
            }
        }
        assert_eq!(got, Some((2, next)));
        // The late shards of frame 1 change nothing.
        assert_eq!(rx.push(&lost[0]).unwrap(), None);
    }
}
