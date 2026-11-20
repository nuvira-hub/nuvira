//! `fec::Receiver::push` with arbitrary datagrams, `seq` kept below 2^63 and
//! `frame_len` below 64 KiB (the masks of the two Phase 2 findings, fixed in review
//! lot 18: `fec_receiver` covers the unmasked fields). Everything else is as
//! received; the frames returned must have strictly increasing `seq`.
//! Input: datagrams, each prefixed by its length (one byte).
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuvira_transport::fec::Receiver;

fuzz_target!(|data: &[u8]| {
    let mut rx = Receiver::default();
    let mut rest = data;
    let mut last = 0u64;
    while let Some((&len, tail)) = rest.split_first() {
        let n = usize::from(len).min(tail.len());
        let (packet, after) = tail.split_at(n);
        let mut packet = packet.to_vec();
        if packet.len() > 8 {
            packet[8] &= 0x7f;
        }
        if packet.len() > 17 {
            packet[16] = 0;
            packet[17] = 0;
        }
        if let Ok(Some((seq, _frame))) = rx.push(&packet) {
            assert!(seq > last, "seq {seq} after {last}");
            last = seq;
        }
        let _ = rx.take_loss();
        let _ = rx.take_rebuilt();
        rest = after;
    }
});
