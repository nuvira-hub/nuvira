//! Real agent session in loopback (plan 7.2 B-MACRO-03/04): the production agent
//! (`nuvira_agent::serve_reverse` on a 127.0.0.1 endpoint, or `serve_direct` with
//! `NUVIRA_BENCH_AGENT_ENTRY=direct`, see [`AgentEntry`]) on the synthetic source,
//! in this process on its own runtime (agent side), and the [`HeadlessViewer`] on
//! the viewer runtime. B-MACRO-04 puts the netem proxy (separate process) between
//! them, with the adaptive bitrate on.
//!
//! One session per run (`ctx.reps` sessions when more are asked): first frame,
//! then the warm-up, then the measured window ([`macro_durations`]). Quality: up to
//! 40 decoded frames of the window are scored against the synthetic source
//! rebuilt from the bar position found in the decoded luma (plan 5.2, 6.4); with
//! more than half of them unaligned the result is `invalid_reference` (the
//! synthetic source changed: not a regression).

use std::time::Duration;

use anyhow::{Context, Result};
use nuvira_codec::{EncoderPlan, EncoderSettings};

use crate::fixtures::synthetic::{frame_with_bar, locate_bar};
use crate::measure::{Better, Distribution, Gate, Metric};
use crate::net::headless::{HeadlessViewer, SessionStats, ViewerOptions};
use crate::net::netem::{Counts, Netem, profile_spec};
use crate::net::{
    AgentEntry, HandshakeInfo, LoopbackAgent, agent_runtime, macro_durations, synthetic_config,
    viewer_runtime,
};
use crate::quality::{Reference, psnr, ssim_y};
use crate::registry::{BOTH_TIERS, Case, CaseResult, FULL_ONLY, Level, Registry, RunCtx, Status, Tier};
use crate::resources::ProcSample;

const MAX_SAMPLES: usize = 40;

pub struct SessionCase {
    pub id: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hardware: bool,
    pub datagrams: bool,
    pub fixed_bitrate: bool,
    pub profile: Option<&'static str>,
    pub tiers: &'static [Tier],
}

pub fn register(reg: &mut Registry) {
    let sizes = [(1280u32, 720u32, 30u32), (1920, 1080, 60), (2560, 1440, 60), (3840, 2160, 30)];
    for (w, h, fps) in sizes {
        for hardware in [false, true] {
            for datagrams in [false, true] {
                for fixed_bitrate in [true, false] {
                    // Full tier: every transport x rate mode at 1080p, stream + fixed
                    // at the other sizes (time budget, plan section 13).
                    if (w, h) != (1920, 1080) && (datagrams || !fixed_bitrate) {
                        continue;
                    }
                    let quick = (w, h) == (1920, 1080) && !datagrams && fixed_bitrate;
                    reg.add_case(SessionCase {
                        id: "B-MACRO-03",
                        width: w,
                        height: h,
                        fps,
                        hardware,
                        datagrams,
                        fixed_bitrate,
                        profile: None,
                        tiers: if quick { BOTH_TIERS } else { FULL_ONLY },
                    });
                }
            }
        }
    }
    for (p, datagrams) in [
        ("N1", false),
        ("N2", false),
        ("N3", false),
        ("N4", false),
        ("N5", false),
        ("N3", true),
        ("N5", true),
    ] {
        reg.add_case(SessionCase {
            id: "B-MACRO-04",
            width: 1920,
            height: 1080,
            fps: 60,
            hardware: false,
            datagrams,
            fixed_bitrate: false,
            profile: Some(p),
            tiers: FULL_ONLY,
        });
    }
}

impl Case for SessionCase {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> String {
        let mut n = format!(
            "{}/{}x{}@{}/{}/{}",
            if self.hardware { "hw" } else { "sw" },
            self.width,
            self.height,
            self.fps,
            if self.datagrams { "datagrams" } else { "stream" },
            if self.fixed_bitrate { "fixed" } else { "abr" }
        );
        if let Some(p) = self.profile {
            n.push('/');
            n.push_str(p);
        }
        n
    }

    fn level(&self) -> Level {
        Level::Macro
    }

