//! B-RES-02 (WP4): session churn.
//!
//! 200 sequential sessions (`full`; 30 in `quick`, `NUVIRA_BENCH_EVENTS` overrides)
//! on one agent: connect, 2 s of video from the first decoded frame, close.
//! - `direct`: `serve_direct(once = false)` ([`LoopbackAgent`] `Direct`);
//! - `server`: an agent registered on a local `nuvira-server`, each session
//!   through signalling, `Route` and `viewer_connect` (registration and relay churn
//!   on the server side, connection churn on the agent's).
//!
//! This process is sampled every 20 sessions (`full`; every 10 in `quick`).
//! Metrics: growth per session after the first sampling point (least squares over
//! the session index) of RSS (KiB), handles or fds, threads and sockets; gate
//! `resource`: > 0 handles per session or > 64 KiB RSS per session.

use std::time::{Duration, Instant};

use anyhow::Result;
use nuvira_transport::signal::TransportMode;

use super::record_series;
use crate::measure::{Better, Gate, Metric, least_squares};
use crate::net::headless::{HeadlessViewer, ViewerOptions};
use crate::net::server_mode::{
    self, LocalServer, NetCase, ServerAgent, ServerSpec, ViewerIdentity, events_for, ms, ms_events,
    session_ended,
};
use crate::net::{AgentEntry, LoopbackAgent};
use crate::registry::{BOTH_TIERS, CaseResult, Level, Registry, RunCtx, Tier};
use crate::report::Status;
use crate::resources::ProcSample;

const VIDEO: Duration = Duration::from_secs(2);

pub fn register(reg: &mut Registry) {
    reg.add_case(NetCase::new(
        "B-RES-02",
        "churn/direct/synthetic-1280x720@30",
        Level::Res,
        BOTH_TIERS,
        direct,
    ));
    reg.add_case(
        NetCase::new("B-RES-02", "churn/server/synthetic-1280x720@30", Level::Res, BOTH_TIERS, via_server)
            .config_ids(&["SRV-PUBLIC"])
            .runnable(server_mode::server_runnable()),
    );
}

/// Sessions and the sampling period.
fn plan(tier: Tier) -> (usize, usize) {
    let n = events_for(tier, 30, 200);
    let every = if tier == Tier::Quick || n < 40 { 10.min(n.max(1)) } else { 20 };
    (n, every)
}

/// The session's agent config: small and light (churn is about set-up and teardown).
fn config() -> nuvira_agent::AgentConfig {
    server_mode::synthetic_config(1280, 720, 30, 3000, true)
}

/// Per-session growth of the sampled values after the first point.
fn record_growth(r: &mut CaseResult, points: &[(usize, ProcSample)]) {
    let skip = points.first().map(|p| p.0).unwrap_or(0);
    let after: Vec<&(usize, ProcSample)> = points.iter().filter(|p| p.0 >= skip).collect();
    let fit = |f: &dyn Fn(&ProcSample) -> Option<f64>| {
        let pts: Vec<(f64, f64)> = after.iter().filter_map(|(i, s)| f(s).map(|v| (*i as f64, v))).collect();
        least_squares(&pts).map(|(slope, _)| slope)
    };
    let m = |unit: &str, v: f64| Metric::single(unit, Better::Lower, Gate::Resource, v);
    if let Some(v) = fit(&|s| Some(s.rss_bytes as f64 / 1024.0)) {
        r.metric("rss_kib_per_session", m("KiB", v));
    }
    if let Some(v) = fit(&|s| s.private_bytes.map(|b| b as f64 / 1024.0)) {
        r.metric("private_kib_per_session", m("KiB", v));
    }
    if let Some(v) = fit(&|s| s.open_objects().map(f64::from)) {
        r.metric("handles_per_session", m("1", v));
    }
    if let Some(v) = fit(&|s| s.threads.map(f64::from)) {
        r.metric("threads_per_session", m("1", v));
    }
    if let Some(v) = fit(&|s| s.sockets.map(f64::from)) {
        r.metric("sockets_per_session", m("1", v));
    }
    let samples: Vec<ProcSample> = points.iter().map(|p| p.1.clone()).collect();
    record_series(r, "self", &samples);
    r.extra.insert(
        "series_session_index".into(),
        serde_json::Value::Array(points.iter().map(|p| serde_json::json!(p.0)).collect()),
    );
}

