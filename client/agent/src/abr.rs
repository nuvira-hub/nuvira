//! Adaptive bitrate based on the latency observed by the viewer.
//!
//! When the network is saturated packets queue up (in routers, in QUIC's
//! buffer) and the network latency (end of encoding → arrival at the viewer, or
//! capture→display with older viewers) rises well above its minimum.
//! The controller compares the recent median latency with the minimum of the
//! last 10 seconds (the "empty network" latency):
//! - queue above 60 ms → bitrate × 0.8 (at most once per second);
//! - queue below 25 ms for a while → bitrate × 1.08 (climbs back slowly).
//!
//! The thresholds are wide on purpose: on a slow link a larger frame also takes
//! longer to get through, and an occasional retransmission raises latency by one
//! RTT; neither is a queue to drain.
//!
//! Changes happen only while the video actually sends something: at least a quarter
//! of the current bitrate, or 150 kbit/s (simple content may stay far below the
//! bitrate and still fill a slow link). On a still screen almost nothing is sent: a latency
//! rise is not a queue we cause (on mobile networks sparse traffic alone makes
//! latency jump by 100 ms or more), lowering the bitrate would not help, and raising
//! it would only be a guess. Measured in the first Internet test: with the screen
//! still (1 kbit/s) the bitrate fell from 4 to 0.5 Mbit/s in 16 s.
//!
//! If a frame left more than 2 s ago and no feedback has arrived since, the
//! network is probably stuck: the bitrate goes down as for a high queue.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const QUEUE_HIGH_MS: f64 = 60.0;
const QUEUE_LOW_MS: f64 = 25.0;
/// Latency counts when at least this share of the bitrate is actually sent...
const BUSY_SHARE: f64 = 0.25;
/// ...or at least this much (a still screen sends 1–90 kbit/s).
const BUSY_BPS: f64 = 150_000.0;
const DECREASE: f64 = 0.8;
const INCREASE: f64 = 1.08;
const DECREASE_INTERVAL: Duration = Duration::from_secs(1);
const INCREASE_AFTER_DECREASE: Duration = Duration::from_secs(3);
const INCREASE_INTERVAL: Duration = Duration::from_secs(1);
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// After a bitrate change measurements are ignored for a while: the encoder (which
/// must be restarted for Quick Sync) and the network queue need to settle, and the
/// latency peak of the change must not be mistaken for congestion.
const SETTLE: Duration = Duration::from_millis(1500);
/// The viewer reports at least once a second (with its clock measurement): beyond
/// this time without feedback for a frame sent, the network is considered stuck.
pub const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Abr {
    bitrate: f64,
    min: f64,
    max: f64,
    /// Minimum latencies reported by the viewer, for the "empty network" baseline.
    mins: VecDeque<(Instant, f64)>,
    last_decrease: Option<Instant>,
    last_increase: Instant,
    settle_until: Option<Instant>,
}

