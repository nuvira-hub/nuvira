//! NexoDesk agent — the controlled device.
//!
//! Library used by the `nexodesk` app and by the `nexodesk-agent` command-line
//! program: it registers with nexodesk-server (or waits for direct LAN
//! connections), streams the screen and applies the mouse and keyboard input it
//! receives. With the server it talks to its user through [`AgentEvent`] and [`AgentCommand`].

mod abr;
pub mod cli;
mod files;
mod fsops;
#[cfg(windows)]
pub mod ipc;
mod pipeline;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nexodesk_capture::{Capturer, SyntheticCapturer};
use nexodesk_codec::{EncoderPlan, EncoderSettings};
use nexodesk_proto::v1::{
    AuthResult, Codec, ControlMessage, DisplayInfo, Displays, Hello, InputEvent, Pong, Quality, VideoFormat,
    VideoFrame, control_message::Msg,
};
use nexodesk_transport::signal::TransportMode;
use nexodesk_transport::{Connection, Fingerprint, FramedRecv, FramedSend};
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use crate::pipeline::Pipeline;

/// Minimum bitrate of the adaptive control.
const MIN_BITRATE: u32 = 500_000;

/// Rejection reasons sent to the viewer are translation keys (`@key`, see
/// nexodesk-i18n): the viewer shows them in its own language.
const WRONG_PASSWORD: &str = "@agent.wrong_password";

/// Time given to the person at the PC to accept a connection request.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// How to capture and encode the screen.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Screen to stream; `None` = the primary one.
    pub display: Option<usize>,
    /// Synthetic image (width, height, fps) instead of the screen (tests).
    pub synthetic: Option<(u32, u32, u32)>,
    /// Maximum bitrate in kbit/s.
    pub bitrate_kbps: u32,
    pub fps: u32,
    /// Encoder chosen at start-up (see `nexodesk_codec::plan_encoder`).
    pub plan: EncoderPlan,
    /// Fixed bitrate instead of adapting it to the network.
    pub fixed_bitrate: bool,
    /// Ignores the viewer's mouse and keyboard.
    pub view_only: bool,
}

impl AgentConfig {
    pub fn encoder_settings(&self) -> EncoderSettings {
        EncoderSettings { bitrate_bps: self.bitrate_kbps * 1000, max_fps: self.fps }
    }

    fn open_capturer(&self) -> Result<Box<dyn Capturer>> {
        match self.synthetic {
            Some((w, h, fps)) => Ok(Box::new(SyntheticCapturer::new(w, h, fps))),
            None => nexodesk_capture::open(self.display),
        }
    }
}

/// Synthetic image in the "WxH@FPS" format, e.g. "1920x1080@60".
pub fn parse_synthetic(spec: &str) -> Result<(u32, u32, u32)> {
    let (size, fps) = spec.split_once('@').unwrap_or((spec, "60"));
    let (w, h) = size.split_once('x').context("expected format WxH@FPS, e.g. 1920x1080@60")?;
    Ok((w.parse()?, h.parse()?, fps.parse()?))
}

/// How to reach nexodesk-server and how to handle requests.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// host[:port] of the server.
    pub server_name: String,
    /// Expected certificate fingerprint; without it, the remembered one is used (or remembered).
    pub server_key: Option<Fingerprint>,
    pub transport: TransportMode,
    /// URL of the WebSocket fallback; without it, `ws://<server>:<port>/nexodesk`.
    pub server_ws: Option<String>,
    pub identity_path: PathBuf,
    /// Whoever uses the temporary password must be confirmed by the person at the PC
    /// (event [`AgentEvent::Incoming`]). Without it, the password is enough.
    pub confirm: bool,
    /// Permanent password for unattended access: never confirmed.
    pub permanent_password_hash: Option<String>,
    /// Automated tests: where to write "ID password" once registered.
    pub info_file: Option<PathBuf>,
}

/// What happens, for whoever shows the state to the user (also through the local
/// channel between service and app, see [`ipc`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    Connecting,
    /// Registered on the server: reachable with this ID.
    Online {
        device_id: String,
        websocket: bool,
    },
    Offline {
        reason: String,
    },
    /// Current temporary password (at start-up and after [`AgentCommand::NewPassword`]).
    Password {
        password: String,
    },
    /// Someone typed the temporary password: answer with
    /// [`AgentCommand::Respond`] within [`CONFIRM_TIMEOUT`].
    Incoming {
        session_id: String,
        viewer_id: String,
        viewer_name: String,
    },
    /// The request is no longer pending (expired or already answered).
    IncomingClosed {
        session_id: String,
    },
    SessionStarted {
        viewer_id: String,
        viewer_name: String,
        direct: bool,
    },
    SessionEnded {
        error: Option<String>,
    },
    /// Chat message from the controlling side.
    Chat {
        text: String,
    },
    /// File sent by the controlling side, saved in `path`.
    FileReceived {
        name: String,
        path: String,
    },
}

