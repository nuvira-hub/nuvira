//! Sessions through a local `nuvira-server` (signalling, `Route`, the relay or the
//! direct path, the WebSocket transport) with agents registered through
//! `serve_via_server`: what a real session goes through, on 127.0.0.1.
//!
//! Pinned here: direct path, forced relay, WebSocket; temporary password right
//! and wrong (a viewer locked after 5 wrong ones, Phase 6 lot 3), confirmation
//! accepted, refused and unanswered,
//! permanent password with a two-factor code (replay refused, lockout after 5
//! wrong codes); an impostor viewer refused by the agent's certificate check;
//! server restart; impaired relay; several agents and viewers at once and the
//! shared session with its guest limit; abrupt kill of the viewer; the idle
//! limit fed by the viewer's input; the baseline bugs (back-to-back sessions
//! refused as busy, churn signalling timeouts). Since Phase 6 lot 2 (AGT-1): an
//! unauthenticated viewer does not keep the agent busy and is let go after 10 s,
//! connections waiting for their password are capped, of two viewers connecting
//! at once the first to authenticate gets the session, and the longer waits
//! (confirmation, two-factor code) are not cut by the 10 s deadline.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_agent::{AgentCommand, AgentEvent};
use nuvira_e2e::rig::{self, AgentSpec, HeadlessViewer, LocalServer, Raw, ServerAgent, Srv};
use nuvira_e2e::runner::{Case, Ctx, Group, Need, ensure};
use nuvira_proto::v1::ControlMessage;
use nuvira_proto::v1::control_message::Msg;
use nuvira_transport::identity::Identity;
use nuvira_transport::p2p;
use nuvira_transport::signal::TransportMode;

const SRV: &[Need] = &[Need::Server];

pub fn cases() -> Vec<Case> {
    vec![
        Case::new("server/udp/session-direct-path", udp_direct).needs(SRV),
        Case::new("server/udp/session-forced-relay", forced_relay)
            .needs(SRV)
            .env(&[("NUVIRA_FORCE_RELAY", "1")]),
        Case::new("server/websocket/session-through-relay", websocket).needs(SRV),
        Case::new("server/relay/impaired-relay-session", impaired_relay)
            .needs(SRV)
            .env(&[("NUVIRA_FORCE_RELAY", "1")]),
        Case::new("server/auth/wrong-passwords-lock-the-viewer", wrong_password).needs(SRV),
        Case::new("server/auth/confirmation-accept-and-refuse", confirmation).needs(SRV),
        Case::new("server/auth/two-factor-replay-and-lockout", two_factor).needs(SRV),
        Case::new("server/security/impostor-viewer-refused", impostor).needs(SRV).timeout_s(240),
        Case::new("server/security/unauthenticated-viewer-does-not-keep-agent-busy", unauth_busy)
            .needs(SRV)
            .timeout_s(240),
        Case::new("server/security/pending-handshakes-capped", pending_capped).needs(SRV).timeout_s(240),
        Case::new("server/auth/two-viewers-authenticate-at-once", two_at_once).needs(SRV),
        Case::new("server/reconnect/characterises_bug_back_to_back_busy", back_to_back).needs(SRV),
        Case::new("server/reconnect/server-restart", server_restart).needs(SRV),
        Case::new("server/kill/viewer-process", kill_viewer).needs(SRV),
        Case::new("server/concurrent/agents-and-viewers", concurrent).needs(SRV),
        Case::new("server/concurrent/shared-session-guests", guests).needs(SRV),
        Case::new("server/auth/confirmation-no-answer", confirmation_no_answer).needs(SRV).group(Group::Long),
        Case::new("server/auth/confirmation-answered-at-29s", confirmation_late).needs(SRV).group(Group::Long),
        Case::new("server/auth/two-factor-code-after-60s", two_factor_late)
            .needs(SRV)
            .group(Group::Long)
            .timeout_s(240),
        Case::new("server/idle/input-counts-as-activity", idle_limit)
            .needs(SRV)
            .group(Group::Long)
            .timeout_s(400),
        Case::new("server/churn/characterises_bug_signalling_timeout", churn)
            .needs(SRV)
            .group(Group::Long)
            .timeout_s(900),
    ]
}

/// A local server with one agent online.
struct Rig {
    server: LocalServer,
    srv: Srv,
    art: tokio::runtime::Runtime,
    vrt: tokio::runtime::Runtime,
    agent: ServerAgent,
    id: String,
}

impl Rig {
    fn start(ctx: &Ctx, spec: AgentSpec) -> Result<Rig> {
        Self::start_with(ctx, spec, None)
    }

    fn start_with(ctx: &Ctx, spec: AgentSpec, impair: Option<&str>) -> Result<Rig> {
        let server = rig::server(&ctx.tmp.join("server"), impair)?;
        let srv = Srv::from(&server);
        let (art, vrt) = rig::runtimes()?;
        let ws = spec.transport == TransportMode::WebSocket;
        let agent = ServerAgent::start(&art, spec, &server, &ctx.tmp.join("agent"))?;
        let (_, id, websocket) = vrt.block_on(agent.online(0, Duration::from_secs(30)))?;
        ensure(websocket == ws, || format!("agent online with websocket={websocket}"))?;
        Ok(Rig { server, srv, art, vrt, agent, id })
    }

    fn viewer(&self, ctx: &Ctx, name: &str) -> Result<Identity> {
        rig::identity(&ctx.tmp.join(name))
    }

    /// The next `SessionEnded` from event `from`: (index, error).
    async fn ended(&self, from: usize, timeout: Duration) -> Result<(usize, Option<String>)> {
        let (i, e) =
            self.agent.events.wait(from, timeout, |e| matches!(e, AgentEvent::SessionEnded { .. })).await?;
        let AgentEvent::SessionEnded { error } = e else { unreachable!() };
        Ok((i, error))
    }

    fn stop(self) {
        let Rig { server, art, vrt, agent, .. } = self;
        art.block_on(agent.stop());
        drop(server);
        art.shutdown_timeout(Duration::from_secs(2));
        vrt.shutdown_timeout(Duration::from_secs(2));
    }
}

