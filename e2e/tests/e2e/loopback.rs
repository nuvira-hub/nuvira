//! Sessions without a server: the agent's `session()` through `serve_reverse` on
//! a 127.0.0.1 endpoint (one session, a password, the switch-sides policy) and,
//! where the firewall allows binding 0.0.0.0, `serve_direct` (LAN mode).
//!
//! Pinned here: frames and close, wrong password, wrong certificate, malformed
//! and oversized first messages, video as datagrams, degraded network (netem
//! proxy), short outages, abrupt kill of either side, and that an
//! unauthenticated connection holds the agent at most 10 s (Phase 6 lot 2).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_bench::net::loopback_connect;
use nuvira_bench::net::netem::Proxy;
use nuvira_e2e::rig::{self, AgentEntry, HeadlessViewer, LoopbackAgent, Raw, ViewerOptions};
use nuvira_e2e::runner::{Case, Ctx, Group, Need, ensure};
use nuvira_proto::v1::control_message::Msg;

pub fn cases() -> Vec<Case> {
    vec![
        Case::new("loopback/reverse/frames-and-close", frames_and_close),
        Case::new("loopback/reverse/video-datagrams", video_datagrams),
        Case::new("loopback/reverse/wrong-password", wrong_password),
        Case::new("loopback/reverse/wrong-certificate-refused", wrong_certificate),
        Case::new("loopback/reverse/malformed-first-message", malformed_first_message),
        Case::new("loopback/reverse/oversized-message-before-auth-refused", max_size_before_auth)
            .env(&[("RUST_LOG", "warn,quinn=warn")]),
        Case::new("loopback/reverse/unauthenticated-connection-released-after-10s", unauthenticated_holds),
        Case::new("loopback/netem/degraded-N3-N5", degraded),
        Case::new("loopback/netem/outage-1s-3s-recovered", outage_short),
        Case::new("loopback/kill/viewer-process", kill_viewer),
        Case::new("loopback/kill/agent-process", kill_agent),
        Case::new("loopback/direct/no-password-and-back-to-back", direct_back_to_back)
            .needs(&[Need::Wildcard]),
        Case::new("loopback/direct/idle-connection-released-after-10s", direct_idle_blocks)
            .needs(&[Need::Wildcard]),
        Case::new("loopback/netem/outage-8s-recovered", outage_8s).group(Group::Long).timeout_s(300),
        Case::new("loopback/netem/outage-8s-recovered-cubic", outage_8s)
            .env(&[("NUVIRA_CC_CUBIC", "1")])
            .group(Group::Long)
            .timeout_s(300),
        Case::new("loopback/netem/outage-12s-agent-ends-at-10s", outage_12s)
            .group(Group::Long)
            .timeout_s(240),
    ]
}

/// A Reverse agent (127.0.0.1) on the small synthetic source.
fn reverse(rt: &tokio::runtime::Runtime, ctx: &Ctx) -> Result<LoopbackAgent> {
    LoopbackAgent::start(rt, rig::config(), AgentEntry::Reverse, &ctx.tmp.join("agent"), true)
}

fn frames_and_close(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    let hello = vrt.block_on(async {
        let s = HeadlessViewer::connect_direct(addr, Some(fp), rig::opts(rig::REVERSE_PASSWORD)).await?;
        let n = rig::frames(&s, 30, Duration::from_secs(20)).await?;
        ctx.note(format!("frames {n}, format {}x{}", s.format.width, s.format.height));
        let hello = s.agent.clone();
        s.close().await;
        anyhow::Ok(hello)
    })?;
    // The agent's offer with the synthetic source and the switch-sides policy.
    ensure(hello.password_required, || "Hello.password_required false".into())?;
    ensure(!hello.file_manager && !hello.paste && !hello.clipboard_files, || format!("{hello:?}"))?;
    ensure(!hello.terminal && !hello.tunnel && !hello.audio && !hello.handover, || format!("{hello:?}"))?;
    // A normal close by the viewer ends serve_reverse without an error.
    let t = Instant::now();
    agent.finish(&art, Duration::from_secs(5)).context("agent after a normal close")?;
    ctx.note(format!("agent ended {:.0} ms after close", t.elapsed().as_secs_f64() * 1e3));
    Ok(())
}

