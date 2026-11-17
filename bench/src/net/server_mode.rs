//! Server mode (WP4, plan 7.2 B-MACRO-05..07): a local `nuvira-server` launched
//! from the side's binary, an in-process agent registered on it through
//! `serve_via_server`, and the viewer half of `open_connection`
//! (`client/viewer/src/net.rs` :432): signalling, `Connect`, `Route`,
//! `p2p::viewer_connect`.
//!
//! The server binary is `NUVIRA_BENCH_SERVER_EXE` (set by `bench-compare.sh` and
//! `bench-remote.sh` per side), else `nuvira-server(.exe)` next to the bench
//! binary. It runs with its own data folder, `NUVIRA_LISTEN` and `NUVIRA_WS_LISTEN`
//! on `127.0.0.1:<free port>` and `NUVIRA_RATE_LIMITS=off` (20 connections a run
//! from one address would hit the per-address limits otherwise).
//!
//! Every WP4 case ([`NetCase`]) runs in a child process of its own, so
//! [`super::isolate_data_dir`] can point `NUVIRA_DATA_DIR` at the run's temporary
//! folder (and force the file key store) before any thread exists.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_agent::{AgentCommand, AgentConfig, AgentEvent, ServerOptions};
use nuvira_proto::v1::{Connect, Role, client_message, server_message};
use nuvira_transport::identity::Identity;
use nuvira_transport::signal::{self, TransportMode};
use nuvira_transport::{Connection, Fingerprint, ServerTransport, p2p};
use tokio::sync::{Notify, mpsc};

/// Fixed temporary password of the in-process agents (any string is accepted).
pub const PASSWORD: &str = "bench-4711-pass";

/// The `nuvira-server` binary of this side, or why there is none.
pub fn server_exe() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("NUVIRA_BENCH_SERVER_EXE") {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!("NUVIRA_BENCH_SERVER_EXE={} does not exist", p.display()))
        };
    }
    let name = format!("nuvira-server{}", std::env::consts::EXE_SUFFIX);
    let next = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(&name)));
    match next {
        Some(p) if p.is_file() => Ok(p),
        _ => Err(format!("no nuvira-server: set NUVIRA_BENCH_SERVER_EXE or put {name} next to nuvira-bench")),
    }
}

/// The agent binary of this side (`NUVIRA_BENCH_AGENT_EXE`, else `nuvira-agent`
/// next to the bench binary), for the two-process variants.
pub fn agent_exe() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("NUVIRA_BENCH_AGENT_EXE") {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!("NUVIRA_BENCH_AGENT_EXE={} does not exist", p.display()))
        };
    }
    let name = format!("nuvira-agent{}", std::env::consts::EXE_SUFFIX);
    let next = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(&name)));
    match next {
        Some(p) if p.is_file() => Ok(p),
        _ => Err(format!("no nuvira-agent: set NUVIRA_BENCH_AGENT_EXE or put {name} next to nuvira-bench")),
    }
}

/// A free port on 127.0.0.1 for both UDP and TCP (the server listens on both).
/// Ports are probed in 20000-39999, below the ephemeral ranges: on Windows the
/// UDP ephemeral ports come out sequentially and can sit for hundreds of tries
/// inside a TCP range excluded by Hyper-V / WinNAT (`netsh int ipv4 show
/// excludedportrange protocol=tcp`).
pub fn free_port() -> Result<u16> {
    let start = (std::process::id() as u64).wrapping_mul(2_654_435_761)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
    for i in 0..2000u64 {
        let port = 20_000 + ((start.wrapping_add(i.wrapping_mul(7919))) % 20_000) as u16;
        let Ok(udp) = std::net::UdpSocket::bind(("127.0.0.1", port)) else { continue };
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            drop(udp);
            return Ok(port);
        }
    }
    bail!("no free port for both UDP and TCP")
}

