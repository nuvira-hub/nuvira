//! What the cases share. Built on the bench's harness (`nuvira_bench::net`):
//! [`LocalServer`], [`HeadlessViewer`], [`LoopbackAgent`], the netem proxy; what
//! the bench does not expose (the agent's commands, the route before
//! `viewer_connect`, a raw control channel) is here.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_agent::{AgentCommand, AgentConfig, AgentEvent, ServerOptions};
use nuvira_proto::v1::control_message::Msg;
use nuvira_proto::v1::{Auth, ControlMessage, Hello, client_message, server_message};
use nuvira_transport::identity::Identity;
use nuvira_transport::signal::{self, Signal, TransportMode};
use nuvira_transport::{Connection, FramedRecv, FramedSend, p2p};
use tokio::sync::{Notify, mpsc};

pub use nuvira_bench::net::headless::{HeadlessSession, HeadlessViewer, ViewerOptions};
pub use nuvira_bench::net::server_mode::{LocalServer, ServerSpec, ViewerIdentity};
pub use nuvira_bench::net::{AgentEntry, LoopbackAgent, REVERSE_PASSWORD};

/// Temporary password of the agents registered on the local server.
pub const PASSWORD: &str = "e2e-pass-4711";

/// Agent and viewer runtimes (the bench's: agent default side, viewer marked).
pub fn runtimes() -> Result<(tokio::runtime::Runtime, tokio::runtime::Runtime)> {
    nuvira_bench::net::server_mode::runtimes()
}

/// A small synthetic source with a fixed bitrate: light on any CPU, so the cases
/// measure behaviour, not throughput.
pub fn config() -> AgentConfig {
    nuvira_bench::net::server_mode::synthetic_config(640, 360, 30, 1500, true)
}