/// What the person at the PC asks for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum AgentCommand {
    Respond { session_id: String, accept: bool },
    EndSession,
    /// Generates a new temporary password (answer: [`AgentEvent::Password`]).
    NewPassword,
    /// Chat message for the controlling side (session in progress).
    Chat { text: String },
}

/// Random 8-character password that is easy to dictate (no 0/O, 1/l/I).
pub fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 8];
    // Operating system random generator (through ring).
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("random generator");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

/// Constant-time comparison (does not reveal how many leading characters are right).
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// LAN mode without a server (phase 0): waits for direct connections on the given UDP port.
pub async fn serve_direct(
    config: AgentConfig,
    port: u16,
    once: bool,
    fingerprint_file: Option<PathBuf>,
) -> Result<()> {
    let bind: SocketAddr = ([0, 0, 0, 0], port).into();
    let (endpoint, fingerprint) = nexodesk_transport::server_endpoint(bind)?;
    info!("waiting for the viewer on UDP {bind}");
    let fingerprint = nexodesk_transport::fingerprint_hex(&fingerprint);
    info!("agent fingerprint: {fingerprint}");
    if let Some(path) = &fingerprint_file {
        std::fs::write(path, &fingerprint).with_context(|| format!("writing {}", path.display()))?;
    }

    // One session at a time: later connections wait in the queue.
    while let Some(incoming) = endpoint.accept().await {
        let conn = match incoming.await {
            Ok(conn) => conn,
            Err(e) => {
                warn!("connection failed: {e}");
                continue;
            }
        };
        let peer = conn.remote_address();
        info!("viewer connected from {peer}");
        let result = match conn.accept_bi().await.context("control channel") {
            Ok(streams) => session(conn.clone(), streams, &config, None).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => info!("session with {peer} ended"),
            Err(e) => warn!("session with {peer} ended: {e:#}"),
        }
        conn.close(0u32.into(), b"end of session");
        if once {
            break;
        }
    }
    endpoint.wait_idle().await;
    Ok(())
}

