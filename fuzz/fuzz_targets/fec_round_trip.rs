//! `fec::encode` then `Receiver::push` with losses chosen by the input: a frame
//! with at most `parity` shards lost per block comes back whole; more lost, it
//! does not come back (no wrong frame ever).
//! Input: max datagram (2 bytes), ratio (1 byte), loss mask seed (8 bytes), frame.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuvira_transport::fec::{HEADER, Receiver, encode};

fuzz_target!(|data: &[u8]| {
    if data.len() < 11 {
        return;
    }
    let max_datagram = (u16::from_le_bytes([data[0], data[1]]) as usize % 1500).max(HEADER + 17);
    let ratio = f64::from(data[2]) / 255.0;
    let mut seed = u64::from_le_bytes(data[3..11].try_into().unwrap()) | 1;
    let frame = &data[11..];
    let Ok(packets) = encode(1, frame, max_datagram, ratio) else { return };
    let mut lost_too_many = false;
    let mut kept = Vec::new();
    let mut i = 0;
    while i < packets.len() {
        let (data_n, parity_n) = (packets[i][12] as usize, packets[i][13] as usize);
        let block = &packets[i..i + data_n + parity_n];
        let mut lost = 0;
        for p in block {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if seed % 4 == 0 {
                lost += 1;
            } else {
                kept.push(p);
            }
        }
        lost_too_many |= lost > parity_n;
        i += data_n + parity_n;
    }
    let mut rx = Receiver::default();
    let mut got = None;
    for p in kept {
        if let Some(done) = rx.push(p).expect("own shards are well formed") {
            assert!(got.is_none(), "one frame, delivered once");
            got = Some(done);
        }
    }
    match got {
        Some((seq, bytes)) => {
            assert!(!lost_too_many);
            assert_eq!((seq, bytes.as_slice()), (1, frame));
        }
        None => assert!(lost_too_many, "recoverable frame not rebuilt"),
    }
});
