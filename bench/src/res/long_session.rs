//! B-RES-01 (WP4): long session, leaks.
//!
//! 1080p60 on the synthetic source with the adaptive bitrate on, 30 min (`full`) /
//! 5 min (`quick`; `NUVIRA_BENCH_LONG_SECS` overrides for short checks), resources
//! sampled every 5 s ([`crate::resources::Sampler`]):
//! - `in-process`: the B-MACRO-03 harness ([`LoopbackAgent`], headless viewer on its
//!   own runtime), this process sampled;
//! - `two-process/nuvira-agent`: the agent binary (`NUVIRA_BENCH_AGENT_EXE`)
//!   `--synthetic 1920x1080@60 --encoder software --port P --fingerprint-file F`
//!   in a process of its own, sampled by pid, the headless viewer here. Without
//!   `--once` the agent outlives the session, so its state 10 s after the session
//!   can be compared with the state before it.
//!
//! Metrics: RSS slope over the window after the first 5 min (`full`) / 1 min
//! (`quick`) in MiB/h (least squares), and RSS, private bytes, threads, handles or
//! fds and sockets 12 s after the session ended minus before it started (gate
//! `resource`: slope > 2 MiB/h, or threads/handles not back).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::{record_delta, record_series, rss_slope_mb_h};
use crate::measure::{Better, Gate, Metric};
use crate::net::headless::{HeadlessViewer, ViewerOptions};
use crate::net::server_mode::{self, NetCase, record_stats};
use crate::net::{AgentEntry, LoopbackAgent};
use crate::registry::{BOTH_TIERS, CaseResult, Level, Registry, RunCtx, Tier};
use crate::resources::{ProcSample, Sampler};

const INTERVAL: Duration = Duration::from_secs(5);
/// After the session: the agent's threads and the runtime's idle blocking threads
/// (10 s keep-alive) have gone.
const SETTLE: Duration = Duration::from_secs(12);

pub fn register(reg: &mut Registry) {
    reg.add_case(
        NetCase::new(
            "B-RES-01",
            "long-session/in-process/synthetic-1920x1080@60/abr",
            Level::Long,
            BOTH_TIERS,
            in_process,
        )
        .config_ids(&["WIN11", "WIN10"]),
    );
    reg.add_case(
        NetCase::new(
            "B-RES-01",
            "long-session/two-process/nuvira-agent/synthetic-1920x1080@60/abr",
            Level::Long,
            BOTH_TIERS,
            two_process,
        )
        .config_ids(&["WIN11", "WIN10"])
        .runnable(server_mode::agent_exe().map(|_| ())),
    );
}

/// The headless viewer of a long session: no per-frame records (their 72 B per frame
/// were the 15 MiB/h "viewer growth" of the Phase 1 baseline, VWR-1); the window is
/// summed up in fixed memory.
fn long_options(password: Option<String>) -> ViewerOptions {
    ViewerOptions { password, keep_frames: false, ..Default::default() }
}

/// Session length and the part skipped for the slope.
fn durations(tier: Tier) -> (Duration, Duration) {
    if let Some(s) = std::env::var("NUVIRA_BENCH_LONG_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        return (Duration::from_secs(s.max(10)), Duration::from_secs(s.max(10) / 5));
    }
    match tier {
        Tier::Quick => (Duration::from_secs(300), Duration::from_secs(60)),
        Tier::Full => (Duration::from_secs(1800), Duration::from_secs(300)),
    }
}

/// Slope over the session after `skip` (the session ran from `start_ms` for `length`).
fn record_slope(
    r: &mut CaseResult,
    prefix: &str,
    samples: &[ProcSample],
    start_ms: u64,
    skip: Duration,
    length: Duration,
) {
    let (from, to) = (start_ms + skip.as_millis() as u64, start_ms + length.as_millis() as u64);
    r.param(&format!("{prefix}slope_window_s"), (to - from) as f64 / 1000.0);
    if let Some(slope) = rss_slope_mb_h(samples, from, to) {
        r.metric(
            &format!("{prefix}rss_slope_mb_h"),
            Metric::single("MiB/h", Better::Lower, Gate::Resource, slope),
        );
    } else {
        r.note(format!("{prefix}: too few samples for a slope"));
    }
}

