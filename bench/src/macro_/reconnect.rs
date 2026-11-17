//! B-MACRO-07 (WP4): reconnection.
//!
//! - (a) `back-to-back/direct` and `back-to-back/server`: the next session right
//!   after the previous one is closed: `serve_direct` with `once = false` (a
//!   [`LoopbackAgent`] `Direct`), and an agent registered on a local server (the
//!   viewer goes through signalling and `viewer_connect` again). Time from the
//!   previous `close()` returning to the next session's first decoded frame.
//! - (b) `outage/NX-n` (n = 1, 3, 8): the session runs through the netem proxy,
//!   which drops everything for n s on demand; time from the end of the outage to
//!   the next decoded frame and to the next decoded frame **captured** after it
//!   (fresh), the freeze around it, keyframe requests and the agent's keyframes.
//!   One session, so the agent is `serve_reverse` on 127.0.0.1 (same `session()`);
//!   (a) direct, (c) need `serve_direct(once = false)`, which binds 0.0.0.0.
//! - (c) `outage/NX-12`: 12 s is longer than the 10 s idle timeout, the session
//!   dies; time from the end of the outage until a new session (same agent,
//!   `once = false`, through the proxy) shows its first frame, and the attempts.
//! - (d) `server-restart`: the local `nuvira-server` is killed and started again
//!   (same port and data); time until the agent is registered again
//!   (`AgentEvent::Online`) and until a new session reaches its first frame.
//!
//! Events per run: 10 (`full`), fewer in `quick` (`NUVIRA_BENCH_EVENTS` overrides).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nuvira_transport::signal::TransportMode;

use crate::measure::{Better, Gate, Metric};
use crate::net::headless::{HeadlessSession, HeadlessViewer, ViewerOptions};
use crate::net::netem::Netem;
use crate::net::server_mode::{
    self, LocalServer, NetCase, ServerAgent, ServerSpec, ViewerIdentity, events_for, ms, ms_events,
    session_ended,
};
use crate::net::{AgentEntry, LoopbackAgent};
use crate::registry::{CaseResult, FULL_ONLY, Level, Registry, RunCtx};
use crate::report::Status;

pub fn register(reg: &mut Registry) {
    let add = |reg: &mut Registry,
               name: &str,
               ids: &'static [&'static str],
               f: fn(&mut RunCtx, &mut CaseResult) -> Result<()>| {
        let mut c = NetCase::new("B-MACRO-07", name, Level::Macro, FULL_ONLY, f).config_ids(ids);
        if name.contains("server") {
            c = c.runnable(server_mode::server_runnable());
        }
        reg.add_case(c);
    };
    add(reg, "back-to-back/direct/synthetic-1920x1080@60", &["NET-DEGRADED"], back_to_back_direct);
    add(reg, "back-to-back/server/synthetic-1920x1080@60", &["SRV-PUBLIC"], back_to_back_server);
    for n in [1u64, 3, 8] {
        let f: fn(&mut RunCtx, &mut CaseResult) -> Result<()> = match n {
            1 => |c, r| outage(c, r, 1),
            3 => |c, r| outage(c, r, 3),
            _ => |c, r| outage(c, r, 8),
        };
        add(reg, &format!("outage/NX-{n}/synthetic-1920x1080@60"), &["NET-DEGRADED"], f);
    }
    add(reg, "outage/NX-12/synthetic-1920x1080@60", &["NET-DEGRADED"], outage_long);
    add(reg, "server-restart/synthetic-1920x1080@60", &["SRV-PUBLIC"], server_restart);
}

fn config() -> nuvira_agent::AgentConfig {
    server_mode::synthetic_config(1920, 1080, 60, 8000, true)
}

fn opts(password: Option<String>) -> ViewerOptions {
    ViewerOptions { password, ..Default::default() }
}

/// (a) direct: `serve_direct(once = false)`, sessions one after the other.
fn back_to_back_direct(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let n = events_for(ctx.tier, 3, 10);
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let agent = LoopbackAgent::start(&agent_rt, config(), AgentEntry::Direct, &ctx.tmp.join("agent"), false)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    let mut errors = Vec::new();
    let (reconnect, first) = viewer_rt.block_on(async {
        let (mut reconnect, mut first) = (Vec::new(), Vec::new());
        let mut closed_at: Option<Instant> = None;
        for i in 0..=n {
            let one = async {
                let s = HeadlessViewer::connect_direct(addr, Some(fp), opts(None)).await?;
                let f = s.first_frame(Duration::from_secs(20)).await?;
                Ok::<_, anyhow::Error>((s, f))
            };
            match tokio::time::timeout(Duration::from_secs(40), one).await {
                Ok(Ok((s, f))) => {
                    let shown = s.started + f;
                    first.push(ms(f));
                    if let Some(c) = closed_at {
                        reconnect.push(ms(shown - c));
                    }
                    s.close().await;
                    closed_at = Some(Instant::now());
                }
                Ok(Err(e)) => {
                    errors.push(format!("session {i}: {e:#}"));
                    closed_at = None;
                }
                Err(_) => {
                    errors.push(format!("session {i}: timed out"));
                    closed_at = None;
                }
            }
        }
        (reconnect, first)
    });
    drop(agent);
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    r.param("sessions", (n + 1) as u64);
    ms_events(r, "reconnect_ms", Gate::MacroLatency, &reconnect);
    ms_events(r, "first_frame_ms", Gate::MacroLatency, &first);
    finish(r, errors, !reconnect.is_empty())
}