/// Options of a local server.
#[derive(Clone, Debug, Default)]
pub struct ServerSpec {
    /// `NUVIRA_RELAY_IMPAIR` (same syntax as the netem profiles).
    pub relay_impair: Option<String>,
}

/// A `nuvira-server` child process on 127.0.0.1.
pub struct LocalServer {
    exe: PathBuf,
    spec: ServerSpec,
    child: Option<Child>,
    pub port: u16,
    pub addr: SocketAddr,
    pub data_dir: PathBuf,
    pub fingerprint: Fingerprint,
    /// Time from spawn to the "started" log line, last start.
    pub start_time: Duration,
    starts: u32,
}

impl LocalServer {
    /// Starts the server with a fresh data folder `dir` and waits until it listens.
    pub fn start(dir: &Path, spec: ServerSpec) -> Result<LocalServer> {
        let exe = server_exe().map_err(anyhow::Error::msg)?;
        let port = free_port()?;
        let mut s = LocalServer {
            exe,
            spec,
            child: None,
            port,
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            data_dir: dir.to_path_buf(),
            fingerprint: [0; 32],
            start_time: Duration::ZERO,
            starts: 0,
        };
        s.spawn()?;
        Ok(s)
    }

    /// `host:port` as agents and viewers are given it.
    pub fn name(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn ws_url(&self) -> String {
        nuvira_transport::ws::default_url(&self.name(), self.addr)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    fn spawn(&mut self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        self.starts += 1;
        let log = self.data_dir.join(format!("server-{}.log", self.starts));
        let err = std::fs::File::create(&log)?;
        let listen = self.name();
        let mut cmd = Command::new(&self.exe);
        cmd.env("NUVIRA_DATA_DIR", &self.data_dir)
            .env("NUVIRA_LISTEN", &listen)
            .env("NUVIRA_WS_LISTEN", &listen)
            .env("NUVIRA_RATE_LIMITS", "off")
            .env("NUVIRA_LOG", "info")
            .env_remove("NUVIRA_RELAY_IMPAIR")
            .env_remove("NUVIRA_RELAY_FAKE_REFLEXIVE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(err);
        if let Some(i) = &self.spec.relay_impair {
            cmd.env("NUVIRA_RELAY_IMPAIR", i);
        }
        let t = Instant::now();
        let child = cmd.spawn().with_context(|| format!("starting {}", self.exe.display()))?;
        self.child = Some(child);
        // Ready = the "nuvira-server started" line (it carries the fingerprint).
        let deadline = t + Duration::from_secs(20);
        loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            if let Some(line) = text.lines().find(|l| l.contains("nuvira-server started")) {
                let fp = line
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("fingerprint="))
                    .context("no fingerprint in the server's start line")?;
                self.fingerprint = nuvira_transport::parse_fingerprint(fp.trim_matches('"'))?;
                self.start_time = t.elapsed();
                return Ok(());
            }
            if let Some(c) = self.child.as_mut()
                && let Some(status) = c.try_wait()?
            {
                bail!("nuvira-server exited with {status}: {}", text.trim());
            }
            if Instant::now() > deadline {
                bail!("nuvira-server did not start in 20 s: {}", text.trim());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Kills the server (no clean shutdown: as a crash or a power cut).
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Kills it and starts it again on the same port and data (same IDs, same
    /// certificate). Returns the time the new process took to listen.
    pub fn restart(&mut self) -> Result<Duration> {
        self.kill();
        // The port is free once the process is gone; give the OS a moment.
        std::thread::sleep(Duration::from_millis(50));
        self.spawn()?;
        Ok(self.start_time)
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Agent events with the instant they arrived.
#[derive(Default)]
pub struct EventLog {
    events: Mutex<Vec<(Instant, AgentEvent)>>,
    changed: Notify,
}

impl EventLog {
    /// Waits for the first event after index `from` matching `pred`; returns its
    /// index and arrival time.
    pub async fn wait(
        &self,
        from: usize,
        timeout: Duration,
        mut pred: impl FnMut(&AgentEvent) -> bool,
    ) -> Result<(usize, Instant, AgentEvent)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            {
                let ev = self.events.lock().expect("events");
                if let Some((i, (t, e))) = ev.iter().enumerate().skip(from).find(|(_, (_, e))| pred(e)) {
                    return Ok((i, *t, e.clone()));
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                bail!("agent event not seen in {timeout:?}");
            }
        }
    }

    pub fn len(&self) -> usize {
        self.events.lock().expect("events").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn errors(&self) -> Vec<String> {
        let ev = self.events.lock().expect("events");
        ev.iter()
            .filter_map(|(_, e)| match e {
                AgentEvent::Offline { reason } => Some(format!("offline: {reason}")),
                // The viewer's own close (code 0) is the normal end, not an error.
                AgentEvent::SessionEnded { error: Some(e) } if !e.contains("(code 0)") => {
                    Some(format!("session: {e}"))
                }
                _ => None,
            })
            .collect()
    }
}

/// An in-process agent registered on a server (`serve_via_server`).
pub struct ServerAgent {
    pub log: Arc<EventLog>,
    commands: Option<mpsc::UnboundedSender<AgentCommand>>,
    task: tokio::task::JoinHandle<Result<()>>,
    pump: tokio::task::JoinHandle<()>,
    pub started: Instant,
}

impl ServerAgent {
    /// Starts the agent on `rt` (the agent runtime). `dir` holds its identity.
    pub fn start(
        rt: &tokio::runtime::Runtime,
        config: AgentConfig,
        server: &LocalServer,
        transport: TransportMode,
        dir: &Path,
    ) -> Result<ServerAgent> {
        std::fs::create_dir_all(dir)?;
        let opts = ServerOptions {
            server_name: server.name(),
            server_key: Some(server.fingerprint),
            transport,
            server_ws: Some(server.ws_url()),
            identity_path: dir.join("device.key"),
            confirm: false,
            permanent_password_hash: None,
            info_file: Some(dir.join("info.txt")),
            unattended: false,
        };
        let password = Arc::new(Mutex::new(PASSWORD.to_string()));
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let log = Arc::new(EventLog::default());
        let pump_log = log.clone();
        let pump = rt.spawn(async move {
            while let Some(e) = events_rx.recv().await {
                pump_log.events.lock().expect("events").push((Instant::now(), e));
                pump_log.changed.notify_waiters();
            }
        });
        let started = Instant::now();
        let task = rt.spawn(nuvira_agent::serve_via_server(config, opts, password, events_tx, commands_rx));
        Ok(ServerAgent { log, commands: Some(commands_tx), task, pump, started })
    }

    /// Waits until the agent is online after event index `from`: (index, time, ID).
    pub async fn online(&self, from: usize, timeout: Duration) -> Result<(usize, Instant, String)> {
        let (i, t, e) = self
            .log
            .wait(from, timeout, |e| matches!(e, AgentEvent::Online { .. }))
            .await
            .with_context(|| format!("agent not online; {:?}", self.log.errors()))?;
        let AgentEvent::Online { device_id, .. } = e else { unreachable!() };
        Ok((i, t, device_id))
    }

    /// Stops the agent (closing the command channel ends `serve_via_server`).
    pub async fn stop(mut self) {
        self.commands.take();
        if tokio::time::timeout(Duration::from_secs(3), &mut self.task).await.is_err() {
            self.task.abort();
        }
        self.pump.abort();
    }
}

/// Timings of one viewer connection through the server, from the start of the call.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnectTimes {
    /// Signalling connected and registered.
    pub signal: Duration,
    /// `Route` received after `Connect`.
    pub route: Duration,
    /// `p2p::viewer_connect` done.
    pub connected: Duration,
    pub direct: bool,
}

/// The viewer's identity (and the device info it registers with).
pub struct ViewerIdentity {
    pub identity: Identity,
}

impl ViewerIdentity {
    pub fn load(dir: &Path) -> Result<ViewerIdentity> {
        std::fs::create_dir_all(dir)?;
        Ok(ViewerIdentity { identity: Identity::load_or_create(&dir.join("viewer.key"))? })
    }
}

/// What `open_connection` does for `Target::Server`: signalling (UDP, WebSocket or
/// auto), `Connect` to `device_id`, wait for `Route`, then the direct/relay race of
/// `p2p::viewer_connect`. `NUVIRA_FORCE_RELAY` (process-wide) forces the relay.
pub async fn viewer_connect(
    server: &LocalServer,
    viewer: &ViewerIdentity,
    device_id: &str,
    mode: TransportMode,
) -> Result<(Connection, ConnectTimes)> {
    let t0 = Instant::now();
    let name = "nuvira-bench viewer".to_string();
    let device = signal::DeviceInfo { name: &name, reachable: false, alias: None };
    let ws_url = server.ws_url();
    let mut sig =
        signal::connect_auto(server.addr, &ws_url, mode, Some(server.fingerprint), &viewer.identity, &device)
            .await
            .context("viewer signalling")?;
    let mut times = ConnectTimes { signal: t0.elapsed(), ..Default::default() };
    sig.tx
        .send(client_message::Msg::Connect(Connect {
            device_id: device_id.to_string(),
            account_token: String::new(),
        }))
        .await?;
    let route = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match sig.rx.recv().await? {
                Some(server_message::Msg::Route(r)) if r.role() == Role::Viewer => return Ok(r),
                Some(server_message::Msg::Error(e)) => bail!("server error: {}", e.message),
                Some(_) => {}
                None => bail!("nuvira-server closed the connection"),
            }
        }
    })
    .await
    .context("no Route in 20 s")??;
    times.route = t0.elapsed();
    let route = p2p::Route {
        relay: server.addr,
        transport: sig.transport.clone(),
        token: route.relay_token.as_slice().try_into().context("invalid relay token")?,
        peer_public_key: route.peer_public_key.as_slice().try_into().context("invalid agent key")?,
        role: p2p::Role::Viewer,
        // The server is given as an IP: no other family (as the viewer does).
        relay_alt: None,
    };
    let (conn, path) = p2p::viewer_connect(&viewer.identity, &route).await.context("viewer_connect")?;
    times.connected = t0.elapsed();
    times.direct = path == p2p::Path::Direct;
    drop(sig);
    Ok((conn, times))
}

/// Transport name of a mode, for case names.
pub fn mode_name(mode: TransportMode) -> &'static str {
    match mode {
        TransportMode::Auto => "auto",
        TransportMode::Udp => "udp",
        TransportMode::WebSocket => "websocket",
    }
}

/// Whether the transport of a signalling connection is the WebSocket.
pub fn is_websocket(t: &ServerTransport) -> bool {
    matches!(t, ServerTransport::WebSocket(_))
}

/// Milliseconds of a duration (results are in ms).
pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// Agent configurations

/// Agent configuration on the synthetic source (software encoder).
pub fn synthetic_config(w: u32, h: u32, fps: u32, kbps: u32, fixed: bool) -> AgentConfig {
    AgentConfig {
        display: None,
        synthetic: Some((w, h, fps)),
        bitrate_kbps: kbps,
        fps,
        plan: nuvira_codec::EncoderPlan::Software,
        fixed_bitrate: fixed,
        view_only: false,
    }
}

/// Agent configuration with the real screen capture (files and input need it:
/// the synthetic source turns them off).
pub fn real_capture_config(kbps: u32, fps: u32) -> AgentConfig {
    AgentConfig {
        display: None,
        synthetic: None,
        bitrate_kbps: kbps,
        fps,
        plan: nuvira_codec::EncoderPlan::Software,
        fixed_bitrate: true,
        view_only: false,
    }
}

// ---------------------------------------------------------------------------
// Case plumbing shared by the WP4 cases

use crate::measure::{Better, Distribution, Gate, Metric};

use crate::registry::{Case, CaseResult, Level, RunCtx, Tier};

/// The body of a [`NetCase`]: fills the result the case created.
pub type CaseBody = Box<dyn FnMut(&mut RunCtx, &mut CaseResult) -> Result<()> + Send>;

/// A case whose body is a closure, with its own environment (always a child process:
/// every WP4 case sets `NUVIRA_DATA_DIR` and the file key store for itself).
pub struct NetCase {
    pub id: &'static str,
    pub name: String,
    pub level: Level,
    pub tiers: &'static [Tier],
    pub config_ids: &'static [&'static str],
    pub env: Vec<(String, String)>,
    pub runnable: Result<(), String>,
    pub body: CaseBody,
}

impl NetCase {
    pub fn new(
        id: &'static str,
        name: impl Into<String>,
        level: Level,
        tiers: &'static [Tier],
        body: impl FnMut(&mut RunCtx, &mut CaseResult) -> Result<()> + Send + 'static,
    ) -> NetCase {
        NetCase {
            id,
            name: name.into(),
            level,
            tiers,
            config_ids: &[],
            env: vec![("NUVIRA_KEY_STORE".into(), "file".into())],
            runnable: Ok(()),
            body: Box::new(body),
        }
    }