fn video_datagrams(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    vrt.block_on(async {
        let o = ViewerOptions { video_datagrams: true, ..rig::opts(rig::REVERSE_PASSWORD) };
        let s = HeadlessViewer::connect_direct(addr, Some(fp), o).await?;
        ensure(s.agent.video_datagrams && s.datagrams(), || "video not as datagrams".into())?;
        rig::frames(&s, 30, Duration::from_secs(20)).await?;
        let st = s.snapshot();
        ctx.note(format!("frames {} shards {} lost {}", st.frames, st.shards, st.shards_lost));
        ensure(st.shards > 0, || "no FEC shards counted".into())?;
        s.close().await;
        anyhow::Ok(())
    })?;
    agent.finish(&art, Duration::from_secs(5))
}

fn wrong_password(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    let (msg, ended, pause) = vrt.block_on(async {
        let mut raw = Raw::open(loopback_connect(addr, Some(fp)).await?).await?;
        let hello = raw.hello(true).await?;
        ensure(hello.password_required, || "no password asked".into())?;
        let answer = raw.auth("not-the-password", "").await?;
        let t = Instant::now();
        let (ended, _) = rig::wait_closed(&raw.conn, Duration::from_secs(10)).await?;
        anyhow::Ok((rig::auth_result(&answer)?, ended, t.elapsed()))
    })?;
    ctx.note(format!("answer {msg:?}, then {ended} after {} ms", pause.as_millis()));
    ensure(msg == (false, "@agent.wrong_password".into()), || format!("answer {msg:?}"))?;
    // The agent waits 2 s before closing (against rapid-fire attempts), with the reason.
    ensure(ended == "closed 0 @agent.wrong_password", || format!("ended: {ended}"))?;
    ensure(pause >= Duration::from_millis(1800) && pause < Duration::from_secs(6), || {
        format!("pause {pause:?}")
    })?;
    // serve_reverse serves one session: it ends with the reason as its error.
    match agent.finish(&art, Duration::from_secs(5)) {
        Err(e) if format!("{e:#}").contains("@agent.wrong_password") => Ok(()),
        other => bail!("agent result {other:?}"),
    }
}

fn wrong_certificate(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let addr = agent.addr;
    let mut wrong = agent.fingerprint;
    wrong[0] ^= 0xff;
    let r = vrt.block_on(async { loopback_connect(addr, Some(wrong)).await.map(|_| ()) });
    let err = match r {
        Ok(()) => bail!("a connection pinning another fingerprint was accepted"),
        Err(e) => format!("{e:#}"),
    };
    ctx.note(format!("viewer: {err}"));
    ensure(err.contains("fingerprint"), || format!("viewer error {err}"))?;
    // The agent's one incoming connection failed: serve_reverse ends with an error.
    match agent.finish(&art, Duration::from_secs(10)) {
        Err(e) => {
            ctx.note(format!("agent: {e:#}"));
            ensure(format!("{e:#}").contains("connection from the other computer"), || format!("{e:#}"))
        }
        Ok(()) => bail!("serve_reverse ended without an error"),
    }
}