/// (a) through the server: close, then signalling + route + viewer_connect again.
fn back_to_back_server(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let n = events_for(ctx.tier, 3, 10);
    let server = LocalServer::start(&ctx.tmp.join("server"), ServerSpec::default())?;
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let agent = ServerAgent::start(&agent_rt, config(), &server, TransportMode::Udp, &ctx.tmp.join("agent"))?;
    let viewer_dir = ctx.tmp.join("viewer");
    let mut errors = Vec::new();
    let result = viewer_rt.block_on(async {
        let (_, _, id) = agent.online(0, Duration::from_secs(30)).await?;
        let viewer = ViewerIdentity::load(&viewer_dir)?;
        let o = opts(Some(server_mode::PASSWORD.into()));
        let (mut reconnect, mut first, mut busy_waits) = (Vec::new(), Vec::new(), 0u32);
        let mut closed_at: Option<Instant> = None;
        for i in 0..=n {
            let one = async {
                let t0 = Instant::now();
                let (conn, _) =
                    server_mode::viewer_connect(&server, &viewer, &id, TransportMode::Udp).await?;
                let s = HeadlessViewer::from_connection(conn, o.clone()).await?;
                let f = s.first_frame(Duration::from_secs(20)).await?;
                Ok::<_, anyhow::Error>((t0, s, f))
            };
            match tokio::time::timeout(Duration::from_secs(60), one).await {
                Ok(Ok((t0, s, f))) => {
                    let shown = s.started + f;
                    first.push(ms(shown - t0));
                    if let Some(c) = closed_at {
                        reconnect.push(ms(shown - c));
                    }
                    s.close().await;
                    closed_at = Some(Instant::now());
                }
                Ok(Err(e)) => {
                    // The agent may still be ending the previous session ("busy"):
                    // counted, and the next attempt starts from there.
                    busy_waits += 1;
                    errors.push(format!("session {i}: {e:#}"));
                }
                Err(_) => errors.push(format!("session {i}: timed out")),
            }
        }
        Ok::<_, anyhow::Error>((reconnect, first, busy_waits))
    });
    agent_rt.block_on(agent.stop());
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    let (reconnect, first, busy) = result?;
    r.param("sessions", (n + 1) as u64);
    ms_events(r, "reconnect_ms", Gate::MacroLatency, &reconnect);
    ms_events(r, "first_frame_ms", Gate::MacroLatency, &first);
    r.metric("failed_attempts", Metric::single("1", Better::Lower, Gate::Info, busy as f64));
    finish(r, errors, !reconnect.is_empty())
}

/// What one outage did to the session, from the frames decoded around it.
struct Recovery {
    /// End of outage -> next decoded frame (may be one captured during the outage).
    next_ms: Option<f64>,
    /// End of outage -> next decoded frame captured after the end.
    fresh_ms: Option<f64>,
    /// Longest gap between decoded frames around the outage.
    freeze_ms: f64,
    keyframe_requests: u64,
    keyframes: u64,
}

fn recovery(s: &HeadlessSession, start_us: u64, end_us: u64) -> Recovery {
    let stats = s.snapshot();
    let mut decoded: Vec<(u64, u64, bool)> =
        stats.per_frame.iter().map(|f| (f.decoded_us, f.capture_us, f.keyframe)).collect();
    decoded.sort_unstable();
    let next = decoded.iter().find(|f| f.0 > end_us).map(|f| (f.0 - end_us) as f64 / 1000.0);
    let fresh =
        decoded.iter().find(|f| f.0 > end_us && f.1 >= end_us).map(|f| (f.0 - end_us) as f64 / 1000.0);
    let mut freeze = 0f64;
    for w in decoded.windows(2) {
        if w[1].0 >= start_us && w[0].0 <= end_us + 30_000_000 {
            freeze = freeze.max((w[1].0 - w[0].0) as f64 / 1000.0);
        }
    }
    let keyframes = decoded.iter().filter(|f| f.0 > start_us && f.2).count() as u64;
    Recovery {
        next_ms: next,
        fresh_ms: fresh,
        freeze_ms: freeze,
        keyframe_requests: stats.keyframe_requests,
        keyframes,
    }
}

