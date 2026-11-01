//! Adaptive bitrate based on the latency observed by the viewer.
//!
//! When the network is saturated packets queue up (in routers, in QUIC's
//! buffer) and the network latency (end of encoding → arrival at the viewer, or
//! capture→display with older viewers) rises well above its minimum and stays there.
//! The controller compares the median latency of the last second with the minimum
//! of the last 10 seconds (the "empty network" latency):
//! - queue above 60 ms for at least a second → bitrate × 0.8, or 85% of the rate
//!   actually sent if lower (the encoder often stays well below the bitrate: lowering
//!   only the cap would change nothing), but never below half the bitrate in one
//!   step (third Internet test: a light screen sending 0.2 Mbit/s went from 8 to
//!   0.5 Mbit/s at once and stayed there);
//! - queue below 25 ms → bitrate × 1.15 (once a second, 3 s after a decrease), but
//!   only while at least half the bitrate is actually sent, and for 30 s after a
//!   congestion at most to 90% of the rate that was being sent when it happened.
//!   Measured on a simulated 2 Mbit/s link (2026-11-01): the bitrate rose from 4 to
//!   5.3 Mbit/s while a nearly still screen sent 0.3 Mbit/s, and after each decrease it
//!   climbed back past the link's rate; every round trip filled the queue and
//!   restarted the encoder, and the 95th percentile of the latency was about 1 s
//!   (93 ms with a fixed bitrate under the link's rate).
//!
//! Single peaks do not count: on mobile networks latency jumps by 150–600 ms every
//! few seconds even with 0.3 Mbit/s sent on a 143 Mbit/s link (second Internet
//! test, 2026-10-27). Reacting to each peak brought the bitrate to the minimum and
//! the image became blurry, while the peaks stayed: they were not caused by us.
//!
//! Changes happen only while the video actually sends something: at least a quarter
//! of the current bitrate, or 150 kbit/s (simple content may stay far below the
//! bitrate and still fill a slow link). On a still screen almost nothing is sent: a
//! latency rise is not a queue we cause, lowering the bitrate would not help, and
//! raising it would only be a guess.
//!
//! If a frame left more than 2 s ago and no feedback has arrived since, the
//! network is probably stuck: the bitrate goes down.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const QUEUE_HIGH_MS: f64 = 60.0;
/// How long the queue must stay high before the bitrate goes down.
const HIGH_FOR: Duration = Duration::from_secs(1);
/// Window of the recent latency (median of the reports in it).
const RECENT_WINDOW: Duration = Duration::from_secs(1);
const QUEUE_LOW_MS: f64 = 25.0;
/// Frames lost count as congestion only with at least this queue.
const QUEUE_LOSS_MS: f64 = 30.0;
/// Latency counts when at least this share of the bitrate is actually sent...
const BUSY_SHARE: f64 = 0.25;
/// ...or at least this much (a still screen sends 1–90 kbit/s).
const BUSY_BPS: f64 = 150_000.0;
const DECREASE: f64 = 0.8;
/// On congestion the bitrate goes at most to this share of the rate actually sent...
const DECREASE_SENT: f64 = 0.85;
/// ...but never below this share of the current bitrate in one step.
const DECREASE_MAX: f64 = 0.5;
const INCREASE: f64 = 1.15;
/// Increases only while at least this share of the bitrate is actually sent: a
/// bitrate that is not used says nothing about what the network can take.
const INCREASE_SENT: f64 = 0.5;
/// After a congestion the bitrate rises at most to this share of the rate that was
/// being sent when it happened...
const CEILING_SHARE: f64 = 0.9;
/// ...for this long; then it probes again (the network may have improved).
const CEILING_HOLD: Duration = Duration::from_secs(30);
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
    /// Median latencies of the reports of the last second.
    recent: VecDeque<(Instant, f64)>,
    /// Since when the queue has been high without interruption.
    high_since: Option<Instant>,
    last_decrease: Option<Instant>,
    last_increase: Instant,
    settle_until: Option<Instant>,
    /// Rate sent at the last congestion, and when.
    ceiling: Option<(Instant, f64)>,
    /// Queue at the last latency report (ms above the minimum).
    last_queue: f64,
}