/// Starts a local `nuvira-server` (rate limits off, 127.0.0.1) and registers its
/// pid for the runner.
///
/// The bench's start waits 20 s for the "started" line; on a busy machine (the
/// NUC, with other builds on the same disk) the first SQLite writes of a new data
/// folder took longer than that a few times: up to 3 attempts, each in a new
/// folder, every failed one noted.
pub fn server(dir: &Path, relay_impair: Option<&str>) -> Result<LocalServer> {
    let spec = ServerSpec { relay_impair: relay_impair.map(String::from) };
    let mut tries = 0;
    let s = loop {
        tries += 1;
        let d = if tries == 1 { dir.to_path_buf() } else { dir.with_extension(format!("try{tries}")) };
        match LocalServer::start(&d, spec.clone()) {
            Ok(s) => break s,
            Err(e) if tries < 3 && format!("{e:#}").contains("did not start") => {
                println!("E2E-NOTE nuvira-server start attempt {tries} failed: {e:#}");
            }
            Err(e) => return Err(e),
        }
    };
    if let Some(pid) = s.pid() {
        crate::runner::track_pid(pid);
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// Agent registered on the server, with its events and commands

#[derive(Default)]
pub struct Events {
    list: Mutex<Vec<(Instant, AgentEvent)>>,
    changed: Notify,
}

impl Events {
    /// First event at index >= `from` matching `pred`: (index, event).
    pub async fn wait(
        &self,
        from: usize,
        timeout: Duration,
        mut pred: impl FnMut(&AgentEvent) -> bool,
    ) -> Result<(usize, AgentEvent)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            {
                let list = self.list.lock().expect("events");
                if let Some((i, (_, e))) = list.iter().enumerate().skip(from).find(|(_, (_, e))| pred(e)) {
                    return Ok((i, e.clone()));
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                bail!("agent event not seen in {timeout:?}; events: {:?}", self.kinds(from));
            }
        }
    }

    pub fn len(&self) -> usize {
        self.list.lock().expect("events").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn all(&self) -> Vec<AgentEvent> {
        self.list.lock().expect("events").iter().map(|(_, e)| e.clone()).collect()
    }

    /// Short names of the events from `from`, for messages.
    pub fn kinds(&self, from: usize) -> Vec<String> {
        self.list
            .lock()
            .expect("events")
            .iter()
            .skip(from)
            .map(|(_, e)| format!("{e:?}").chars().take(120).collect())
            .collect()
    }
}

/// How an agent on the server is set up.
#[derive(Clone)]
pub struct AgentSpec {
    pub config: AgentConfig,
    pub transport: TransportMode,
    /// `ServerOptions::confirm`: the person at the PC must accept.
    pub confirm: bool,
    /// `ServerOptions::permanent_password_hash`.
    pub permanent_hash: Option<String>,
}

impl Default for AgentSpec {
    fn default() -> Self {
        AgentSpec { config: config(), transport: TransportMode::Udp, confirm: false, permanent_hash: None }
    }
}

/// `nuvira_agent::serve_via_server` on a runtime of this process.
pub struct ServerAgent {
    pub events: Arc<Events>,
    commands: Option<mpsc::UnboundedSender<AgentCommand>>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    pump: tokio::task::JoinHandle<()>,
    /// Folder of its identity (`device.key`) and of `access.json`.
    pub dir: std::path::PathBuf,
}

impl ServerAgent {
    pub fn start(
        rt: &tokio::runtime::Runtime,
        spec: AgentSpec,
        server: &LocalServer,
        dir: &Path,
    ) -> Result<ServerAgent> {
        std::fs::create_dir_all(dir)?;
        let opts = ServerOptions {
            server_name: server.name(),
            server_key: Some(server.fingerprint),
            transport: spec.transport,
            server_ws: Some(server.ws_url()),
            identity_path: dir.join("device.key"),
            confirm: spec.confirm,
            permanent_password_hash: spec.permanent_hash,
            info_file: Some(dir.join("info.txt")),
            unattended: false,
        };
        let password = Arc::new(Mutex::new(PASSWORD.to_string()));
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let events = Arc::new(Events::default());
        let log = events.clone();
        let pump = rt.spawn(async move {
            while let Some(e) = events_rx.recv().await {
                log.list.lock().expect("events").push((Instant::now(), e));
                log.changed.notify_waiters();
            }
        });
        let task =
            rt.spawn(nuvira_agent::serve_via_server(spec.config, opts, password, events_tx, commands_rx));
        Ok(ServerAgent {
            events,
            commands: Some(commands_tx),
            task: Some(task),
            pump,
            dir: dir.to_path_buf(),
        })
    }

    /// Waits for `Online` after `from`: (index, device ID, websocket).
    pub async fn online(&self, from: usize, timeout: Duration) -> Result<(usize, String, bool)> {
        let (i, e) = self.events.wait(from, timeout, |e| matches!(e, AgentEvent::Online { .. })).await?;
        let AgentEvent::Online { device_id, websocket } = e else { unreachable!() };
        Ok((i, device_id, websocket))
    }

    pub fn command(&self, c: AgentCommand) -> Result<()> {
        self.commands.as_ref().context("agent stopped")?.send(c).map_err(|_| anyhow::anyhow!("agent gone"))
    }

    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Stops it (closing the commands ends `serve_via_server`).
    pub async fn stop(mut self) {
        self.commands.take();
        if let Some(mut t) = self.task.take()
            && tokio::time::timeout(Duration::from_secs(3), &mut t).await.is_err()
        {
            t.abort();
        }
        self.pump.abort();
    }
}

impl Drop for ServerAgent {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.pump.abort();
    }
}

// ---------------------------------------------------------------------------
// Viewer through the server

/// Where a viewer finds the server (a [`LocalServer`] of this process or of
/// another one).
#[derive(Clone, Debug)]
pub struct Srv {
    pub addr: std::net::SocketAddr,
    pub fingerprint: nuvira_transport::Fingerprint,
    pub ws: String,
}

impl From<&LocalServer> for Srv {
    fn from(s: &LocalServer) -> Srv {
        Srv { addr: s.addr, fingerprint: s.fingerprint, ws: s.ws_url() }
    }
}

/// Signalling of a viewer (what `open_connection` does first).
pub async fn viewer_signal(server: &Srv, viewer: &Identity, mode: TransportMode) -> Result<Signal> {
    let name = "nuvira-e2e viewer".to_string();
    let device = signal::DeviceInfo { name: &name, reachable: false, alias: None };
    signal::connect_auto(server.addr, &server.ws, mode, Some(server.fingerprint), viewer, &device)
        .await
        .context("viewer signalling")
}

/// `Connect` to `device_id` and the `Route` that answers it (or the server's error).
pub async fn request_route(server: &Srv, sig: &mut Signal, device_id: &str) -> Result<p2p::Route> {
    sig.tx
        .send(client_message::Msg::Connect(nuvira_proto::v1::Connect {
            device_id: device_id.to_string(),
            account_token: String::new(),
        }))
        .await?;
    let route = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match sig.rx.recv().await? {
                Some(server_message::Msg::Route(r)) if r.role() == nuvira_proto::v1::Role::Viewer => {
                    return Ok(r);
                }
                Some(server_message::Msg::Error(e)) => bail!("server error: {}", e.message),
                Some(_) => {}
                None => bail!("nuvira-server closed the connection"),
            }
        }
    })
    .await
    .context("no Route in 20 s")??;
    Ok(p2p::Route {
        relay: server.addr,
        transport: sig.transport.clone(),
        token: route.relay_token.as_slice().try_into().context("invalid relay token")?,
        peer_public_key: route.peer_public_key.as_slice().try_into().context("invalid agent key")?,
        role: p2p::Role::Viewer,
        relay_alt: None,
    })
}