/// (b) outages of `secs` through the proxy, on one session.
fn outage(ctx: &mut RunCtx, r: &mut CaseResult, secs: u64) -> Result<()> {
    let n = events_for(ctx.tier, if secs >= 8 { 1 } else { 3 }, 10);
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    // One session: `serve_reverse` on 127.0.0.1 (nothing on 0.0.0.0), same `session()`.
    let agent = LoopbackAgent::start(&agent_rt, config(), AgentEntry::Reverse, &ctx.tmp.join("agent"), true)?;
    let proxy = Netem::spawn("127.0.0.1:0".parse()?, agent.addr, "N0")?;
    let fp = agent.fingerprint;
    let password = agent.password.clone();
    let outage = Duration::from_secs(secs);
    let mut errors = Vec::new();
    let res = viewer_rt.block_on(async {
        let s = HeadlessViewer::connect_direct(proxy.addr, Some(fp), opts(password)).await?;
        s.first_frame(Duration::from_secs(20)).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut out = Vec::new();
        for i in 0..n {
            s.mark();
            tokio::time::sleep(Duration::from_secs(1)).await;
            let start_us = nuvira_proto::now_us();
            proxy.blackhole(outage)?;
            tokio::time::sleep(outage).await;
            let end_us = start_us + outage.as_micros() as u64;
            // Up to 15 s for the session to come back, then 2 s more of frames.
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let back = recovery(&s, start_us, end_us).fresh_ms.is_some();
                if back || Instant::now() > deadline || s.error().is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            let rec = recovery(&s, start_us, end_us);
            if rec.fresh_ms.is_none() {
                errors.push(format!("outage {i}: no fresh frame within 15 s ({:?})", s.error()));
            }
            out.push(rec);
            if let Some(e) = s.error() {
                errors.push(format!("session broke: {e}"));
                break;
            }
        }
        s.close().await;
        Ok::<_, anyhow::Error>(out)
    });
    let counts = proxy.stop().ok();
    let _ = agent.finish(&agent_rt, Duration::from_secs(3));
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    let recs = res?;
    r.param("outage_s", secs);
    r.param("agent_entry", AgentEntry::Reverse.name());
    r.param("outages", recs.len() as u64);
    if let Some(c) = counts {
        r.param("netem_dropped_blackhole", c.up.dropped_blackhole + c.down.dropped_blackhole);
    }
    let pick = |f: fn(&Recovery) -> Option<f64>| recs.iter().filter_map(f).collect::<Vec<_>>();
    ms_events(r, "recovery_next_frame_ms", Gate::MacroLatency, &pick(|x| x.next_ms));
    ms_events(r, "recovery_fresh_frame_ms", Gate::MacroLatency, &pick(|x| x.fresh_ms));
    ms_events(r, "freeze_ms", Gate::Info, &pick(|x| Some(x.freeze_ms)));
    let kr: Vec<f64> = recs.iter().map(|x| x.keyframe_requests as f64).collect();
    r.metric("keyframe_requests", Metric::from_values("1", Better::Lower, Gate::Info, kr));
    let kf: Vec<f64> = recs.iter().map(|x| x.keyframes as f64).collect();
    r.metric("keyframes_after_outage", Metric::from_values("1", Better::Lower, Gate::Info, kf));
    let ok = recs.iter().any(|x| x.fresh_ms.is_some());
    finish(r, errors, ok)
}

