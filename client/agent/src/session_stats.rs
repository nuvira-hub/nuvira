//! The agent's part of the optional statistics log (`NUVIRA_STATS`, see
//! `nuvira_transport::stats` and docs/48-statistics-log.md): counters updated by the
//! pipeline and the session only when it is on, read once per interval by the
//! recorder thread.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nuvira_proto::v1::Feedback;
use nuvira_transport::stats::{Histogram, Obj, Snapshot, Source, Stat};

/// Counters of one session; shared by the capture and encoding threads, the video
/// task and the control loop.
#[derive(Default)]
pub(crate) struct AgentStats {
    pub frames_captured: AtomicU64,
    /// Captured pictures overwritten in the slot before the encoder took them.
    pub frames_skipped: AtomicU64,
    pub frames_sent: AtomicU64,
    pub keyframes: AtomicU64,
    pub keyframe_requests: AtomicU64,
    /// From the picture handed over by the OS to its copy in the encoder's slot.
    pub capture: Histogram,
    pub encode: Histogram,
    /// Time the encoding thread waited for the network to take a frame.
    pub send_blocked_us: AtomicU64,
    /// Target of the adaptive bitrate (bit/s); 0 with a fixed bitrate.
    pub abr_target_bps: AtomicU32,
    feedback: Mutex<FeedbackSum>,
    info: Mutex<Info>,
}

#[derive(Default, Clone)]
struct Info {
    encoder: Option<String>,
    size: Option<(u32, u32)>,
}

/// The viewer's reports (`Feedback`, every ~250 ms) of one interval.
#[derive(Default, Clone, Copy)]
struct FeedbackSum {
    reports: u64,
    latency_p50_ms: u32,
    latency_min_ms: u32,
    transit_p50_ms: Option<u32>,
    frames_lost: Option<u64>,
    shards_lost: Option<u64>,
}

impl AgentStats {
    /// The pipeline started: its encoder and screen (the first ones go in the header).
    pub fn started(&self, encoder: &str, width: u32, height: u32) {
        let mut info = self.info.lock().expect("stats info");
        info.encoder.get_or_insert_with(|| encoder.to_string());
        info.size.get_or_insert((width, height));
    }

    pub fn feedback(&self, fb: &Feedback) {
        let mut f = self.feedback.lock().expect("stats feedback");
        f.reports += 1;
        f.latency_p50_ms = fb.latency_ms_p50;
        f.latency_min_ms = fb.latency_ms_min;
        f.transit_p50_ms = fb.transit_ms_p50;
        let add = |sum: Option<u64>, n: Option<u32>| n.map(|n| sum.unwrap_or(0) + u64::from(n)).or(sum);
        f.frames_lost = add(f.frames_lost, fb.frames_lost);
        f.shards_lost = add(f.shards_lost, fb.shards_lost);
    }
}

/// What the recorder needs besides the counters.
pub(crate) struct AgentSource {
    pub stats: Arc<AgentStats>,
    pub conn: quinn::Connection,
    pub plan: String,
    pub fps_max: Arc<AtomicU32>,
    /// Video bytes on the network (the session's own counter).
    pub sent_bytes: Arc<AtomicU64>,
    /// Parity per data shard in thousandths, with the video as datagrams.
    pub fec_permille: Option<Arc<AtomicU32>>,
    pub adaptive: bool,
    pub last: Last,
}

/// Values of the previous interval, for differences.
#[derive(Default)]
pub(crate) struct Last {
    captured: u64,
    skipped: u64,
    sent: u64,
    keyframes: u64,
    requests: u64,
    blocked_us: u64,
    bytes: u64,
    quic: Option<quinn::ConnectionStats>,
    total_frames: u64,
    first_bytes: Option<u64>,
}

fn delta(counter: &AtomicU64, last: &mut u64) -> u64 {
    let now = counter.load(Ordering::Relaxed);
    now.saturating_sub(std::mem::replace(last, now))
}

fn kbps(bytes: u64, dt: Duration) -> u64 {
    (bytes as f64 * 8.0 / dt.as_secs_f64().max(1e-3) / 1000.0).round() as u64
}

impl Source for AgentSource {
    fn header(&mut self, o: &mut Obj<'_>) {
        let info = self.stats.info.lock().expect("stats info").clone();
        o.opt_str("encoder", info.encoder.as_deref())
            .str("plan", &self.plan)
            .opt_u64("width", info.size.map(|s| u64::from(s.0)))
            .opt_u64("height", info.size.map(|s| u64::from(s.1)))
            .u64("fps_max", u64::from(self.fps_max.load(Ordering::Relaxed)));
    }

