//! Opus through `unsafe-libopus` (libopus translated to Rust: no C compiler in the
//! build). 48 kHz stereo, one packet per 20 ms.

use anyhow::{Result, bail, ensure};
use unsafe_libopus as ffi;

use crate::{CHANNELS, FRAME, RATE};

/// Bitrate: music sounds clean, a little over 1% of a 8 Mbit/s video.
pub const BITRATE: i32 = 96_000;
/// Largest packet libopus writes for one frame.
const MAX_PACKET: usize = 1500;

pub struct Encoder(*mut ffi::OpusEncoder);

// The encoder is used by one thread at a time.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Self> {
        let mut err = 0;
        // SAFETY: plain constructor; the pointer is checked below.
        let st = unsafe { ffi::opus_encoder_create(RATE as i32, CHANNELS as i32, ffi::OPUS_APPLICATION_AUDIO, &mut err) };
        ensure!(!st.is_null() && err == ffi::OPUS_OK, "Opus encoder: error {err}");
        let enc = Encoder(st);
        // SAFETY: st is a valid encoder; each request takes one i32.
        unsafe {
            ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_BITRATE_REQUEST, BITRATE);
            // Redundancy for the previous packet: one lost datagram is rebuilt from the next.
            ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_INBAND_FEC_REQUEST, 1);
            ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5);
        }
        Ok(enc)
    }

    /// One packet from [`FRAME`] interleaved stereo samples.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        ensure!(pcm.len() == FRAME * CHANNELS, "{} samples instead of {}", pcm.len(), FRAME * CHANNELS);
        let mut out = vec![0u8; MAX_PACKET];
        // SAFETY: pcm holds FRAME frames, out MAX_PACKET bytes.
        let n = unsafe { ffi::opus_encode_float(self.0, pcm.as_ptr(), FRAME as i32, out.as_mut_ptr(), out.len() as i32) };
        if n < 0 {
            bail!("Opus encoding: error {n}");
        }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create, destroyed once.
        unsafe { ffi::opus_encoder_destroy(self.0) }
    }
}

pub struct Decoder(*mut ffi::OpusDecoder);

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self> {
        let mut err = 0;
        // SAFETY: plain constructor; the pointer is checked below.
        let st = unsafe { ffi::opus_decoder_create(RATE as i32, CHANNELS as i32, &mut err) };
        ensure!(!st.is_null() && err == ffi::OPUS_OK, "Opus decoder: error {err}");
        Ok(Decoder(st))
    }

    /// The samples of `packet` (interleaved stereo). `None`: the packet was lost,
    /// libopus makes up a plausible frame. `fec`: rebuild the packet *before* this
    /// one from the redundancy carried here.
    pub fn decode(&mut self, packet: Option<&[u8]>, fec: bool) -> Result<Vec<f32>> {
        let mut pcm = vec![0f32; FRAME * CHANNELS];
        let (data, len) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: data is packet (or null for a loss), pcm holds FRAME frames.
        let n = unsafe { ffi::opus_decode_float(self.0, data, len, pcm.as_mut_ptr(), FRAME as i32, fec as i32) };
        if n < 0 {
            bail!("Opus decoding: error {n}");
        }
        pcm.truncate(n as usize * CHANNELS);
        Ok(pcm)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create, destroyed once.
        unsafe { ffi::opus_decoder_destroy(self.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1 kHz tone through the codec comes out as a 1 kHz tone.
    #[test]
    fn tone_round_trip() {
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut out = Vec::new();
        for f in 0..50 {
            let pcm: Vec<f32> = (0..FRAME)
                .flat_map(|i| {
                    let t = (f * FRAME + i) as f32 / RATE as f32;
                    let s = (t * 1000.0 * std::f32::consts::TAU).sin() * 0.5;
                    [s, s]
                })
                .collect();
            let packet = enc.encode(&pcm).unwrap();
            assert!(packet.len() < 600, "packet of {} bytes", packet.len());
            out.extend(dec.decode(Some(&packet), false).unwrap());
        }
        // Lost packet: concealment gives a full frame.
        assert_eq!(dec.decode(None, false).unwrap().len(), FRAME * CHANNELS);
        let left: Vec<f32> = out.iter().step_by(2).copied().skip(RATE as usize / 2).collect();
        assert!((crate::player::tests_support::frequency(&left, RATE) - 1000.0).abs() < 20.0);
    }
}