/// A headless session through the server with the temporary password.
async fn open(
    rig: &Rig,
    viewer: &Identity,
    mode: TransportMode,
) -> Result<(rig::HeadlessSession, p2p::Path)> {
    let (conn, path) = rig::connect_via(&rig.srv, viewer, &rig.id, mode).await?;
    Ok((HeadlessViewer::from_connection(conn, rig::opts(rig::PASSWORD)).await?, path))
}

/// One whole session through the server: frames, SessionStarted (with the path),
/// a normal close and SessionEnded. Returns the path.
fn session(ctx: &Ctx, rig: &Rig, mode: TransportMode) -> Result<p2p::Path> {
    let viewer = rig.viewer(ctx, "viewer")?;
    let from = rig.agent.events.len();
    let path = rig.vrt.block_on(async {
        let t = Instant::now();
        let (s, path) = open(rig, &viewer, mode).await?;
        let n = rig::frames(&s, 30, Duration::from_secs(20)).await?;
        ctx.note(format!("{path:?}: {n} frames, {} ms from Connect", t.elapsed().as_millis()));
        ensure(s.agent.password_required && !s.agent.file_manager, || format!("{:?}", s.agent))?;
        ensure(s.agent.handover, || "no handover offered through the server".into())?;
        s.close().await;
        anyhow::Ok(path)
    })?;
    let (_, error) = rig.vrt.block_on(rig.ended(from, Duration::from_secs(15)))?;
    let events = rig.agent.events.all();
    let started = events[from..].iter().find_map(|e| match e {
        AgentEvent::SessionStarted { direct, .. } => Some(*direct),
        _ => None,
    });
    ensure(started == Some(path == p2p::Path::Direct), || {
        format!("SessionStarted {started:?} for path {path:?}")
    })?;
    ctx.note(format!("SessionEnded error: {error:?}"));
    // The viewer's normal close is reported with its code 0 reason.
    ensure(error.as_deref().is_none_or(|e| e.contains("(code 0)")), || format!("SessionEnded {error:?}"))?;
    Ok(path)
}

fn udp_direct(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let path = session(ctx, &rig, TransportMode::Udp)?;
    // On one machine the peers' local addresses answer: the direct path wins.
    ensure(path == p2p::Path::Direct, || format!("path {path:?}"))?;
    rig.stop();
    Ok(())
}

fn forced_relay(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let path = session(ctx, &rig, TransportMode::Udp)?;
    ensure(path == p2p::Path::Relay, || format!("path {path:?}"))?;
    rig.stop();
    Ok(())
}

fn websocket(ctx: &mut Ctx) -> Result<()> {
    let spec = AgentSpec { transport: TransportMode::WebSocket, ..Default::default() };
    let rig = Rig::start(ctx, spec)?;
    // No direct attempt over the WebSocket: always the relay.
    let path = session(ctx, &rig, TransportMode::WebSocket)?;
    ensure(path == p2p::Path::Relay, || format!("path {path:?}"))?;
    rig.stop();
    Ok(())
}