    pub fn config_ids(mut self, ids: &'static [&'static str]) -> Self {
        self.config_ids = ids;
        self
    }

    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn runnable(mut self, r: Result<(), String>) -> Self {
        if self.runnable.is_ok() {
            self.runnable = r;
        }
        self
    }
}

impl Case for NetCase {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn level(&self) -> Level {
        self.level
    }
    fn tiers(&self) -> &'static [Tier] {
        self.tiers
    }
    fn config_ids(&self) -> &'static [&'static str] {
        self.config_ids
    }
    fn runnable(&self) -> Result<(), String> {
        self.runnable.clone()
    }
    fn env(&self) -> Vec<(String, String)> {
        self.env.clone()
    }
    fn isolated(&self) -> bool {
        true
    }
    fn run(&mut self, ctx: &mut RunCtx) -> Result<CaseResult> {
        let mut r = CaseResult::new(self);
        super::isolate_data_dir(&ctx.tmp.join("data"))?;
        (self.body)(ctx, &mut r)?;
        Ok(r)
    }
}

/// A latency-like metric in ms (lower is better).
pub fn ms_metric(gate: Gate, values: Vec<f64>) -> Metric {
    Metric::from_values("ms", Better::Lower, gate, values)
}

/// Records `values` (ms, one per event) as metric `name` and as distribution
/// `name` (p50/p95/p99/max of the events).
pub fn ms_events(r: &mut CaseResult, name: &str, gate: Gate, values: &[f64]) {
    r.metric(name, ms_metric(gate, values.to_vec()));
    r.distribution(name, Distribution::of(values));
}

