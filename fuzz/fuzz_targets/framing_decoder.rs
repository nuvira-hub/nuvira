//! `framing::Decoder` fed arbitrary bytes in arbitrary pieces: never a panic, and
//! draining always ends. The first byte picks the piece size.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuvira_proto::framing::Decoder;
use nuvira_proto::v1::{ControlMessage, InputEvent, VideoFrame};

fuzz_target!(|data: &[u8]| {
    let Some((&cut, rest)) = data.split_first() else { return };
    let piece = usize::from(cut).max(1);
    let mut dec = Decoder::default();
    for (i, chunk) in rest.chunks(piece).enumerate() {
        dec.push(chunk);
        // Each round at most as many messages as bytes pushed so far, plus one.
        for _ in 0..=rest.len() {
            let r = match i % 3 {
                0 => dec.next_message::<ControlMessage>().map(|m| m.is_some()),
                1 => dec.next_message::<InputEvent>().map(|m| m.is_some()),
                _ => dec.next_message::<VideoFrame>().map(|m| m.is_some()),
            };
            match r {
                Ok(true) => {}
                // An error is sticky for a length above the limit, so stop here.
                Ok(false) | Err(_) => break,
            }
        }
    }
});
