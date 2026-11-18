//! `fec::Receiver::push` with arbitrary datagrams, as a peer can send them (no
//! masking: overflows of `seq` and the capacity reserved from `frame_len` are
//! reachable here, see `fec_receiver_bounded` for the rest of the input space).
//! Input: datagrams, each prefixed by its length (one byte).
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuvira_transport::fec::Receiver;

fuzz_target!(|data: &[u8]| {
    let mut rx = Receiver::default();
    let mut rest = data;
    while let Some((&len, tail)) = rest.split_first() {
        let n = usize::from(len).min(tail.len());
        let (packet, after) = tail.split_at(n);
        let _ = rx.push(packet);
        let _ = rx.take_loss();
        let _ = rx.take_rebuilt();
        rest = after;
    }
});