/// Registers the device with nexodesk-server and accepts the incoming sessions.
/// If the server goes away, it reconnects. It ends only when `commands` is closed.
pub async fn serve_via_server(
    config: AgentConfig,
    opts: ServerOptions,
    password: Arc<Mutex<String>>,
    events: mpsc::UnboundedSender<AgentEvent>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
) -> Result<()> {
    let server = nexodesk_transport::resolve_addr(&opts.server_name, nexodesk_transport::signal::DEFAULT_PORT)?;
    let identity = Arc::new(nexodesk_transport::identity::Identity::load_or_create(&opts.identity_path)?);
    let ctx = Arc::new(SessionContext {
        config,
        password,
        permanent: opts.permanent_password_hash.clone(),
        confirm: opts.confirm,
        pending: Mutex::new(HashMap::new()),
        events,
        chat: Mutex::new(None),
        busy: AtomicBool::new(false),
        end_session: Notify::new(),
    });
    let current = ctx.password.lock().expect("password").clone();
    let _ = ctx.events.send(AgentEvent::Password { password: current });

    loop {
        let _ = ctx.events.send(AgentEvent::Connecting);
        let expected =
            opts.server_key.or_else(|| nexodesk_transport::signal::known_server::get(&opts.server_name));
        match signal_loop(&ctx, &opts, &identity, server, expected, &mut commands).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!("connection with nexodesk-server lost: {e:#}; retrying in 5 s");
                let _ = ctx.events.send(AgentEvent::Offline { reason: format!("{e:#}") });
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// State shared between the signalling loop and the session in progress.
struct SessionContext {
    config: AgentConfig,
    /// Temporary password, shown to the person at the PC.
    password: Arc<Mutex<String>>,
    permanent: Option<String>,
    confirm: bool,
    /// Requests waiting for the answer of the person at the PC, by session.
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
    events: mpsc::UnboundedSender<AgentEvent>,
    /// Chat towards the viewer of the session in progress.
    chat: Mutex<Option<mpsc::UnboundedSender<String>>>,
    busy: AtomicBool,
    end_session: Notify,
}

/// Request accepted by the server, waiting for the route.
struct Request {
    viewer_id: String,
    viewer_name: String,
}

async fn signal_loop(
    ctx: &Arc<SessionContext>,
    opts: &ServerOptions,
    identity: &Arc<nexodesk_transport::identity::Identity>,
    server: SocketAddr,
    expected: Option<Fingerprint>,
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
) -> Result<()> {
    use nexodesk_proto::v1::{Accept, Reject, client_message, server_message};
    let device_name = std::env::var("COMPUTERNAME").unwrap_or_default();
    let ws_url =
        opts.server_ws.clone().unwrap_or_else(|| nexodesk_transport::ws::default_url(&opts.server_name, server));
    let mut signal = nexodesk_transport::signal::connect_auto(
        server,
        &ws_url,
        opts.transport,
        expected,
        identity,
        &nexodesk_transport::signal::DeviceInfo { name: &device_name, reachable: true },
    )
    .await?;
    let websocket = matches!(signal.transport, nexodesk_transport::ServerTransport::WebSocket(_));
    if let nexodesk_transport::ServerTransport::WebSocket(url) = &signal.transport {
        info!("connected to nexodesk-server through the WebSocket {url}");
    }
    if expected.is_none() {
        nexodesk_transport::signal::known_server::remember(&opts.server_name, &signal.server_fingerprint)?;
        info!(
            "nexodesk-server fingerprint remembered: {}",
            nexodesk_transport::fingerprint_hex(&signal.server_fingerprint)
        );
    }
    let id = signal.device_id.clone();
    info!("registered on {}: ID {} {} {}", opts.server_name, &id[..3], &id[3..6], &id[6..]);
    if let Some(path) = &opts.info_file {
        // In automated tests the viewer reads ID and password from here.
        let password = ctx.password.lock().expect("password").clone();
        std::fs::write(path, format!("{id} {password}"))?;
    }
    let _ = ctx.events.send(AgentEvent::Online { device_id: id, websocket });

    let mut accepted: HashMap<String, Request> = HashMap::new();

    loop {
        let reply = tokio::select! {
            msg = signal.rx.recv() => match msg? {
                Some(server_message::Msg::Incoming(inc)) => {
                    let request = Request { viewer_id: inc.viewer_id, viewer_name: inc.viewer_name };
                    info!("connection request from {} ({})", request.viewer_name, request.viewer_id);
                    if ctx.busy.load(Ordering::Relaxed) {
                        Some(client_message::Msg::Reject(Reject {
                            session_id: inc.session_id,
                            reason: "@agent.busy".into(),
                        }))
                    } else {
                        // Confirmation by the person at the PC, if needed, comes after the password.
                        accepted.insert(inc.session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id: inc.session_id }))
                    }
                }
                Some(server_message::Msg::Route(route)) => {
                    let request = accepted.remove(&route.session_id);
                    let p2p_route = nexodesk_transport::p2p::Route {
                        relay: server,
                        transport: signal.transport.clone(),
                        token: route.relay_token.as_slice().try_into().context("invalid relay token")?,
                        peer_public_key: route
                            .peer_public_key
                            .as_slice()
                            .try_into()
                            .context("invalid viewer key")?,
                        role: nexodesk_transport::p2p::Role::Agent,
                    };
                    let request = request.unwrap_or(Request { viewer_id: route.peer_id, viewer_name: String::new() });
                    ctx.busy.store(true, Ordering::Relaxed);
                    let (ctx, identity) = (ctx.clone(), identity.clone());
                    let session_id = route.session_id.clone();
                    tokio::spawn(async move {
                        let result = relayed_session(&ctx, &identity, &p2p_route, &request, &session_id).await;
                        if let Err(e) = &result {
                            warn!("session ended: {e:#}");
                        }
                        ctx.busy.store(false, Ordering::Relaxed);
                        let _ = ctx.events.send(AgentEvent::SessionEnded { error: result.err().map(|e| format!("{e:#}")) });
                    });
                    None
                }
                Some(server_message::Msg::Error(e)) => {
                    warn!("nexodesk-server: {}", e.message);
                    None
                }
                Some(_) => None,
                None => bail!("nexodesk-server closed the connection"),
            },
            cmd = commands.recv() => match cmd {
                Some(AgentCommand::Respond { session_id, accept }) => {
                    if let Some(answer) = ctx.pending.lock().expect("requests").remove(&session_id) {
                        let _ = answer.send(accept);
                    }
                    None
                }
                Some(AgentCommand::Chat { text }) => {
                    if let Some(chat) = ctx.chat.lock().expect("chat").as_ref() {
                        let _ = chat.send(text);
                    }
                    None
                }
                Some(AgentCommand::NewPassword) => {
                    let password = random_password();
                    *ctx.password.lock().expect("password") = password.clone();
                    let _ = ctx.events.send(AgentEvent::Password { password });
                    None
                }
                Some(AgentCommand::EndSession) => {
                    ctx.end_session.notify_waiters();
                    None
                }
                None => return Ok(()),
            },
        };
        if let Some(reply) = reply {
            signal.tx.send(reply).await?;
        }
    }
}

