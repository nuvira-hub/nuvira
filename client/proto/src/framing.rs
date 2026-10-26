//! Framing dei messaggi sugli stream affidabili: lunghezza varint + messaggio
//! Protobuf (lo stesso formato di `prost::Message::encode_length_delimited`).

use bytes::{Buf, BytesMut};
use prost::Message;

/// Dimensione massima di un singolo messaggio (frame video compresi).
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    TooLarge(usize),
    Decode(prost::DecodeError),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(n) => write!(f, "messaggio troppo grande: {n} byte"),
            FrameError::Decode(e) => write!(f, "messaggio non valido: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Serializza un messaggio con il prefisso di lunghezza.
pub fn encode<M: Message>(msg: &M) -> Vec<u8> {
    msg.encode_length_delimited_to_vec()
}

/// Accumula i byte ricevuti e restituisce i messaggi completi.
#[derive(Default)]
pub struct Decoder {
    buf: BytesMut,
}

impl Decoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Prossimo messaggio completo, se è già arrivato per intero.
    pub fn next<M: Message + Default>(&mut self) -> Result<Option<M>, FrameError> {
        let mut peek = &self.buf[..];
        let len = match prost::encoding::decode_varint(&mut peek) {
            Ok(len) => len as usize,
            // Varint incompleto: servono altri byte (al massimo 10).
            Err(_) if self.buf.len() < 10 => return Ok(None),
            Err(e) => return Err(FrameError::Decode(e)),
        };
        if len > MAX_MESSAGE_LEN {
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
                ping = dec.next::<Ping>().unwrap();
            }
        }
        assert_eq!(ping, Some(a));
        assert_eq!(dec.next::<VideoFrame>().unwrap(), Some(b));
        assert_eq!(dec.next::<VideoFrame>().unwrap(), None);
    }

    #[test]
    fn rejects_oversized() {
        let mut dec = Decoder::default();
        let mut wire = Vec::new();
        prost::encoding::encode_varint((MAX_MESSAGE_LEN + 1) as u64, &mut wire);
        dec.push(&wire);
        assert!(matches!(dec.next::<Ping>(), Err(FrameError::TooLarge(_))));
    }
}