/// Signalling, `Connect`, `Route`, `p2p::viewer_connect` (as the viewer does).
pub async fn connect_via(
    server: &Srv,
    viewer: &Identity,
    device_id: &str,
    mode: TransportMode,
) -> Result<(Connection, p2p::Path)> {
    let mut sig = viewer_signal(server, viewer, mode).await?;
    let route = request_route(server, &mut sig, device_id).await?;
    let r = p2p::viewer_connect(viewer, &route).await.context("viewer_connect");
    drop(sig);
    r
}

/// A viewer identity in `dir` (file key store: `NUVIRA_KEY_STORE=file`).
pub fn identity(dir: &Path) -> Result<Identity> {
    std::fs::create_dir_all(dir)?;
    Identity::load_or_create(&dir.join("viewer.key"))
}

// ---------------------------------------------------------------------------
// Raw control channel (handshake cases)

/// The viewer's side of the control stream, message by message.
pub struct Raw {
    pub conn: Connection,
    pub tx: FramedSend,
    pub rx: FramedRecv,
}

pub fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

/// The Hello of the bench's headless viewer (current protocol).
pub fn viewer_hello(two_factor: bool) -> Hello {
    Hello {
        protocol_version: nuvira_proto::PROTOCOL_VERSION,
        device_name: "nuvira-e2e raw".into(),
        os: std::env::consts::OS.into(),
        app_version: "nuvira-e2e".into(),
        two_factor,
        ..Default::default()
    }
}

impl Raw {
    pub async fn open(conn: Connection) -> Result<Raw> {
        let (send, recv) = conn.open_bi().await?;
        Ok(Raw { conn, tx: FramedSend::new(send), rx: FramedRecv::new(recv) })
    }

    /// Next control message within `timeout` (`None`: the stream ended).
    pub async fn next(&mut self, timeout: Duration) -> Result<Option<ControlMessage>> {
        tokio::time::timeout(timeout, self.rx.recv::<ControlMessage>()).await.context("no control message")?
    }

    /// Sends our Hello and returns the agent's.
    pub async fn hello(&mut self, two_factor: bool) -> Result<Hello> {
        self.tx.send(&control(Msg::Hello(viewer_hello(two_factor)))).await?;
        match self.next(Duration::from_secs(10)).await? {
            Some(ControlMessage { msg: Some(Msg::Hello(h)) }) => Ok(h),
            other => bail!("expected the agent's Hello, got {other:?}"),
        }
    }