/// What the agent does with a first control message that is not a valid Hello:
/// it closes the connection with code 0 and the generic reason, before any
/// password, and serve_reverse ends with the error.
fn malformed_first_message(ctx: &mut Ctx) -> Result<()> {
    let ping = nuvira_proto::framing::encode(&rig::control(Msg::Ping(nuvira_proto::v1::Ping { sent_us: 1 })));
    let mut too_large = Vec::new();
    prost::encoding::encode_varint((nuvira_proto::framing::MAX_MESSAGE_LEN + 1) as u64, &mut too_large);
    too_large.extend_from_slice(&[0u8; 64]);
    let variants: Vec<(&str, Vec<u8>, &str)> = vec![
        ("ping-first", ping, "did not introduce itself"),
        ("invalid-protobuf", vec![0x05, 0xff, 0xff, 0xff, 0xff, 0xff], "invalid message"),
        ("length-over-16MiB", too_large, "message too large"),
        ("empty-stream", Vec::new(), "did not introduce itself"),
    ];
    for (name, bytes, agent_error) in variants {
        let (art, vrt) = rig::runtimes()?;
        let agent = reverse(&art, ctx)?;
        let (addr, fp) = (agent.addr, agent.fingerprint);
        let ended = vrt.block_on(async {
            let conn = loopback_connect(addr, Some(fp)).await?;
            let (mut send, _recv) = conn.open_bi().await?;
            send.write_all(&bytes).await?;
            if bytes.is_empty() {
                send.finish()?;
            }
            let (ended, after) = rig::wait_closed(&conn, Duration::from_secs(10)).await?;
            anyhow::Ok(format!("{ended} ({} ms)", after.as_millis()))
        })?;
        let result = agent.finish(&art, Duration::from_secs(5));
        ctx.note(format!("{name}: {ended}; agent {result:?}"));
        ensure(ended.starts_with("closed 0 end of session"), || format!("{name}: {ended}"))?;
        match result {
            Err(e) if format!("{e:#}").contains(agent_error) => {}
            other => bail!("{name}: agent result {other:?}, expected {agent_error:?}"),
        }
    }
    Ok(())
}

/// Was K35 (Phase 2): before any password the agent read and decoded a control
/// message of the full 16 MiB limit (and logged the whole device name in it).
/// Fixed in Phase 6 lot 2 (AGT-3, NET-3): before authentication the control
/// stream takes at most 64 KiB; a Hello of exactly that size is still answered
/// (its name is cut in the log), one byte more is refused from its length.
fn max_size_before_auth(ctx: &mut Ctx) -> Result<()> {
    use prost::Message;
    let limit = nuvira_agent::HANDSHAKE_MESSAGE_LIMIT;
    let hello_of = |len: usize| {
        let mut hello = rig::viewer_hello(true);
        let base = rig::control(Msg::Hello(hello.clone())).encoded_len();
        let mut n = len - base - 8;
        loop {
            hello.device_name = "x".repeat(n);
            let m = rig::control(Msg::Hello(hello.clone()));
            match m.encoded_len() {
                l if l == len => break m,
                l if l < len => n += len - l,
                l => n -= l - len,
            }
        }
    };
    let max = nuvira_proto::framing::MAX_MESSAGE_LEN;
    for (len, answered) in [(limit, true), (limit + 1, false), (max, false)] {
        let msg = hello_of(len);
        let (art, vrt) = rig::runtimes()?;
        let agent = reverse(&art, ctx)?;
        let (addr, fp) = (agent.addr, agent.fingerprint);
        vrt.block_on(async {
            let mut raw = Raw::open(loopback_connect(addr, Some(fp)).await?).await?;
            let t = Instant::now();
            // The agent may close while a refused message is still being sent.
            let _ = raw.tx.send(&msg).await;
            match raw.next(Duration::from_secs(30)).await {
                Ok(Some(nuvira_proto::v1::ControlMessage { msg: Some(Msg::Hello(h)) })) if answered => {
                    ctx.note(format!("{len} B Hello answered in {} ms", t.elapsed().as_millis()));
                    ensure(h.password_required, || "no password asked".into())?;
                    raw.conn.close(0u32.into(), b"done");
                }
                other if !answered => {
                    let (ended, after) = rig::wait_closed(&raw.conn, Duration::from_secs(10)).await?;
                    ctx.note(format!("{len} B Hello: {other:?}, {ended} after {} ms", after.as_millis()));
                    ensure(ended == "closed 0 end of session", || ended.clone())?;
                    ensure(!matches!(other, Ok(Some(_))), || format!("{len} B: answered {other:?}"))?;
                }
                other => bail!("{len} B: expected the agent's Hello, got {other:?}"),
            }
            anyhow::Ok(())
        })?;
        let r = agent.finish(&art, Duration::from_secs(5));
        if !answered {
            ensure(format!("{r:?}").contains("message too large"), || format!("{len} B: agent {r:?}"))?;
        }
    }
    Ok(())
}