    fn tiers(&self) -> &'static [Tier] {
        self.tiers
    }

    fn config_ids(&self) -> &'static [&'static str] {
        if self.profile.is_some() { &["NET-DEGRADED"] } else { &[] }
    }

    fn runnable(&self) -> Result<(), String> {
        Ok(())
    }

    fn isolated(&self) -> bool {
        true
    }

    fn run(&mut self, ctx: &mut RunCtx) -> Result<CaseResult> {
        crate::net::isolate_data_dir(&ctx.tmp.join("data"))?;
        let config = synthetic_config(self.width, self.height, self.fps, self.hardware, self.fixed_bitrate);
        if self.hardware {
            let settings = EncoderSettings {
                bitrate_bps: config.bitrate_kbps * 1000,
                max_fps: self.fps,
                ..Default::default()
            };
            if let Err(e) = nuvira_codec::create_encoder(EncoderPlan::Hardware, settings, None) {
                return Ok(CaseResult::not_runnable(self, format!("no hardware encoder: {e:#}")));
            }
        }
        let entry = AgentEntry::from_env();
        let (warm, measure) = macro_durations(ctx.tier);
        let mut runs = Vec::new();
        for k in 0..ctx.reps.max(1) {
            let dir = ctx.tmp.join(format!("session-{k}"));
            runs.push(one_session(self, config.clone(), entry, &dir, warm, measure)?);
        }
        let mut r = CaseResult::new(self);
        r.param("width", self.width)
            .param("height", self.height)
            .param("fps", self.fps)
            .param("plan", format!("{:?}", config.plan))
            .param("bitrate_kbps", config.bitrate_kbps)
            .param("fixed_bitrate", self.fixed_bitrate)
            .param("mode", if self.datagrams { "datagrams" } else { "stream" })
            .param("agent_entry", entry.name())
            .param("warmup_s", warm.as_secs_f64())
            .param("measure_s", measure.as_secs_f64())
            .param("sessions", runs.len());
        if let Some(p) = self.profile {
            r.param("profile", p).param("netem_spec", profile_spec(p));
        }
        fill(&mut r, &runs, self);
        Ok(r)
    }
}

struct Outcome {
    stats: SessionStats,
    handshake: HandshakeInfo,
    first_frame_ms: f64,
    proc_end: ProcSample,
    psnr_yuv: Vec<f64>,
    psnr_y: Vec<f64>,
    ssim: Vec<f64>,
    unaligned: usize,
    netem: Option<Counts>,
    agent_note: Option<String>,
    datagrams_granted: bool,
}

fn one_session(
    case: &SessionCase,
    config: nuvira_agent::AgentConfig,
    entry: AgentEntry,
    dir: &std::path::Path,
    warm: Duration,
    measure: Duration,
) -> Result<Outcome> {
    let agent_rt = agent_runtime(0)?;
    let viewer_rt = viewer_runtime(2)?;
    let agent = LoopbackAgent::start(&agent_rt, config, entry, dir, true)?;
    let netem = match case.profile {
        Some(p) => Some(Netem::spawn("127.0.0.1:0".parse()?, agent.addr, p)?),
        None => None,
    };
    let target = netem.as_ref().map_or(agent.addr, |n| n.addr);
    let opts = ViewerOptions {
        video_datagrams: case.datagrams,
        password: agent.password.clone(),
        quality_samples: MAX_SAMPLES,
        sample_every: ((measure.as_secs_f64() * case.fps as f64) as u32 / MAX_SAMPLES as u32).max(1),
        ..Default::default()
    };
    let fp = agent.fingerprint;
    let result = viewer_rt.block_on(async {
        let s = HeadlessViewer::connect_direct(target, Some(fp), opts).await?;
        let mut s = s;
        let first = s.first_frame(Duration::from_secs(20)).await?;
        let handshake = HandshakeInfo { times: s.times, agent_os: s.agent.os.clone() };
        let granted = s.datagrams();
        tokio::time::sleep(warm).await;
        let stats = s.run(measure).await;
        let proc_end = ProcSample::of_self();
        s.close().await;
        anyhow::Ok((stats?, handshake, first, proc_end, granted))
    });
    let agent_note = agent.finish(&agent_rt, Duration::from_secs(5)).err().map(|e| format!("agent: {e:#}"));
    viewer_rt.shutdown_timeout(Duration::from_secs(2));
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    let netem_counts = netem.map(|n| n.stop()).transpose()?;
    let (stats, handshake, first, proc_end, granted) = result.context("headless session")?;

    // Quality against the synthetic reference.
    let (w, h) = (case.width & !1, case.height & !1);
    let mut out = Outcome {
        first_frame_ms: first.as_secs_f64() * 1e3,
        stats,
        handshake,
        proc_end,
        psnr_yuv: Vec::new(),
        psnr_y: Vec::new(),
        ssim: Vec::new(),
        unaligned: 0,
        netem: netem_counts,
        agent_note,
        datagrams_granted: granted,
    };
    for s in out.stats.samples.iter() {
        let f = &s.frame;
        if (f.width, f.height) != (w, h) {
            out.unaligned += 1;
            continue;
        }
        let Some(bar) = locate_bar(&f.y, f.width, f.height, f.width as usize) else {
            out.unaligned += 1;
            continue;
        };
        let bgra = frame_with_bar(f.width, f.height, bar);
        let img = nuvira_codec::BgraImage {
            width: f.width,
            height: f.height,
            stride: f.width as usize * 4,
            data: &bgra,
        };
        let reference = Reference::from_bgra(&img);
        let p = psnr(f, &reference);
        out.psnr_yuv.push(p.yuv);
        out.psnr_y.push(p.y);
        out.ssim.push(ssim_y(f, &reference));
    }
    Ok(out)
}

