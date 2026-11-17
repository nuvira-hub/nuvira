//! B-MACRO-05 / B-MACRO-06 (WP4): connection establishment in server mode.
//!
//! A local `nuvira-server` (the side's binary, [`LocalServer`]), an in-process agent
//! registered on it (`serve_via_server`, synthetic 1920x1080@60, software encoder,
//! fixed 8 Mbit/s), and the viewer half of `open_connection`
//! ([`server_mode::viewer_connect`]) followed by the headless viewer's handshake
//! ([`HeadlessViewer::from_connection`]). Every connection is timed from the start
//! of the viewer's signalling: registered, `Route` received, `viewer_connect` done
//! (direct or relay), `VideoFormat`, first decoded frame. The session is then
//! closed as the viewer closes it, and the next one starts once the agent reports
//! `SessionEnded` (it serves one session at a time).
//!
//! B-MACRO-06 variants: forced relay (`NUVIRA_FORCE_RELAY=1`, process-wide: the
//! case runs in its own child), WebSocket signalling and relay for both sides
//! (`TransportMode::WebSocket`: NET-UDP-BLOCKED simulated), relay impaired by the
//! server's own `NUVIRA_RELAY_IMPAIR` with N2/N3/N4 (relay forced, since the
//! impairment applies to relayed traffic only). Each variant also measures its
//! last session over the macro window (`net::macro_durations`) with the headless
//! viewer's metrics (as B-MACRO-03).

use std::time::{Duration, Instant};

use anyhow::Result;
use nuvira_transport::signal::TransportMode;

use crate::measure::{Better, Gate, Metric};
use crate::net::headless::{HeadlessViewer, ViewerOptions};
use crate::net::server_mode::{
    self, LocalServer, NetCase, ServerAgent, ServerSpec, ViewerIdentity, ms, ms_events, session_ended,
};
use crate::net::{macro_durations, netem};
use crate::registry::{BOTH_TIERS, CaseResult, FULL_ONLY, Level, Registry, RunCtx, Tier};
use crate::report::Status;

/// Connections per run (plan: 20).
const CONNECTIONS: usize = 20;
/// B-MACRO-06: fewer connections, then one measured session.
const VARIANT_CONNECTIONS: usize = 5;

#[derive(Clone, Copy, Debug)]
struct Variant {
    name: &'static str,
    transport: TransportMode,
    force_relay: bool,
    impair: Option<&'static str>,
    /// Measure the last session over the macro window.
    session: bool,
}

const BASE: Variant =
    Variant { name: "udp", transport: TransportMode::Udp, force_relay: false, impair: None, session: false };

pub fn register(reg: &mut Registry) {
    reg.add_case(case("B-MACRO-05", BASE, CONNECTIONS, BOTH_TIERS));
    let variants = [
        Variant { name: "relay", force_relay: true, session: true, ..BASE },
        Variant { name: "websocket", transport: TransportMode::WebSocket, session: true, ..BASE },
        Variant { name: "relay-N2", force_relay: true, impair: Some("N2"), session: true, ..BASE },
        Variant { name: "relay-N3", force_relay: true, impair: Some("N3"), session: true, ..BASE },
        Variant { name: "relay-N4", force_relay: true, impair: Some("N4"), session: true, ..BASE },
    ];
    for v in variants {
        reg.add_case(case("B-MACRO-06", v, VARIANT_CONNECTIONS, FULL_ONLY));
    }
}

fn case(id: &'static str, v: Variant, connections: usize, tiers: &'static [Tier]) -> NetCase {
    let name = format!("server/{}/synthetic-1920x1080@60", v.name);
    let ids: &'static [&'static str] = if v.transport == TransportMode::WebSocket {
        &["SRV-PUBLIC", "NET-UDP-BLOCKED"]
    } else if v.impair.is_some() {
        &["SRV-PUBLIC", "NET-DEGRADED"]
    } else {
        &["SRV-PUBLIC", "NET-NAT"]
    };
    let mut c = NetCase::new(id, name, Level::Macro, tiers, move |ctx, r| run(ctx, r, v, connections))
        .config_ids(ids)
        .runnable(server_mode::server_runnable());
    if v.force_relay {
        c = c.env("NUVIRA_FORCE_RELAY", "1");
    }
    c
}

