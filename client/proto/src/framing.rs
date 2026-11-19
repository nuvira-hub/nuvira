//! Message framing on reliable streams: varint length + Protobuf message
//! (the same format as `prost::Message::encode_length_delimited`).

use bytes::{Buf, BytesMut};
use prost::Message;

/// Maximum size of a single message (video frames included).
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    TooLarge(usize),
    Decode(prost::DecodeError),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(n) => write!(f, "message too large: {n} bytes"),
            FrameError::Decode(e) => write!(f, "invalid message: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Serialises a message with its length prefix.
pub fn encode<M: Message>(msg: &M) -> Vec<u8> {
    msg.encode_length_delimited_to_vec()
}

/// Collects the received bytes and returns the complete messages.
pub struct Decoder {
    buf: BytesMut,
    limit: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::with_limit(MAX_MESSAGE_LEN)
    }
}

impl Decoder {
    /// A decoder that refuses messages longer than `limit` bytes (instead of
    /// [`MAX_MESSAGE_LEN`]), e.g. on a stream whose peer is not authenticated yet.
    pub fn with_limit(limit: usize) -> Self {
        Self { buf: BytesMut::new(), limit }
    }

    /// Changes the limit for the messages not decoded yet (e.g. raised once the
    /// peer has authenticated).
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Next complete message, if it has fully arrived.
    pub fn next_message<M: Message + Default>(&mut self) -> Result<Option<M>, FrameError> {
        let mut peek = &self.buf[..];
        let len = match prost::encoding::decode_varint(&mut peek) {
            Ok(len) => len as usize,
            // Incomplete varint: more bytes are needed (at most 10).
            Err(_) if self.buf.len() < 10 => return Ok(None),
            Err(e) => return Err(FrameError::Decode(e)),
        };
        if len > self.limit {
            return Err(FrameError::TooLarge(len));
        }
        let header = self.buf.len() - peek.len();
        if peek.len() < len {
            return Ok(None);
        }
        self.buf.advance(header);
        let body = self.buf.split_to(len);
        M::decode(body.freeze()).map(Some).map_err(FrameError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{Ping, VideoFrame};

    #[test]
    fn round_trip_split_in_pieces() {
        let a = Ping { sent_us: 42 };
        let b = VideoFrame { seq: 7, data: vec![1; 300], ..Default::default() };
        let mut wire = encode(&a);
        wire.extend(encode(&b));

        let mut dec = Decoder::default();
        let mut ping = None;
        for chunk in wire.chunks(3) {
            dec.push(chunk);
            if ping.is_none() {
                ping = dec.next_message::<Ping>().unwrap();
            }
        }
        assert_eq!(ping, Some(a));
        assert_eq!(dec.next_message::<VideoFrame>().unwrap(), Some(b));
        assert_eq!(dec.next_message::<VideoFrame>().unwrap(), None);
    }

    #[test]
    fn rejects_oversized() {
        let mut dec = Decoder::default();
        let mut wire = Vec::new();
        prost::encoding::encode_varint((MAX_MESSAGE_LEN + 1) as u64, &mut wire);
        dec.push(&wire);
        assert!(matches!(dec.next_message::<Ping>(), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn limit_per_decoder() {
        let ping = encode(&Ping { sent_us: 1 });
        let mut dec = Decoder::with_limit(ping.len() - 2);
        dec.push(&ping);
        assert!(matches!(dec.next_message::<Ping>(), Err(FrameError::TooLarge(n)) if n == ping.len() - 1));
        // Raised: the same bytes, still there, decode.
        dec.set_limit(MAX_MESSAGE_LEN);
        assert_eq!(dec.next_message::<Ping>().unwrap(), Some(Ping { sent_us: 1 }));
    }
}