fn impaired_relay(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start_with(ctx, AgentSpec::default(), Some("delay=40ms,jitter=10ms,loss=1%"))?;
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        let (mut s, path) = open(&rig, &viewer, TransportMode::Udp).await?;
        ensure(path == p2p::Path::Relay, || format!("path {path:?}"))?;
        s.first_frame(Duration::from_secs(20)).await?;
        let st = s.run(Duration::from_secs(6)).await?;
        ctx.note(format!("{:.1} fps, rtt p50 {:.0} ms, {} freezes", st.fps, st.rtt_ms.p50, st.freezes));
        // Frames keep flowing (the rate is a benchmark matter, and the NUC under
        // load encoded at 3-4 fps).
        ensure(st.frames >= 10, || format!("{} frames ({:.1} fps)", st.frames, st.fps))?;
        // The relay's delay (both directions) shows in the round trip.
        ensure(st.rtt_ms.p50 >= 60.0, || format!("rtt p50 {:.1} ms", st.rtt_ms.p50))?;
        s.close().await;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// One attempt on the raw control channel: Hello, Auth, and the first answer.
struct Attempt {
    raw: Raw,
    first: Msg,
}

async fn attempt(
    rig: &Rig,
    viewer: &Identity,
    password: &str,
    code: &str,
    two_factor: bool,
) -> Result<Attempt> {
    let (conn, _) = rig::connect_via(&rig.srv, viewer, &rig.id, TransportMode::Udp).await?;
    let mut raw = Raw::open(conn).await?;
    let hello = raw.hello(two_factor).await?;
    ensure(hello.password_required, || "no password asked".into())?;
    let first = raw.auth(password, code).await?;
    Ok(Attempt { raw, first })
}

impl Attempt {
    /// The AuthResult (after a CodeRequired answered with `code`, if given).
    async fn result(&mut self, password: &str, code: Option<&str>) -> Result<(bool, String)> {
        if let (Msg::CodeRequired(_), Some(code)) = (&self.first, code) {
            self.first = self.raw.auth(password, code).await?;
        }
        rig::auth_result(&self.first)
    }

    async fn closed(&self) -> Result<(String, Duration)> {
        rig::wait_closed(&self.raw.conn, Duration::from_secs(10)).await
    }
}

/// Temporary password (review AGT-4, Phase 6 lot 3; was the Phase 2 pin
/// `server/auth/wrong-password-no-lockout`, K36): each of the first 5 wrong ones
/// is refused after a 2 s pause; from then that viewer ID is refused like a wrong
/// password, also with the right one, and the connection closes without the 2 s
/// pause; another viewer ID still gets in with the right password.
fn wrong_password(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        let mut from = rig.agent.events.len();
        for i in 0..7 {
            // 0..5 wrong and checked; 5 wrong and 6 right, both refused unchecked.
            let (password, checked) = match i {
                0..5 => (format!("wrong-{i}"), true),
                5 => ("wrong-5".to_string(), false),
                _ => (rig::PASSWORD.to_string(), false),
            };
            let mut a = attempt(&rig, &viewer, &password, "", true).await?;
            let r = a.result("", None).await?;
            let (ended, after) = a.closed().await?;
            ensure(r == (false, "@agent.wrong_password".into()), || format!("attempt {i}: {r:?}"))?;
            ensure(ended == "closed 0 @agent.wrong_password", || format!("attempt {i}: {ended}"))?;
            let pause_ok = match checked {
                true => after >= Duration::from_millis(1800),
                false => after < Duration::from_millis(1500),
            };
            ensure(pause_ok, || format!("attempt {i}: closed after {after:?}"))?;
            let (i2, error) = rig.ended(from, Duration::from_secs(15)).await?;
            ensure(error.as_deref().is_some_and(|e| e.contains("@agent.wrong_password")), || {
                format!("{error:?}")
            })?;
            from = i2 + 1;
        }
        ctx.note("5 wrong passwords refused after about 2 s, then that viewer at once (right password too)");
        let other = rig.viewer(ctx, "viewer2")?;
        let (s, _) = open(&rig, &other, TransportMode::Udp).await?;
        rig::frames(&s, 10, Duration::from_secs(20)).await.context("another viewer with the right password")?;
        s.close().await;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// `confirm`: after the right password the viewer is told to wait
/// (ConfirmationPending, 30 s) and the app gets `Incoming`; accepted -> session,
/// refused -> `@agent.not_accepted`; a wrong password is refused without asking.
fn confirmation(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec { confirm: true, ..Default::default() })?;
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        for accept in [true, false] {
            let from = rig.agent.events.len();
            let mut a = attempt(&rig, &viewer, rig::PASSWORD, "", true).await?;
            let Msg::ConfirmationPending(p) = &a.first else {
                bail!("expected ConfirmationPending, got {:?}", a.first)
            };
            ensure(p.timeout_s == 30, || format!("timeout {}", p.timeout_s))?;
            let (_, e) = rig
                .agent
                .events
                .wait(from, Duration::from_secs(10), |e| matches!(e, AgentEvent::Incoming { .. }))
                .await?;
            let AgentEvent::Incoming { session_id, viewer_name, .. } = e else { unreachable!() };
            ensure(viewer_name == "nuvira-e2e viewer", || format!("viewer name {viewer_name:?}"))?;
            rig.agent.command(AgentCommand::Respond { session_id, accept })?;
            let r = match a.raw.next(Duration::from_secs(10)).await? {
                Some(ControlMessage { msg: Some(m) }) => rig::auth_result(&m)?,
                other => bail!("after Respond: {other:?}"),
            };
            rig.agent
                .events
                .wait(from, Duration::from_secs(5), |e| matches!(e, AgentEvent::IncomingClosed { .. }))
                .await?;
            if accept {
                ensure(r == (true, String::new()), || format!("accepted: {r:?}"))?;
                a.raw.conn.close(0u32.into(), b"done");
            } else {
                ensure(r == (false, "@agent.not_accepted".into()), || format!("refused: {r:?}"))?;
                let (ended, _) = a.closed().await?;
                ensure(ended == "closed 0 @agent.not_accepted", || ended.clone())?;
            }
            rig.ended(from, Duration::from_secs(15)).await?;
            ctx.note(format!("accept={accept}: {r:?}"));
        }
        // A wrong password: no question to the person at the PC.
        let from = rig.agent.events.len();
        let mut a = attempt(&rig, &viewer, "wrong", "", true).await?;
        let r = a.result("", None).await?;
        ensure(r == (false, "@agent.wrong_password".into()), || format!("wrong password: {r:?}"))?;
        rig.ended(from, Duration::from_secs(15)).await?;
        let asked = rig.agent.events.all()[from..].iter().any(|e| matches!(e, AgentEvent::Incoming { .. }));
        ensure(!asked, || "the person at the PC was asked about a wrong password".into())?;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// Confirmation not answered: `@agent.no_answer` after the 30 s.
fn confirmation_no_answer(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec { confirm: true, ..Default::default() })?;
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        let mut a = attempt(&rig, &viewer, rig::PASSWORD, "", true).await?;
        ensure(matches!(a.first, Msg::ConfirmationPending(_)), || format!("{:?}", a.first))?;
        let t = Instant::now();
        let r = match a.raw.next(Duration::from_secs(40)).await? {
            Some(ControlMessage { msg: Some(m) }) => rig::auth_result(&m)?,
            other => bail!("{other:?}"),
        };
        ctx.note(format!("{r:?} after {} ms", t.elapsed().as_millis()));
        ensure(r == (false, "@agent.no_answer".into()), || format!("{r:?}"))?;
        ensure(t.elapsed() >= Duration::from_secs(29), || format!("after {:?}", t.elapsed()))?;
        // The app is told too (the event may arrive just after the answer).
        rig.agent
            .events
            .wait(0, Duration::from_secs(5), |e| matches!(e, AgentEvent::IncomingClosed { .. }))
            .await
            .context("no IncomingClosed")?;
        a.raw.conn.close(0u32.into(), b"done");
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// Permanent password with a two-factor code (`access.json` next to the device
/// key): CodeRequired, then the code; the same code again refused; a viewer that
/// cannot ask for codes told to update; a code of an earlier step refused; after
/// 5 wrong codes every permanent password is refused (`@agent.code_locked`), also
/// with a valid unused code, while the temporary password still works.
fn two_factor(ctx: &mut Ctx) -> Result<()> {
    const PERMANENT: &str = "permanent-e2e-pass";
    let secret = nuvira_secret::totp_secret();
    let spec = AgentSpec { permanent_hash: Some(nuvira_secret::hash(PERMANENT)), ..Default::default() };
    let rules = nuvira_agent::access::AccessRules { totp: Some(secret.clone()), ..Default::default() };
    rules.save(&ctx.tmp.join("agent"))?;
    let rig = Rig::start(ctx, spec)?;
    let viewer = rig.viewer(ctx, "viewer")?;
    let step =
        || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 30);
    rig.vrt.block_on(async {
        let now = step();
        let code = nuvira_secret::totp_code(&secret, now).context("code")?;
        let mut from = rig.agent.events.len();
        let mut a = attempt(&rig, &viewer, PERMANENT, "", true).await?;
        ensure(matches!(a.first, Msg::CodeRequired(_)), || {
            format!("expected CodeRequired, got {:?}", a.first)
        })?;
        let r = a.result(PERMANENT, Some(&code)).await?;
        ensure(r == (true, String::new()), || format!("right code: {r:?}"))?;
        a.raw.conn.close(0u32.into(), b"done");
        from = rig.ended(from, Duration::from_secs(15)).await?.0 + 1;
        let checks: Vec<(&str, &str, String, bool, &str)> = vec![
            // The same code again (replay): refused, wrong code 1.
            ("replayed code", PERMANENT, code.clone(), true, "@agent.code_wrong"),
            // A viewer without two-factor support: told to update (not counted).
            ("viewer without two_factor", PERMANENT, String::new(), false, "@server.old_version"),
            // A code of an earlier step than the last accepted: wrong code 2.
            (
                "earlier step",
                PERMANENT,
                nuvira_secret::totp_code(&secret, now - 1).context("code")?,
                true,
                "@agent.code_wrong",
            ),
            ("wrong code 3", PERMANENT, "000000".into(), true, "@agent.code_wrong"),
            ("wrong code 4", PERMANENT, "000001".into(), true, "@agent.code_wrong"),
            ("wrong code 5", PERMANENT, "000002".into(), true, "@agent.code_wrong"),
            // Locked: even a valid code of the next step, never used.
            (
                "valid code after 5 wrong",
                PERMANENT,
                nuvira_secret::totp_code(&secret, step() + 1).context("code")?,
                true,
                "@agent.code_locked",
            ),
            // The temporary password is not affected by the lock.
            ("temporary password while locked", rig::PASSWORD, String::new(), true, ""),
        ];
        for (label, password, code, two, want) in checks {
            let mut a = attempt(&rig, &viewer, password, &code, two).await?;
            let r = a.result(password, None).await?;
            ctx.note(format!("{label}: {r:?}"));
            ensure(r == (want.is_empty(), want.to_string()), || {
                format!("{label}: {r:?}, expected {want:?}")
            })?;
            if want.is_empty() {
                a.raw.conn.close(0u32.into(), b"done");
            }
            from = rig.ended(from, Duration::from_secs(15)).await?.0 + 1;
        }
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// A viewer that got the route with its key cannot be replaced by another key:
/// the agent's certificate check refuses the impostor (and gives up on the
/// session after 20 s); a viewer expecting another agent key refuses the agent.
/// Afterwards the right viewer connects.
fn impostor(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let viewer = rig.viewer(ctx, "viewer")?;
    let other = rig.viewer(ctx, "impostor")?;
    rig.vrt.block_on(async {
        // 1. The route asked by `viewer`, the connection made with `other`'s key.
        let from = rig.agent.events.len();
        let mut sig = rig::viewer_signal(&rig.srv, &viewer, TransportMode::Udp).await?;
        let route = rig::request_route(&rig.srv, &mut sig, &rig.id).await?;
        let t = Instant::now();
        let r = match tokio::time::timeout(Duration::from_secs(25), p2p::viewer_connect(&other, &route)).await
        {
            Err(_) => "no connection in 25 s".to_string(),
            Ok(Err(e)) => format!("refused: {e:#}"),
            // TLS 1.3: the client certificate is checked after the client finished
            // its handshake: the first stream fails then.
            Ok(Ok((conn, _))) => match Raw::open(conn).await {
                Ok(mut raw) => match raw.hello(true).await {
                    Ok(h) => bail!("the impostor got the agent's Hello: {h:?}"),
                    Err(e) => format!("connected, then refused: {e:#}"),
                },
                Err(e) => format!("connected, then refused: {e:#}"),
            },
        };
        ctx.note(format!("impostor key: {r} ({} ms)", t.elapsed().as_millis()));
        drop(sig);
        let (_, error) = rig.ended(from, Duration::from_secs(30)).await?;
        ctx.note(format!("agent: {error:?} {} ms after the route", t.elapsed().as_millis()));
        ensure(error.as_deref().is_some_and(|e| e.contains("did not connect within 20 s")), || {
            format!("{error:?}")
        })?;
        // 2. The viewer expects another agent key.
        let from = rig.agent.events.len();
        let mut sig = rig::viewer_signal(&rig.srv, &viewer, TransportMode::Udp).await?;
        let mut route = rig::request_route(&rig.srv, &mut sig, &rig.id).await?;
        route.peer_public_key[0] ^= 0xff;
        let r =
            match tokio::time::timeout(Duration::from_secs(25), p2p::viewer_connect(&viewer, &route)).await {
                Err(_) => "no connection in 25 s".to_string(),
                Ok(Err(e)) => format!("{e:#}"),
                Ok(Ok(_)) => bail!("the viewer accepted an agent with another key"),
            };
        ctx.note(format!("wrong agent key: {r}"));
        drop(sig);
        let (_, error) = rig.ended(from, Duration::from_secs(30)).await?;
        ctx.note(format!("agent: {error:?}"));
        // 3. The right viewer.
        let (s, _) = open(&rig, &viewer, TransportMode::Udp).await?;
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        s.close().await;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// Was KNOWN BUG K33 (Phase 2): a viewer that knew only the device ID connected,
/// sent its Hello and never the password, and the agent refused every other
/// viewer as busy for as long as it stayed. Fixed in Phase 6 lot 2 (AGT-1): it
/// does not make the agent busy (another viewer is served at once), the agent
/// lets it go after 10 s, and its end is not reported to the app while the other
/// session goes on.
fn unauth_busy(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let squatter = rig.viewer(ctx, "squatter")?;
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        let (conn, _) = rig::connect_via(&rig.srv, &squatter, &rig.id, TransportMode::Udp).await?;
        let mut raw = Raw::open(conn).await?;
        raw.hello(true).await?;
        let t = Instant::now();
        let from = rig.agent.events.len();
        let (s, _) = open(&rig, &viewer, TransportMode::Udp).await.context("viewer while the squatter waits")?;
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        let served = t.elapsed();
        ctx.note(format!("other viewer served {} ms after the squatter's Hello", served.as_millis()));
        ensure(raw.conn.close_reason().is_none(), || {
            format!("squatter closed before the other viewer was served: {:?}", raw.conn.close_reason())
        })?;
        let (ended, _) = rig::wait_closed(&raw.conn, Duration::from_secs(20)).await?;
        let after = t.elapsed();
        ctx.note(format!("squatter: {ended} {} ms after its Hello", after.as_millis()));
        ensure(ended == "closed 0 end of session", || ended.clone())?;
        ensure(after >= Duration::from_secs(9) && after <= Duration::from_secs(16), || format!("after {after:?}"))?;
        // The session goes on, and the app still sees it.
        rig::frames(&s, 10, Duration::from_secs(20)).await.context("frames after the squatter left")?;
        let ends = rig.agent.events.all()[from..]
            .iter()
            .filter(|e| matches!(e, AgentEvent::SessionEnded { .. }))
            .count();
        ensure(ends == 0, || format!("SessionEnded during the session: {:?}", rig.agent.events.kinds(from)))?;
        s.close().await;
        let (_, error) = rig.ended(from, Duration::from_secs(15)).await?;
        ctx.note(format!("SessionEnded of the session: {error:?}"));
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// AGT-1 (Phase 6 lot 2): at most MAX_PENDING_SESSIONS sessions may wait for
/// their viewer's password at once. Squatters that ask for a route and never
/// connect (the agent waits for each until the relay gives up on it, about 15 s
/// after the route, or 20 s for the connection): with one place left a viewer is
/// served, with none it is refused as busy, and once the waits end it is served.
fn pending_capped(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let viewer = rig.viewer(ctx, "viewer")?;
    let max = nuvira_agent::MAX_PENDING_SESSIONS as usize;
    rig.vrt.block_on(async {
        let mut squatters = Vec::new();
        let t = Instant::now();
        for i in 0..max {
            if i == max - 1 {
                let from = rig.agent.events.len();
                let (s, _) = open(&rig, &viewer, TransportMode::Udp).await.context("with one place left")?;
                rig::frames(&s, 10, Duration::from_secs(20)).await?;
                s.close().await;
                rig.ended(from, Duration::from_secs(15)).await?;
                ctx.note(format!("served with {} waiting, {} ms", squatters.len(), t.elapsed().as_millis()));
            }
            let who = rig.viewer(ctx, &format!("squatter-{i}"))?;
            let mut sig = rig::viewer_signal(&rig.srv, &who, TransportMode::Udp).await?;
            rig::request_route(&rig.srv, &mut sig, &rig.id).await?;
            squatters.push(sig);
        }
        let full = t.elapsed();
        match open(&rig, &viewer, TransportMode::Udp).await {
            Ok((s, _)) => {
                s.close().await;
                bail!("served with {max} sessions waiting for their password")
            }
            Err(e) => {
                let e = format!("{e:#}");
                ctx.note(format!("with {max} waiting ({} ms): {e}", full.as_millis()));
                ensure(e.contains("@agent.busy"), || e.clone())?;
            }
        }
        ensure(t.elapsed() < Duration::from_secs(19), || format!("too slow to fill: {:?}", t.elapsed()))?;
        // The waits end (relay bind about 15 s, connection 20 s): the agent serves again.
        let mut tries = Vec::new();
        let s = loop {
            match open(&rig, &viewer, TransportMode::Udp).await {
                Ok((s, _)) => break s,
                Err(e) if t.elapsed() < Duration::from_secs(45) => {
                    tries.push(format!("{:.1} s: {e:#}", t.elapsed().as_secs_f64()));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => bail!("not served after the waits ended: {tries:?} {e:#}"),
            }
        };
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        ctx.note(format!("served again {} ms after the first squatter; refused before: {tries:?}", t.elapsed().as_millis()));
        s.close().await;
        drop(squatters);
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// AGT-1 (Phase 6 lot 2): two viewers in the handshake at once (neither counts
/// as busy before its password): the first to authenticate gets the session, the
/// other is refused as busy after its right password, and the app sees one
/// session that ends once, when it ends.
fn two_at_once(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let (v1, v2) = (rig.viewer(ctx, "first")?, rig.viewer(ctx, "second")?);
    rig.vrt.block_on(async {
        let from = rig.agent.events.len();
        let mut a = Raw::open(rig::connect_via(&rig.srv, &v1, &rig.id, TransportMode::Udp).await?.0).await?;
        a.hello(true).await?;
        let mut b = Raw::open(rig::connect_via(&rig.srv, &v2, &rig.id, TransportMode::Udp).await?.0).await?;
        b.hello(true).await?;
        let ra = rig::auth_result(&a.auth(rig::PASSWORD, "").await?)?;
        ensure(ra == (true, String::new()), || format!("first: {ra:?}"))?;
        let rb = rig::auth_result(&b.auth(rig::PASSWORD, "").await?)?;
        ensure(rb == (false, "@agent.busy".into()), || format!("second: {rb:?}"))?;
        let (ended, _) = rig::wait_closed(&b.conn, Duration::from_secs(10)).await?;
        ensure(ended == "closed 0 @agent.busy", || ended.clone())?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let events = rig.agent.events.all();
        let count = |f: fn(&AgentEvent) -> bool| events[from..].iter().filter(|e| f(e)).count();
        let started = count(|e| matches!(e, AgentEvent::SessionStarted { .. }));
        let ends = count(|e| matches!(e, AgentEvent::SessionEnded { .. }));
        ctx.note(format!("events: {:?}", rig.agent.events.kinds(from)));
        ensure(started == 1 && ends == 0, || format!("started {started}, ended {ends}"))?;
        ensure(a.conn.close_reason().is_none(), || format!("first closed: {:?}", a.conn.close_reason()))?;
        a.conn.close(0u32.into(), b"done");
        rig.ended(from, Duration::from_secs(15)).await?;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// AGT-1 scope guard (Phase 6 lot 2): the 10 s handshake deadline is not around
/// the confirmation: an answer 29 s after the request is still accepted.
fn confirmation_late(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec { confirm: true, ..Default::default() })?;
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        let from = rig.agent.events.len();
        let mut a = attempt(&rig, &viewer, rig::PASSWORD, "", true).await?;
        ensure(matches!(a.first, Msg::ConfirmationPending(_)), || format!("{:?}", a.first))?;
        let (_, e) = rig
            .agent
            .events
            .wait(from, Duration::from_secs(10), |e| matches!(e, AgentEvent::Incoming { .. }))
            .await?;
        let AgentEvent::Incoming { session_id, .. } = e else { unreachable!() };
        tokio::time::sleep(Duration::from_secs(29)).await;
        rig.agent.command(AgentCommand::Respond { session_id, accept: true })?;
        let r = match a.raw.next(Duration::from_secs(10)).await? {
            Some(ControlMessage { msg: Some(m) }) => rig::auth_result(&m)?,
            other => bail!("after Respond: {other:?}"),
        };
        ctx.note(format!("answered after 29 s: {r:?}"));
        ensure(r == (true, String::new()), || format!("{r:?}"))?;
        a.raw.conn.close(0u32.into(), b"done");
        rig.ended(from, Duration::from_secs(15)).await?;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// AGT-1 scope guard (Phase 6 lot 2): the 10 s handshake deadline is not around
/// the two-factor code: a code typed 60 s after CodeRequired is still accepted.
fn two_factor_late(ctx: &mut Ctx) -> Result<()> {
    const PERMANENT: &str = "permanent-e2e-pass";
    let secret = nuvira_secret::totp_secret();
    let spec = AgentSpec { permanent_hash: Some(nuvira_secret::hash(PERMANENT)), ..Default::default() };
    let rules = nuvira_agent::access::AccessRules { totp: Some(secret.clone()), ..Default::default() };
    rules.save(&ctx.tmp.join("agent"))?;
    let rig = Rig::start(ctx, spec)?;
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        let from = rig.agent.events.len();
        let mut a = attempt(&rig, &viewer, PERMANENT, "", true).await?;
        ensure(matches!(a.first, Msg::CodeRequired(_)), || format!("expected CodeRequired, got {:?}", a.first))?;
        tokio::time::sleep(Duration::from_secs(60)).await;
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 30);
        let code = nuvira_secret::totp_code(&secret, now).context("code")?;
        let r = a.result(PERMANENT, Some(&code)).await?;
        ctx.note(format!("code after 60 s: {r:?}"));
        ensure(r == (true, String::new()), || format!("{r:?}"))?;
        a.raw.conn.close(0u32.into(), b"done");
        rig.ended(from, Duration::from_secs(15)).await?;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// KNOWN BUG (Phase 2, baseline B-MACRO-07 back-to-back/server): a new session
/// asked right after the previous one closed is refused (`@agent.busy`, or the
/// connection closes before the agent's Hello): the agent still counts the old
/// session while it closes its endpoint (`endpoint.wait_idle()` in
/// `relayed_session`), and lets the new viewer in as a guest of the session that
/// is ending, whose guest session then fails. Once `SessionEnded` came, the next
/// one works. Expected after fix: the immediate reconnection works too.
///
/// Whether a reconnection lands in that window depends on how fast it is: with a
/// new signalling connection (as the viewer does) it took about 650 ms here and
/// missed the window on this small source, while the baseline (1080p60, bench)
/// hit it every time. Pinned deterministically: the `Connect` sent on a
/// signalling connection already open, right after the close. The fresh-signalling
/// variant is recorded in the notes only.
fn back_to_back(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let viewer = rig.viewer(ctx, "viewer")?;
    let wait_ended = |from: usize, n: usize| {
        let rig = &rig;
        async move {
            let count = || {
                rig.agent.events.all()[from..]
                    .iter()
                    .filter(|e| matches!(e, AgentEvent::SessionEnded { .. }))
                    .count()
            };
            let deadline = Instant::now() + Duration::from_secs(15);
            while count() < n && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ensure(count() >= n, || format!("{} SessionEnded, expected {n}", count()))
        }
    };
    rig.vrt.block_on(async {
        let mut immediate = Vec::new();
        for i in 0..3 {
            let from = rig.agent.events.len();
            let (s, _) =
                open(&rig, &viewer, TransportMode::Udp).await.with_context(|| format!("session {i}"))?;
            rig::frames(&s, 10, Duration::from_secs(20)).await.with_context(|| format!("session {i}"))?;
            // The next session's signalling is ready before the close.
            let mut sig = rig::viewer_signal(&rig.srv, &viewer, TransportMode::Udp).await?;
            s.close().await;
            let t = Instant::now();
            let r = async {
                let route = rig::request_route(&rig.srv, &mut sig, &rig.id).await?;
                let (conn, _) = p2p::viewer_connect(&viewer, &route).await?;
                let s = HeadlessViewer::from_connection(conn, rig::opts(rig::PASSWORD)).await?;
                rig::frames(&s, 10, Duration::from_secs(20)).await?;
                s.close().await;
                anyhow::Ok(())
            }
            .await
            .map_err(|e| format!("{e:#}"));
            drop(sig);
            ctx.note(format!("immediate reconnection {i}: {r:?} ({} ms)", t.elapsed().as_millis()));
            let ok = r.is_ok();
            immediate.push(r);
            wait_ended(from, if ok { 2 } else { 1 }).await?;
            // After SessionEnded: works.
            let from = rig.agent.events.len();
            let (s, _) =
                open(&rig, &viewer, TransportMode::Udp).await.with_context(|| format!("after {i}"))?;
            rig::frames(&s, 10, Duration::from_secs(20)).await.with_context(|| format!("after {i}"))?;
            // Fresh signalling right after the close (recorded, not asserted).
            s.close().await;
            let t = Instant::now();
            let r = async {
                let (s, _) = open(&rig, &viewer, TransportMode::Udp).await?;
                rig::frames(&s, 10, Duration::from_secs(20)).await?;
                s.close().await;
                anyhow::Ok(())
            }
            .await
            .map_err(|e| format!("{e:#}"));
            ctx.note(format!("fresh-signalling reconnection {i}: {r:?} ({} ms)", t.elapsed().as_millis()));
            wait_ended(from, if r.is_ok() { 2 } else { 1 }).await?;
        }
        let refused = |e: &String| e.contains("@agent.busy") || e.contains("did not introduce itself");
        ensure(immediate.iter().all(|r| r.as_ref().is_err_and(refused)), || {
            format!("immediate {immediate:?}")
        })?;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// The server is killed and started again (same port, data, certificate): the
/// agent goes offline, registers again with the same ID (5 s retry), and a new
/// session works.
fn server_restart(ctx: &mut Ctx) -> Result<()> {
    let mut rig = Rig::start(ctx, AgentSpec::default())?;
    let from = rig.agent.events.len();
    let t = Instant::now();
    rig.server.restart()?;
    if let Some(pid) = rig.server.pid() {
        nuvira_e2e::runner::track_pid(pid);
    }
    let (_, id, _) = rig.vrt.block_on(rig.agent.online(from, Duration::from_secs(40)))?;
    ctx.note(format!("online again after {} ms", t.elapsed().as_millis()));
    ensure(id == rig.id, || format!("came back with another ID {id}"))?;
    let offline = rig.agent.events.all()[from..].iter().any(|e| matches!(e, AgentEvent::Offline { .. }));
    ensure(offline, || "no Offline event".into())?;
    session(ctx, &rig, TransportMode::Udp)?;
    rig.stop();
    Ok(())
}

/// The viewer's process is killed in a session through the server: the agent
/// ends the session by the idle timeout and is free for the next viewer.
fn kill_viewer(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let fp = nuvira_transport::fingerprint_hex(&rig.srv.fingerprint);
    let dir = ctx.tmp.join("viewer-host").display().to_string();
    let from = rig.agent.events.len();
    let addr = rig.srv.addr.to_string();
    let mut host = rig::Host::spawn(&[
        "--e2e-viewer-host",
        "server",
        &addr,
        &fp,
        &rig.srv.ws,
        &rig.id,
        rig::PASSWORD,
        &dir,
    ])?;
    host.line("frames", Duration::from_secs(40))?;
    host.kill();
    let t = Instant::now();
    let (_, error) = rig.vrt.block_on(rig.ended(from, Duration::from_secs(30)))?;
    let after = t.elapsed();
    ctx.note(format!("SessionEnded {} ms after the kill: {error:?}", after.as_millis()));
    ensure(error.as_deref().is_some_and(|e| e.contains("timed out")), || format!("{error:?}"))?;
    ensure(after >= Duration::from_secs(5) && after <= Duration::from_secs(16), || format!("{after:?}"))?;
    session(ctx, &rig, TransportMode::Udp)?;
    rig.stop();
    Ok(())
}

/// Three agents on one server and three viewers at once, one per agent, twice.
fn concurrent(ctx: &mut Ctx) -> Result<()> {
    let server = rig::server(&ctx.tmp.join("server"), None)?;
    let srv = Srv::from(&server);
    let (art, vrt) = rig::runtimes()?;
    let mut agents = Vec::new();
    for i in 0..3 {
        agents.push(ServerAgent::start(
            &art,
            AgentSpec::default(),
            &server,
            &ctx.tmp.join(format!("agent{i}")),
        )?);
    }
    vrt.block_on(async {
        let mut ids = Vec::new();
        for a in &agents {
            ids.push(a.online(0, Duration::from_secs(30)).await?.1);
        }
        let mut distinct = ids.clone();
        distinct.sort();
        distinct.dedup();
        ensure(distinct.len() == 3, || format!("IDs {ids:?}"))?;
        for round in 0..2 {
            let t = Instant::now();
            let mut tasks = Vec::new();
            for (i, id) in ids.iter().enumerate() {
                let (srv, id) = (srv.clone(), id.clone());
                let viewer = rig::identity(&ctx.tmp.join(format!("viewer{i}")))?;
                tasks.push(tokio::spawn(async move {
                    let (conn, path) = rig::connect_via(&srv, &viewer, &id, TransportMode::Udp).await?;
                    let s = HeadlessViewer::from_connection(conn, rig::opts(rig::PASSWORD)).await?;
                    let n = rig::frames(&s, 30, Duration::from_secs(30)).await?;
                    s.close().await;
                    anyhow::Ok((path, n))
                }));
            }
            for (i, task) in tasks.into_iter().enumerate() {
                let (path, n) = task.await?.with_context(|| format!("round {round}, viewer {i}"))?;
                ctx.note(format!("round {round}, viewer {i}: {path:?}, {n} frames"));
            }
            ctx.note(format!("round {round}: {} ms", t.elapsed().as_millis()));
            // Every agent ends its session before the next round (see the busy bug).
            for a in &agents {
                let ends =
                    || a.events.all().iter().filter(|e| matches!(e, AgentEvent::SessionEnded { .. })).count();
                let deadline = Instant::now() + Duration::from_secs(15);
                while ends() < round + 1 && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                ensure(ends() == round + 1, || format!("round {round}: {} sessions ended", ends()))?;
            }
        }
        anyhow::Ok(())
    })?;
    for a in agents {
        art.block_on(a.stop());
    }
    drop(server);
    Ok(())
}

/// Shared session: three more viewers join the first one's session as guests
/// (`MAX_GUESTS`), each gets frames, the agent reports them in `Guests`; a fifth
/// viewer is refused; when the first viewer closes, the guests' sessions end.
fn guests(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    rig.vrt.block_on(async {
        let (first, _) = open(&rig, &rig.viewer(ctx, "viewer0")?, TransportMode::Udp).await?;
        rig::frames(&first, 10, Duration::from_secs(20)).await?;
        let mut guests = Vec::new();
        for i in 1..=3 {
            let (s, _) = open(&rig, &rig.viewer(ctx, &format!("viewer{i}"))?, TransportMode::Udp)
                .await
                .with_context(|| format!("guest {i}"))?;
            let n =
                rig::frames(&s, 10, Duration::from_secs(20)).await.with_context(|| format!("guest {i}"))?;
            // A guest's Hello: no file manager, no handover of its own.
            ensure(!s.agent.file_manager && !s.agent.handover, || format!("guest {i} Hello {:?}", s.agent))?;
            ctx.note(format!("guest {i}: {n} frames"));
            guests.push(s);
        }
        let (_, e) = rig
            .agent
            .events
            .wait(
                0,
                Duration::from_secs(5),
                |e| matches!(e, AgentEvent::Guests { names } if names.len() == 3),
            )
            .await?;
        ctx.note(format!("{e:?}"));
        // A fourth guest: refused.
        let fourth = rig.viewer(ctx, "viewer4")?;
        let r = async {
            let (s, _) = open(&rig, &fourth, TransportMode::Udp).await?;
            s.close().await;
            anyhow::Ok(())
        }
        .await
        .map_err(|e| format!("{e:#}"));
        ctx.note(format!("fourth guest: {r:?}"));
        ensure(r.as_ref().is_err_and(|e| e.contains("@agent.busy")), || format!("fourth guest {r:?}"))?;
        // The first viewer leaves: the guests' sessions end.
        first.close().await;
        let t = Instant::now();
        for (i, g) in guests.iter().enumerate() {
            // The agent closes each guest's connection (the headless viewer's own
            // error stays empty: a close with code 0 ends its tasks quietly).
            let (ended, _) = rig::wait_closed(&g.conn, Duration::from_secs(15)).await?;
            ctx.note(format!("guest {}: {ended} after {} ms", i + 1, t.elapsed().as_millis()));
            ensure(ended == "closed 0 end of session", || format!("guest {}: {ended}", i + 1))?;
        }
        for g in guests {
            g.close().await;
        }
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// The idle limit (1 minute, `AgentCommand::IdleMinutes`): the viewer's input
/// keeps the session alive (the synthetic source injects nothing, but the agent
/// reads the input stream and counts it as activity), the warning comes at once
/// (limit minus 60 s = 0), and without input the session ends with
/// `@agent.idle_ended`; pings and feedback do not count.
fn idle_limit(ctx: &mut Ctx) -> Result<()> {
    use nuvira_proto::v1::{InputEvent, MouseMove, input_event::Event};
    let rig = Rig::start(ctx, AgentSpec::default())?;
    rig.agent.command(AgentCommand::IdleMinutes { minutes: 1 })?;
    std::thread::sleep(Duration::from_millis(300));
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        let (mut s, _) = open(&rig, &viewer, TransportMode::Udp).await?;
        ensure(s.agent.idle_limit, || "Hello.idle_limit false".into())?;
        rig::frames(&s, 10, Duration::from_secs(20)).await?;
        let t = Instant::now();
        let mut x = 0.25f32;
        while t.elapsed() < Duration::from_secs(80) {
            x = 1.0 - x;
            let ev = InputEvent { event: Some(Event::MouseMove(MouseMove { x, y: 0.5 })) };
            s.input().send(&ev).await?;
            tokio::time::sleep(Duration::from_secs(5)).await;
            ensure(s.error().is_none(), || {
                format!("ended with input after {:?}: {:?}", t.elapsed(), s.error())
            })?;
        }
        ctx.note("80 s with input every 5 s: still in session");
        let t = Instant::now();
        let (ended, _) = rig::wait_closed(&s.conn, Duration::from_secs(90)).await?;
        let after = t.elapsed();
        ctx.note(format!("without input: {ended} after {} ms", after.as_millis()));
        ensure(ended == "closed 0 @agent.idle_ended", || ended.clone())?;
        ensure(after >= Duration::from_secs(55) && after <= Duration::from_secs(75), || {
            format!("after {after:?}")
        })?;
        let mut warnings = 0;
        while let Ok(m) = s.events().try_recv() {
            if let Some(Msg::IdleWarning(_)) = m.msg {
                warnings += 1;
            }
        }
        ctx.note(format!("idle warnings {warnings}"));
        ensure(warnings >= 1, || "no IdleWarning".into())?;
        s.close().await;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}

/// KNOWN BUG (Phase 2, baseline B-RES-02 churn/server): in long series of
/// sessions through the server the viewer's signalling connection sometimes
/// times out (sessions 2, 22, 31, 44 in the baseline; 2 of 4 runs). Not
/// deterministic: pinned are the failure mode (only "viewer signalling ... timed
/// out", the next session works) and a bound on its rate (at most 5 of 60).
/// Expected after fix: no failure.
fn churn(ctx: &mut Ctx) -> Result<()> {
    let rig = Rig::start(ctx, AgentSpec::default())?;
    let viewer = rig.viewer(ctx, "viewer")?;
    rig.vrt.block_on(async {
        let mut failures = Vec::new();
        let n = 60;
        for i in 0..n {
            let from = rig.agent.events.len();
            let r = async {
                let (s, _) = open(&rig, &viewer, TransportMode::Udp).await?;
                rig::frames(&s, 10, Duration::from_secs(20)).await?;
                s.close().await;
                anyhow::Ok(())
            }
            .await;
            match r {
                Ok(()) => {
                    rig.ended(from, Duration::from_secs(15)).await.with_context(|| format!("session {i}"))?;
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    ctx.note(format!("session {i}: {e}"));
                    failures.push((i, e));
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        ctx.note(format!("{} failures in {n} sessions", failures.len()));
        ensure(
            failures.iter().all(|(_, e)| e.contains("viewer signalling") && e.contains("timed out")),
            || format!("other failures: {failures:?}"),
        )?;
        ensure(failures.len() <= 5, || format!("{} failures", failures.len()))?;
        anyhow::Ok(())
    })?;
    rig.stop();
    Ok(())
}