async fn relayed_session(
    ctx: &SessionContext,
    identity: &nexodesk_transport::identity::Identity,
    route: &nexodesk_transport::p2p::Route,
    request: &Request,
    session_id: &str,
) -> Result<()> {
    let endpoint = nexodesk_transport::p2p::agent_endpoint(identity, route).await?;
    // The viewer may open several connections (direct and through the relay): the one
    // it opens the control channel on counts; it closes the others itself.
    let (tx, mut rx) = mpsc::channel(2);
    let acceptor = {
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    if let Ok(streams) = conn.accept_bi().await {
                        let _ = tx.send((conn, streams)).await;
                    }
                });
            }
        })
    };
    let (conn, streams) = tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .context("the viewer did not connect within 20 s")?
        .context("endpoint closed")?;
    acceptor.abort();
    let direct = conn.remote_address() != route.relay;
    info!(
        "viewer connected {} ({})",
        if direct { "directly" } else { "through the relay" },
        conn.remote_address()
    );
    let gate = Gate { ctx, session_id, request, direct };
    let result = tokio::select! {
        r = session(conn.clone(), streams, &ctx.config, Some(&gate)) => r,
        _ = ctx.end_session.notified() => {
            info!("session ended by the person at the PC");
            Ok(())
        }
    };
    // The whole endpoint is closed: including a connection left over through the relay,
    // which would otherwise keep the agent "busy" until its timeout.
    endpoint.close(0u32.into(), b"end of session");
    endpoint.wait_idle().await;
    if result.is_ok() {
        info!("session ended");
    }
    result
}

/// Access control of a session arriving from the server: permanent password
/// (unattended access) or temporary password, with confirmation by the person
/// at the PC if required.
struct Gate<'a> {
    ctx: &'a SessionContext,
    session_id: &'a str,
    request: &'a Request,
    direct: bool,
}

impl Gate<'_> {
    /// `Err` with the reason to show to the viewer.
    async fn authorize(&self, given: &str, pending: Option<&mut FramedSend>) -> Result<(), String> {
        let ctx = self.ctx;
        if ctx.permanent.as_deref().is_some_and(|hash| nexodesk_secret::verify(given, hash)) {
            info!("access with the permanent password");
            return self.started();
        }
        let temporary = ctx.password.lock().expect("password").clone();
        if !same_secret(given, &temporary) {
            return Err(WRONG_PASSWORD.into());
        }
        if !ctx.confirm {
            return self.started();
        }
        if let Some(ctrl_tx) = pending {
            let waiting = nexodesk_proto::v1::ConfirmationPending { timeout_s: CONFIRM_TIMEOUT.as_secs() as u32 };
            let _ = ctrl_tx.send(&control(Msg::ConfirmationPending(waiting))).await;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.pending.lock().expect("requests").insert(self.session_id.to_string(), tx);
        let _ = ctx.events.send(AgentEvent::Incoming {
            session_id: self.session_id.to_string(),
            viewer_id: self.request.viewer_id.clone(),
            viewer_name: self.request.viewer_name.clone(),
        });
        let answer = tokio::time::timeout(CONFIRM_TIMEOUT, rx).await;
        ctx.pending.lock().expect("requests").remove(self.session_id);
        let _ = ctx.events.send(AgentEvent::IncomingClosed { session_id: self.session_id.to_string() });
        match answer {
            Ok(Ok(true)) => self.started(),
            // Rejected by the person at the PC, or no app open to ask them.
            Ok(Ok(false)) => {
                info!("connection not accepted on the device");
                Err("@agent.not_accepted".into())
            }
            _ => {
                info!("no answer to the connection request: rejected");
                Err("@agent.no_answer".into())
            }
        }
    }

    fn started(&self) -> Result<(), String> {
        let _ = self.ctx.events.send(AgentEvent::SessionStarted {
            viewer_id: self.request.viewer_id.clone(),
            viewer_name: self.request.viewer_name.clone(),
            direct: self.direct,
        });
        Ok(())
    }
}