/// (c) NX-12: the session dies at the idle timeout; a new one through the proxy.
fn outage_long(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let n = events_for(ctx.tier, 1, 10);
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let agent = LoopbackAgent::start(&agent_rt, config(), AgentEntry::Direct, &ctx.tmp.join("agent"), false)?;
    let proxy = Netem::spawn("127.0.0.1:0".parse()?, agent.addr, "N0")?;
    let fp = agent.fingerprint;
    let outage = Duration::from_secs(12);
    let mut errors = Vec::new();
    let res = viewer_rt.block_on(async {
        let mut s = HeadlessViewer::connect_direct(proxy.addr, Some(fp), opts(None)).await?;
        s.first_frame(Duration::from_secs(20)).await?;
        let (mut back, mut attempts, mut old_died) = (Vec::new(), Vec::new(), 0u32);
        for i in 0..n {
            tokio::time::sleep(Duration::from_secs(2)).await;
            proxy.blackhole(outage)?;
            tokio::time::sleep(outage).await;
            let end = Instant::now();
            old_died += u32::from(s.error().is_some());
            // The old session is gone (idle timeout): drop it as the viewer would.
            s.close().await;
            let mut tries = 0u32;
            let new = loop {
                tries += 1;
                let one = async {
                    let s = HeadlessViewer::connect_direct(proxy.addr, Some(fp), opts(None)).await?;
                    let f = s.first_frame(Duration::from_secs(10)).await?;
                    Ok::<_, anyhow::Error>((s, f))
                };
                match tokio::time::timeout(Duration::from_secs(12), one).await {
                    Ok(Ok(v)) => break Some(v),
                    _ if end.elapsed() > Duration::from_secs(60) => break None,
                    _ => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            };
            let Some((new, f)) = new else {
                errors.push(format!("outage {i}: no new session within 60 s"));
                return Ok::<_, anyhow::Error>((back, attempts, old_died));
            };
            back.push(ms((new.started + f) - end));
            attempts.push(tries as f64);
            s = new;
        }
        s.close().await;
        Ok((back, attempts, old_died))
    });
    drop(proxy);
    drop(agent);
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    let (back, attempts, old_died) = res?;
    r.param("outage_s", 12u64);
    ms_events(r, "new_session_ms", Gate::MacroLatency, &back);
    r.metric("attempts", Metric::from_values("1", Better::Lower, Gate::Info, attempts));
    r.metric("old_session_died", Metric::single("1", Better::Higher, Gate::Info, old_died as f64));
    finish(r, errors, !back.is_empty())
}

/// (d) kill and restart the local server.
fn server_restart(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let n = events_for(ctx.tier, 2, 10);
    let mut server = LocalServer::start(&ctx.tmp.join("server"), ServerSpec::default())?;
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let agent = ServerAgent::start(&agent_rt, config(), &server, TransportMode::Udp, &ctx.tmp.join("agent"))?;
    let viewer_dir = ctx.tmp.join("viewer");
    let mut errors = Vec::new();
    let res = viewer_rt.block_on(async {
        let (mut seen, _, id) = agent.online(0, Duration::from_secs(30)).await?;
        seen += 1;
        let viewer = ViewerIdentity::load(&viewer_dir)?;
        let o = opts(Some(server_mode::PASSWORD.into()));
        let (mut online, mut first, mut starts) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..n {
            let killed = Instant::now();
            let start = server.restart().context("restarting nuvira-server")?;
            starts.push(ms(start));
            let (idx, at, new_id) = match agent.online(seen, Duration::from_secs(60)).await {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("restart {i}: {e:#}"));
                    break;
                }
            };
            seen = idx + 1;
            if new_id != id {
                errors.push(format!("restart {i}: the agent came back with another ID ({new_id})"));
            }
            online.push(ms(at - killed));
            let one = async {
                let (conn, _) =
                    server_mode::viewer_connect(&server, &viewer, &new_id, TransportMode::Udp).await?;
                let s = HeadlessViewer::from_connection(conn, o.clone()).await?;
                let f = s.first_frame(Duration::from_secs(20)).await?;
                Ok::<_, anyhow::Error>((s, f))
            };
            match tokio::time::timeout(Duration::from_secs(60), one).await {
                Ok(Ok((s, f))) => {
                    first.push(ms((s.started + f) - killed));
                    s.close().await;
                    if let Ok((idx, _)) = session_ended(&agent.log, seen, Duration::from_secs(15)).await {
                        seen = idx + 1;
                    }
                }
                Ok(Err(e)) => errors.push(format!("restart {i}: session: {e:#}")),
                Err(_) => errors.push(format!("restart {i}: session timed out")),
            }
        }
        Ok::<_, anyhow::Error>((online, first, starts))
    });
    agent_rt.block_on(agent.stop());
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    drop(server);
    let (online, first, starts) = res?;
    r.param("restarts", n as u64);
    ms_events(r, "agent_online_ms", Gate::MacroLatency, &online);
    ms_events(r, "first_frame_ms", Gate::MacroLatency, &first);
    ms_events(r, "server_start_ms", Gate::Info, &starts);
    finish(r, errors, !first.is_empty())
}

/// Notes the errors; `failed` when some events failed, an error when none worked.
fn finish(r: &mut CaseResult, errors: Vec<String>, any_ok: bool) -> Result<()> {
    for e in &errors {
        r.note(e.clone());
    }
    anyhow::ensure!(any_ok, "no event succeeded: {errors:?}");
    if !errors.is_empty() {
        r.status = Status::Failed;
    }
    Ok(())
}
