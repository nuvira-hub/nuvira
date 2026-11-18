//! Every message the network delivers, decoded from arbitrary bytes: signalling
//! (`ClientMessage`, `ServerMessage`, API calls) and session (`ControlMessage`,
//! `InputEvent`, `FileChunk`, `StreamOpen`, `TunnelResult`, `VideoFrame`). Decoding
//! never panics; what decodes re-encodes to bytes that decode and re-encode the same.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuvira_proto::v1::{
    ApiRequest, ApiResponse, ClientMessage, ControlMessage, FileChunk, InputEvent, ServerMessage, StreamOpen,
    TunnelResult, VideoFrame,
};
use prost::Message;

fn check<M: Message + Default + PartialEq + std::fmt::Debug>(data: &[u8]) {
    if let Ok(m) = M::decode(data) {
        let bytes = m.encode_to_vec();
        let again = M::decode(bytes.as_slice()).expect("re-encoded message decodes");
        // Compared as bytes: floats may be NaN (NaN != NaN), the bytes are the same.
        assert_eq!(again.encode_to_vec(), bytes);
    }
}

fuzz_target!(|data: &[u8]| {
    check::<ClientMessage>(data);
    check::<ServerMessage>(data);
    check::<ApiRequest>(data);
    check::<ApiResponse>(data);
    check::<ControlMessage>(data);
    check::<InputEvent>(data);
    check::<FileChunk>(data);
    check::<StreamOpen>(data);
    check::<TunnelResult>(data);
    check::<VideoFrame>(data);
});