/// Events (connections, outages, sessions) per run: `quick` or `full` by tier;
/// `NUVIRA_BENCH_EVENTS=n` overrides both (short checks; recorded in the run's env).
pub fn events_for(tier: Tier, quick: usize, full: usize) -> usize {
    if let Some(n) = std::env::var("NUVIRA_BENCH_EVENTS").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
        return n.max(1);
    }
    match tier {
        Tier::Quick => quick,
        Tier::Full => full,
    }
}

/// `runnable()` of the server-mode cases.
pub fn server_runnable() -> Result<(), String> {
    server_exe().map(|_| ())
}

/// Real screen capture and input are allowed here: on the NUC only with the user's
/// OK (`NUVIRA_BENCH_REAL_CAPTURE=1`), the plan's rule (section 7.2, B-MACRO-10).
pub fn real_capture_runnable() -> Result<(), String> {
    if std::env::var_os("NUVIRA_BENCH_REAL_CAPTURE").is_some() {
        Ok(())
    } else {
        Err("needs the real screen capture: set NUVIRA_BENCH_REAL_CAPTURE=1 on a machine where that is \
             allowed (PC-LABORATORIO; the NUC only with the user's OK)"
            .into())
    }
}

/// Input injection moves the real mouse: only with `NUVIRA_BENCH_INPUT=1` (set by
/// the LAB task; never on the NUC without the user's OK).
pub fn input_runnable() -> Result<(), String> {
    if std::env::var_os("NUVIRA_BENCH_INPUT").is_some() {
        real_capture_runnable()
    } else {
        Err("moves the real mouse: set NUVIRA_BENCH_INPUT=1 and NUVIRA_BENCH_REAL_CAPTURE=1 in a desktop \
             session (PC-LABORATORIO via a scheduled task; the NUC only with the user's OK)"
            .into())
    }
}

