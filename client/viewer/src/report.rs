//! Session results for automated tests (`--stats-file`).
//!
//! The JSON file is written when the viewer exits and read back by the test
//! script: it tells whether the session started, how many frames arrived and with what latency.

use std::time::Instant;

#[derive(Default)]
pub struct Report {
    pub renderer: Option<String>,
    pub agent: Option<String>,
    pub resolution: Option<(u32, u32)>,
    pub frames: u64,
    pub bytes: u64,
    pub encode_us: u64,
    pub decode_us: u64,
    /// Time to upload and draw the frames shown.
    pub render_us: u64,
    /// Frames decoded but overtaken by a more recent one before being shown.
    pub dropped: u64,
    /// Capture→display latency of every frame, when the clocks are aligned.
    pub latencies_us: Vec<u32>,
    pub rtt_us: Vec<u32>,
    pub first_frame_ms: Option<u64>,
    /// Pauses between two frames shown longer than [`FREEZE`], and their total time:
    /// with a screen that keeps moving (the tests' animation) each is a visible freeze,
    /// which the latency of the frames that do arrive does not show.
    pub freezes: u64,
    pub freeze_ms: u64,
    last_shown: Option<Instant>,
    pub exit_reason: Option<String>,
    /// Session ended by the test duration and not by an error.
    pub completed: bool,
}

/// A longer pause between two frames shown counts as a freeze.
pub const FREEZE: std::time::Duration = std::time::Duration::from_millis(200);

fn percentile(sorted: &[u32], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    Some(sorted[i] as f64 / 1000.0)
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("null".into(), |v| v.to_string())
}

impl Report {
    /// A frame was shown now. Returns the pause before it if it was a freeze.
    pub fn shown(&mut self, now: Instant) -> Option<std::time::Duration> {
        let gap = now.duration_since(self.last_shown.replace(now)?);
        if gap <= FREEZE {
            return None;
        }
        self.freezes += 1;
        self.freeze_ms += gap.as_millis() as u64;
        Some(gap)
    }

    pub fn to_json(&self, started: Instant) -> String {
        let secs = started.elapsed().as_secs_f64();
        let streaming_secs = secs - self.first_frame_ms.map_or(secs, |ms| ms as f64 / 1000.0);
        let mut lat = self.latencies_us.clone();
        lat.sort_unstable();
        let mut rtt = self.rtt_us.clone();
        rtt.sort_unstable();
        let per_frame = |total: u64| (self.frames > 0).then(|| total as f64 / self.frames as f64 / 1000.0);
        let fields = [
            ("completed", self.completed.to_string()),
            ("exit_reason", self.exit_reason.as_deref().map_or("null".into(), json_str)),
            ("renderer", self.renderer.as_deref().map_or("null".into(), json_str)),
            ("agent", self.agent.as_deref().map_or("null".into(), json_str)),
            ("width", json_opt(self.resolution.map(|r| r.0))),
            ("height", json_opt(self.resolution.map(|r| r.1))),
            ("seconds", format!("{secs:.2}")),
            ("first_frame_ms", json_opt(self.first_frame_ms)),
            ("frames", self.frames.to_string()),
            // From the first frame on: session start-up does not count.
            ("fps", format!("{:.1}", self.frames as f64 / streaming_secs.max(0.001))),
            ("mbit_s", format!("{:.2}", self.bytes as f64 * 8.0 / secs.max(0.001) / 1e6)),
            ("latency_ms_p50", json_opt(percentile(&lat, 0.5))),
            ("latency_ms_p95", json_opt(percentile(&lat, 0.95))),
            ("latency_ms_max", json_opt(percentile(&lat, 1.0))),
            ("rtt_ms_p50", json_opt(percentile(&rtt, 0.5))),
            ("encode_ms_avg", json_opt(per_frame(self.encode_us).map(|v| format!("{v:.2}")))),
            ("decode_ms_avg", json_opt(per_frame(self.decode_us).map(|v| format!("{v:.2}")))),
            ("render_ms_avg", json_opt(per_frame(self.render_us).map(|v| format!("{v:.2}")))),
            ("dropped", self.dropped.to_string()),
            ("freezes", self.freezes.to_string()),
            ("freeze_ms", self.freeze_ms.to_string()),
        ];
        let body: Vec<String> = fields.iter().map(|(k, v)| format!("  \"{k}\": {v}")).collect();
        format!("{{\n{}\n}}\n", body.join(",\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_well_formed() {
        let r = Report {
            frames: 3,
            latencies_us: vec![30_000, 10_000, 20_000],
            exit_reason: Some("end \"test\"".into()),
            completed: true,
            ..Default::default()
        };
        let json = r.to_json(Instant::now());
        assert!(json.contains("\"latency_ms_p50\": 20"));
        assert!(json.contains("\"exit_reason\": \"end \\\"test\\\"\""));
        assert!(json.contains("\"renderer\": null"));
    }

    #[test]
    fn long_pauses_are_freezes() {
        let mut r = Report::default();
        let t0 = Instant::now();
        let ms = std::time::Duration::from_millis;
        for at in [0, 20, 40, 60, 400, 420, 1000] {
            r.shown(t0 + ms(at));
        }
        assert_eq!((r.freezes, r.freeze_ms), (2, 340 + 580));
    }
}
