//! The QP of each frame, chosen here for drivers with only constant QP.

use crate::EncoderSettings;

/// Bitrate followed by changing the QP, for drivers with only constant QP.
///
/// Each frame's size is compared with its share of the bitrate (the time since
/// the previous frame, from one frame at the highest rate to 100 ms; a keyframe
/// may use eight times as much). Six QP steps halve the size, so the error in QP
/// is `6 * log2(size / share)`: the QP goes up by half of it at once (at most 3
/// a frame), and down by 15 % of it, at most one a frame; nearly empty frames
/// (a still screen) say nothing about the picture and lower it only by 0.1.
pub(super) struct QpControl {
    qp: f32,
    bitrate: u32,
    max_fps: u32,
    last: Option<std::time::Instant>,
}

impl QpControl {
    const MIN: f32 = 20.0;
    const MAX: f32 = 46.0;

    pub(super) fn new(settings: &EncoderSettings) -> Self {
        Self { qp: 28.0, bitrate: settings.bitrate_bps.max(1), max_fps: settings.max_fps.max(1), last: None }
    }

    pub(super) fn qp(&self) -> u8 {
        self.qp.round() as u8
    }

    pub(super) fn set_bitrate(&mut self, bps: u32) {
        let bps = bps.max(1);
        // Half of the change at once, the rest from the next frames.
        let change = 6.0 * (self.bitrate as f32 / bps as f32).log2() * 0.5;
        self.qp = (self.qp + change).clamp(Self::MIN, Self::MAX);
        self.bitrate = bps;
    }

    pub(super) fn frame(&mut self, bytes: usize, keyframe: bool) {
        let now = std::time::Instant::now();
        let min = 1.0 / self.max_fps as f32;
        let dt = self.last.map_or(min, |t| (now - t).as_secs_f32()).clamp(min, 0.1f32.max(min));
        self.last = Some(now);
        let mut share = self.bitrate as f32 * dt;
        if keyframe {
            share *= 8.0;
        }
        let ratio = (bytes as f32 * 8.0 / share).max(1e-3);
        let error = 6.0 * ratio.log2();
        let step = if error > 0.0 {
            (error * 0.5).min(3.0)
        } else if ratio < 0.125 {
            -0.1
        } else {
            (error * 0.15).max(-1.0)
        };
        self.qp = (self.qp + step).clamp(Self::MIN, Self::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::QpControl;
    use crate::EncoderSettings;

    #[test]
    fn qp_control() {
        let settings = EncoderSettings { bitrate_bps: 3_000_000, max_fps: 30, ltr: false };
        let mut c = QpControl::new(&settings);
        // 100 kB a frame at 30 fps (24 Mbit/s against 3): the QP goes up 3 a frame.
        c.frame(100_000, false);
        assert_eq!(c.qp(), 31);
        for _ in 0..10 {
            c.frame(100_000, false);
        }
        assert_eq!(c.qp(), 46);
        // A frame on its share (12.5 kB) leaves it where it is.
        let mut c = QpControl::new(&settings);
        c.frame(12_500, false);
        assert_eq!(c.qp(), 28);
        // A keyframe may be eight times larger.
        c.frame(100_000, true);
        assert_eq!(c.qp(), 28);
        // Half the share: down by 0.9.
        c.frame(6_250, false);
        assert!((c.qp - 27.1).abs() < 0.01, "{}", c.qp);
        // A still screen: down slowly.
        c.frame(100, false);
        assert!((c.qp - 27.0).abs() < 0.01, "{}", c.qp);
        // A third of the bitrate: half of 9.5 QP up at once.
        c.set_bitrate(1_000_000);
        assert_eq!(c.qp(), 32);
    }
}
