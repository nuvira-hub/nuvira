//! Messaggi del protocollo Nexo (generati da `proto/`) e framing.
//!
//! Questo crate non dipende dal sistema operativo né dalla rete: deve
//! compilare anche per `wasm32-unknown-unknown` (decisione D9).

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/nexo.v1.rs"));
}

pub mod framing;

/// Versione del protocollo di sessione parlata da questa build.
pub const PROTOCOL_VERSION: u32 = v1::ProtocolVersion::ProtocolVersion0 as u32;

/// Orologio di sistema in microsecondi dal 1970, usato nei timestamp dei messaggi.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