/// Was KNOWN BUG K34 (Phase 2): an unauthenticated connection held the agent
/// with no time limit, as long as the viewer's keep-alives arrived: no control
/// stream at all, or a Hello and never the password. Fixed in Phase 6 lot 2
/// (AGT-1, AGT-2): the agent gives up on either after 10 s.
fn unauthenticated_holds(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let silent = reverse(&art, ctx)?;
    let no_auth =
        LoopbackAgent::start(&art, rig::config(), AgentEntry::Reverse, &ctx.tmp.join("agent2"), true)?;
    vrt.block_on(async {
        let c1 = loopback_connect(silent.addr, Some(silent.fingerprint)).await?;
        let t1 = Instant::now();
        let mut raw = Raw::open(loopback_connect(no_auth.addr, Some(no_auth.fingerprint)).await?).await?;
        raw.hello(true).await?;
        let t2 = Instant::now();
        for (what, conn, t) in [("no control stream", &c1, t1), ("Hello without password", &raw.conn, t2)] {
            let (ended, _) = rig::wait_closed(conn, Duration::from_secs(20)).await?;
            let after = t.elapsed();
            ctx.note(format!("{what}: {ended} after {} ms", after.as_millis()));
            ensure(ended == "closed 0 end of session", || format!("{what}: {ended}"))?;
            ensure(after >= Duration::from_secs(9) && after <= Duration::from_secs(15), || {
                format!("{what}: after {after:?}")
            })?;
        }
        anyhow::Ok(())
    })?;
    let r = silent.finish(&art, Duration::from_secs(5));
    ensure(format!("{r:?}").contains("did not open the control channel within 10 s"), || format!("{r:?}"))?;
    let r = no_auth.finish(&art, Duration::from_secs(5));
    ensure(format!("{r:?}").contains("did not send the password within 10 s"), || format!("{r:?}"))?;
    Ok(())
}

/// Frames keep coming through the netem proxy with the N3 and N5 profiles
/// (stream mode, fixed bitrate below the N3 rate).
fn degraded(ctx: &mut Ctx) -> Result<()> {
    for profile in ["N3", "N5"] {
        let (art, vrt) = rig::runtimes()?;
        let agent = reverse(&art, ctx)?;
        let proxy = Proxy::start("127.0.0.1:0".parse()?, agent.addr, profile)?;
        let fp = agent.fingerprint;
        let paddr = proxy.addr;
        let st = vrt.block_on(async {
            let mut s =
                HeadlessViewer::connect_direct(paddr, Some(fp), rig::opts(rig::REVERSE_PASSWORD)).await?;
            s.first_frame(Duration::from_secs(20)).await?;
            let st = s.run(Duration::from_secs(6)).await?;
            s.close().await;
            anyhow::Ok(st)
        })?;
        let counts = proxy.stop();
        ctx.note(format!(
            "{profile}: {} frames in {:.1} s ({:.1} fps), {} freezes, dropped {}",
            st.frames,
            st.window_s,
            st.fps,
            st.freezes,
            counts.dropped()
        ));
        // Frames keep flowing (the rate is a benchmark matter, and the NUC under
        // load encoded at 3-4 fps).
        ensure(st.frames >= 10, || format!("{profile}: {} frames ({:.1} fps)", st.frames, st.fps))?;
        let _ = agent.finish(&art, Duration::from_secs(5));
    }
    Ok(())
}

