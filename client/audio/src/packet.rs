//! One Opus packet in one QUIC datagram:
//! `tag u8 (0xA0), seq u32 (little endian), packet`.
//!
//! The tag tells it apart from the video's datagrams (`nuvira_transport::fec`, whose
//! first byte is its version, 1). Audio goes only to viewers that announced it
//! (`Hello.audio`), so an older viewer never sees one.

use anyhow::{Result, ensure};

pub const TAG: u8 = 0xA0;
pub const HEADER: usize = 5;

pub fn is_audio(datagram: &[u8]) -> bool {
    datagram.first() == Some(&TAG)
}

pub fn encode(seq: u32, packet: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + packet.len());
    out.push(TAG);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(packet);
    out
}

pub fn decode(datagram: &[u8]) -> Result<(u32, &[u8])> {
    ensure!(datagram.len() > HEADER && datagram[0] == TAG, "not an audio datagram");
    let seq = u32::from_le_bytes(datagram[1..HEADER].try_into()?);
    Ok((seq, &datagram[HEADER..]))
}

#[test]
fn round_trip() {
    let d = encode(7, b"opus");
    assert!(is_audio(&d) && !is_audio(&[1, 0, 0]));
    assert_eq!(decode(&d).unwrap(), (7, &b"opus"[..]));
    assert!(decode(&d[..HEADER]).is_err());
}