    fn interval(&mut self, o: &mut Obj<'_>, dt: Duration) {
        let s = &*self.stats;
        let l = &mut self.last;
        let sent = delta(&s.frames_sent, &mut l.sent);
        l.total_frames += sent;
        let bytes_now = self.sent_bytes.load(Ordering::Relaxed);
        l.first_bytes.get_or_insert(bytes_now);
        let bytes = bytes_now.saturating_sub(std::mem::replace(&mut l.bytes, bytes_now));
        let (capture, encode): (Snapshot, Snapshot) = (s.capture.take(), s.encode.take());
        o.u64("frames_captured", delta(&s.frames_captured, &mut l.captured))
            .u64("frames_sent", sent)
            .u64("frames_skipped", delta(&s.frames_skipped, &mut l.skipped))
            .u64("keyframes", delta(&s.keyframes, &mut l.keyframes))
            .u64("keyframe_requests", delta(&s.keyframe_requests, &mut l.requests))
            .hist("capture_ms", &capture, &[Stat::P50, Stat::P95, Stat::Max])
            .hist("encode_ms", &encode, &[Stat::P50, Stat::P95, Stat::Max])
            .f64("send_blocked_ms", delta(&s.send_blocked_us, &mut l.blocked_us) as f64 / 1000.0)
            .u64("bytes_sent", bytes)
            .u64("kbps_sent", kbps(bytes, dt))
            .opt_u64(
                "abr_target_kbps",
                self.adaptive.then(|| u64::from(s.abr_target_bps.load(Ordering::Relaxed)) / 1000),
            )
            .opt_f64(
                "fec_ratio",
                self.fec_permille.as_ref().map(|p| f64::from(p.load(Ordering::Relaxed)) / 1000.0),
            );
        let fb = std::mem::take(&mut *s.feedback.lock().expect("stats feedback"));
        o.opt_obj("feedback", (fb.reports > 0).then_some(fb), |o, fb| {
            o.u64("reports", fb.reports)
                .u64("latency_p50_ms", u64::from(fb.latency_p50_ms))
                .u64("latency_min_ms", u64::from(fb.latency_min_ms))
                .opt_u64("transit_p50_ms", fb.transit_p50_ms.map(u64::from))
                .opt_u64("frames_lost", fb.frames_lost)
                .opt_u64("shards_lost", fb.shards_lost);
        });
        let q = self.conn.stats();
        let prev = l.quic.replace(q).unwrap_or_default();
        o.obj("quic", |o| {
            o.f64("rtt_ms", q.path.rtt.as_secs_f64() * 1000.0)
                .u64("cwnd", q.path.cwnd)
                .u64("sent_packets", q.path.sent_packets.saturating_sub(prev.path.sent_packets))
                .u64("lost_packets", q.path.lost_packets.saturating_sub(prev.path.lost_packets))
                .u64(
                    "congestion_events",
                    q.path.congestion_events.saturating_sub(prev.path.congestion_events),
                )
                .u64("udp_tx_bytes", q.udp_tx.bytes.saturating_sub(prev.udp_tx.bytes));
        });
    }

    fn summary(&mut self, o: &mut Obj<'_>, seconds: f64) {
        let l = &self.last;
        let bytes = l.bytes.saturating_sub(l.first_bytes.unwrap_or(l.bytes));
        let secs = Duration::from_secs_f64(seconds.max(1e-3));
        o.u64("frames", l.total_frames)
            .f64("fps", l.total_frames as f64 / secs.as_secs_f64())
            // The agent gets only the viewer's medians (in the intervals), not the distribution.
            .null("latency_ms")
            .u64("kbps", kbps(bytes, secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_sums_losses_and_keeps_the_last_latency() {
        let s = AgentStats::default();
        s.feedback(&Feedback { latency_ms_p50: 30, latency_ms_min: 20, frames: 15, ..Default::default() });
        let f = *s.feedback.lock().unwrap();
        // Stream mode: no losses reported, they stay unknown.
        assert_eq!((f.reports, f.frames_lost, f.shards_lost, f.transit_p50_ms), (1, None, None, None));
        s.feedback(&Feedback {
            latency_ms_p50: 31,
            latency_ms_min: 22,
            transit_ms_p50: Some(4),
            frames_lost: Some(1),
            shards_lost: Some(3),
            ..Default::default()
        });
        s.feedback(&Feedback {
            latency_ms_p50: 33,
            frames_lost: Some(2),
            shards_lost: Some(0),
            ..Default::default()
        });
        let f = *s.feedback.lock().unwrap();
        assert_eq!((f.reports, f.latency_p50_ms, f.frames_lost, f.shards_lost), (3, 33, Some(3), Some(3)));
    }

    #[test]
    fn first_encoder_and_size_are_kept() {
        let s = AgentStats::default();
        s.started("openh264", 1920, 1080);
        s.started("other", 1280, 720);
        let info = s.info.lock().unwrap().clone();
        assert_eq!((info.encoder.as_deref(), info.size), (Some("openh264"), Some((1920, 1080))));
    }

    #[test]
    fn rates() {
        assert_eq!(kbps(612_345, Duration::from_secs(1)), 4899);
        let (c, mut last) = (AtomicU64::new(10), 4);
        assert_eq!(delta(&c, &mut last), 6);
        assert_eq!(delta(&c, &mut last), 0);
    }
}