fn fill(r: &mut CaseResult, runs: &[Outcome], case: &SessionCase) {
    let mut per = |name: &str, unit: &str, better: Better, gate: Gate, f: &dyn Fn(&Outcome) -> f64| {
        r.metric(name, Metric::from_values(unit, better, gate, runs.iter().map(f).collect()));
    };
    let frames = |o: &Outcome| o.stats.frames.max(1) as f64;
    per("first_frame_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| o.first_frame_ms);
    per("video_format_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| o.handshake.times.video_format_ms);
    // The QUIC handshake alone (info): on Windows loopback (PC-LABORATORIO) the first packet
    // of the viewer's new socket is lost in about half the sessions and quinn resends it
    // after its first PTO (333 ms initial RTT: ~1 s): connected_ms and video_format_ms are
    // ~52 or ~1060 ms there, first_frame_ms ~225 or ~1240 (12 sessions on 2026-11-20, 6 of
    // each), the client-socket effect of T6 / SRV-15 (B-GO-06). The same two figures counted
    // from the established connection are what the session's start costs, and they gate.
    let connected = |o: &Outcome| o.handshake.times.connected_ms.unwrap_or_default();
    per("connected_ms", "ms", Better::Lower, Gate::Info, &connected);
    per("video_format_after_connect_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| {
        o.handshake.times.video_format_ms - connected(o)
    });
    per("first_frame_after_connect_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| {
        o.first_frame_ms - connected(o)
    });
    per("fps", "1/s", Better::Higher, Gate::MacroThroughput, &|o| o.stats.fps);
    per("frames_missing_pct", "%", Better::Lower, Gate::Info, &|o| {
        (100.0 * (1.0 - o.stats.fps / case.fps as f64)).max(0.0)
    });
    per("latency_p50_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| o.stats.latency_ms.p50);
    per("latency_p95_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| o.stats.latency_ms.p95);
    per("latency_max_ms", "ms", Better::Lower, Gate::Info, &|o| o.stats.latency_ms.max);
    per("latency_received_p50_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| {
        o.stats.latency_received_ms.p50
    });
    per("latency_received_p95_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| {
        o.stats.latency_received_ms.p95
    });
    per("transit_p50_ms", "ms", Better::Lower, Gate::Info, &|o| o.stats.transit_ms.p50);
    per("encode_ms_p50", "ms", Better::Lower, Gate::MacroLatency, &|o| o.stats.encode_ms.p50);
    per("decode_ms_p50", "ms", Better::Lower, Gate::MacroLatency, &|o| o.stats.decode_ms.p50);
    per("rtt_ms_p50", "ms", Better::Lower, Gate::Info, &|o| o.stats.rtt_ms.p50);
    per("mbit_s", "Mbit/s", Better::Lower, Gate::Info, &|o| o.stats.mbit_s);
    per("freezes", "1", Better::Lower, Gate::Info, &|o| o.stats.freezes as f64);
    per("freeze_ms", "ms", Better::Lower, Gate::Info, &|o| o.stats.freeze_ms);
    per("keyframes", "1", Better::Lower, Gate::Info, &|o| o.stats.keyframes as f64);
    per("cpu_ms_frame_agent", "ms", Better::Lower, Gate::MacroCpu, &|o| {
        o.stats.usage.agent_cpu_ms / frames(o)
    });
    per("cpu_ms_frame_viewer", "ms", Better::Lower, Gate::MacroCpu, &|o| {
        o.stats.usage.viewer_cpu_ms / frames(o)
    });
    per("cpu_cores_agent", "cores", Better::Lower, Gate::Info, &|o| {
        o.stats.usage.agent_cpu_ms / 1000.0 / o.stats.usage.wall_s.max(1e-3)
    });
    per("allocs_frame_agent", "1", Better::Lower, Gate::Micro, &|o| {
        o.stats.usage.allocs.agent.allocs as f64 / frames(o)
    });
    per("allocs_frame_viewer", "1", Better::Lower, Gate::Micro, &|o| {
        o.stats.usage.allocs.viewer.allocs as f64 / frames(o)
    });
    per("alloc_bytes_frame_agent", "B", Better::Lower, Gate::Micro, &|o| {
        o.stats.usage.allocs.agent.bytes as f64 / frames(o)
    });
    per("alloc_bytes_frame_viewer", "B", Better::Lower, Gate::Micro, &|o| {
        o.stats.usage.allocs.viewer.bytes as f64 / frames(o)
    });
    per("rss_mb", "MB", Better::Lower, Gate::Resource, &|o| o.proc_end.rss_bytes as f64 / 1048576.0);
    per("threads", "1", Better::Lower, Gate::Info, &|o| o.proc_end.threads.unwrap_or(0) as f64);
    per("handles", "1", Better::Lower, Gate::Info, &|o| o.proc_end.open_objects().unwrap_or(0) as f64);
    let mean = |v: &[f64]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().map(|&x| crate::quality::cap(x)).sum::<f64>() / v.len() as f64
        }
    };
    // Through an impaired link (netem profile other than N0) the decoded pictures depend
    // on which packets the seeded-but-timing-dependent proxy drops and on the rate
    // control's reaction: the quality is not deterministic (two builds of the same code
    // differed by 0.22 dB on MSI-UBUNTU, and most runs scored no sampled frame at all),
    // so it informs there; it gates on the clean link (plan section 13.4).
    let qgate = if case.profile.is_some_and(|p| p != "N0") { Gate::Info } else { Gate::Quality };
    per("psnr_yuv_db", "dB", Better::Higher, qgate, &|o| mean(&o.psnr_yuv));
    per("psnr_y_db", "dB", Better::Higher, qgate, &|o| mean(&o.psnr_y));
    per("ssim_y", "1", Better::Higher, qgate, &|o| mean(&o.ssim));
    per("quality_unaligned", "1", Better::Lower, Gate::Info, &|o| o.unaligned as f64);
    if case.datagrams {
        per("shards_lost_pct", "%", Better::Lower, Gate::Info, &|o| {
            o.stats.shards_lost as f64 * 100.0 / o.stats.shards.max(1) as f64
        });
        per("frames_lost", "1", Better::Lower, Gate::Info, &|o| o.stats.frames_lost as f64);
        per("frames_rebuilt", "1", Better::Higher, Gate::Info, &|o| o.stats.frames_rebuilt as f64);
    }
    if case.profile.is_some() {
        per("settle_after_freeze_ms", "ms", Better::Lower, Gate::Info, &|o| {
            crate::measure::median(&o.stats.settle_after_freezes_ms())
        });
        per("keyframe_requests", "1", Better::Lower, Gate::Info, &|o| o.stats.keyframe_requests as f64);
    }
    let d = |f: fn(&SessionStats) -> Distribution| {
        Distribution::median_of(&runs.iter().map(|o| f(&o.stats)).collect::<Vec<_>>())
    };
    r.distribution("latency_ms", d(|s| s.latency_ms));
    r.distribution("latency_received_ms", d(|s| s.latency_received_ms));
    r.distribution("transit_ms", d(|s| s.transit_ms));
    r.distribution("encode_ms", d(|s| s.encode_ms));
    r.distribution("decode_ms", d(|s| s.decode_ms));
    if let Some(o) = runs.first() {
        r.extra.insert("handshake".into(), serde_json::to_value(&o.handshake).unwrap_or_default());
        r.extra.insert(
            "bitrate_trace_kbps".into(),
            serde_json::to_value(&o.stats.bitrate_trace_kbps).unwrap_or_default(),
        );
        r.extra.insert(
            "control_messages".into(),
            serde_json::to_value(&o.stats.control_messages).unwrap_or_default(),
        );
        if let Some(n) = &o.netem {
            r.extra.insert("netem".into(), serde_json::to_value(n).unwrap_or_default());
        }
    }
    let usage: Vec<serde_json::Value> =
        runs.iter().map(|o| serde_json::to_value(o.stats.usage).unwrap_or_default()).collect();
    r.extra.insert("usage".into(), usage.into());
    for o in runs {
        if let Some(n) = &o.agent_note {
            r.note(n.clone());
        }
        if case.datagrams && !o.datagrams_granted {
            r.note("the agent did not grant video datagrams");
        }
    }
    let samples: usize = runs.iter().map(|o| o.psnr_yuv.len() + o.unaligned).sum();
    let unaligned: usize = runs.iter().map(|o| o.unaligned).sum();
    if runs.iter().any(|o| o.stats.frames == 0) {
        r.status = Status::Failed;
        r.note("no frame decoded in the measured window");
    } else if samples >= 4 && unaligned * 2 > samples {
        r.status = Status::InvalidReference;
        r.note(format!("{unaligned} of {samples} sampled frames do not match the synthetic reference"));
    }
}