/// End of an outage of `secs` -> the first frame captured after it, on one session
/// through the proxy (None: none within 15 s). The session's error, if it broke.
async fn outage(s: &rig::HeadlessSession, proxy: &Proxy, secs: u64) -> (Option<Duration>, Option<String>) {
    s.mark();
    let start = nuvira_proto::now_us();
    proxy.blackhole(Duration::from_secs(secs));
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let end_us = start + secs * 1_000_000;
    let end = Instant::now();
    while end.elapsed() < Duration::from_secs(15) {
        if s.snapshot().per_frame.iter().any(|f| f.capture_us >= end_us) {
            return (Some(end.elapsed()), None);
        }
        if let Some(e) = s.error() {
            return (None, Some(e));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (None, s.error())
}

fn outage_short(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let proxy = Proxy::start("127.0.0.1:0".parse()?, agent.addr, "N0")?;
    let (fp, paddr) = (agent.fingerprint, proxy.addr);
    vrt.block_on(async {
        let s = HeadlessViewer::connect_direct(paddr, Some(fp), rig::opts(rig::REVERSE_PASSWORD)).await?;
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        for secs in [1u64, 3, 1, 3] {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let (back, err) = outage(&s, &proxy, secs).await;
            ctx.note(format!("outage {secs} s: fresh frame after {back:?} ({err:?})"));
            ensure(back.is_some(), || format!("outage {secs} s not recovered: {err:?}"))?;
        }
        s.close().await;
        anyhow::Ok(())
    })?;
    proxy.stop();
    agent.finish(&art, Duration::from_secs(5))
}

/// Was KNOWN BUG K38 (Phase 2, baseline B-MACRO-07 outage/NX-8): repeated 8 s
/// outages on one session. The sender of the video stays congestion-blocked after
/// an outage and quinn then sends nothing, not even an ACK, until a backed-off
/// loss probe up to ~18 s after the viewer's last receipt, so the viewer's 10 s
/// idle timeout ended the session ("timed out") in some runs (Phase 4 E1: 2 of 5
/// runs with BBR, 5 of 5 with Cubic). Fixed in Phase 6 lot 9 (NET-1): session
/// connections have a 30 s idle timeout, and the agent ends by itself a session
/// whose viewer is silent for 10 s. Now each of the 6 outages of 8 s (1 s apart)
/// recovers with a fresh frame within 15 s, also with `NUVIRA_CC_CUBIC=1` (the
/// `-cubic` case), which made it much worse before.
fn outage_8s(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    // The baseline's source (B-MACRO-07: 1080p60 at a fixed 8 Mbit/s).
    let config = nuvira_bench::net::server_mode::synthetic_config(1920, 1080, 60, 8000, true);
    let agent = LoopbackAgent::start(&art, config, AgentEntry::Reverse, &ctx.tmp.join("agent"), true)?;
    let proxy = Proxy::start("127.0.0.1:0".parse()?, agent.addr, "N0")?;
    let (fp, paddr) = (agent.fingerprint, proxy.addr);
    vrt.block_on(async {
        let s = HeadlessViewer::connect_direct(paddr, Some(fp), rig::opts(rig::REVERSE_PASSWORD)).await?;
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        let mut recovered = 0;
        for i in 1..=6 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let (back, err) = outage(&s, &proxy, 8).await;
            ctx.note(format!("outage {i} of 8 s: fresh frame after {back:?} ({err:?})"));
            match (back, err) {
                (Some(_), None) => recovered += 1,
                (b, e) => bail!("outage {i} of 8 s not recovered: fresh frame {b:?}, error {e:?}"),
            }
        }
        ctx.note(format!("8 s outages: {recovered} of 6 recovered"));
        s.close().await;
        anyhow::Ok(())
    })?;
    proxy.stop();
    // The viewer closed normally: serve_reverse ends without an error.
    agent.finish(&art, Duration::from_secs(5))
}

/// A 12 s outage: the agent hears nothing from the viewer for 10 s and ends the
/// session by itself ("timed out", NET-1), during the outage. The viewer's own
/// QUIC idle timeout is 30 s since Phase 6 lot 9 (it was 10 s and both sides timed
/// out): it gets no fresh frame and its session ends with "timed out" (its idle
/// timeout, or the agent's close "timed out" if that still reaches it), about 30 s
/// after its last packet from the agent.
fn outage_12s(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let proxy = Proxy::start("127.0.0.1:0".parse()?, agent.addr, "N0")?;
    let (fp, paddr) = (agent.fingerprint, proxy.addr);
    vrt.block_on(async {
        let s = HeadlessViewer::connect_direct(paddr, Some(fp), rig::opts(rig::REVERSE_PASSWORD)).await?;
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        let start_us = nuvira_proto::now_us();
        let start = Instant::now();
        proxy.blackhole(Duration::from_secs(12));
        // The agent ends its side during the outage.
        while !agent.is_finished() && start.elapsed() < Duration::from_secs(30) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let agent_end = start.elapsed();
        while s.error().is_none() && start.elapsed() < Duration::from_secs(45) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let viewer_end = start.elapsed();
        let err = s.error();
        let fresh = s.snapshot().per_frame.iter().any(|f| f.capture_us >= start_us + 12_000_000);
        ctx.note(format!(
            "outage 12 s: agent ended after {} ms, viewer after {} ms ({err:?}), fresh frame {fresh}",
            agent_end.as_millis(),
            viewer_end.as_millis()
        ));
        ensure(agent_end >= Duration::from_secs(9) && agent_end <= Duration::from_secs(12), || {
            format!("agent ended after {agent_end:?}")
        })?;
        ensure(!fresh, || "a fresh frame after the agent ended the session".into())?;
        ensure(err.as_deref().is_some_and(|e| e.contains("timed out")), || format!("viewer error {err:?}"))?;
        ensure(viewer_end >= Duration::from_secs(25) && viewer_end <= Duration::from_secs(36), || {
            format!("viewer ended after {viewer_end:?}")
        })?;
        s.close().await;
        anyhow::Ok(())
    })?;
    proxy.stop();
    match agent.finish(&art, Duration::from_secs(5)) {
        Err(e) if format!("{e:#}").contains("timed out") => Ok(()),
        other => bail!("agent result {other:?}"),
    }
}

/// The viewer's process is killed: the agent notices that nothing arrives from it
/// any more (10 s, NET-1; the QUIC idle timeout is 30 s) and serve_reverse ends
/// with "timed out".
fn kill_viewer(ctx: &mut Ctx) -> Result<()> {
    let (art, _vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let fp = nuvira_transport::fingerprint_hex(&agent.fingerprint);
    let mut host = rig::Host::spawn(&[
        "--e2e-viewer-host",
        "direct",
        &agent.addr.to_string(),
        &fp,
        rig::REVERSE_PASSWORD,
    ])?;
    host.line("frames", Duration::from_secs(30))?;
    host.kill();
    let t = Instant::now();
    while !agent.is_finished() && t.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let after = t.elapsed();
    let r = agent.finish(&art, Duration::from_secs(1));
    ctx.note(format!("agent ended {} ms after the kill: {r:?}", after.as_millis()));
    ensure(after >= Duration::from_secs(5) && after <= Duration::from_secs(16), || {
        format!("after {after:?}")
    })?;
    match r {
        Err(e) if format!("{e:#}").contains("timed out") => Ok(()),
        other => bail!("agent result {other:?}"),
    }
}

/// The agent's process is killed: the viewer's session fails with "timed out"
/// after the QUIC idle timeout, 30 s since Phase 6 lot 9 (NET-1; it was 10 s). The
/// session view stays frozen meanwhile: the cost of keeping sessions through
/// outages of a few seconds.
fn kill_agent(ctx: &mut Ctx) -> Result<()> {
    let vrt = nuvira_bench::net::viewer_runtime(2)?;
    let dir = ctx.tmp.join("agent-host");
    let dir_s = dir.display().to_string();
    let mut host = rig::Host::spawn(&[
        "--e2e-agent-host",
        "--synthetic",
        "640x360@30",
        "--bitrate",
        "1500",
        "--fixed-bitrate",
        "--entry",
        "reverse",
        "--data-dir",
        &dir_s,
    ])?;
    let line = host.line("agent ", Duration::from_secs(30))?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    let [addr, fp, password] = parts[..] else { bail!("agent line {line:?}") };
    let (addr, fp) = (addr.parse()?, nuvira_transport::parse_fingerprint(fp)?);
    let password = password.to_string();
    vrt.block_on(async {
        let s = HeadlessViewer::connect_direct(addr, Some(fp), rig::opts(&password)).await?;
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        host.kill();
        let t = Instant::now();
        while s.error().is_none() && t.elapsed() < Duration::from_secs(45) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let err = s.error();
        ctx.note(format!("viewer error {} ms after the kill: {err:?}", t.elapsed().as_millis()));
        ensure(err.as_deref().is_some_and(|e| e.contains("timed out")), || format!("viewer error {err:?}"))?;
        ensure(t.elapsed() >= Duration::from_secs(25) && t.elapsed() <= Duration::from_secs(36), || {
            format!("after {:?}", t.elapsed())
        })?;
        s.close().await;
        anyhow::Ok(())
    })
}

/// LAN mode (`serve_direct`): no password at all, and a new session right after
/// the previous one closes works.
fn direct_back_to_back(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = LoopbackAgent::start(&art, rig::config(), AgentEntry::Direct, &ctx.tmp.join("agent"), false)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    vrt.block_on(async {
        for i in 0..3 {
            let t = Instant::now();
            let s = HeadlessViewer::connect_direct(addr, Some(fp), ViewerOptions::default()).await?;
            ensure(!s.agent.password_required, || "serve_direct asked for a password".into())?;
            rig::frames(&s, 10, Duration::from_secs(20)).await.with_context(|| format!("session {i}"))?;
            ctx.note(format!("session {i}: {} ms to 10 frames", t.elapsed().as_millis()));
            s.close().await;
        }
        anyhow::Ok(())
    })?;
    drop(agent);
    Ok(())
}

/// Was KNOWN BUG K34 (Phase 2): in LAN mode one connection that never opened the
/// control stream kept every other viewer out (one session at a time, no
/// handshake timeout). Fixed in Phase 6 lot 2 (AGT-2): the agent drops it after
/// 10 s and serves the next viewer, while the idle one is still open on its side.
fn direct_idle_blocks(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = LoopbackAgent::start(&art, rig::config(), AgentEntry::Direct, &ctx.tmp.join("agent"), false)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    vrt.block_on(async {
        let idle = loopback_connect(addr, Some(fp)).await?;
        let t = Instant::now();
        // A viewer that arrived meanwhile waits in the queue (its own QUIC handshake
        // may time out first: then it tries again, as a person would).
        let mut attempts = Vec::new();
        let s = loop {
            match HeadlessViewer::connect_direct(addr, Some(fp), ViewerOptions::default()).await {
                Ok(s) => break s,
                Err(e) if t.elapsed() < Duration::from_secs(30) => attempts.push(format!("{e:#}")),
                Err(e) => bail!("no session while the idle connection waits: {attempts:?} {e:#}"),
            }
        };
        let served = t.elapsed();
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        let idle_end = match idle.close_reason() {
            Some(e) => rig::ended(&e),
            None => "still open".into(),
        };
        let ms = served.as_millis();
        ctx.note(format!(
            "served {ms} ms after the idle connection; idle one {idle_end}; before: {attempts:?}"
        ));
        let in_time = served >= Duration::from_secs(9) && served <= Duration::from_secs(25);
        ensure(in_time, || format!("served after {served:?}"))?;
        ensure(idle_end == "closed 0 end of session", || idle_end.clone())?;
        s.close().await;
        anyhow::Ok(())
    })?;
    drop(agent);
    Ok(())
}