fn in_process(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let (length, skip) = durations(ctx.tier);
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    // Both runtimes up before the "before" sample, so their threads count on both sides.
    agent_rt.block_on(async {});
    viewer_rt.block_on(async {});
    let pre = ProcSample::of_self();
    let sampler = Sampler::start(std::process::id(), INTERVAL);
    let entry = AgentEntry::from_env();
    let config = crate::net::synthetic_config(1920, 1080, 60, false, false);
    let agent = LoopbackAgent::start(&agent_rt, config, entry, &ctx.tmp.join("agent"), true)?;
    let (addr, fp, password) = (agent.addr, agent.fingerprint, agent.password.clone());
    let stats = viewer_rt.block_on(async {
        let mut s = HeadlessViewer::connect_direct(addr, Some(fp), long_options(password)).await?;
        s.first_frame(Duration::from_secs(20)).await?;
        let start_ms = crate::resources::unix_ms();
        let st = s.run(length).await;
        s.close().await;
        st.map(|st| (st, start_ms))
    });
    let ended = agent.finish(&agent_rt, Duration::from_secs(5));
    std::thread::sleep(SETTLE);
    let post = ProcSample::of_self();
    let samples = sampler.finish();
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    let (stats, start_ms) = stats?;
    if let Err(e) = ended {
        r.note(format!("agent end: {e:#}"));
    }
    r.param("seconds", length.as_secs());
    r.param("agent_entry", entry.name());
    record_stats(r, "session_", &stats);
    record_slope(r, "", &samples, start_ms, skip, length);
    record_delta(r, "", &pre, &post);
    record_series(r, "self", &samples);
    Ok(())
}

fn two_process(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let exe = server_mode::agent_exe().map_err(anyhow::Error::msg)?;
    let (length, skip) = durations(ctx.tier);
    let port = crate::net::free_udp_port()?;
    let data: PathBuf = ctx.tmp.join("agent-proc");
    std::fs::create_dir_all(&data)?;
    let fp_file = data.join("fingerprint.txt");
    let log = std::fs::File::create(data.join("agent.log"))?;
    let mut child = Command::new(&exe)
        .args(["--synthetic", "1920x1080@60", "--encoder", "software", "--port", &port.to_string()])
        .arg("--fingerprint-file")
        .arg(&fp_file)
        .env("NUVIRA_DATA_DIR", &data)
        .env("NUVIRA_KEY_STORE", "file")
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    let pid = child.id();
    let mut run = || -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let fp = loop {
            if let Ok(t) = std::fs::read_to_string(&fp_file)
                && t.trim().len() == 64
            {
                break nuvira_transport::parse_fingerprint(t.trim())?;
            }
            if let Some(st) = child.try_wait()? {
                bail!("nuvira-agent exited at start with {st}");
            }
            if Instant::now() > deadline {
                bail!("nuvira-agent wrote no fingerprint in 30 s");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Its start-up (encoder probing, runtime) settled before the "before" sample.
        std::thread::sleep(Duration::from_secs(2));
        let pre = ProcSample::of_pid(pid)?;
        let sampler = Sampler::start(pid, INTERVAL);
        let viewer_rt = crate::net::viewer_runtime(2)?;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let stats = viewer_rt.block_on(async {
            let mut s = HeadlessViewer::connect_direct(addr, Some(fp), long_options(None)).await?;
            s.first_frame(Duration::from_secs(20)).await?;
            let start_ms = crate::resources::unix_ms();
            let st = s.run(length).await;
            s.close().await;
            st.map(|st| (st, start_ms))
        });
        viewer_rt.shutdown_timeout(Duration::from_secs(2));
        std::thread::sleep(SETTLE);
        let post = ProcSample::of_pid(pid).context("the agent process is gone after the session")?;
        let samples = sampler.finish();
        let (stats, start_ms) = stats?;
        r.param("seconds", length.as_secs());
        r.param("agent_exe", exe.display().to_string());
        record_stats(r, "session_", &stats);
        // The agent is another process: its CPU comes from its own times.
        r.metrics.remove("session_cpu_ms_frame_agent");
        if stats.frames > 0 && samples.len() >= 2 {
            let (a, b) = (&samples[0], &samples[samples.len() - 1]);
            let window_frames = stats.fps * (b.unix_ms - a.unix_ms) as f64 / 1000.0;
            if window_frames > 0.0 {
                r.metric(
                    "session_cpu_ms_frame_agent_process",
                    Metric::single(
                        "ms",
                        Better::Lower,
                        Gate::MacroCpu,
                        (b.cpu_total_ms() - a.cpu_total_ms()) / window_frames,
                    ),
                );
            }
        }
        record_slope(r, "agent_", &samples, start_ms, skip, length);
        record_delta(r, "agent_", &pre, &post);
        record_series(r, "agent", &samples);
        Ok(())
    };
    let res = run();
    let _ = child.kill();
    let _ = child.wait();
    res
}