fn direct(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let (n, every) = plan(ctx.tier);
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let agent = LoopbackAgent::start(&agent_rt, config(), AgentEntry::Direct, &ctx.tmp.join("agent"), false)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    let mut errors = Vec::new();
    let (points, first) = viewer_rt.block_on(async {
        let (mut points, mut first) = (Vec::new(), Vec::new());
        for i in 0..n {
            let one = async {
                let s = HeadlessViewer::connect_direct(addr, Some(fp), ViewerOptions::default()).await?;
                let f = s.first_frame(Duration::from_secs(20)).await?;
                tokio::time::sleep(VIDEO).await;
                s.close().await;
                Ok::<_, anyhow::Error>(f)
            };
            match tokio::time::timeout(Duration::from_secs(40), one).await {
                Ok(Ok(f)) => first.push(ms(f)),
                Ok(Err(e)) => errors.push(format!("session {i}: {e:#}")),
                Err(_) => errors.push(format!("session {i}: timed out")),
            }
            if (i + 1) % every == 0 {
                // A moment for the agent to finish the session's teardown.
                tokio::time::sleep(Duration::from_millis(500)).await;
                points.push((i + 1, ProcSample::of_self()));
            }
        }
        (points, first)
    });
    drop(agent);
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    finish(r, n, every, &points, &first, errors)
}

fn via_server(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let (n, every) = plan(ctx.tier);
    let server = LocalServer::start(&ctx.tmp.join("server"), ServerSpec::default())?;
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let agent = ServerAgent::start(&agent_rt, config(), &server, TransportMode::Udp, &ctx.tmp.join("agent"))?;
    let viewer_dir = ctx.tmp.join("viewer");
    let server_pid = server.pid();
    let mut errors = Vec::new();
    let res = viewer_rt.block_on(async {
        let (mut seen, _, id) = agent.online(0, Duration::from_secs(30)).await?;
        let viewer = ViewerIdentity::load(&viewer_dir)?;
        let o = ViewerOptions { password: Some(server_mode::PASSWORD.into()), ..Default::default() };
        let (mut points, mut first, mut server_points) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..n {
            let one = async {
                let t0 = Instant::now();
                let (conn, _) =
                    server_mode::viewer_connect(&server, &viewer, &id, TransportMode::Udp).await?;
                let s = HeadlessViewer::from_connection(conn, o.clone()).await?;
                let f = s.first_frame(Duration::from_secs(20)).await?;
                let shown = s.started + f - t0;
                tokio::time::sleep(VIDEO).await;
                s.close().await;
                Ok::<_, anyhow::Error>(shown)
            };
            match tokio::time::timeout(Duration::from_secs(60), one).await {
                Ok(Ok(f)) => first.push(ms(f)),
                Ok(Err(e)) => errors.push(format!("session {i}: {e:#}")),
                Err(_) => errors.push(format!("session {i}: timed out")),
            }
            match session_ended(&agent.log, seen, Duration::from_secs(15)).await {
                Ok((idx, _)) => seen = idx + 1,
                Err(e) => errors.push(format!("session {i}: the agent's session did not end: {e:#}")),
            }
            if (i + 1) % every == 0 {
                points.push((i + 1, ProcSample::of_self()));
                if let Some(p) = server_pid
                    && let Ok(s) = ProcSample::of_pid(p)
                {
                    server_points.push((i + 1, s));
                }
            }
        }
        Ok::<_, anyhow::Error>((points, first, server_points))
    });
    agent_rt.block_on(agent.stop());
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    drop(server);
    let (points, first, server_points) = res?;
    // The Go server's own growth per session (its RSS and handles), as info.
    let fit = |f: &dyn Fn(&ProcSample) -> Option<f64>| {
        let pts: Vec<(f64, f64)> =
            server_points.iter().filter_map(|(i, s)| f(s).map(|v| (*i as f64, v))).collect();
        least_squares(&pts).map(|(slope, _)| slope)
    };
    if let Some(v) = fit(&|s| Some(s.rss_bytes as f64 / 1024.0)) {
        r.metric("server_rss_kib_per_session", Metric::single("KiB", Better::Lower, Gate::Info, v));
    }
    if let Some(v) = fit(&|s| s.open_objects().map(f64::from)) {
        r.metric("server_handles_per_session", Metric::single("1", Better::Lower, Gate::Info, v));
    }
    finish(r, n, every, &points, &first, errors)
}

fn finish(
    r: &mut CaseResult,
    n: usize,
    every: usize,
    points: &[(usize, ProcSample)],
    first: &[f64],
    errors: Vec<String>,
) -> Result<()> {
    r.param("sessions", n as u64);
    r.param("sample_every", every as u64);
    r.param("video_s", VIDEO.as_secs());
    ms_events(r, "first_frame_ms", Gate::Info, first);
    r.metric("sessions_ok", Metric::single("1", Better::Higher, Gate::Info, first.len() as f64));
    record_growth(r, points);
    for e in errors.iter().take(20) {
        r.note(e.clone());
    }
    anyhow::ensure!(!first.is_empty(), "no session succeeded: {:?}", errors.first());
    if !errors.is_empty() {
        r.status = Status::Failed;
    }
    Ok(())
}