fn run(ctx: &mut RunCtx, r: &mut CaseResult, v: Variant, connections: usize) -> Result<()> {
    let spec = ServerSpec { relay_impair: v.impair.map(|p| netem::profile_spec(p).to_string()) };
    let server = LocalServer::start(&ctx.tmp.join("server"), spec)?;
    r.param("server_start_ms", ms(server.start_time));
    r.param("transport", server_mode::mode_name(v.transport));
    r.param("force_relay", v.force_relay);
    if let Some(p) = v.impair {
        r.param("network_profile", p);
        r.param("relay_impair", netem::profile_spec(p));
    }
    r.param("connections", connections as u64);
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let config = server_mode::synthetic_config(1920, 1080, 60, 8000, true);
    let agent = ServerAgent::start(&agent_rt, config, &server, v.transport, &ctx.tmp.join("agent"))?;
    let tier = ctx.tier;
    let viewer_dir = ctx.tmp.join("viewer");
    let result = viewer_rt.block_on(async {
        let (mut seen, online_at, device_id) = agent.online(0, Duration::from_secs(30)).await?;
        r.metric(
            "agent_register_ms",
            Metric::single("ms", Better::Lower, Gate::MacroLatency, ms(online_at - agent.started)),
        );
        let viewer = ViewerIdentity::load(&viewer_dir)?;
        let opts = ViewerOptions { password: Some(server_mode::PASSWORD.into()), ..Default::default() };
        let (mut signal, mut route, mut connected, mut format, mut first) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut direct = 0u32;
        let mut errors = Vec::new();
        for i in 0..connections {
            let one = async {
                let t0 = Instant::now();
                let (conn, times) =
                    server_mode::viewer_connect(&server, &viewer, &device_id, v.transport).await?;
                let s = HeadlessViewer::from_connection(conn, opts.clone()).await?;
                let f = s.first_frame(Duration::from_secs(20)).await?;
                Ok::<_, anyhow::Error>((t0, times, s, f))
            };
            match tokio::time::timeout(Duration::from_secs(60), one).await {
                Ok(Ok((t0, times, mut s, f))) => {
                    let base = s.started - t0;
                    signal.push(ms(times.signal));
                    route.push(ms(times.route));
                    connected.push(ms(times.connected));
                    format.push(ms(base) + s.times.video_format_ms);
                    first.push(ms(base + f));
                    direct += u32::from(times.direct);
                    if i + 1 == connections && v.session {
                        let (warm, measured) = macro_durations(tier);
                        tokio::time::sleep(warm).await;
                        match s.run(measured).await {
                            Ok(stats) => server_mode::record_stats(r, "session_", &stats),
                            Err(e) => errors.push(format!("measured session: {e:#}")),
                        }
                        r.param("session_warmup_s", warm.as_secs_f64());
                    }
                    s.close().await;
                }
                Ok(Err(e)) => errors.push(format!("connection {i}: {e:#}")),
                Err(_) => errors.push(format!("connection {i}: timed out after 60 s")),
            }
            // The agent is free for the next one once its session ended.
            match session_ended(&agent.log, seen, Duration::from_secs(15)).await {
                Ok((idx, _)) => seen = idx + 1,
                Err(e) => errors.push(format!("connection {i}: the agent's session did not end: {e:#}")),
            }
        }
        ms_events(r, "signal_ms", Gate::MacroLatency, &signal);
        ms_events(r, "route_ms", Gate::MacroLatency, &route);
        ms_events(r, "connected_ms", Gate::MacroLatency, &connected);
        ms_events(r, "video_format_ms", Gate::MacroLatency, &format);
        ms_events(r, "first_frame_ms", Gate::MacroLatency, &first);
        let ok = first.len();
        r.metric("connections_ok", Metric::single("1", Better::Higher, Gate::Info, ok as f64));
        let frac = if ok == 0 { 0.0 } else { direct as f64 / ok as f64 };
        r.metric("direct_fraction", Metric::single("1", Better::Higher, Gate::Info, frac));
        r.param(
            "path",
            if direct as usize == ok {
                "direct"
            } else if direct == 0 {
                "relay"
            } else {
                "mixed"
            },
        );
        for e in &errors {
            r.note(e.clone());
        }
        for e in agent.log.errors() {
            r.note(format!("agent: {e}"));
        }
        anyhow::ensure!(ok > 0, "no connection succeeded: {errors:?}");
        if !errors.is_empty() {
            r.status = Status::Failed;
        }
        Ok(())
    });
    agent_rt.block_on(agent.stop());
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    result
}
