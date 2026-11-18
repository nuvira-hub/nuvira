//! Sessions without a server: the agent's `session()` through `serve_reverse` on
//! a 127.0.0.1 endpoint (one session, a password, the switch-sides policy) and,
//! where the firewall allows binding 0.0.0.0, `serve_direct` (LAN mode).
//!
//! Pinned here: frames and close, wrong password, wrong certificate, malformed
//! and oversized first messages, video as datagrams, degraded network (netem
//! proxy), short outages, abrupt kill of either side, and how long an
//! unauthenticated connection can hold the agent.

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
        Case::new("loopback/reverse/max-size-message-before-auth", max_size_before_auth)
            .env(&[("RUST_LOG", "warn,quinn=warn")]),
        Case::new("loopback/reverse/unauthenticated-connection-holds-agent", unauthenticated_holds),
        Case::new("loopback/netem/degraded-N3-N5", degraded),
        Case::new("loopback/netem/outage-1s-3s-recovered", outage_short),
        Case::new("loopback/kill/viewer-process", kill_viewer),
        Case::new("loopback/kill/agent-process", kill_agent),
        Case::new("loopback/direct/no-password-and-back-to-back", direct_back_to_back)
            .needs(&[Need::Wildcard]),
        Case::new("loopback/direct/idle-connection-blocks-next-viewer", direct_idle_blocks)
            .needs(&[Need::Wildcard]),
        Case::new("loopback/netem/characterises_bug_outage_8s_not_always_recovered", outage_8s)
            .group(Group::Long)
            .timeout_s(300),
        Case::new("loopback/netem/outage-12s-session-lost", outage_12s).group(Group::Long).timeout_s(240),
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

/// Before any password the agent reads and decodes a control message of the full
/// 16 MiB limit (and logs the device name in it).
fn max_size_before_auth(ctx: &mut Ctx) -> Result<()> {
    use prost::Message;
    let max = nuvira_proto::framing::MAX_MESSAGE_LEN;
    let mut hello = rig::viewer_hello(true);
    let base = rig::control(Msg::Hello(hello.clone())).encoded_len();
    let mut n = max - base - 8;
    let msg = loop {
        hello.device_name = "x".repeat(n);
        let m = rig::control(Msg::Hello(hello.clone()));
        match m.encoded_len() {
            l if l == max => break m,
            l if l < max => n += max - l,
            l => n -= l - max,
        }
    };
    let (art, vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    vrt.block_on(async {
        let mut raw = Raw::open(loopback_connect(addr, Some(fp)).await?).await?;
        let t = Instant::now();
        raw.tx.send(&msg).await?;
        match raw.next(Duration::from_secs(30)).await? {
            Some(nuvira_proto::v1::ControlMessage { msg: Some(Msg::Hello(h)) }) => {
                ctx.note(format!("16 MiB Hello answered in {} ms", t.elapsed().as_millis()));
                ensure(h.password_required, || "no password asked".into())
            }
            other => bail!("expected the agent's Hello, got {other:?}"),
        }?;
        raw.conn.close(0u32.into(), b"done");
        anyhow::Ok(())
    })?;
    let _ = agent.finish(&art, Duration::from_secs(5));
    Ok(())
}

/// KNOWN BUG (Phase 2, new): an unauthenticated connection holds the agent with
/// no time limit, as long as the viewer's keep-alives arrive: no control stream
/// at all, or a Hello and never the password. Expected after fix: the agent
/// gives up after a handshake timeout.
fn unauthenticated_holds(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let silent = reverse(&art, ctx)?;
    let no_auth =
        LoopbackAgent::start(&art, rig::config(), AgentEntry::Reverse, &ctx.tmp.join("agent2"), true)?;
    let wait = Duration::from_secs(12);
    vrt.block_on(async {
        let c1 = loopback_connect(silent.addr, Some(silent.fingerprint)).await?;
        let mut raw = Raw::open(loopback_connect(no_auth.addr, Some(no_auth.fingerprint)).await?).await?;
        raw.hello(true).await?;
        // Longer than the 10 s idle timeout: the keep-alives keep both open.
        tokio::time::sleep(wait).await;
        ensure(c1.close_reason().is_none() && raw.conn.close_reason().is_none(), || {
            format!("closed: {:?} {:?}", c1.close_reason(), raw.conn.close_reason())
        })?;
        ensure(!silent.is_finished() && !no_auth.is_finished(), || "an agent gave up".into())?;
        ctx.note(format!("both agents still waiting after {wait:?}"));
        c1.close(0u32.into(), b"bye");
        raw.conn.close(0u32.into(), b"bye");
        anyhow::Ok(())
    })?;
    // Closed by the viewer: both end.
    for a in [silent, no_auth] {
        let r = a.finish(&art, Duration::from_secs(5));
        ensure(!format!("{r:?}").contains("aborted"), || format!("agent still running: {r:?}"))?;
    }
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

/// KNOWN BUG (Phase 2, baseline B-MACRO-07 outage/NX-8): repeated 8 s outages on
/// one session. In the baseline (bench, 1080p60 8 Mbit/s, netem process) the
/// first was recovered and the second or third killed the session ("connection
/// lost: timed out") in 4 of 4 runs, while 1, 3 and 12 s behaved. Here (same
/// source, in-process proxy) every 8 s outage was recovered on all three
/// machines: whether the session survives depends on where the peers' PTO
/// back-off puts the first packet after the outage relative to the 10 s idle
/// timeout, so it is not deterministic.
/// Pinned (the deterministic part): each of 6 outages of 8 s (1 s apart) either
/// recovers with a fresh frame within 15 s or ends the session with "timed out"
/// (never another error, never a session left open without frames); the counts
/// are in the notes. Expected after fix: every 8 s outage recovered (it is shorter
/// than the idle timeout).
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
        let (mut recovered, mut lost) = (0, 0);
        for i in 1..=6 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let (back, err) = outage(&s, &proxy, 8).await;
            ctx.note(format!("outage {i} of 8 s: fresh frame after {back:?} ({err:?})"));
            match (back, err) {
                (Some(_), None) => recovered += 1,
                (None, Some(e)) if e.contains("timed out") => {
                    lost += 1;
                    break;
                }
                (b, e) => bail!("outage {i}: neither recovered nor timed out: {b:?} {e:?}"),
            }
        }
        ctx.note(format!("8 s outages: {recovered} recovered, {lost} lost the session"));
        s.close().await;
        anyhow::Ok(())
    })?;
    proxy.stop();
    let _ = agent.finish(&art, Duration::from_secs(5));
    Ok(())
}