impl Abr {
    pub fn new(start_bps: u32, min_bps: u32, max_bps: u32) -> Self {
        Self {
            bitrate: start_bps.clamp(min_bps, max_bps) as f64,
            min: min_bps as f64,
            max: max_bps as f64,
            mins: VecDeque::new(),
            recent: VecDeque::new(),
            high_since: None,
            last_decrease: None,
            last_increase: Instant::now(),
            settle_until: None,
            ceiling: None,
            last_queue: 0.0,
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
        if sent_bps < (self.bitrate * BUSY_SHARE).min(BUSY_BPS) {
            // Latency measured while idle says nothing about the queue we would cause.
            self.recent.clear();
            self.high_since = None;
            return None;
        }
        self.recent.push_back((now, p50_ms));
        while self.recent.front().is_some_and(|(t, _)| now.duration_since(*t) > RECENT_WINDOW) {
            self.recent.pop_front();
        }
        let base = self.mins.iter().map(|(_, m)| *m).fold(f64::INFINITY, f64::min);
        let mut recent: Vec<f64> = self.recent.iter().map(|(_, l)| *l).collect();
        recent.sort_unstable_by(f64::total_cmp);
        let queue = recent[recent.len() / 2] - base;
        self.last_queue = queue;
        if queue > QUEUE_HIGH_MS {
            let since = *self.high_since.get_or_insert(now);
            return if now.duration_since(since) >= HIGH_FOR {
                let to = (self.bitrate * DECREASE).min(sent_bps * DECREASE_SENT);
                let changed = self.decrease(now, to.max(self.bitrate * DECREASE_MAX));
                if changed.is_some() {
                    self.ceiling = Some((now, sent_bps));
                }
                changed
            } else {
                None
            };
        }
        self.high_since = None;
        let calm = self.last_decrease.is_none_or(|t| now.duration_since(t) > INCREASE_AFTER_DECREASE);
        if queue < QUEUE_LOW_MS
            && calm
            && sent_bps >= self.bitrate * INCREASE_SENT
            && now.duration_since(self.last_increase) > INCREASE_INTERVAL
        {
            self.last_increase = now;
            let mut to = self.bitrate * INCREASE;
            if let Some((at, rate)) = self.ceiling
                && now.duration_since(at) < CEILING_HOLD
            {
                to = to.min(rate * CEILING_SHARE);
            }
            return if to > self.bitrate { self.set_at(now, to) } else { None };
        }
        None
    }

    /// Video as datagrams: frames the parity could not rebuild. On a link that drops
    /// what does not fit (a full queue) they come before the latency rises for long,
    /// and each one freezes the image until a keyframe: down at once, as for a lasting
    /// queue, and the rate sent becomes the ceiling. Only with a queue building up:
    /// random losses (Wi-Fi, mobile) are not cured by sending less, and lowering the
    /// bitrate for them only made the image worse (2% random loss: 3.4 → 0.8 Mbit/s).
    pub fn on_loss(&mut self, now: Instant, frames_lost: u32, sent_bps: f64) -> Option<u32> {
        if frames_lost == 0 || self.last_queue < QUEUE_LOSS_MS || self.settle_until.is_some_and(|t| now < t) {
            return None;
        }
        let sent = if sent_bps.is_finite() { sent_bps } else { self.bitrate };
        let to = (self.bitrate * DECREASE).min(sent * DECREASE_SENT);
        let changed = self.decrease(now, to.max(self.bitrate * DECREASE_MAX));
        if changed.is_some() {
            self.ceiling = Some((now, sent));
        }
        changed
    }

    /// No feedback while frames are being sent: treated as a high queue.
    pub fn on_silence(&mut self, now: Instant) -> Option<u32> {
        self.decrease(now, self.bitrate * DECREASE)
    }

    fn decrease(&mut self, now: Instant, bps: f64) -> Option<u32> {
        if self.last_decrease.is_some_and(|t| now.duration_since(t) < DECREASE_INTERVAL) {
            return None;
        }
        self.last_decrease = Some(now);
        self.set_at(now, bps)
    }

    fn set(&mut self, bps: f64) -> Option<u32> {
        self.set_at(Instant::now(), bps)
    }

    /// Feeds the same report every 250 ms (tests).
    #[cfg(test)]
    fn feed(
        &mut self,
        from: Instant,
        ms: std::ops::Range<u64>,
        p50: f64,
        min: f64,
        sent: f64,
    ) -> Option<u32> {
        let mut changed = None;
        for at in ms.step_by(250) {
            if let Some(b) = self.on_feedback(from + Duration::from_millis(at), p50, min, sent) {
                changed = Some(b);
            }
        }
        changed
    }

    fn set_at(&mut self, now: Instant, bps: f64) -> Option<u32> {
        let new = bps.clamp(self.min, self.max);
        if (new - self.bitrate).abs() < 1.0 {
            return None;
        }
        self.bitrate = new;
        self.settle_until = Some(now + SETTLE);
        // Measurements from before the change no longer describe the network.
        self.recent.clear();
        self.high_since = None;
        Some(new as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sent bitrate high enough for every bitrate in the tests.
    const BUSY: f64 = 10_000_000.0;

    #[test]
    fn decreases_only_on_a_lasting_queue_and_recovers() {
        let t0 = Instant::now();
        let t = |ms| t0 + Duration::from_millis(ms);
        let mut abr = Abr::new(4_000_000, 500_000, 8_000_000);
        // Free network: latency steady at 20 ms, the bitrate rises by 15%.
        assert_eq!(abr.feed(t0, 0..1300, 20.0, 15.0, BUSY), Some(4_600_000));
        // While settling (1.5 s) measurements do not count.
        assert_eq!(abr.feed(t0, 1300..2700, 500.0, 400.0, BUSY), None);
        // Queue: 150 ms over a minimum of 15. Nothing for the first second...
        assert_eq!(abr.feed(t0, 2750..3700, 150.0, 140.0, BUSY), None);
        // ...then down.
        assert_eq!(abr.on_feedback(t(3750), 150.0, 140.0, BUSY), Some(3_680_000));
        // After a decrease no increases for 3 s, even if the queue empties.
        assert_eq!(abr.feed(t0, 5300..6700, 20.0, 15.0, BUSY), None);
        assert!(abr.feed(t0, 6800..7100, 20.0, 15.0, BUSY).is_some());
    }

    #[test]
    fn mobile_peaks_are_not_a_queue() {
        let t0 = Instant::now();
        let mut abr = Abr::new(3_200_000, 500_000, 8_000_000);
        // As measured on the mobile network: mostly 10–30 ms, one report out of
        // three at 300 ms, while sending 0.3 Mbit/s.
        for i in 0..40u64 {
            let p50 = if i % 3 == 2 { 300.0 } else { 20.0 };
            if let Some(b) = abr.on_feedback(t0 + Duration::from_millis(i * 250), p50, p50, 300_000.0) {
                assert!(b > 3_200_000, "lowered to {b} on single peaks");
            }
        }
    }

    #[test]
    fn still_screen_changes_nothing() {
        let t0 = Instant::now();
        let mut abr = Abr::new(4_000_000, 500_000, 8_000_000);
        // Latency jumps but only 1 kbit/s is sent: no decrease, no increase.
        assert_eq!(abr.feed(t0, 0..3000, 185.0, 30.0, 1_000.0), None);
        assert_eq!(abr.feed(t0, 3000..5000, 20.0, 15.0, 1_000.0), None);
        assert_eq!(abr.bitrate(), 4_000_000);
        // The screen moves: even simple content at 800 kbit/s (a fifth of the bitrate)
        // can fill a slow link, so a lasting queue counts again. 85% of what is sent
        // would be 680 kbit/s, but one step goes at most to half the bitrate.
        assert_eq!(abr.feed(t0, 5000..6300, 185.0, 150.0, 800_000.0), Some(2_000_000));
    }

    #[test]
    fn decrease_follows_the_rate_sent() {
        let t0 = Instant::now();
        let mut abr = Abr::new(2_000_000, 500_000, 8_000_000);
        // 1.2 Mbit/s actually sent into a lasting queue: 85% of it, below the 80% cap.
        assert_eq!(abr.feed(t0, 0..1300, 200.0, 20.0, 1_200_000.0), Some(1_020_000));
    }

    #[test]
    fn no_increase_while_the_bitrate_is_not_used() {
        let t0 = Instant::now();
        let mut abr = Abr::new(4_000_000, 500_000, 8_000_000);
        // A nearly still screen sends 0.3 Mbit/s on a calm network: the cap stays.
        assert_eq!(abr.feed(t0, 0..5000, 20.0, 15.0, 300_000.0), None);
        // Using it (more than half) lets it rise.
        assert_eq!(abr.feed(t0, 5000..6300, 20.0, 15.0, 3_000_000.0), Some(4_600_000));
    }

    #[test]
    fn after_a_congestion_stays_under_the_rate_that_filled_the_queue() {
        let t0 = Instant::now();
        let t = |ms| t0 + Duration::from_millis(ms);
        let mut abr = Abr::new(2_400_000, 500_000, 8_000_000);
        // A 2 Mbit/s link: 2 Mbit/s go out and the queue grows. Down to 85% of it.
        assert_eq!(abr.feed(t0, 0..1300, 200.0, 40.0, 2_000_000.0), Some(1_700_000));
        // Calm again, the whole bitrate used: up only to 90% of 2 Mbit/s, then no more.
        assert_eq!(abr.feed(t0, 3000..6000, 45.0, 40.0, 1_700_000.0), Some(1_800_000));
        assert_eq!(abr.feed(t0, 6000..30_000, 45.0, 40.0, 1_800_000.0), None);
        // After 30 s it probes again.
        let probed = abr.on_feedback(t(31_500), 45.0, 40.0, 1_800_000.0);
        assert!(probed.is_some_and(|b| b.abs_diff(2_070_000) <= 1), "{probed:?}");
    }

    #[test]
    fn lost_frames_lower_the_bitrate_at_once() {
        let t0 = Instant::now();
        let t = |ms| t0 + Duration::from_millis(ms);
        let mut abr = Abr::new(4_000_000, 500_000, 8_000_000);
        // Random losses, no queue: nothing changes.
        assert_eq!(abr.on_feedback(t(0), 45.0, 40.0, 3_000_000.0), None);
        assert_eq!(abr.on_loss(t(0), 3, 3_000_000.0), None);
        // A queue building up (100 ms) and frames lost while 2.4 Mbit/s go out: 85% of
        // that at once, without waiting for the queue to last a second.
        assert_eq!(abr.on_feedback(t(0), 140.0, 40.0, 2_400_000.0), None);
        assert_eq!(abr.on_loss(t(0), 0, 3_000_000.0), None);
        assert_eq!(abr.on_loss(t(0), 3, 2_400_000.0), Some(2_040_000));
        // While settling, the losses of the old rate do not count again.
        assert_eq!(abr.on_loss(t(1000), 2, 2_000_000.0), None);
        // No rise past 90% of the rate that was losing frames.
        assert_eq!(abr.feed(t0, 4000..9000, 45.0, 40.0, 2_000_000.0), Some(2_160_000));
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
