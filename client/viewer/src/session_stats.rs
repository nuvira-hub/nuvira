//! The viewer's part of the optional statistics log (`NUVIRA_STATS`, see
//! `nuvira_transport::stats` and docs/48-statistics-log.md): counters updated by the
//! network, decoding and window code only when it is on, read once per interval by the
//! recorder thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nuvira_transport::stats::{Histogram, Obj, Snapshot, Source, Stat};

use crate::net::FrameSlot;

/// Counters of one session.
#[derive(Default)]
pub struct ViewerStats {
    pub frames_received: AtomicU64,
    pub bytes_received: AtomicU64,
    pub frames_decoded: AtomicU64,
    pub frames_shown: AtomicU64,
    pub freezes: AtomicU64,
    pub freeze_ms: AtomicU64,
    /// Capture (agent) to display (here), with the clock offset of the pings.
    pub latency: Histogram,
    /// End of encoding (agent) to arrival of the whole frame (here).
    pub transit: Histogram,
    pub decode: Histogram,
    pub render: Histogram,
    /// Time the network task waited for the decoding queue to take a frame.
    pub decode_queue_wait_us: AtomicU64,
    /// Last ping round trip + 1 (0: none yet).
    rtt_us: AtomicU64,
    /// Video as datagrams.
    pub shards: AtomicU64,
    pub shards_lost: AtomicU64,
    pub frames_rebuilt: AtomicU64,
    pub frames_lost: AtomicU64,
    info: Mutex<Info>,
}

#[derive(Default, Clone)]
struct Info {
    decoder: Option<String>,
    size: Option<(u32, u32)>,
}

impl ViewerStats {
    pub fn rtt(&self, rtt_us: u64) {
        self.rtt_us.store(rtt_us.saturating_add(1), Ordering::Relaxed);
    }

    /// The decoder in use and the first picture's size (the first ones go in the header).
    pub fn decoder(&self, name: &str) {
        self.info.lock().expect("stats info").decoder.get_or_insert_with(|| name.to_string());
    }

    pub fn size(&self, width: u32, height: u32) {
        self.info.lock().expect("stats info").size.get_or_insert((width, height));
    }
}

/// What the recorder needs besides the counters.
pub struct ViewerSource {
    pub stats: Arc<ViewerStats>,
    pub conn: quinn::Connection,
    pub slot: Arc<FrameSlot>,
    /// Video as datagrams (with error correction): the `fec` object is filled.
    pub datagrams: bool,
    pub last: Last,
}

/// Values of the previous interval, for differences, and the session's totals.
#[derive(Default)]
pub struct Last {
    received: u64,
    bytes: u64,
    decoded: u64,
    shown: u64,
    dropped: u64,
    freezes: u64,
    freeze_ms: u64,
    wait_us: u64,
    shards: u64,
    shards_lost: u64,
    rebuilt: u64,
    lost: u64,
    quic_lost: Option<u64>,
    latency_total: Snapshot,
}

fn delta(counter: &AtomicU64, last: &mut u64) -> u64 {
    let now = counter.load(Ordering::Relaxed);
    now.saturating_sub(std::mem::replace(last, now))
}

fn kbps(bytes: u64, dt: Duration) -> u64 {
    (bytes as f64 * 8.0 / dt.as_secs_f64().max(1e-3) / 1000.0).round() as u64
}

impl Source for ViewerSource {
    fn header(&mut self, o: &mut Obj<'_>) {
        let info = self.stats.info.lock().expect("stats info").clone();
        o.opt_str("decoder", info.decoder.as_deref())
            .opt_u64("width", info.size.map(|s| u64::from(s.0)))
            .opt_u64("height", info.size.map(|s| u64::from(s.1)));
    }

    fn interval(&mut self, o: &mut Obj<'_>, dt: Duration) {
        let s = &*self.stats;
        let l = &mut self.last;
        let bytes = delta(&s.bytes_received, &mut l.bytes);
        let dropped = self.slot.dropped();
        let latency = s.latency.take();
        l.latency_total.merge(&latency);
        let (transit, decode, render) = (s.transit.take(), s.decode.take(), s.render.take());
        o.u64("frames_received", delta(&s.frames_received, &mut l.received))
            .u64("frames_decoded", delta(&s.frames_decoded, &mut l.decoded))
            .u64("frames_shown", delta(&s.frames_shown, &mut l.shown))
            .u64("frames_dropped_slot", dropped.saturating_sub(std::mem::replace(&mut l.dropped, dropped)))
            .hist("latency_ms", &latency, &[Stat::P50, Stat::P95, Stat::P99, Stat::Max])
            .hist("transit_ms", &transit, &[Stat::P50, Stat::P95])
            .hist("decode_ms", &decode, &[Stat::P50, Stat::P95])
            .hist("render_ms", &render, &[Stat::P50, Stat::P95])
            .f64("decode_queue_wait_ms", delta(&s.decode_queue_wait_us, &mut l.wait_us) as f64 / 1000.0)
            .u64("freezes", delta(&s.freezes, &mut l.freezes))
            .u64("freeze_ms", delta(&s.freeze_ms, &mut l.freeze_ms))
            .opt_f64("rtt_ms", s.rtt_us.load(Ordering::Relaxed).checked_sub(1).map(|us| us as f64 / 1000.0))
            .u64("kbps_received", kbps(bytes, dt));
        let fec = self.datagrams.then_some(());
        o.opt_obj("fec", fec, |o, ()| {
            o.u64("shards", delta(&s.shards, &mut l.shards))
                .u64("shards_lost", delta(&s.shards_lost, &mut l.shards_lost))
                .u64("frames_rebuilt", delta(&s.frames_rebuilt, &mut l.rebuilt))
                .u64("frames_lost", delta(&s.frames_lost, &mut l.lost));
        });
        let q = self.conn.stats();
        let lost = q.path.lost_packets.saturating_sub(l.quic_lost.replace(q.path.lost_packets).unwrap_or(0));
        o.obj("quic", |o| {
            o.f64("rtt_ms", q.path.rtt.as_secs_f64() * 1000.0).u64("lost_packets", lost);
        });
    }

    fn summary(&mut self, o: &mut Obj<'_>, seconds: f64) {
        let l = &self.last;
        let secs = Duration::from_secs_f64(seconds.max(1e-3));
        o.u64("frames", l.shown)
            .f64("fps", l.shown as f64 / secs.as_secs_f64())
            .hist("latency_ms", &l.latency_total, &[Stat::P50, Stat::P95, Stat::Max])
            .u64("kbps", kbps(l.bytes, secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_unknown_until_measured() {
        let s = ViewerStats::default();
        assert_eq!(s.rtt_us.load(Ordering::Relaxed).checked_sub(1), None);
        s.rtt(0);
        assert_eq!(s.rtt_us.load(Ordering::Relaxed).checked_sub(1), Some(0));
        s.rtt(1300);
        assert_eq!(s.rtt_us.load(Ordering::Relaxed).checked_sub(1), Some(1300));
    }

    #[test]
    fn first_decoder_and_size_are_kept() {
        let s = ViewerStats::default();
        s.decoder("mf");
        s.decoder("openh264");
        s.size(1920, 1080);
        s.size(64, 64);
        let info = s.info.lock().unwrap().clone();
        assert_eq!((info.decoder.as_deref(), info.size), (Some("mf"), Some((1920, 1080))));
    }

    #[test]
    fn rates() {
        assert_eq!(kbps(611_250, Duration::from_secs(1)), 4890);
        assert_eq!(kbps(0, Duration::ZERO), 0);
    }
}
