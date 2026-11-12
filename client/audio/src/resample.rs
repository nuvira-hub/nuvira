//! Conversion between a device's sample rate and Opus's 48 kHz, on interleaved
//! samples arriving in pieces of any size.

use anyhow::Result;
use rubato::{FftFixedInOut, Resampler as _};

pub struct Resampler {
    inner: Option<FftFixedInOut<f32>>,
    channels: usize,
    /// Input not yet converted, one Vec per channel.
    pending: Vec<Vec<f32>>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Result<Self> {
        let inner = match from == to {
            true => None,
            false => Some(FftFixedInOut::new(from as usize, to as usize, 480, channels)?),
        };
        Ok(Resampler { inner, channels, pending: vec![Vec::new(); channels] })
    }

    /// Converts `input` (interleaved): the output comes in steps, so it can be shorter
    /// or longer than the input's share.
    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        let Some(inner) = self.inner.as_mut() else { return Ok(input.to_vec()) };
        for frame in input.chunks_exact(self.channels) {
            for (c, s) in frame.iter().enumerate() {
                self.pending[c].push(*s);
            }
        }
        let mut out = Vec::new();
        loop {
            let need = inner.input_frames_next();
            if self.pending[0].len() < need {
                break;
            }
            let chunk: Vec<Vec<f32>> = self.pending.iter_mut().map(|ch| ch.drain(..need).collect()).collect();
            let converted = inner.process(&chunk, None)?;
            for i in 0..converted[0].len() {
                out.extend(converted.iter().map(|ch| ch[i]));
            }
        }
        Ok(out)
    }
}

/// `input` (interleaved, `from` channels) as stereo: mono doubled, more channels
/// mixed into left and right (front ones first, the rest shared).
pub fn to_stereo(input: &[f32], from: usize) -> Vec<f32> {
    match from {
        2 => input.to_vec(),
        1 => input.iter().flat_map(|s| [*s, *s]).collect(),
        _ => input
            .chunks_exact(from)
            .flat_map(|f| {
                let rest: f32 = f[2..].iter().sum::<f32>() / (from - 2) as f32 * 0.5;
                [(f[0] + rest).clamp(-1.0, 1.0), (f[1] + rest).clamp(-1.0, 1.0)]
            })
            .collect(),
    }
}

/// Stereo `input` for a device with `to` channels: mono mixed, more channels get
/// left and right on the first two and silence on the others.
pub fn from_stereo(input: &[f32], to: usize) -> Vec<f32> {
    match to {
        2 => input.to_vec(),
        1 => input.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect(),
        _ => input
            .chunks_exact(2)
            .flat_map(|f| {
                let mut frame = vec![0.0; to];
                frame[0] = f[0];
                frame[1] = f[1];
                frame
            })
            .collect(),
    }
}

#[test]
fn rates_and_channels() {
    let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
    let mut out = 0;
    for _ in 0..100 {
        out += r.process(&vec![0.1; 441 * 2]).unwrap().len();
    }
    // 1 s at 44.1 kHz is about 1 s at 48 kHz (less what is still pending).
    assert!((46_000..=48_000).contains(&(out / 2)), "{out}");
    assert_eq!(to_stereo(&[0.5], 1), vec![0.5, 0.5]);
    assert_eq!(from_stereo(&[0.2, 0.4], 1), vec![0.3f32 * 1.0]);
    assert_eq!(from_stereo(&[0.2, 0.4], 4), vec![0.2, 0.4, 0.0, 0.0]);
}
