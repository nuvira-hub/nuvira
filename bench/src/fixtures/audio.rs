//! Audio fixture for B-MICRO-15: 48 kHz stereo f32 (interleaved, the format of
//! `nuvira_audio::opus`), a seeded multi-tone plus noise.
//!
//! Only IEEE-754 basic operations (+, -, *, /) are used, never `sin` from the
//! platform's maths library (whose last bit may differ between systems): the phase
//! comes from an integer sample counter and the sine from a fixed polynomial.

use crate::rng::Xoshiro256;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms frames.
pub const FRAME_SAMPLES: usize = 960;

const AUDIO_SEED: u64 = 0x4155_4449_4f53_4545; // "AUDIOSEE"

/// Tones in millihertz with their amplitude (left, right).
const TONES: [(u64, f64, f64); 5] = [
    (220_000, 0.20, 0.10),
    (440_000, 0.12, 0.18),
    (1_234_500, 0.08, 0.06),
    (3_100_000, 0.04, 0.05),
    (7_900_000, 0.02, 0.03),
];

/// sin(2 pi t) for t in [0, 1): reduced to [-pi/2, pi/2], odd Taylor polynomial to
/// x^15 (error < 1e-9, far below what an Opus test can see).
fn sin_turns(t: f64) -> f64 {
    let mut t = t - (t as i64) as f64; // [0, 1)
    if t >= 0.5 {
        t -= 1.0; // [-0.5, 0.5)
    }
    let mut x = t * 2.0 * std::f64::consts::PI; // [-pi, pi)
    if x > std::f64::consts::FRAC_PI_2 {
        x = std::f64::consts::PI - x;
    } else if x < -std::f64::consts::FRAC_PI_2 {
        x = -std::f64::consts::PI - x;
    }
    let x2 = x * x;
    let mut term = x;
    let mut sum = x;
    for k in 1..8 {
        term = -term * x2 / ((2 * k) as f64 * (2 * k + 1) as f64);
        sum += term;
    }
    sum
}

/// `seconds` of interleaved stereo samples.
pub fn pcm_stereo(seconds: u32) -> Vec<f32> {
    let n = (SAMPLE_RATE * seconds) as u64;
    let mut rng = Xoshiro256::new(AUDIO_SEED);
    let mut out = Vec::with_capacity(n as usize * CHANNELS);
    let period = SAMPLE_RATE as u64 * 1000;
    for i in 0..n {
        let (mut l, mut r) = (0.0f64, 0.0f64);
        for &(mhz, al, ar) in &TONES {
            let t = ((mhz * i) % period) as f64 / period as f64;
            let s = sin_turns(t);
            l += al * s;
            r += ar * s;
        }
        let noise = (rng.next_f64() - 0.5) * 0.02;
        out.push((l + noise) as f32);
        out.push((r - noise) as f32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_polynomial_is_accurate() {
        for k in 0..1000 {
            let t = k as f64 / 1000.0;
            assert!((sin_turns(t) - (t * 2.0 * std::f64::consts::PI).sin()).abs() < 1e-8, "{t}");
        }
    }

    #[test]
    fn deterministic_and_in_range() {
        let a = pcm_stereo(1);
        assert_eq!(a.len(), 48_000 * 2);
        assert_eq!(a, pcm_stereo(1));
        assert!(a.iter().all(|s| s.abs() < 1.0));
    }
}