/// 12 s is longer than the 10 s idle timeout: the session is lost (timed out)
/// on both sides.
fn outage_12s(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = reverse(&art, ctx)?;
    let proxy = Proxy::start("127.0.0.1:0".parse()?, agent.addr, "N0")?;
    let (fp, paddr) = (agent.fingerprint, proxy.addr);
    vrt.block_on(async {
        let s = HeadlessViewer::connect_direct(paddr, Some(fp), rig::opts(rig::REVERSE_PASSWORD)).await?;
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        let (back, err) = outage(&s, &proxy, 12).await;
        ctx.note(format!("outage 12 s: {back:?} {err:?}"));
        ensure(back.is_none() && err.as_deref().is_some_and(|e| e.contains("timed out")), || {
            format!("12 s outage: back {back:?}, error {err:?}")
        })?;
        s.close().await;
        anyhow::Ok(())
    })?;
    proxy.stop();
    match agent.finish(&art, Duration::from_secs(15)) {
        Err(e) if format!("{e:#}").contains("timed out") => Ok(()),
        other => bail!("agent result {other:?}"),
    }
}

/// The viewer's process is killed: the agent notices by the idle timeout (10 s)
/// and serve_reverse ends with "timed out".
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
/// after the idle timeout.
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
        while s.error().is_none() && t.elapsed() < Duration::from_secs(30) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let err = s.error();
        ctx.note(format!("viewer error {} ms after the kill: {err:?}", t.elapsed().as_millis()));
        ensure(err.as_deref().is_some_and(|e| e.contains("timed out")), || format!("viewer error {err:?}"))?;
        ensure(t.elapsed() >= Duration::from_secs(5) && t.elapsed() <= Duration::from_secs(16), || {
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

/// KNOWN BUG (Phase 2, new): in LAN mode one connection that never opens the
/// control stream keeps every other viewer out (one session at a time, no
/// handshake timeout). Expected after fix: the next viewer is served after a
/// handshake timeout.
fn direct_idle_blocks(ctx: &mut Ctx) -> Result<()> {
    let (art, vrt) = rig::runtimes()?;
    let agent = LoopbackAgent::start(&art, rig::config(), AgentEntry::Direct, &ctx.tmp.join("agent"), false)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    vrt.block_on(async {
        let idle = loopback_connect(addr, Some(fp)).await?;
        let second = tokio::time::timeout(
            Duration::from_secs(12),
            HeadlessViewer::connect_direct(addr, Some(fp), ViewerOptions::default()),
        )
        .await;
        let second = match second {
            Err(_) => "no session in 12 s".to_string(),
            Ok(Err(e)) => format!("failed: {e:#}"),
            Ok(Ok(s)) => {
                s.close().await;
                bail!("the second viewer got a session while the first held the agent");
            }
        };
        ctx.note(format!("second viewer while the first is idle: {second}"));
        // Once the idle one closes, a next viewer is served; the agent first works
        // through the handshakes that waited meanwhile (each until its own 10 s
        // timeout), so one more attempt may time out before.
        idle.close(0u32.into(), b"bye");
        let t = Instant::now();
        let mut attempts = Vec::new();
        let s = loop {
            match HeadlessViewer::connect_direct(addr, Some(fp), ViewerOptions::default()).await {
                Ok(s) => break s,
                Err(e) if attempts.len() < 3 => attempts.push(format!("{e:#}")),
                Err(e) => bail!("no session after the idle one closed: {attempts:?} {e:#}"),
            }
        };
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        ctx.note(format!(
            "served {} ms after the idle one closed; failed attempts before: {attempts:?}",
            t.elapsed().as_millis()
        ));
        s.close().await;
        anyhow::Ok(())
    })?;
    drop(agent);
    Ok(())
}
