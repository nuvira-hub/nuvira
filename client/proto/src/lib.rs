//! NexoDesk protocol messages (generated from `proto/`) and framing.
//!
//! This crate depends on neither the operating system nor the network: it must
//! also build for `wasm32-unknown-unknown` (decision D9).

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/nexodesk.v1.rs"));
}

pub mod framing;

/// Session protocol version spoken by this build.
pub const PROTOCOL_VERSION: u32 = v1::ProtocolVersion::ProtocolVersion0 as u32;

/// System clock in microseconds since 1970, used for message timestamps.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