impl Abr {
    pub fn new(start_bps: u32, min_bps: u32, max_bps: u32) -> Self {
        Self {
            bitrate: start_bps.clamp(min_bps, max_bps) as f64,
            min: min_bps as f64,
            max: max_bps as f64,
            mins: VecDeque::new(),
            last_decrease: None,
            last_increase: Instant::now(),
            settle_until: None,
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate as u32
    }

    /// New cap (quality change). Returns the new bitrate if it changes.
    pub fn set_max(&mut self, max_bps: u32) -> Option<u32> {
        self.max = (max_bps as f64).max(self.min);
        self.set(self.bitrate.min(self.max))
    }

    /// New feedback: median and minimum latency of the last interval (ms), and the
    /// video bitrate actually sent recently. Returns the new bitrate if it changes.
    pub fn on_feedback(&mut self, now: Instant, p50_ms: f64, min_ms: f64, sent_bps: f64) -> Option<u32> {
        if self.settle_until.is_some_and(|t| now < t) {
            return None;
        }
        self.mins.push_back((now, min_ms));
        while self.mins.front().is_some_and(|(t, _)| now.duration_since(*t) > BASE_WINDOW) {
            self.mins.pop_front();
        }
        let base = self.mins.iter().map(|(_, m)| *m).fold(f64::INFINITY, f64::min);
        let queue = p50_ms - base;
        if sent_bps < (self.bitrate * BUSY_SHARE).min(BUSY_BPS) {
            return None;
        }
        if queue > QUEUE_HIGH_MS {
            return self.decrease(now);
        }
        let calm = self.last_decrease.is_none_or(|t| now.duration_since(t) > INCREASE_AFTER_DECREASE);
        if queue < QUEUE_LOW_MS && calm && now.duration_since(self.last_increase) > INCREASE_INTERVAL {
            self.last_increase = now;
            return self.set(self.bitrate * INCREASE);
        }
        None
    }

    /// No feedback while frames are being sent: treated as a high queue.
    pub fn on_silence(&mut self, now: Instant) -> Option<u32> {
        self.decrease(now)
    }

    fn decrease(&mut self, now: Instant) -> Option<u32> {
        if self.last_decrease.is_some_and(|t| now.duration_since(t) < DECREASE_INTERVAL) {
            return None;
        }
        self.last_decrease = Some(now);
        self.set_at(now, self.bitrate * DECREASE)
    }

    fn set(&mut self, bps: f64) -> Option<u32> {
        self.set_at(Instant::now(), bps)
    }

    fn set_at(&mut self, now: Instant, bps: f64) -> Option<u32> {
        let new = bps.clamp(self.min, self.max);
        if (new - self.bitrate).abs() < 1.0 {
            return None;
        }
        self.bitrate = new;
        self.settle_until = Some(now + SETTLE);
        Some(new as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sent bitrate high enough for every bitrate in the tests.
    const BUSY: f64 = 10_000_000.0;

    #[test]
    fn decreases_on_queue_and_recovers() {
        let t0 = Instant::now();
        let mut abr = Abr::new(4_000_000, 300_000, 8_000_000);
        // Free network: latency steady at 40 ms, the bitrate rises.
        assert_eq!(abr.on_feedback(t0, 40.0, 35.0, BUSY), None);
        let t = |ms| t0 + Duration::from_millis(ms);
        let up = abr.on_feedback(t(1100), 40.0, 35.0, BUSY);
        assert_eq!(up, Some(4_320_000));
        // While settling (1.5 s) measurements do not count, not even a peak.
        assert_eq!(abr.on_feedback(t(1300), 500.0, 400.0, BUSY), None);
        // Queue: the median rises to 120 ms with a historical minimum of 35 → down.
        let down = abr.on_feedback(t(2700), 120.0, 100.0, BUSY);
        assert_eq!(down, Some(3_456_000));
        // After settling, down again if the queue stays.
        assert!(abr.on_feedback(t(4300), 120.0, 100.0, BUSY).is_some());
        // After a decrease no increases for 3 s, even if the queue empties.
        assert_eq!(abr.on_feedback(t(6000), 40.0, 35.0, BUSY), None);
        assert!(abr.on_feedback(t(7400), 40.0, 35.0, BUSY).is_some());
    }

    #[test]
    fn still_screen_changes_nothing() {
        let t0 = Instant::now();
        let t = |ms| t0 + Duration::from_millis(ms);
        let mut abr = Abr::new(4_000_000, 500_000, 8_000_000);
        assert_eq!(abr.on_feedback(t0, 100.0, 30.0, 1_000.0), None);
        // Latency jumps but only 1 kbit/s is sent: no decrease, no increase.
        assert_eq!(abr.on_feedback(t(1100), 185.0, 185.0, 1_000.0), None);
        assert_eq!(abr.on_feedback(t(2200), 40.0, 30.0, 1_000.0), None);
        assert_eq!(abr.bitrate(), 4_000_000);
        // The screen moves: even simple content at 800 kbit/s (a fifth of the bitrate)
        // can fill a slow link, so latency counts again.
        assert_eq!(abr.on_feedback(t(3300), 185.0, 150.0, 800_000.0), Some(3_200_000));
    }

    #[test]
    fn respects_limits() {
        let t0 = Instant::now();
        let mut abr = Abr::new(550_000, 500_000, 8_000_000);
        abr.on_silence(t0);
        assert_eq!(abr.bitrate(), 500_000);
        assert_eq!(abr.on_silence(t0 + Duration::from_secs(1)), None);
    }
}