/// A session with a viewer already connected. With `gate` the viewer must
/// authenticate (and possibly be confirmed) before receiving any image.
async fn session(
    conn: Connection,
    (send, recv): (quinn::SendStream, quinn::RecvStream),
    config: &AgentConfig,
    gate: Option<&Gate<'_>>,
) -> Result<()> {
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    let Some(ControlMessage { msg: Some(Msg::Hello(hello)) }) = ctrl_rx.recv().await? else {
        bail!("the viewer did not introduce itself");
    };
    info!(device = hello.device_name, os = hello.os, version = hello.app_version, "viewer");
    if hello.protocol_version < nexodesk_proto::MIN_PROTOCOL_VERSION {
        bail!("viewer protocol version not supported: {}", hello.protocol_version);
    }
    let viewer_protocol = hello.protocol_version;
    let hello = Hello {
        password_required: gate.is_some(),
        file_manager: !config.view_only && config.synthetic.is_none(),
        ..local_hello()
    };
    ctrl_tx.send(&control(Msg::Hello(hello))).await?;
    if let Some(gate) = gate {
        let given = match ctrl_rx.recv::<ControlMessage>().await? {
            Some(ControlMessage { msg: Some(Msg::Auth(auth)) }) => auth.password,
            _ => bail!("the viewer did not send the password"),
        };
        // Viewers that know it are told when the person at the PC is being asked.
        let pending = (viewer_protocol >= nexodesk_proto::v1::ProtocolVersion::ConfirmationPending as u32).then_some(&mut ctrl_tx);
        let verdict = gate.authorize(&given, pending).await;
        let (ok, message) = match &verdict {
            Ok(()) => (true, String::new()),
            Err(reason) => (false, reason.clone()),
        };
        ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok, message }))).await?;
        if let Err(reason) = verdict {
            // The reason must reach the viewer before the connection closes; with a
            // wrong password the pause is longer, against rapid-fire attempts.
            let pause = if reason == WRONG_PASSWORD { 2000 } else { 500 };
            tokio::time::sleep(Duration::from_millis(pause)).await;
            bail!("{reason}");
        }
    }

    let keyframe = Arc::new(AtomicBool::new(true));
    let max_bps = config.bitrate_kbps * 1000;
    // Start from half the maximum: the unknown network might not take more.
    let mut abr = (!config.fixed_bitrate).then(|| abr::Abr::new(max_bps / 2, MIN_BITRATE, max_bps));
    let bitrate = Arc::new(AtomicU32::new(abr.as_ref().map_or(max_bps, |a| a.bitrate())));
    let fps = Arc::new(AtomicU32::new(config.fps));
    let session_start = Instant::now();
    // First frame sent after the last feedback (ms from the start, 0 = none): tells
    // whether feedback is missing. Measuring from the last frame instead would make a
    // still screen (one frame every few seconds) look like a stuck network.
    let unanswered_since_ms = Arc::new(AtomicU64::new(0));
    // Video bytes sent since the start: the adaptive bitrate needs the rate actually used.
    let sent_bytes = Arc::new(AtomicU64::new(0));
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    let source = VideoSource {
        config: config.clone(),
        frames: frame_tx,
        keyframe: keyframe.clone(),
        bitrate: bitrate.clone(),
        fps: fps.clone(),
    };
    let (mut display, first) = source.start(config.display).await?;
    ctrl_tx.send(&video_format(&display)).await?;
    if let Some(msg) = displays_message(config, &display) {
        ctrl_tx.send(&msg).await?;
    }
    // Capture in progress: the control channel replaces it when the screen changes.
    let pipeline = std::sync::Mutex::new(Some(first));
    let (screen_tx, mut screen_rx) = tokio::sync::watch::channel(screen_rect(&display));

    // Chat: messages from the person at the PC come from the app through the context.
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<String>();
    let events = gate.map(|g| {
        *g.ctx.chat.lock().expect("chat") = Some(chat_tx.clone());
        g.ctx.events.clone()
    });
    // Outcome of the received files, to report to the viewer.
    let (file_tx, mut file_rx) = mpsc::unbounded_channel::<nexodesk_proto::v1::FileReceived>();
    // Answers of the file manager, computed off the control loop.
    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<nexodesk_proto::v1::FsResponse>();
    // Shared clipboard: what is copied here goes to the viewer, what is copied there arrives here.
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<nexodesk_clipboard::Content>();
    let clipboard = if config.view_only || config.synthetic.is_some() {
        None
    } else {
        match nexodesk_clipboard::spawn_sync(move |content| clip_tx.send(content).is_ok()) {
            Ok(tx) => Some(tx),
            Err(e) => {
                warn!("clipboard not shared: {e:#}");
                None
            }
        }
    };
    // Images arrive on their own streams, received with the files.
    let clipboard_in = clipboard.clone();

    let mut video_tx = FramedSend::new(conn.open_uni().await?);
    tracing::debug!("video channel open");
    let video = {
        let unanswered_since_ms = unanswered_since_ms.clone();
        let sent_bytes = sent_bytes.clone();
        async move {
            while let Some(frame) = frame_rx.recv().await {
                tracing::trace!(seq = frame.seq, bytes = frame.data.len(), "sending frame");
                video_tx.send(&frame).await?;
                sent_bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                let now_ms = session_start.elapsed().as_millis().max(1) as u64;
                let _ = unanswered_since_ms.compare_exchange(0, now_ms, Ordering::Relaxed, Ordering::Relaxed);
            }
            anyhow::Ok(())
        }
    };

    let control_loop = async {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        // Samples (time, bytes sent) of the last 2 s, for the rate actually used.
        let mut sent_samples = std::collections::VecDeque::<(Instant, u64)>::new();
        loop {
            let msg = tokio::select! {
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(msg) => msg,
                    None => return anyhow::Ok(()),
                },
                Some(content) = clip_rx.recv() => {
                    match content {
                        nexodesk_clipboard::Content::Text(text) => {
                            ctrl_tx.send(&control(Msg::Clipboard(nexodesk_proto::v1::Clipboard { text }))).await?;
                        }
                        // On its own stream, not to hold up the control channel.
                        nexodesk_clipboard::Content::Image(png) => {
                            let conn = conn.clone();
                            tokio::spawn(async move {
                                let header = nexodesk_proto::v1::FileChunk { clipboard_image: true, ..Default::default() };
                                if let Err(e) = nexodesk_transport::send_stream(&conn, header, &png).await {
                                    warn!("clipboard image not sent: {e:#}");
                                }
                            });
                        }
                    }
                    continue;
                }
                Some(text) = chat_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Chat(nexodesk_proto::v1::Chat { text }))).await?;
                    continue;
                }
                Some(answer) = fs_rx.recv() => {
                    ctrl_tx.send(&control(Msg::FsResponse(answer))).await?;
                    continue;
                }
                Some(done) = file_rx.recv() => {
                    ctrl_tx.send(&control(Msg::FileReceived(done))).await?;
                    continue;
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    sent_samples.push_back((now, sent_bytes.load(Ordering::Relaxed)));
                    while sent_samples.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(2)) {
                        sent_samples.pop_front();
                    }
                    // A frame left long ago and no feedback since: the network is probably stuck.
                    let now_ms = session_start.elapsed().as_millis() as u64;
                    let since = unanswered_since_ms.load(Ordering::Relaxed);
                    if let Some(abr) = abr.as_mut()
                        && since != 0
                        && now_ms.saturating_sub(since) > abr::FEEDBACK_TIMEOUT.as_millis() as u64
                        && let Some(bps) = abr.on_silence(Instant::now())
                    {
                        bitrate.store(bps, Ordering::Relaxed);
                        info!("bitrate {} kbit/s (no feedback from the viewer)", bps / 1000);
                    }
                    continue;
                }
            };
            match msg.msg {
                Some(Msg::Ping(ping)) => {
                    let pong = Pong { ping_sent_us: ping.sent_us, agent_us: nexodesk_proto::now_us() };
                    ctrl_tx.send(&control(Msg::Pong(pong))).await?;
                }
                Some(Msg::RequestKeyframe(_)) => keyframe.store(true, Ordering::Relaxed),
                Some(Msg::FsRequest(request)) => {
                    let fs_tx = fs_tx.clone();
                    if config.view_only || config.synthetic.is_some() {
                        let error = "@agent.files_view_only".to_string();
                        let _ = fs_tx.send(nexodesk_proto::v1::FsResponse { id: request.id, error, ..Default::default() });
                        continue;
                    }
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let _ = fs_tx.send(fsops::handle(request.id, request.op, conn).await);
                    });
                }
                Some(Msg::Feedback(fb)) => {
                    unanswered_since_ms.store(0, Ordering::Relaxed);
                    // The network part when the viewer reports it: a slow encoder or
                    // graphics card is not congestion and lowering the bitrate would not help.
                    let (p50, min) = match (fb.transit_ms_p50, fb.transit_ms_min) {
                        (Some(p50), Some(min)) => (p50, min),
                        _ => (fb.latency_ms_p50, fb.latency_ms_min),
                    };
                    let sent_bps = match (sent_samples.front(), sent_samples.back()) {
                        (Some((t0, b0)), Some((t1, b1))) if t1 > t0 => {
                            (b1 - b0) as f64 * 8.0 / t1.duration_since(*t0).as_secs_f64()
                        }
                        // Start of the session: no measurement yet, latency counts.
                        _ => f64::INFINITY,
                    };
                    if let Some(abr) = abr.as_mut()
                        && fb.frames > 0
                        && let Some(bps) = abr.on_feedback(Instant::now(), p50 as f64, min as f64, sent_bps)
                    {
                        bitrate.store(bps, Ordering::Relaxed);
                        info!(
                            "bitrate {} kbit/s (network {p50} ms, minimum {min} ms; total latency {} ms; sending {} kbit/s)",
                            bps / 1000,
                            fb.latency_ms_p50,
                            (sent_bps.min(1e12) / 1000.0) as u64
                        );
                    }
                }
                Some(Msg::SelectDisplay(select)) if config.synthetic.is_none() => {
                    let index = select.index as usize;
                    if index == display.index {
                        continue;
                    }
                    // First the current capture is stopped, then the new one is opened.
                    let old = pipeline.lock().expect("pipeline").take();
                    let _ = tokio::task::spawn_blocking(move || drop(old)).await;
                    let (new_display, new_pipeline) = match source.start(Some(index)).await {
                        Ok(started) => started,
                        Err(e) => {
                            warn!("screen {index} not available: {e:#}; staying on the previous one");
                            source.start(Some(display.index)).await?
                        }
                    };
                    *pipeline.lock().expect("pipeline") = Some(new_pipeline);
                    display = new_display;
                    ctrl_tx.send(&video_format(&display)).await?;
                    if let Some(msg) = displays_message(config, &display) {
                        ctrl_tx.send(&msg).await?;
                    }
                    let _ = screen_tx.send(screen_rect(&display));
                }
                Some(Msg::Clipboard(clip)) => {
                    if let Some(clipboard) = &clipboard {
                        let _ = clipboard.send(nexodesk_clipboard::Content::Text(clip.text));
                    }
                }
                Some(Msg::Chat(chat)) => match &events {
                    Some(events) => {
                        let _ = events.send(AgentEvent::Chat { text: chat.text });
                    }
                    None => info!("chat from the viewer: {}", chat.text),
                },
                Some(Msg::SetQuality(q)) => {
                    let (limit_bps, limit_fps) = quality_limits(config, q.quality());
                    fps.store(limit_fps, Ordering::Relaxed);
                    let bps = match abr.as_mut() {
                        Some(abr) => abr.set_max(limit_bps),
                        None => Some(limit_bps),
                    };
                    if let Some(bps) = bps {
                        bitrate.store(bps, Ordering::Relaxed);
                    }
                    info!("quality {:?}: up to {} kbit/s and {limit_fps} fps", q.quality(), limit_bps / 1000);
                }
                _ => {}
            }
        }
    };

    let input = async {
        let mut rx = FramedRecv::new(conn.accept_uni().await?);
        let injector = if config.view_only || config.synthetic.is_some() {
            None
        } else {
            let screen = *screen_rx.borrow_and_update();
            Some(input_thread(nexodesk_input::open(screen)?)?)
        };
        let closed = || anyhow!("input thread ended");
        loop {
            tokio::select! {
                event = rx.recv::<InputEvent>() => match event? {
                    Some(event) => if let Some(injector) = &injector {
                        injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                    },
                    None => break,
                },
                Ok(()) = screen_rx.changed() => if let Some(injector) = &injector {
                    let screen = *screen_rx.borrow_and_update();
                    injector.send(InputCmd::Screen(screen)).map_err(|_| closed())?;
                },
                // After the input one, every unidirectional stream of the viewer is a file
                // or an image copied to the clipboard.
                stream = conn.accept_uni() => {
                    let mut stream = stream?;
                    if injector.is_none() {
                        // View only: no files.
                        let _ = stream.stop(0u32.into());
                        continue;
                    }
                    let (file_tx, events, clipboard) = (file_tx.clone(), events.clone(), clipboard_in.clone());
                    tokio::spawn(async move {
                        let mut rx = FramedRecv::new(stream);
                        let first = match rx.recv::<nexodesk_proto::v1::FileChunk>().await {
                            Ok(Some(first)) => first,
                            _ => return,
                        };
                        // File manager: into the chosen folder, with resume.
                        if first.transfer_id != 0 {
                            let (name, transfer_id) = (first.name.clone(), first.transfer_id);
                            let done = match fsops::receive(first, rx).await {
                                Ok(path) => nexodesk_proto::v1::FileReceived {
                                    name,
                                    ok: true,
                                    transfer_id,
                                    path: path.display().to_string(),
                                    ..Default::default()
                                },
                                Err(e) => {
                                    warn!("file {name} not received: {e:#}");
                                    nexodesk_proto::v1::FileReceived { name, transfer_id, message: format!("{e:#}"), ..Default::default() }
                                }
                            };
                            let _ = file_tx.send(done);
                            return;
                        }
                        if first.clipboard_image {
                            match nexodesk_transport::read_stream(&mut rx, &first, nexodesk_clipboard::MAX_IMAGE_PNG as u64).await {
                                Ok(png) => if let Some(clipboard) = clipboard {
                                    let _ = clipboard.send(nexodesk_clipboard::Content::Image(png));
                                },
                                Err(e) => warn!("clipboard image not received: {e:#}"),
                            }
                            return;
                        }
                        let done = match files::receive(first, rx).await {
                            Ok((name, path)) => {
                                if let Some(events) = &events {
                                    let path = path.display().to_string();
                                    let _ = events.send(AgentEvent::FileReceived { name: name.clone(), path });
                                }
                                nexodesk_proto::v1::FileReceived { name, ok: true, message: "@agent.downloads".into(), ..Default::default() }
                            }
                            Err((name, e)) => {
                                warn!("file {name} not received: {e:#}");
                                nexodesk_proto::v1::FileReceived { name, ok: false, message: format!("{e:#}"), ..Default::default() }
                            }
                        };
                        let _ = file_tx.send(done);
                    });
                }
            }
        }
        anyhow::Ok(())
    };

    let result = tokio::select! {
        r = video => r.context("sending video"),
        r = control_loop => r.context("control channel"),
        r = input => r.context("input channel"),
    };
    let last = pipeline.lock().expect("pipeline").take();
    let _ = tokio::task::spawn_blocking(move || drop(last)).await;
    if let Some(gate) = gate {
        *gate.ctx.chat.lock().expect("chat") = None;
    }
    drop(chat_tx);
    result
}