/// Waits for the agent's session to end after event `from` (the agent is free again).
pub async fn session_ended(log: &EventLog, from: usize, timeout: Duration) -> Result<(usize, Instant)> {
    let (i, t, _) = log.wait(from, timeout, |e| matches!(e, AgentEvent::SessionEnded { .. })).await?;
    Ok((i, t))
}

/// The two runtimes of a case: the agent's (production side, tokio's default
/// workers) and the bench viewer's (threads marked as the viewer side, CPU
/// accounted: [`super::viewer_runtime`]). Agents are spawned inside
/// `agent.enter()`; the viewer's code runs in `viewer.block_on`.
pub fn runtimes() -> Result<(tokio::runtime::Runtime, tokio::runtime::Runtime)> {
    Ok((super::agent_runtime(0)?, super::viewer_runtime(2)?))
}

/// Records the receive-side metrics of a headless window (`prefix` + name):
/// fps, Mbit/s, capture->decoded latency p50/p95, transit p50, freezes, keyframe
/// requests, viewer/agent CPU per frame.
pub fn record_stats(r: &mut CaseResult, prefix: &str, s: &super::headless::SessionStats) {
    let m = |unit: &str, better, gate, v: f64| Metric::single(unit, better, gate, v);
    let n = |x: &str| format!("{prefix}{x}");
    r.metric(&n("fps"), m("fps", Better::Higher, Gate::MacroThroughput, s.fps));
    r.metric(&n("mbit_s"), m("Mbit/s", Better::Lower, Gate::Info, s.mbit_s));
    r.metric(&n("latency_p50_ms"), m("ms", Better::Lower, Gate::MacroLatency, s.latency_ms.p50));
    r.metric(&n("latency_p95_ms"), m("ms", Better::Lower, Gate::MacroLatency, s.latency_ms.p95));
    r.metric(&n("transit_p50_ms"), m("ms", Better::Lower, Gate::Info, s.transit_ms.p50));
    r.metric(&n("freezes"), m("1", Better::Lower, Gate::Info, s.freezes as f64));
    r.metric(&n("freeze_ms"), m("ms", Better::Lower, Gate::Info, s.freeze_ms));
    r.metric(&n("keyframe_requests"), m("1", Better::Lower, Gate::Info, s.keyframe_requests as f64));
    if s.frames > 0 {
        let f = s.frames as f64;
        r.metric(
            &n("cpu_ms_frame_viewer"),
            m("ms", Better::Lower, Gate::MacroCpu, s.usage.viewer_cpu_ms / f),
        );
        r.metric(&n("cpu_ms_frame_agent"), m("ms", Better::Lower, Gate::MacroCpu, s.usage.agent_cpu_ms / f));
    }
    r.distribution(&n("latency_ms"), s.latency_ms);
    r.param(&n("window_s"), s.window_s);
    r.param(&n("frames"), s.frames);
}