    /// Sends `Auth` and returns the next message (AuthResult, CodeRequired or
    /// ConfirmationPending).
    pub async fn auth(&mut self, password: &str, code: &str) -> Result<Msg> {
        let auth = Auth { password: password.into(), code: code.into(), ..Default::default() };
        self.tx.send(&control(Msg::Auth(auth))).await?;
        match self.next(Duration::from_secs(40)).await? {
            Some(ControlMessage { msg: Some(m) }) => Ok(m),
            other => bail!("no answer to Auth: {other:?}"),
        }
    }
}

/// How a connection ended, as a short text: `closed <code> <reason>` (by the
/// other side), `timed out`, `locally closed`, ...
pub fn ended(e: &quinn::ConnectionError) -> String {
    match e {
        quinn::ConnectionError::ApplicationClosed(a) => {
            format!("closed {} {}", a.error_code, String::from_utf8_lossy(&a.reason))
        }
        quinn::ConnectionError::TimedOut => "timed out".into(),
        quinn::ConnectionError::LocallyClosed => "locally closed".into(),
        other => format!("{other}"),
    }
}

/// Waits up to `timeout` for the connection to end; how it ended and when.
pub async fn wait_closed(conn: &Connection, timeout: Duration) -> Result<(String, Duration)> {
    let t = Instant::now();
    let e = tokio::time::timeout(timeout, conn.closed()).await.context("connection still open")?;
    Ok((ended(&e), t.elapsed()))
}

/// `AuthResult` of a message, or an error naming what came instead.
pub fn auth_result(m: &Msg) -> Result<(bool, String)> {
    match m {
        Msg::AuthResult(r) => Ok((r.ok, r.message.clone())),
        other => bail!("expected AuthResult, got {other:?}"),
    }
}

/// Waits for the first frame, then for `frames` more decoded frames (a new
/// measured window: the headless viewer counts frames only inside one).
pub async fn frames(s: &HeadlessSession, frames: u64, timeout: Duration) -> Result<u64> {
    s.first_frame(timeout).await?;
    s.mark();
    let deadline = Instant::now() + timeout;
    loop {
        let n = s.snapshot().frames;
        if n >= frames {
            return Ok(n);
        }
        if let Some(e) = s.error() {
            bail!("session failed after {n} frames: {e}");
        }
        if Instant::now() > deadline {
            bail!("only {n} frames in {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Viewer options with a password.
pub fn opts(password: &str) -> ViewerOptions {
    ViewerOptions { password: Some(password.into()), ..Default::default() }
}

/// SHA-256 of a file, hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes `size` bytes of a deterministic pattern (xorshift) to `path`.
pub fn make_file(path: &Path, size: u64, seed: u64) -> Result<()> {
    use std::io::Write;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    let mut x = seed | 1;
    let mut left = size;
    let mut buf = vec![0u8; 1 << 16];
    while left > 0 {
        for chunk in buf.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let b = x.to_le_bytes();
            chunk.copy_from_slice(&b[..chunk.len()]);
        }
        let n = left.min(buf.len() as u64) as usize;
        f.write_all(&buf[..n])?;
        left -= n as u64;
    }
    f.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Child processes of this test binary (see `tests/e2e/hosts.rs`)

/// A child process of the test binary in another role, with its stdout lines.
pub struct Host {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Host {
    /// Starts `current_exe() args...` (registered for the runner's clean-up).
    pub fn spawn(args: &[&str]) -> Result<Host> {
        use std::io::BufRead;
        let exe = std::env::current_exe()?;
        let mut child = std::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;
        crate::runner::track_pid(child.id());
        let stdout = child.stdout.take().context("stdout")?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Host { child, lines })
    }

    /// The first line starting with `prefix` within `timeout` (the rest of it).
    pub fn line(&self, prefix: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(l) => {
                    if let Some(rest) = l.strip_prefix(prefix) {
                        return Ok(rest.trim().to_string());
                    }
                    if let Some(why) = l.strip_prefix("failed ") {
                        bail!("host failed: {why}");
                    }
                }
                Err(_) => bail!("no line {prefix:?} from the host in {timeout:?}"),
            }
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Kills it at once (no clean close: as a crash or a power cut).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.kill();
    }
}