/// Capture and encoding of a screen, restartable on another one during the session.
struct VideoSource {
    config: AgentConfig,
    /// Copy of the sender: keeps the video channel open while the screen changes.
    frames: mpsc::Sender<VideoFrame>,
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    fps: Arc<AtomicU32>,
}

impl VideoSource {
    async fn start(&self, display: Option<usize>) -> Result<(nexodesk_capture::Display, Pipeline)> {
        let mut cap_config = self.config.clone();
        cap_config.display = display;
        // The encoder starts from the current bitrate of the adaptive control.
        let settings = EncoderSettings {
            bitrate_bps: self.bitrate.load(Ordering::Relaxed),
            max_fps: self.fps.load(Ordering::Relaxed),
        };
        let plan = self.config.plan;
        // A new stream must start with a keyframe.
        self.keyframe.store(true, Ordering::Relaxed);
        let (frames, keyframe, bitrate, fps) =
            (self.frames.clone(), self.keyframe.clone(), self.bitrate.clone(), self.fps.clone());
        tokio::task::spawn_blocking(move || {
            Pipeline::start(
                move || cap_config.open_capturer(),
                move |gpu| nexodesk_codec::create_encoder(plan, settings, gpu),
                plan == EncoderPlan::HardwareGpu,
                fps,
                frames,
                keyframe,
                bitrate,
            )
        })
        .await?
        .context("starting capture and encoding")
    }
}

fn video_format(display: &nexodesk_capture::Display) -> ControlMessage {
    control(Msg::VideoFormat(VideoFormat { codec: Codec::H264 as i32, width: display.width, height: display.height }))
}

/// List of screens for the viewer (none with the tests' synthetic image).
fn displays_message(config: &AgentConfig, current: &nexodesk_capture::Display) -> Option<ControlMessage> {
    if config.synthetic.is_some() {
        return None;
    }
    let list = nexodesk_capture::displays().ok()?;
    let all_available = list.len() > 1;
    let displays = list
        .into_iter()
        .map(|d| DisplayInfo {
            index: d.index as u32,
            name: d.name,
            width: d.width,
            height: d.height,
            primary: d.primary,
        })
        .collect();
    Some(control(Msg::Displays(Displays { displays, current: current.index as u32, all_available })))
}

fn screen_rect(display: &nexodesk_capture::Display) -> nexodesk_input::ScreenRect {
    nexodesk_input::ScreenRect { left: display.left, top: display.top, width: display.width, height: display.height }
}

/// Bitrate cap (bit/s) and fps for the quality chosen by the viewer.
fn quality_limits(config: &AgentConfig, quality: Quality) -> (u32, u32) {
    let (bps, fps) = (config.bitrate_kbps * 1000, config.fps);
    match quality {
        Quality::Sharp => (bps * 2, fps.min(30)),
        Quality::Fast => ((bps / 2).max(MIN_BITRATE), fps),
        Quality::Balanced | Quality::Unspecified => (bps, fps),
    }
}

/// What the input thread receives.
enum InputCmd {
    Event(InputEvent),
    /// Screen change: mouse coordinates refer to this one.
    Screen(nexodesk_input::ScreenRect),
}

/// Injects input on a dedicated thread, which can follow the active desktop
/// (UAC, lock screen) without touching tokio's threads. Closing the channel
/// ends the thread, which releases the keys still pressed.
fn input_thread(
    mut injector: Box<dyn nexodesk_input::InputInjector>,
) -> Result<std::sync::mpsc::Sender<InputCmd>> {
    let (tx, rx) = std::sync::mpsc::channel::<InputCmd>();
    std::thread::Builder::new().name("input".into()).spawn(move || {
        for cmd in rx {
            let event = match cmd {
                InputCmd::Event(event) => event,
                InputCmd::Screen(screen) => {
                    injector.set_screen(screen);
                    continue;
                }
            };
            #[cfg(windows)]
            if let Some(name) = nexodesk_capture::desktop::follow_input_desktop() {
                info!("input on desktop {name}");
            }
            if let Err(e) = injector.inject(&event) {
                warn!("input not injected: {e:#}");
            }
        }
    })?;
    Ok(tx)
}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

fn local_hello() -> Hello {
    Hello {
        protocol_version: nexodesk_proto::PROTOCOL_VERSION,
        device_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
        os: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        password_required: false,
        file_manager: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_dictatable() {
        let p = random_password();
        assert_eq!(p.len(), 8);
        assert!(p.chars().all(|c| !"01lIoO".contains(c)));
        assert!(same_secret(&p, &p.clone()));
        assert!(!same_secret(&p, "abcdefgh") || p == "abcdefgh");
        assert!(!same_secret("abc", "abcd"));
    }

    #[test]
    fn synthetic_spec() {
        assert_eq!(parse_synthetic("1280x720@30").unwrap(), (1280, 720, 30));
        assert_eq!(parse_synthetic("640x480").unwrap(), (640, 480, 60));
        assert!(parse_synthetic("abc").is_err());
    }
}
