//! NuviraHub agent — the controlled device.
//!
//! Library used by the `nuvira` app and by the `nuvira-agent` command-line
//! program: it registers with nuvira-server (or waits for direct LAN
//! connections), streams the screen and applies the mouse and keyboard input it
//! receives. With the server it talks to its user through [`AgentEvent`] and [`AgentCommand`].

mod abr;
pub mod access;
pub mod cli;
#[cfg(windows)]
pub mod clip_user;
mod files;
mod fsops;
#[cfg(any(windows, unix))]
pub mod ipc;
mod inventory;
mod lock;
mod pipeline;
mod printer;
mod pty;
mod restart;
pub mod reverse;
mod session_stats;
mod shared;
mod switches;
mod terminal;
mod tunnel;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nuvira_capture::{Capturer, SyntheticCapturer};
use nuvira_codec::{EncoderPlan, EncoderSettings};
use nuvira_proto::v1::{
    Auth, AuthResult, Codec, ControlMessage, DisplayInfo, Displays, Hello, InputEvent, Pong, Quality, VideoFormat,
    VideoFrame, control_message::Msg,
};
use nuvira_transport::signal::TransportMode;
use nuvira_transport::{Connection, Fingerprint, FramedRecv, FramedSend};
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use crate::pipeline::{Controls, Pipeline};

/// Minimum bitrate of the adaptive control.
const MIN_BITRATE: u32 = 500_000;
/// Parity of the video as datagrams (shards per data shard): at the start, and the
/// range it moves in with the losses reported (3 × the share lost, plus the minimum).
const FEC_START: f64 = 0.2;
const FEC_MIN: f64 = 0.1;
const FEC_MAX: f64 = 0.5;

/// Rejection reasons sent to the viewer are translation keys (`@key`, see
/// nuvira-i18n): the viewer shows them in its own language.
const WRONG_PASSWORD: &str = "@agent.wrong_password";
/// The permanent password is right, the two-factor code is missing.
const CODE_REQUIRED: &str = "@agent.code_required";
/// The viewer's computer is not among those allowed to connect.
const NOT_ALLOWED: &str = "@agent.not_allowed";
/// Two-factor codes: after this many wrong ones in [`CODE_WINDOW`], the permanent
/// passwords are refused until the window has passed.
const CODE_MAX_WRONG: usize = 5;
const CODE_WINDOW: Duration = Duration::from_secs(600);

/// Time given to the person at the PC to accept a connection request.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
/// With the rule "record every session", how long the viewer has to start recording.
const RECORDING_START: Duration = Duration::from_secs(15);
/// How long the agent waits for each message of the handshake before the viewer
/// has authenticated: its Hello, then its password (a viewer sends each at once,
/// one round trip after the previous one). The confirmation by the person at the
/// PC ([`CONFIRM_TIMEOUT`]) and the two-factor code have their own, longer waits.
pub const HANDSHAKE_READ: Duration = Duration::from_secs(10);
/// Sessions through the server whose viewer has not authenticated yet: they do
/// not make the computer busy (anyone who knows the ID can start one), but at
/// most this many wait for their password at once; more are refused as busy.
pub const MAX_PENDING_SESSIONS: u32 = 8;

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
    /// Encoder chosen at start-up (see `nuvira_codec::plan_encoder`).
    pub plan: EncoderPlan,
    /// Fixed bitrate instead of adapting it to the network.
    pub fixed_bitrate: bool,
    /// Ignores the viewer's mouse and keyboard.
    pub view_only: bool,
}

impl AgentConfig {
    pub fn encoder_settings(&self) -> EncoderSettings {
        EncoderSettings { bitrate_bps: self.bitrate_kbps * 1000, max_fps: self.fps, ..Default::default() }
    }

    fn open_capturer(&self) -> Result<Box<dyn Capturer>> {
        match self.synthetic {
            Some((w, h, fps)) => Ok(Box::new(SyntheticCapturer::new(w, h, fps))),
            None => {
                // DMA-BUF capture where the encoder takes it (VA-API with video processing).
                #[cfg(target_os = "linux")]
                nuvira_capture::set_dmabuf_modifiers(if self.plan == EncoderPlan::Software {
                    Vec::new()
                } else {
                    nuvira_codec::dmabuf_import_modifiers()
                });
                nuvira_capture::open(self.display)
            }
        }
    }
}

/// Synthetic image in the "WxH@FPS" format, e.g. "1920x1080@60".
pub fn parse_synthetic(spec: &str) -> Result<(u32, u32, u32)> {
    let (size, fps) = spec.split_once('@').unwrap_or((spec, "60"));
    let (w, h) = size.split_once('x').context("expected format WxH@FPS, e.g. 1920x1080@60")?;
    Ok((w.parse()?, h.parse()?, fps.parse()?))
}

/// How to reach nuvira-server and how to handle requests.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// host[:port] of the server.
    pub server_name: String,
    /// Expected certificate fingerprint; without it, the remembered one is used (or remembered).
    pub server_key: Option<Fingerprint>,
    pub transport: TransportMode,
    /// URL of the WebSocket fallback; without it, `ws://<server>:<port>/nuvira`.
    pub server_ws: Option<String>,
    pub identity_path: PathBuf,
    /// Whoever uses the temporary password must be confirmed by the person at the PC
    /// (event [`AgentEvent::Incoming`]). Without it, the password is enough.
    pub confirm: bool,
    /// Permanent password for unattended access: never confirmed.
    pub permanent_password_hash: Option<String>,
    /// Automated tests: where to write "ID password" once registered.
    pub info_file: Option<PathBuf>,
    /// Started for unattended access (by the service): it comes back by itself after
    /// a restart of the computer, which the viewer may then ask for.
    pub unattended: bool,
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
        /// A technician who took this device's support request (support queue).
        #[serde(default)]
        support: bool,
    },
    /// The request is no longer pending (expired or already answered).
    IncomingClosed {
        session_id: String,
    },
    SessionStarted {
        viewer_id: String,
        viewer_name: String,
        direct: bool,
        /// Terminal only, no desktop: nothing shows on the screen.
        #[serde(default)]
        terminal: bool,
    },
    SessionEnded {
        error: Option<String>,
    },
    /// Who joined the session in progress besides the viewer who started it
    /// (shared sessions): at every change, empty when the last one leaves.
    Guests {
        names: Vec<String>,
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
    /// The controlling side started (`on`) or stopped recording the session.
    Recording {
        on: bool,
    },
    /// What the session in progress has (`offered`) and what the person at the PC
    /// switched off of it (`off`), by name: "input", "clipboard", "files",
    /// "terminal", "tunnel", "print". At the start and at every change.
    Permissions {
        offered: Vec<String>,
        off: Vec<String>,
    },
    /// This device's alias on the server, and why the one asked for is not it.
    Alias {
        alias: String,
        error: String,
    },
    /// The technician's microphone is played here (`on`), or not any more.
    Microphone {
        on: bool,
    },
    /// The rules of unattended access of this computer, as the app shows them (no
    /// hashes, no secret): at the start and when the app asks ([`AgentCommand::GetAccess`]).
    Access {
        view: access::View,
    },
    /// Rules of the server that the app shows (at the registration and when they change).
    Rules {
        recording_required: bool,
        /// The computer is locked at the end of every session.
        #[serde(default)]
        lock_required: bool,
        /// Sessions end after this many minutes without activity (0: no rule).
        #[serde(default)]
        idle_minutes: u32,
    },
    /// State of this device's support request (support queue, NuviraHub Pro):
    /// its place in the queue, who took it, or closed.
    Support {
        position: u32,
        taken_by: String,
        closed: bool,
        /// Why the request was not queued ("@key"), e.g. an expired invitation.
        #[serde(default)]
        error: String,
        /// The technician the request is for (an invitation).
        #[serde(default)]
        invited_by: String,
    },
}

/// What the person at the PC asks for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum AgentCommand {
    Respond {
        session_id: String,
        accept: bool,
    },
    EndSession,
    /// Generates a new temporary password (answer: [`AgentEvent::Password`]).
    NewPassword,
    /// Chat message for the controlling side (session in progress).
    Chat {
        text: String,
    },
    /// Asks the server's team for support (support queue, NuviraHub Pro).
    RequestSupport {
        message: String,
        /// An invitation's code: the request is for the technician who made it.
        #[serde(default)]
        invite: String,
    },
    CancelSupport,
    /// The person at the PC allows privacy mode or not (app settings). Kept in the
    /// agent's data folder, so that it holds also while the app is closed.
    AllowPrivacy {
        allow: bool,
    },
    /// The person at the PC allows the remote terminal or not (kept like `AllowPrivacy`).
    AllowTerminal {
        allow: bool,
    },
    /// The person at the PC allows TCP tunnels to the local network or not.
    AllowTunnel {
        allow: bool,
    },
    /// The person at the PC allows remote printing or not (the NuviraHub printer
    /// sends what is printed on it to the technician).
    AllowPrint {
        allow: bool,
    },
    /// The person at the PC allows whoever controls it to record the session or not
    /// (the app shows a notice while it happens).
    AllowRecording {
        allow: bool,
    },
    /// The app wants the rules of unattended access again (it saved new ones).
    GetAccess,
    /// A readable name instead of the ID on the server ("" none).
    SetAlias {
        alias: String,
    },
    /// The person at the PC mutes the technician's microphone, or not.
    MuteMicrophone {
        muted: bool,
    },
    /// Sessions end after this many minutes without activity from the viewer, 0
    /// never (kept like `AllowPrivacy`).
    IdleMinutes {
        minutes: u32,
    },
    /// The person at the PC wants the computer locked when a session ends, or not
    /// (kept like `AllowPrivacy`).
    LockOnEnd {
        on: bool,
    },
    /// Switches something off (or back on) for the session in progress only, from the
    /// strip of the app: `name` as in [`AgentEvent::Permissions`].
    Switch {
        name: String,
        on: bool,
    },
}

/// Choices of the person at the PC kept by the agent (`permissions.json` in its
/// data folder): with the service it is the service's protected folder.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Permissions {
    #[serde(default = "yes")]
    privacy: bool,
    #[serde(default = "yes")]
    terminal: bool,
    #[serde(default = "yes")]
    tunnel: bool,
    #[serde(default = "yes")]
    print: bool,
    #[serde(default = "yes")]
    recording: bool,
    /// Lock the computer when a session ends.
    #[serde(default)]
    lock_on_end: bool,
    /// Sessions end after this many minutes without activity (0: never).
    #[serde(default)]
    idle_minutes: u32,
}

fn yes() -> bool {
    true
}

impl Permissions {
    fn path() -> PathBuf {
        nuvira_transport::identity::data_dir().join("permissions.json")
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Self {
                privacy: true,
                terminal: true,
                tunnel: true,
                print: true,
                recording: true,
                lock_on_end: false,
                idle_minutes: 0,
            })
    }

    fn save(self) {
        let path = Self::path();
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, serde_json::to_string(&self).unwrap_or_default()));
        if let Err(e) = result {
            warn!("{} not saved: {e}", path.display());
        }
    }
}

/// Ticket of a support request: 128 random bits in hex.
fn support_ticket() -> String {
    let mut bytes = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("random generator");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    nuvira_input::privacy::recover();
    recover_screens(&nuvira_transport::identity::data_dir());
    let bind: SocketAddr = ([0, 0, 0, 0], port).into();
    let (endpoint, fingerprint) = nuvira_transport::server_endpoint(bind)?;
    info!("waiting for the viewer on UDP {bind}");
    let fingerprint = nuvira_transport::fingerprint_hex(&fingerprint);
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
        let peer = nuvira_transport::p2p::canonical(conn.remote_address());
        info!("viewer connected from {peer}");
        let result = match control_channel(&conn).await {
            Ok(streams) => session(conn.clone(), streams, &config, None, None).await,
            Err(e) => Err(e),
        };
        match &result {
            Ok(()) => info!("session with {peer} ended"),
            Err(e) => warn!("session with {peer} ended: {e:#}"),
        }
        conn.close(0u32.into(), &close_reason(&result));
        if once {
            break;
        }
    }
    endpoint.wait_idle().await;
    Ok(())
}

/// Registers the device with nuvira-server and accepts the incoming sessions.
/// If the server goes away, it reconnects. It ends only when `commands` is closed.
pub async fn serve_via_server(
    config: AgentConfig,
    opts: ServerOptions,
    password: Arc<Mutex<String>>,
    events: mpsc::UnboundedSender<AgentEvent>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
) -> Result<()> {
    // A privacy mode left on by a killed agent (Linux: local devices still off).
    nuvira_input::privacy::recover();
    // Screens left at a session's resolution by a killed agent.
    recover_screens(opts.identity_path.parent().unwrap_or(&nuvira_transport::identity::data_dir()));
    let server = nuvira_transport::resolve_addr(&opts.server_name, nuvira_transport::signal::DEFAULT_PORT)?;
    let identity = Arc::new(nuvira_transport::identity::Identity::load_or_create(&opts.identity_path)?);
    let permissions = Permissions::load();
    let ctx = Arc::new(SessionContext {
        config,
        password,
        permanent: opts.permanent_password_hash.clone(),
        confirm: opts.confirm,
        pending: Mutex::new(HashMap::new()),
        events,
        chat: Mutex::new(None),
        busy: AtomicU32::new(0),
        attempts: AtomicU32::new(0),
        claims: Mutex::new(()),
        waiting: Mutex::new(None),
        end_session: Notify::new(),
        shared: Mutex::new(None),
        handover: Mutex::new(None),
        policy: Default::default(),
        support_ticket: Mutex::new(None),
        support_request: Mutex::new(None),
        allow_privacy: AtomicBool::new(permissions.privacy),
        allow_terminal: AtomicBool::new(permissions.terminal),
        allow_tunnel: AtomicBool::new(permissions.tunnel),
        allow_print: AtomicBool::new(permissions.print),
        allow_recording: AtomicBool::new(permissions.recording),
        lock_on_end: AtomicBool::new(permissions.lock_on_end),
        lock_asked: AtomicBool::new(false),
        idle_minutes: AtomicU32::new(permissions.idle_minutes),
        mic_muted: tokio::sync::watch::channel(false).0,
        access_dir: opts
            .identity_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(nuvira_transport::identity::data_dir),
        last_code_step: AtomicU64::new(0),
        wrong_codes: Mutex::new(Vec::new()),
        features: Mutex::new(Vec::new()),
        unattended: opts.unattended,
        restarting: Mutex::new(None),
        switches: Mutex::new(None),
    });
    let current = ctx.password.lock().expect("password").clone();
    let _ = ctx.events.send(AgentEvent::Password { password: current });
    let _ = ctx.events.send(AgentEvent::Access { view: access::View::of(&ctx.access_dir) });

    loop {
        let _ = ctx.events.send(AgentEvent::Connecting);
        let expected =
            opts.server_key.or_else(|| nuvira_transport::signal::known_server::get(&opts.server_name));
        match signal_loop(&ctx, &opts, &identity, server, expected, &mut commands).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!("connection with nuvira-server lost: {e:#}; retrying in 5 s");
                let _ = ctx.events.send(AgentEvent::Offline { reason: format!("{e:#}") });
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// State shared between the signalling loop and the session in progress.
/// Counters of the session in progress (one at a time), for the report to the server.
pub(crate) mod stats {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    pub static FILES_IN: AtomicU32 = AtomicU32::new(0);
    pub static FILES_OUT: AtomicU32 = AtomicU32::new(0);
    pub static BYTES_IN: AtomicU64 = AtomicU64::new(0);
    pub static BYTES_OUT: AtomicU64 = AtomicU64::new(0);

    pub fn reset() {
        for c in [&FILES_IN, &FILES_OUT] {
            c.store(0, Ordering::Relaxed);
        }
        for c in [&BYTES_IN, &BYTES_OUT] {
            c.store(0, Ordering::Relaxed);
        }
    }

    pub fn file_in(bytes: u64) {
        FILES_IN.fetch_add(1, Ordering::Relaxed);
        BYTES_IN.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn file_out(bytes: u64) {
        FILES_OUT.fetch_add(1, Ordering::Relaxed);
        BYTES_OUT.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// The viewer closed the session on purpose (QUIC close with code 0): not an error
/// for the session log.
fn closed_normally(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        let closed = |c: &quinn::ConnectionError| {
            matches!(c, quinn::ConnectionError::ApplicationClosed(a) if a.error_code == quinn::VarInt::from_u32(0))
        };
        cause.downcast_ref::<quinn::ConnectionError>().is_some_and(closed)
            || cause.downcast_ref::<quinn::ReadError>().is_some_and(|r| matches!(r, quinn::ReadError::ConnectionLost(c) if closed(c)))
            || cause.downcast_ref::<quinn::WriteError>().is_some_and(|w| matches!(w, quinn::WriteError::ConnectionLost(c) if closed(c)))
            || cause
                .downcast_ref::<std::io::Error>()
                .and_then(|io| io.get_ref())
                .and_then(|inner| inner.downcast_ref::<quinn::ReadError>())
                .is_some_and(|r| matches!(r, quinn::ReadError::ConnectionLost(c) if closed(c)))
            || cause
                .downcast_ref::<quinn::ReadExactError>()
                .is_some_and(|r| matches!(r, quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(c)) if closed(c)))
    })
}

/// What the report to the server says about a session, filled while it runs.
#[derive(Default)]
struct Outcome {
    /// The viewer authenticated and this was the session in progress ([`Gate::claim`]):
    /// the computer stays busy until the end is handled.
    claimed: bool,
    connected: Option<std::time::Instant>,
    direct: bool,
    access: std::sync::Mutex<&'static str>,
}

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
    /// Sessions whose viewer authenticated and that have not ended: one at most
    /// ([`Gate::claim`]); guests are not counted. Other viewers are refused (or
    /// join, see `shared`) while it is not 0.
    busy: AtomicU32,
    /// Sessions routed here (not guests) that have not ended, authenticated or not:
    /// at most [`MAX_PENDING_SESSIONS`] are let in while none is `busy`.
    attempts: AtomicU32,
    /// Held while a session claims `busy` and while a session ends: the app never
    /// hears of an attempt ending after another viewer's session started.
    claims: Mutex<()>,
    /// Viewer the agent is waiting for (its connection not open yet) and how to stop
    /// waiting: a new request from the same viewer (window closed while connecting,
    /// then reopened) replaces the attempt instead of finding the PC busy.
    waiting: Mutex<Option<(String, Arc<Notify>)>>,
    end_session: Notify,
    /// Rules of the server (confirmation always, no files, no clipboard).
    policy: Mutex<nuvira_proto::v1::Policy>,
    /// Ticket of the open support request: whoever presents it in a session is
    /// treated as the technician who took it, and still has to be accepted.
    support_ticket: Mutex<Option<String>>,
    /// The support request sent with that ticket: sent again after reconnecting to
    /// the server (a network cut, the server restarted), while the ticket is open.
    support_request: Mutex<Option<nuvira_proto::v1::SupportRequest>>,
    /// The person at the PC allows privacy mode ([`AgentCommand::AllowPrivacy`]).
    allow_privacy: AtomicBool,
    /// The person at the PC allows the remote terminal ([`AgentCommand::AllowTerminal`]).
    allow_terminal: AtomicBool,
    /// The person at the PC allows TCP tunnels ([`AgentCommand::AllowTunnel`]).
    allow_tunnel: AtomicBool,
    /// The person at the PC allows remote printing ([`AgentCommand::AllowPrint`]).
    allow_print: AtomicBool,
    /// The person at the PC allows recordings ([`AgentCommand::AllowRecording`]).
    allow_recording: AtomicBool,
    /// Optional modules of the server (NuviraHub Pro), from the registration.
    features: Mutex<Vec<String>>,
    /// See [`ServerOptions::unattended`].
    unattended: bool,
    /// A restart of the computer is under way (since then): new sessions are
    /// refused until the agent ends, so that a viewer waiting to reconnect finds the
    /// computer restarted. After [`RESTART_GIVEN_UP`] the restart did not happen
    /// (on macOS an app with unsaved work can stop it) and sessions are accepted again.
    restarting: Mutex<Option<Instant>>,
    /// Switches of the session in progress ([`AgentCommand::Switch`]).
    switches: Mutex<Option<Arc<switches::Switches>>>,
    /// The session in progress, which other viewers can join ([`shared`]).
    shared: Mutex<Option<Arc<shared::Shared>>>,
    /// The last session was handed over to a guest, who connects again with this.
    handover: Mutex<Option<shared::Handover>>,
    /// The person at the PC wants it locked when a session ends ([`AgentCommand::LockOnEnd`]).
    lock_on_end: AtomicBool,
    /// The viewer of the session in progress asked for the lock at the end (`SetLockOnEnd`).
    lock_asked: AtomicBool,
    /// Minutes without activity after which sessions end ([`AgentCommand::IdleMinutes`]).
    idle_minutes: AtomicU32,
    /// The person at the PC muted the technician's microphone (for the session in
    /// progress; [`AgentCommand::MuteMicrophone`]).
    mic_muted: tokio::sync::watch::Sender<bool>,
    /// Folder of the device key, where the rules of unattended access are ([`access`]).
    access_dir: PathBuf,
    /// Time step of the last two-factor code accepted: each code works once.
    last_code_step: AtomicU64,
    /// When the last wrong two-factor codes came.
    wrong_codes: Mutex<Vec<Instant>>,
}

impl SessionContext {
    fn save_permissions(&self) {
        Permissions {
            privacy: self.allow_privacy.load(Ordering::Relaxed),
            terminal: self.allow_terminal.load(Ordering::Relaxed),
            tunnel: self.allow_tunnel.load(Ordering::Relaxed),
            print: self.allow_print.load(Ordering::Relaxed),
            recording: self.allow_recording.load(Ordering::Relaxed),
            lock_on_end: self.lock_on_end.load(Ordering::Relaxed),
            idle_minutes: self.idle_minutes.load(Ordering::Relaxed),
        }
        .save();
    }
}

fn set_policy(ctx: &SessionContext, policy: nuvira_proto::v1::Policy) {
    info!(
        confirm_always = policy.confirm_always,
        no_files = policy.no_files,
        no_clipboard = policy.no_clipboard,
        no_privacy = policy.no_privacy,
        no_terminal = policy.no_terminal,
        no_tunnel = policy.no_tunnel,
        no_print = policy.no_print,
        record_always = policy.record_always,
        no_resolution = policy.no_resolution,
        no_audio = policy.no_audio,
        "rules of the server"
    );
    // The app shows the setting of the recordings locked when the rules want them.
    let _ = ctx.events.send(AgentEvent::Rules {
        recording_required: policy.record_always,
        lock_required: policy.lock_on_end,
        idle_minutes: policy.idle_minutes,
    });
    *ctx.policy.lock().expect("policy") = policy;
}

/// Request accepted by the server, waiting for the route.
struct Request {
    viewer_id: String,
    viewer_name: String,
    /// What this session may not do on top of the server's rules (Incoming.limits).
    limits: nuvira_proto::v1::Policy,
    /// It joins the session in progress ([`shared`]).
    join: bool,
}

/// The rules and the limits of a session together: every limit of either holds.
fn merge_limits(a: nuvira_proto::v1::Policy, b: &nuvira_proto::v1::Policy) -> nuvira_proto::v1::Policy {
    nuvira_proto::v1::Policy {
        confirm_always: a.confirm_always || b.confirm_always,
        no_files: a.no_files || b.no_files,
        no_clipboard: a.no_clipboard || b.no_clipboard,
        no_privacy: a.no_privacy || b.no_privacy,
        no_terminal: a.no_terminal || b.no_terminal,
        no_tunnel: a.no_tunnel || b.no_tunnel,
        no_print: a.no_print || b.no_print,
        record_always: a.record_always || b.record_always,
        no_input: a.no_input || b.no_input,
        no_restart: a.no_restart || b.no_restart,
        no_record: a.no_record || b.no_record,
        no_resolution: a.no_resolution || b.no_resolution,
        no_audio: a.no_audio || b.no_audio,
        lock_on_end: a.lock_on_end || b.lock_on_end,
        idle_minutes: shorter_idle(a.idle_minutes, b.idle_minutes),
        no_microphone: a.no_microphone || b.no_microphone,
    }
}

/// The shorter of two idle limits in minutes (0: none).
fn shorter_idle(a: u32, b: u32) -> u32 {
    match (a, b) {
        (0, b) => b,
        (a, 0) => a,
        (a, b) => a.min(b),
    }
}

async fn signal_loop(
    ctx: &Arc<SessionContext>,
    opts: &ServerOptions,
    identity: &Arc<nuvira_transport::identity::Identity>,
    server: SocketAddr,
    expected: Option<Fingerprint>,
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
) -> Result<()> {
    use nuvira_proto::v1::{Accept, Reject, client_message, server_message};
    let device_name = nuvira_transport::device_name();
    let ws_url = opts
        .server_ws
        .clone()
        .unwrap_or_else(|| nuvira_transport::ws::default_url(&opts.server_name, server));
    let mut signal = nuvira_transport::signal::connect_auto(
        server,
        &ws_url,
        opts.transport,
        expected,
        identity,
        &nuvira_transport::signal::DeviceInfo {
            name: &device_name,
            reachable: true,
            alias: std::fs::read_to_string(ctx.access_dir.join(ALIAS_FILE)).ok().map(|a| a.trim().to_string()),
        },
    )
    .await?;
    let websocket = matches!(signal.transport, nuvira_transport::ServerTransport::WebSocket(_));
    if let nuvira_transport::ServerTransport::WebSocket(url) = &signal.transport {
        info!("connected to nuvira-server through the WebSocket {url}");
    }
    if expected.is_none() {
        nuvira_transport::signal::known_server::remember(&opts.server_name, &signal.server_fingerprint)?;
        info!(
            "nuvira-server fingerprint remembered: {}",
            nuvira_transport::fingerprint_hex(&signal.server_fingerprint)
        );
    }
    let id = signal.device_id.clone();
    info!("registered on {}: ID {} {} {}", opts.server_name, &id[..3], &id[3..6], &id[6..]);
    // The relay's address of the other IP family (IPv6 next to IPv4), for the
    // direct path of the sessions: resolved at every connection to the server.
    let relay_alt = match signal.transport {
        nuvira_transport::ServerTransport::Udp => nuvira_transport::other_family(&opts.server_name, server).await,
        nuvira_transport::ServerTransport::WebSocket(_) => None,
    };
    set_policy(ctx, signal.policy);
    if let Some(path) = &opts.info_file {
        // In automated tests the viewer reads ID and password from here.
        let password = ctx.password.lock().expect("password").clone();
        std::fs::write(path, format!("{id} {password}"))?;
    }
    let _ = ctx.events.send(AgentEvent::Online { device_id: id, websocket });
    if !signal.alias_error.is_empty() {
        warn!(error = signal.alias_error, "alias refused by the server");
    }
    keep_alias(&ctx.access_dir, &signal.alias, &signal.alias_error);
    let _ = ctx.events.send(AgentEvent::Alias { alias: signal.alias.clone(), error: signal.alias_error.clone() });

    let mut accepted: HashMap<String, Request> = HashMap::new();
    let (report_tx, mut reports) = mpsc::unbounded_channel::<nuvira_proto::v1::SessionReport>();
    // Light RMM: the inventory goes only to a server that asks for it.
    let send_inventory = signal.features.iter().any(|f| f == "pro.inventory");
    *ctx.features.lock().expect("features") = signal.features.clone();
    let mut inventory_tick = tokio::time::interval(inventory::INTERVAL);

    // A support request still open from the last connection (a network cut, the
    // server restarted): the server dropped it when this device went away, so it is
    // asked again with the same ticket; the server gives it back its place.
    let open_ticket = ctx.support_ticket.lock().expect("support").clone();
    let resend = ctx
        .support_request
        .lock()
        .expect("support")
        .clone()
        .filter(|r| open_ticket.as_deref() == Some(r.ticket.as_str()));
    if let Some(request) = resend {
        info!("support request still open: asked again after reconnecting");
        signal.tx.send(client_message::Msg::SupportRequest(request)).await?;
    }

    loop {
        let reply = tokio::select! {
            Some(report) = reports.recv() => Some(client_message::Msg::SessionReport(report)),
            _ = inventory_tick.tick(), if send_inventory => {
                match tokio::task::spawn_blocking(inventory::collect).await {
                    Ok(inv) => Some(client_message::Msg::Inventory(inv)),
                    Err(_) => None,
                }
            }
            msg = signal.rx.recv() => match msg? {
                Some(server_message::Msg::Policy(policy)) => {
                    set_policy(ctx, policy);
                    None
                }
                Some(server_message::Msg::Incoming(inc)) => {
                    let mut request = Request {
                        viewer_id: inc.viewer_id,
                        viewer_name: inc.viewer_name,
                        limits: inc.limits.unwrap_or_default(),
                        join: false,
                    };
                    info!("connection request from {} ({})", request.viewer_name, request.viewer_id);
                    if request.limits != Default::default() {
                        info!(limits = ?request.limits, "limits of this session");
                    }
                    let retry = ctx
                        .waiting
                        .lock()
                        .expect("waiting")
                        .as_ref()
                        .filter(|(viewer, _)| *viewer == request.viewer_id)
                        .map(|(_, cancel)| cancel.clone());
                    if let Some(cancel) = retry {
                        info!("same viewer again: its previous attempt is abandoned");
                        cancel.notify_one();
                        accepted.insert(inc.session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id: inc.session_id }))
                    } else if ctx.busy.load(Ordering::Relaxed) > 0 && shared::can_join(ctx) {
                        // Another viewer joins the session in progress, after the same
                        // access control.
                        info!("a session is in progress: this viewer would join it");
                        request.join = true;
                        accepted.insert(inc.session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id: inc.session_id }))
                    } else if ctx.busy.load(Ordering::Relaxed) > 0
                        || ctx.attempts.load(Ordering::Relaxed) >= MAX_PENDING_SESSIONS
                    {
                        if ctx.busy.load(Ordering::Relaxed) == 0 {
                            warn!("too many connections waiting for their password: refused as busy");
                        }
                        Some(client_message::Msg::Reject(Reject {
                            session_id: inc.session_id,
                            reason: "@agent.busy".into(),
                        }))
                    } else {
                        // Not busy until the viewer has authenticated (Gate::claim): one that
                        // never does cannot keep the others out. Confirmation by the person
                        // at the PC, if needed, comes after the password.
                        accepted.insert(inc.session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id: inc.session_id }))
                    }
                }
                Some(server_message::Msg::Route(route)) => {
                    let request = accepted.remove(&route.session_id);
                    let p2p_route = nuvira_transport::p2p::Route {
                        relay: server,
                        transport: signal.transport.clone(),
                        token: route.relay_token.as_slice().try_into().context("invalid relay token")?,
                        peer_public_key: route
                            .peer_public_key
                            .as_slice()
                            .try_into()
                            .context("invalid viewer key")?,
                        role: nuvira_transport::p2p::Role::Agent,
                        relay_alt,
                    };
                    let Some(request) = request else {
                        // Only routes of requests accepted here: their limits are known.
                        warn!("route of a session not accepted here: ignored");
                        continue;
                    };
                    let join = request.join;
                    if !join {
                        ctx.attempts.fetch_add(1, Ordering::Relaxed);
                    }
                    let (ctx, identity) = (ctx.clone(), identity.clone());
                    let session_id = route.session_id.clone();
                    let report_tx = report_tx.clone();
                    tokio::spawn(async move {
                        let mut outcome = Outcome::default();
                        let result = relayed_session(&ctx, &identity, &p2p_route, &request, &session_id, &mut outcome).await;
                        if let Err(e) = &result {
                            warn!("session ended: {e:#}");
                        }
                        // The counters are reset when the session is claimed: a guest's
                        // transfers are none (they stay the session's), and an attempt
                        // that never authenticated had none.
                        let counted = join || outcome.claimed;
                        let (files_in, files_out, bytes_in, bytes_out) = match counted {
                            true => (
                                stats::FILES_IN.load(Ordering::Relaxed),
                                stats::FILES_OUT.load(Ordering::Relaxed),
                                stats::BYTES_IN.load(Ordering::Relaxed),
                                stats::BYTES_OUT.load(Ordering::Relaxed),
                            ),
                            false => (0, 0, 0, 0),
                        };
                        let _ = report_tx.send(nuvira_proto::v1::SessionReport {
                            session_id: session_id.clone(),
                            duration_s: outcome.connected.map_or(0, |t| t.elapsed().as_secs() as u32),
                            direct: outcome.direct,
                            access: outcome.access.lock().expect("access").to_string(),
                            files_in,
                            files_out,
                            bytes_in,
                            bytes_out,
                            error: result
                                .as_ref()
                                .err()
                                .filter(|e| !closed_normally(e))
                                .map(|e| format!("{e:#}"))
                                .unwrap_or_default(),
                        });
                        if !join {
                            let ended = AgentEvent::SessionEnded { error: result.err().map(|e| format!("{e:#}")) };
                            if outcome.claimed {
                                lock_after(&ctx, &request, outcome.connected.is_some());
                            }
                            let _claims = ctx.claims.lock().unwrap_or_else(|e| e.into_inner());
                            ctx.attempts.fetch_sub(1, Ordering::Relaxed);
                            if outcome.claimed {
                                ctx.busy.fetch_sub(1, Ordering::Relaxed);
                                let _ = ctx.events.send(ended);
                            } else if ctx.busy.load(Ordering::Relaxed) == 0 {
                                // As before: nobody else is connected.
                                lock_after(&ctx, &request, outcome.connected.is_some());
                                let _ = ctx.events.send(ended);
                            }
                            // Otherwise an attempt that never authenticated ended while another
                            // viewer's session goes on: the app is not told (it would close that one).
                        }
                    });
                    None
                }
                Some(server_message::Msg::Wake(wake)) => {
                    tokio::task::spawn_blocking(move || inventory::wake(&wake.macs));
                    None
                }
                Some(server_message::Msg::AliasState(state)) => {
                    info!(alias = state.alias, error = state.error, "alias on the server");
                    keep_alias(&ctx.access_dir, &state.alias, &state.error);
                    let _ = ctx.events.send(AgentEvent::Alias { alias: state.alias, error: state.error });
                    None
                }
                Some(server_message::Msg::SupportStatus(status)) => {
                    if status.closed {
                        *ctx.support_ticket.lock().expect("support") = None;
                    }
                    if !status.error.is_empty() {
                        warn!(error = status.error, "support request refused");
                    }
                    let _ = ctx.events.send(AgentEvent::Support {
                        position: status.position,
                        taken_by: status.taken_by,
                        closed: status.closed,
                        error: status.error,
                        invited_by: status.invited_by,
                    });
                    None
                }
                Some(server_message::Msg::Error(e)) => {
                    warn!("nuvira-server: {}", e.message);
                    None
                }
                Some(_) => None,
                None => bail!("nuvira-server closed the connection"),
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
                Some(AgentCommand::RequestSupport { message, invite }) => {
                    let ticket = support_ticket();
                    *ctx.support_ticket.lock().expect("support") = Some(ticket.clone());
                    info!(invitation = !invite.is_empty(), "support requested");
                    let request = nuvira_proto::v1::SupportRequest {
                        ticket,
                        name: device_name.clone(),
                        message,
                        cancel: false,
                        invite,
                    };
                    *ctx.support_request.lock().expect("support") = Some(request.clone());
                    Some(client_message::Msg::SupportRequest(request))
                }
                Some(AgentCommand::AllowPrivacy { allow }) => {
                    if ctx.allow_privacy.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "privacy mode allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::AllowTunnel { allow }) => {
                    if ctx.allow_tunnel.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "tunnels allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::AllowPrint { allow }) => {
                    if ctx.allow_print.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "remote printing allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::AllowRecording { allow }) => {
                    if ctx.allow_recording.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "session recording allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::IdleMinutes { minutes }) => {
                    if ctx.idle_minutes.swap(minutes, Ordering::Relaxed) != minutes {
                        info!(minutes, "sessions end after a time without activity, by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::MuteMicrophone { muted }) => {
                    info!(muted, "technician's microphone muted by the person at the PC");
                    ctx.mic_muted.send_replace(muted);
                    None
                }
                Some(AgentCommand::SetAlias { alias }) => {
                    // Kept once the server takes it (AliasState).
                    let alias = alias.trim().to_lowercase();
                    Some(client_message::Msg::SetAlias(nuvira_proto::v1::SetAlias { alias }))
                }
                Some(AgentCommand::GetAccess) => {
                    let _ = ctx.events.send(AgentEvent::Access { view: access::View::of(&ctx.access_dir) });
                    None
                }
                Some(AgentCommand::LockOnEnd { on }) => {
                    if ctx.lock_on_end.swap(on, Ordering::Relaxed) != on {
                        info!(on, "lock at the end of sessions, by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::AllowTerminal { allow }) => {
                    if ctx.allow_terminal.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "remote terminal allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::Switch { name, on }) => {
                    let switches = ctx.switches.lock().expect("switches").clone();
                    match (switches, switches::bit(&name)) {
                        (Some(switches), Some(bit)) if switches.set(bit, on) => {
                            info!(name, on, "switched by the person at the PC for this session");
                        }
                        _ => warn!(name, "nothing to switch in this session"),
                    }
                    None
                }
                Some(AgentCommand::CancelSupport) => {
                    *ctx.support_ticket.lock().expect("support") = None;
                    info!("support request withdrawn");
                    Some(client_message::Msg::SupportRequest(nuvira_proto::v1::SupportRequest {
                        cancel: true,
                        ..Default::default()
                    }))
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
    identity: &nuvira_transport::identity::Identity,
    route: &nuvira_transport::p2p::Route,
    request: &Request,
    session_id: &str,
    outcome: &mut Outcome,
) -> Result<()> {
    let endpoint = nuvira_transport::p2p::agent_endpoint(identity, route).await?;
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
    let cancel = Arc::new(Notify::new());
    *ctx.waiting.lock().expect("waiting") = Some((request.viewer_id.clone(), cancel.clone()));
    let connected = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(20), rx.recv()) => {
            r.context("the viewer did not connect within 20 s").map(|c| c.context("endpoint closed"))
        }
        () = cancel.notified() => Err(anyhow::anyhow!("the viewer asked again before connecting")),
    };
    {
        let mut waiting = ctx.waiting.lock().expect("waiting");
        if waiting.as_ref().is_some_and(|(_, c)| Arc::ptr_eq(c, &cancel)) {
            *waiting = None;
        }
    }
    acceptor.abort();
    let (conn, streams) = connected??;
    let remote = nuvira_transport::p2p::canonical(conn.remote_address());
    let direct = remote != route.relay;
    outcome.connected = Some(std::time::Instant::now());
    outcome.direct = direct;
    info!(
        "viewer connected {} ({remote})",
        match (direct, remote.is_ipv6()) {
            (true, true) => "directly over IPv6",
            (true, false) => "directly over IPv4",
            (false, _) => "through the relay",
        }
    );
    let gate = Gate {
        ctx,
        session_id,
        request,
        direct,
        path: match (direct, &route.transport) {
            (true, _) => "direct",
            (false, nuvira_transport::ServerTransport::WebSocket(_)) => "websocket",
            (false, nuvira_transport::ServerTransport::Udp) => "relay",
        },
        access: &outcome.access,
        terminal_only: AtomicBool::new(false),
        profile_off: AtomicU8::new(0),
        claimed: AtomicBool::new(false),
    };
    let result = tokio::select! {
        r = async {
            match request.join {
                true => shared::guest_session(conn.clone(), streams, &gate).await,
                false => session(conn.clone(), streams, &ctx.config, Some(&gate), None).await,
            }
        } => r,
        _ = ctx.end_session.notified() => {
            info!("session ended by the person at the PC");
            Ok(())
        }
    };
    // The whole endpoint is closed: including a connection left over through the relay,
    // which would otherwise keep the agent "busy" until its timeout.
    endpoint.close(0u32.into(), &close_reason(&result));
    endpoint.wait_idle().await;
    if result.is_ok() {
        info!("session ended");
    }
    // The caller frees the computer, after locking it if wanted.
    outcome.claimed = gate.claimed.swap(false, Ordering::Relaxed);
    result
}

/// Whether the computer is locked when the session in progress ends: by the
/// computer's setting, the server's rules (with this session's limits) or the
/// viewer's request.
fn lock_wanted(ctx: &SessionContext, request: &Request) -> (bool, bool) {
    let forced = ctx.lock_on_end.load(Ordering::Relaxed)
        || merge_limits(*ctx.policy.lock().expect("policy"), &request.limits).lock_on_end;
    (forced || ctx.lock_asked.load(Ordering::Relaxed), forced)
}

/// At the end of a session (not a guest's): locks the computer when wanted, unless
/// the session was handed over (the guest carries on) or the computer is restarting.
fn lock_after(ctx: &SessionContext, request: &Request, connected: bool) {
    let (wanted, _) = lock_wanted(ctx, request);
    ctx.lock_asked.store(false, Ordering::Relaxed);
    if !wanted || !connected || !lock::available() {
        return;
    }
    if ctx.restarting.lock().expect("restarting").is_some() {
        info!("not locked: the computer is restarting");
        return;
    }
    if ctx.handover.lock().expect("handover").is_some() {
        info!("not locked: the session was handed over");
        return;
    }
    match lock::lock() {
        Ok(()) => info!("computer locked at the end of the session"),
        Err(e) => warn!("computer not locked at the end of the session: {e:#}"),
    }
}

/// Checks the viewer's `Auth`. When the computer also wants a two-factor code and
/// the viewer did not send one, a viewer that can answer is asked for it
/// (`CodeRequired`) and everything is checked again with it; an older viewer is
/// told to update. `Err` only when the connection broke.
async fn authorize_with_code(
    gate: &Gate<'_>,
    mut given: Auth,
    viewer_two_factor: bool,
    viewer_protocol: u32,
    ctrl_tx: &mut FramedSend,
    ctrl_rx: &mut FramedRecv,
) -> Result<Result<(), String>> {
    // Viewers that know it are told when the person at the PC is being asked.
    let tells_pending = viewer_protocol >= nuvira_proto::v1::ProtocolVersion::ConfirmationPending as u32;
    let verdict = gate.authorize(&given, tells_pending.then_some(&mut *ctrl_tx)).await;
    if !matches!(&verdict, Err(e) if e == CODE_REQUIRED) {
        return Ok(verdict);
    }
    if !viewer_two_factor {
        info!("two-factor code needed: the viewer cannot ask for it");
        // A key older viewers know: "protocol version not supported: update NuviraHub".
        return Ok(Err("@server.old_version".into()));
    }
    info!("password right: asking the viewer for the two-factor code");
    ctrl_tx.send(&control(Msg::CodeRequired(nuvira_proto::v1::CodeRequired {}))).await?;
    let code = match tokio::time::timeout(Duration::from_secs(180), ctrl_rx.recv::<ControlMessage>()).await {
        Ok(Ok(Some(ControlMessage { msg: Some(Msg::Auth(auth)) }))) => auth.code,
        Ok(Err(e)) => return Err(e),
        _ => return Ok(Err("@agent.no_code".into())),
    };
    given.code = code;
    Ok(gate.authorize(&given, tells_pending.then_some(ctrl_tx)).await)
}

/// See [`SessionContext::restarting`].
const RESTART_GIVEN_UP: Duration = Duration::from_secs(180);

/// Access control of a session arriving from the server: permanent password
/// (unattended access) or temporary password, with confirmation by the person
/// at the PC if required.
struct Gate<'a> {
    ctx: &'a SessionContext,
    session_id: &'a str,
    request: &'a Request,
    direct: bool,
    /// `direct`, `relay` or `websocket`, for the statistics log.
    path: &'static str,
    /// How access went, for the report to the server.
    access: &'a std::sync::Mutex<&'static str>,
    /// The viewer asked for a terminal only (set from its Hello).
    terminal_only: AtomicBool,
    /// Switches off because of the profile whose password was given ([`access`]).
    profile_off: AtomicU8,
    /// This viewer's session is the one in progress ([`Gate::claim`]).
    claimed: AtomicBool,
}

/// A gate dropped with its claim still on (its session over) frees the computer.
impl Drop for Gate<'_> {
    fn drop(&mut self) {
        if self.claimed.swap(false, Ordering::Relaxed) {
            let _claims = self.ctx.claims.lock().unwrap_or_else(|e| e.into_inner());
            self.ctx.busy.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Gate<'_> {
    /// From here this viewer's session is the one in progress (not for a guest):
    /// the computer is busy until it ends. Taken once the viewer's credentials are
    /// right, before the person at the PC is asked, so that they are asked about
    /// one viewer at a time. `Err` when another viewer authenticated first while
    /// both were connecting.
    fn claim(&self) -> Result<(), String> {
        if self.request.join || self.claimed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _claims = self.ctx.claims.lock().unwrap_or_else(|e| e.into_inner());
        if self.ctx.busy.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            info!("another viewer's session started first: refused as busy");
            return Err("@agent.busy".into());
        }
        self.claimed.store(true, Ordering::Relaxed);
        // The transfer counters of the report are this session's.
        stats::reset();
        Ok(())
    }

    /// `Err` with the reason to show to the viewer.
    async fn authorize(&self, auth: &Auth, pending: Option<&mut FramedSend>) -> Result<(), String> {
        let ctx = self.ctx;
        if ctx.restarting.lock().expect("restarting").is_some_and(|t| t.elapsed() < RESTART_GIVEN_UP) {
            return Err("@agent.restarting".into());
        }
        let given = auth.password.as_str();
        let confirm_always = self.policy().confirm_always;
        let set = |access: &'static str| *self.access.lock().expect("access") = access;
        let rules = access::AccessRules::load(&ctx.access_dir);
        if let Err(e) = &rules {
            warn!("rules of unattended access not read: {e}");
        }
        let viewer = self.request.viewer_id.as_str();
        // Who may connect, also with the temporary password when the rules say so.
        let attended_allowed = rules.as_ref().map_or(true, |r| r.allows(viewer, true));
        if (!auth.handover.is_empty() || !auth.support_ticket.is_empty()) && !attended_allowed {
            info!(viewer, "this computer is not among those allowed to connect");
            set("not_allowed");
            return Err(NOT_ALLOWED.into());
        }
        // The guest a session was handed over to, with its token: the person at the
        // PC accepted it already (not asked again).
        if !auth.handover.is_empty() {
            let mut slot = ctx.handover.lock().expect("handover");
            let valid = !self.request.join
                && slot.as_ref().is_some_and(|h| {
                    h.at.elapsed() < shared::HANDOVER_VALID
                        && h.viewer_id == self.request.viewer_id
                        && same_secret(&auth.handover, &h.token)
                });
            if valid {
                *slot = None;
                drop(slot);
                info!("access with the token of a session handed over");
                set("handover");
                return self.started();
            }
            set("wrong_password");
            return Err(WRONG_PASSWORD.into());
        }
        // The ticket of this device's support request, used once: it replaces the
        // password, never the consent of the person at the PC.
        let support = !auth.support_ticket.is_empty() && {
            let mut open = ctx.support_ticket.lock().expect("support");
            let matches = open.as_deref().is_some_and(|t| same_secret(&auth.support_ticket, t));
            if matches {
                *open = None;
            }
            matches
        };
        if support {
            info!("access with the support request's ticket: asking the person at the PC");
            set("support");
        } else if !auth.support_ticket.is_empty() {
            set("wrong_password");
            return Err("@agent.support_ticket_invalid".into());
        } else if let Some(profile) = self.permanent(given, rules.as_ref().ok()) {
            // A permanent password: the rules hold, and an unreadable file refuses it.
            let Ok(rules) = &rules else {
                set("not_allowed");
                return Err("@agent.access_rules_invalid".into());
            };
            if !rules.allows(viewer, false) {
                info!(viewer, "this computer is not among those allowed to connect");
                set("not_allowed");
                return Err(NOT_ALLOWED.into());
            }
            if let Some(secret) = &rules.totp {
                self.check_code(secret, &auth.code)?;
            }
            match profile {
                Some(profile) => {
                    let off = profile.off.iter().filter_map(|n| switches::bit(n)).fold(0, |a, b| a | b);
                    let (name, off_names) = (&profile.name, &profile.off);
                    info!(profile = name, off = ?off_names, "access with the permanent password of a profile");
                    self.profile_off.store(off, Ordering::Relaxed);
                    set("profile");
                }
                None => {
                    info!("access with the permanent password");
                    set("permanent");
                }
            }
            if !confirm_always {
                return self.started();
            }
            info!("the server's rules require confirmation also with the permanent password");
        } else {
            let temporary = ctx.password.lock().expect("password").clone();
            if !same_secret(given, &temporary) {
                set("wrong_password");
                return Err(WRONG_PASSWORD.into());
            }
            if !attended_allowed {
                info!(viewer, "this computer is not among those allowed to connect");
                set("not_allowed");
                return Err(NOT_ALLOWED.into());
            }
            set("temporary");
            if !ctx.confirm && !confirm_always {
                return self.started();
            }
        }
        // The credentials are right: the session is this viewer's from here.
        self.claim()?;
        if let Some(ctrl_tx) = pending {
            let waiting =
                nuvira_proto::v1::ConfirmationPending { timeout_s: CONFIRM_TIMEOUT.as_secs() as u32 };
            let _ = ctrl_tx.send(&control(Msg::ConfirmationPending(waiting))).await;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.pending.lock().expect("requests").insert(self.session_id.to_string(), tx);
        let _ = ctx.events.send(AgentEvent::Incoming {
            session_id: self.session_id.to_string(),
            viewer_id: self.request.viewer_id.clone(),
            viewer_name: self.request.viewer_name.clone(),
            support,
        });
        let answer = tokio::time::timeout(CONFIRM_TIMEOUT, rx).await;
        ctx.pending.lock().expect("requests").remove(self.session_id);
        let _ = ctx.events.send(AgentEvent::IncomingClosed { session_id: self.session_id.to_string() });
        match answer {
            Ok(Ok(true)) => {
                set("confirmed");
                self.started()
            }
            // Rejected by the person at the PC, or no app open to ask them.
            Ok(Ok(false)) => {
                info!("connection not accepted on the device");
                set("not_accepted");
                Err("@agent.not_accepted".into())
            }
            _ => {
                info!("no answer to the connection request: rejected");
                set("no_answer");
                Err("@agent.no_answer".into())
            }
        }
    }

    /// `given` is a permanent password: `Some(None)` the main one, `Some(Some(_))`
    /// a profile's (only where there is a main one: unattended access).
    fn permanent(&self, given: &str, rules: Option<&access::AccessRules>) -> Option<Option<access::Profile>> {
        let main = self.ctx.permanent.as_deref()?;
        if given.is_empty() {
            return None;
        }
        if nuvira_secret::verify(given, main) {
            return Some(None);
        }
        rules?.profile(given).cloned().map(Some)
    }

    /// The two-factor code: each works once, a few wrong ones lock it for a while.
    fn check_code(&self, secret: &str, code: &str) -> Result<(), String> {
        let ctx = self.ctx;
        let mut wrong = ctx.wrong_codes.lock().expect("codes");
        wrong.retain(|t| t.elapsed() < CODE_WINDOW);
        if wrong.len() >= CODE_MAX_WRONG {
            warn!("too many wrong two-factor codes: permanent passwords refused for a while");
            *self.access.lock().expect("access") = "code_locked";
            return Err("@agent.code_locked".into());
        }
        if code.trim().is_empty() {
            return Err(CODE_REQUIRED.into());
        }
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        match nuvira_secret::totp_verify(secret, code, now) {
            Some(step) if ctx.last_code_step.fetch_max(step, Ordering::Relaxed) < step => Ok(()),
            found => {
                if found.is_some() {
                    info!("two-factor code already used");
                }
                wrong.push(Instant::now());
                *self.access.lock().expect("access") = "wrong_code";
                Err("@agent.code_wrong".into())
            }
        }
    }

    /// Rules of the server now, with the limits of this session.
    fn policy(&self) -> nuvira_proto::v1::Policy {
        merge_limits(*self.ctx.policy.lock().expect("policy"), &self.request.limits)
    }

    fn started(&self) -> Result<(), String> {
        // A guest is told to the app with the list of guests (AgentEvent::Guests).
        if self.request.join {
            return Ok(());
        }
        self.claim()?;
        let _ = self.ctx.events.send(AgentEvent::SessionStarted {
            viewer_id: self.request.viewer_id.clone(),
            viewer_name: self.request.viewer_name.clone(),
            direct: self.direct,
            terminal: self.terminal_only.load(Ordering::Relaxed),
        });
        Ok(())
    }
}

/// Switch sides: the technician's computer shows its screen to the person at the
/// controlled one, inside the session already open (`nuvira_transport::nested`).
/// The one viewer that connects must give `password`; it gets the picture, the
/// keyboard and the mouse, nothing else (`reverse_policy`).
pub struct Reverse {
    pub password: String,
}

/// What the person at the controlled computer can do on the technician's: see and
/// use the screen; no files, clipboard, terminal, tunnels, printing, recording,
/// privacy mode, restart, resolution or sound.
fn reverse_policy() -> nuvira_proto::v1::Policy {
    nuvira_proto::v1::Policy {
        no_files: true,
        no_clipboard: true,
        no_privacy: true,
        no_terminal: true,
        no_tunnel: true,
        no_print: true,
        no_record: true,
        no_restart: true,
        no_resolution: true,
        no_audio: true,
        ..Default::default()
    }
}

/// Switch sides, at the technician's: serves the one session of `endpoint` (on a
/// stream of the session in progress), with `reverse`'s password, until it ends.
pub async fn serve_reverse(endpoint: quinn::Endpoint, config: AgentConfig, reverse: Reverse) -> Result<()> {
    let incoming = tokio::time::timeout(Duration::from_secs(60), endpoint.accept())
        .await
        .context("the other computer did not open the window within 60 s")?
        .context("closed before the other computer connected")?;
    let conn = incoming.await.context("connection from the other computer")?;
    info!("switch sides: the other computer sees this screen");
    // Diagnosis (debug): the inner connection's state every 5 s.
    let watched = conn.clone();
    let stats = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let s = watched.stats();
            tracing::debug!(
                rtt_ms = s.path.rtt.as_millis() as u64,
                cwnd = s.path.cwnd,
                lost = s.path.lost_packets,
                sent = s.path.sent_packets,
                udp_tx = s.udp_tx.bytes,
                udp_rx = s.udp_rx.bytes,
                tx_blocked = s.frame_tx.stream_data_blocked,
                tx_data_blocked = s.frame_tx.data_blocked,
                rx_max_stream_data = s.frame_rx.max_stream_data,
                rx_max_data = s.frame_rx.max_data,
                "switch sides: inner connection"
            );
        }
    });
    let result = match control_channel(&conn).await {
        Ok(streams) => session(conn.clone(), streams, &config, None, Some(&reverse)).await,
        Err(e) => Err(e),
    };
    stats.abort();
    conn.close(0u32.into(), &close_reason(&result));
    endpoint.close(0u32.into(), b"");
    info!("switch sides: ended");
    // The window closed by the person there is how it normally ends.
    match result {
        Err(e) if closed_normally(&e) => Ok(()),
        r => r,
    }
}

/// The viewer's Hello, the first message on the control stream (logged as `what`),
/// of a protocol version this agent still serves. Shared by [`run_session`] and
/// [`shared::guest_session`].
async fn read_hello(ctrl_rx: &mut FramedRecv, what: &str) -> Result<Hello> {
    let first = tokio::time::timeout(HANDSHAKE_READ, ctrl_rx.recv::<ControlMessage>())
        .await
        .map_err(|_| anyhow!("the viewer did not introduce itself within {} s", HANDSHAKE_READ.as_secs()))?;
    let Some(ControlMessage { msg: Some(Msg::Hello(hello)) }) = first? else {
        bail!("the viewer did not introduce itself");
    };
    info!(device = hello.device_name, os = hello.os, version = hello.app_version, "{what}");
    if hello.protocol_version < nuvira_proto::MIN_PROTOCOL_VERSION {
        bail!("viewer protocol version not supported: {}", hello.protocol_version);
    }
    Ok(hello)
}

/// The viewer's first `Auth`, right after the agent's Hello (within
/// [`HANDSHAKE_READ`]: a viewer that never sends it is let go).
async fn read_auth(ctrl_rx: &mut FramedRecv) -> Result<Auth> {
    let auth = tokio::time::timeout(HANDSHAKE_READ, ctrl_rx.recv::<ControlMessage>())
        .await
        .map_err(|_| anyhow!("the viewer did not send the password within {} s", HANDSHAKE_READ.as_secs()))?;
    match auth? {
        Some(ControlMessage { msg: Some(Msg::Auth(auth)) }) => Ok(auth),
        _ => bail!("the viewer did not send the password"),
    }
}

/// The control stream the viewer opens first, within [`HANDSHAKE_READ`]: LAN mode
/// and switch sides serve one connection at a time, and one that never opens it
/// would keep the next viewer out.
async fn control_channel(conn: &Connection) -> Result<(quinn::SendStream, quinn::RecvStream)> {
    tokio::time::timeout(HANDSHAKE_READ, conn.accept_bi())
        .await
        .map_err(|_| anyhow!("the viewer did not open the control channel within {} s", HANDSHAKE_READ.as_secs()))?
        .context("control channel")
}

/// [`run_session`], ending its statistics log (if any) with the reason it ended.
async fn session(
    conn: Connection,
    streams: (quinn::SendStream, quinn::RecvStream),
    config: &AgentConfig,
    gate: Option<&Gate<'_>>,
    reverse: Option<&Reverse>,
) -> Result<()> {
    let recorder = Mutex::new(None);
    let result = run_session(conn, streams, config, gate, reverse, &recorder).await;
    if let Some(r) = recorder.into_inner().ok().flatten() {
        r.finish(nuvira_transport::stats::exit_reason(&result));
    }
    result
}

/// A session with a viewer already connected. With `gate` the viewer must
/// authenticate (and possibly be confirmed) before receiving any image; with
/// `reverse` (switch sides) it must give its password and gets the screen only.
async fn run_session(
    conn: Connection,
    (send, recv): (quinn::SendStream, quinn::RecvStream),
    config: &AgentConfig,
    gate: Option<&Gate<'_>>,
    reverse: Option<&Reverse>,
    recorder: &Mutex<Option<nuvira_transport::stats::Recorder>>,
) -> Result<()> {
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);
    // Rules of the server at the start of the session, with its limits (none without
    // a server; the screen only when the sides are switched).
    let policy = gate.map(Gate::policy).or(reverse.map(|_| reverse_policy())).unwrap_or_default();
    // What the person at the PC switches off during the session.
    let (switches, off) = switches::channel();

    let hello = read_hello(&mut ctrl_rx, "viewer").await?;
    let viewer_protocol = hello.protocol_version;
    let terminal_only = hello.terminal_only;
    // The server's rules want every session recorded: by the viewer, which must be able to.
    let recording_required = policy.record_always && !terminal_only;
    let viewer_records = hello.recording;
    let viewer_two_factor = hello.two_factor;
    // The wallpaper can be hidden in sessions through a server, with a screen.
    let wallpaper_state = gate
        .filter(|g| !g.request.join && !terminal_only && config.synthetic.is_none())
        .filter(|_| nuvira_capture::wallpaper::available())
        .map(|g| g.ctx.access_dir.join(WALLPAPER_STATE));
    let _wallpaper = RestoreWallpaper(wallpaper_state.clone());
    // Minutes without activity after which the session ends: the computer's
    // setting and the server's rules, the shorter (0: never). Not for guests nor
    // terminal-only sessions.
    let idle_limit = match gate {
        Some(g) if !g.request.join && !terminal_only => {
            shorter_idle(g.ctx.idle_minutes.load(Ordering::Relaxed), policy.idle_minutes)
        }
        _ => 0,
    };
    if let Some(gate) = gate {
        gate.terminal_only.store(terminal_only, Ordering::Relaxed);
    }
    // Why a terminal is refused, if it is: checked at every terminal opened.
    let terminal_off = off.clone();
    let terminal_refused = move || -> Option<&'static str> {
        if config.view_only || config.synthetic.is_some() {
            Some("@agent.terminal_unavailable")
        } else if reverse.is_some() || gate.is_some_and(|g| g.policy().no_terminal) {
            Some("@agent.terminal_disabled")
        } else if gate.is_some_and(|g| !g.ctx.allow_terminal.load(Ordering::Relaxed)) {
            Some("@agent.terminal_not_allowed")
        } else if !terminal_off.is_on(switches::TERMINAL) {
            Some("@agent.terminal_switched_off")
        } else {
            None
        }
    };
    // Tunnels: only with a Pro server that offers them (never in a LAN session).
    let tunnel_off = off.clone();
    let tunnel_refused = move || -> Option<&'static str> {
        let Some(g) = gate else { return Some("@agent.tunnel_unavailable") };
        let licensed = g.ctx.features.lock().expect("features").iter().any(|f| f == "pro.tunnel");
        if config.view_only || config.synthetic.is_some() || !licensed {
            Some("@agent.tunnel_unavailable")
        } else if g.policy().no_tunnel {
            Some("@agent.tunnel_disabled")
        } else if !g.ctx.allow_tunnel.load(Ordering::Relaxed) {
            Some("@agent.tunnel_not_allowed")
        } else if !tunnel_off.is_on(switches::TUNNEL) {
            Some("@agent.tunnel_switched_off")
        } else {
            None
        }
    };
    // Privacy mode allowed by the person at the PC (always without the app: LAN tests).
    let privacy_allowed = || gate.is_none_or(|g| g.ctx.allow_privacy.load(Ordering::Relaxed));
    // Restart of the computer: only where the agent comes back by itself afterwards.
    let restart_available = gate.is_some_and(|g| g.ctx.unattended)
        && !policy.no_restart
        && !config.view_only
        && config.synthetic.is_none()
        && restart::available();
    // The technician's microphone: played here through a server session, with a
    // sound output, when the rules allow it.
    let mic_offered = gate.is_some_and(|g| !g.request.join)
        && !terminal_only
        && hello.microphone
        && !policy.no_microphone
        && config.synthetic.is_none()
        && nuvira_audio::player::available();
    // Safe mode with networking: Windows, the service's agent (it comes back).
    let safe_mode_offered = restart_available
        && !terminal_only
        && gate.is_some_and(|g| restart::safe_mode_available(g.ctx.unattended));
    // Remote printing, to a viewer that prints: allowed by the person at the PC (always
    // without the app: LAN tests) and by the server's rules, during the session only.
    let print_off = off.clone();
    let print_allowed = move || {
        print_off.is_on(switches::PRINT) && gate.is_none_or(|g| g.ctx.allow_print.load(Ordering::Relaxed))
    };
    // Recording of the session by the viewer, allowed by the person at the PC (always
    // without the app: LAN tests), who is told when it starts.
    // The server's rules come first (every session recorded, the person is told anyway).
    let recording_allowed = || {
        recording_required
            || (!policy.no_record && gate.is_none_or(|g| g.ctx.allow_recording.load(Ordering::Relaxed)))
    };
    let printer = match hello.print
        && !terminal_only
        && !config.view_only
        && config.synthetic.is_none()
        && !policy.no_print
        && gate.is_none_or(|g| g.ctx.allow_print.load(Ordering::Relaxed))
    {
        true => printer::listen(),
        false => None,
    };
    // Adapting the screen's resolution to the viewer's window.
    let resolution_offered = !terminal_only
        && !config.view_only
        && config.synthetic.is_none()
        && !policy.no_resolution
        && nuvira_capture::resolution::supported();
    // Video as datagrams with error correction, when the viewer asks for it.
    let video_datagrams = hello.video_datagrams && conn.max_datagram_size().is_some();
    // The pointer apart from the picture, to viewers that draw it (Linux, and not
    // with the DMA-BUF capture, which would lose it from recordings).
    let cursor_cache = hello.cursor_cache;
    // The viewer offers files copied here on its clipboard, and serves its own.
    let viewer_files = hello.clipboard_files;
    let cursor_offered = hello.cursor
        && !terminal_only
        && !recording_required
        && config.synthetic.is_none()
        && pointer::possible(config.plan);
    pointer::set_wanted(cursor_offered);
    let _pointer_reset = pointer::Reset;
    // The computer's sound, as datagrams, to viewers that play it.
    let audio_offered = !terminal_only
        && hello.audio
        && !policy.no_audio
        && config.synthetic.is_none()
        && conn.max_datagram_size().is_some()
        && nuvira_audio::capture::available();
    let hello = Hello {
        password_required: gate.is_some() || reverse.is_some(),
        file_manager: !config.view_only && config.synthetic.is_none() && !policy.no_files,
        video_datagrams,
        privacy_mode: !config.view_only
            && config.synthetic.is_none()
            && !policy.no_privacy
            && privacy_allowed()
            && nuvira_input::privacy::available(),
        terminal: terminal_refused().is_none(),
        tunnel: tunnel_refused().is_none(),
        restart: restart_available && !terminal_only,
        restart_users: restart_available && !terminal_only && restart::knows_users(),
        print: printer.is_some(),
        recording: !terminal_only && recording_allowed(),
        recording_required,
        resolution: resolution_offered,
        // Characters as they are (InputEvent.text), with the keyboard.
        text: !terminal_only && !config.view_only,
        // Clipboard.paste: with the clipboard and the keyboard.
        paste: !terminal_only && !config.view_only && config.synthetic.is_none() && !policy.no_clipboard,
        audio: audio_offered,
        cursor: cursor_offered,
        cursor_cache: cursor_offered && cursor_cache,
        // Others can join only sessions through a server: then it can be handed over.
        handover: gate.is_some() && !terminal_only,
        // Switch sides: a window with the viewer's screen, for the person signed in
        // (not inside a switched session, nor without a screen).
        switch_sides: gate.is_some()
            && reverse.is_none()
            && !terminal_only
            && config.synthetic.is_none()
            && reverse::viewer_available(),
        lock_on_end: gate.is_some() && lock::available(),
        idle_limit: idle_limit > 0,
        wallpaper: wallpaper_state.is_some(),
        restart_safe_mode: safe_mode_offered,
        microphone: mic_offered,
        // Files through the clipboard: with the clipboard and the files.
        clipboard_files: !terminal_only
            && !config.view_only
            && config.synthetic.is_none()
            && !policy.no_clipboard
            && !policy.no_files,
        ..local_hello()
    };
    // What the person at the PC can switch off: what this session has.
    let mut offered = 0;
    for (bit, has) in [
        (switches::INPUT, !terminal_only && !config.view_only && config.synthetic.is_none() && !policy.no_input),
        (switches::CLIPBOARD, !terminal_only && !config.view_only && config.synthetic.is_none() && !policy.no_clipboard),
        (switches::FILES, !terminal_only && hello.file_manager),
        (switches::TERMINAL, hello.terminal),
        (switches::TUNNEL, hello.tunnel),
        (switches::PRINT, hello.print),
        (switches::RESOLUTION, hello.resolution),
        (switches::AUDIO, hello.audio),
    ] {
        if has {
            offered |= bit;
        }
    }
    switches.set_offered(offered);
    let clipboard_files_offered = hello.clipboard_files;
    // Switch sides: what the window reports goes to the viewer on the control channel.
    let switch_offered = hello.switch_sides;
    let (switch_tx, mut switch_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::SwitchSides>();
    ctrl_tx.send(&control(Msg::Hello(hello))).await?;
    if let Some(gate) = gate {
        let given = read_auth(&mut ctrl_rx).await?;
        // A viewer that does not record cannot work here (older ones, the web client).
        if recording_required && !viewer_records {
            let message = "@agent.recording_required".to_string();
            ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok: false, message: message.clone() }))).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            bail!("{message}");
        }
        let (tx, rx) = (&mut ctrl_tx, &mut ctrl_rx);
        let verdict = authorize_with_code(gate, given, viewer_two_factor, viewer_protocol, tx, rx).await?;
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
        *gate.ctx.switches.lock().expect("switches") = Some(switches.clone());
        gate.ctx.lock_asked.store(false, Ordering::Relaxed);
        gate.ctx.mic_muted.send_replace(false);
        // The profile of the password given: what it does not allow is off for the
        // whole session, and the person at the PC cannot switch it back on.
        let profile_off = gate.profile_off.load(Ordering::Relaxed) & offered;
        if profile_off != 0 {
            for (bit, _) in switches::ALL {
                if profile_off & bit != 0 {
                    switches.set(bit, false);
                }
            }
            offered &= !profile_off;
            switches.set_offered(offered);
        }
    } else if let Some(reverse) = reverse {
        let given = read_auth(&mut ctrl_rx).await?;
        let ok = same_secret(&given.password, &reverse.password);
        let message = if ok { String::new() } else { WRONG_PASSWORD.to_string() };
        ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok, message }))).await?;
        if !ok {
            tokio::time::sleep(Duration::from_millis(2000)).await;
            bail!("{WRONG_PASSWORD}");
        }
    }
    // Cleared at the end, however the session ends.
    let _clear = ClearSwitches(gate.map(|g| g.ctx));
    // The screens this session changed are put back however it ends.
    let owner = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let _screens = RestoreScreens(owner);
    let tell_permissions = |off: u8| {
        if let Some(g) = gate {
            let _ = g.ctx.events.send(AgentEvent::Permissions {
                offered: switches::names(offered),
                off: switches::names(off & offered),
            });
        }
        control(Msg::Permissions(permissions(offered, off)))
    };

    // Terminal only: no capture, no video, nothing on the screen.
    if terminal_only {
        ctrl_tx.send(&tell_permissions(0)).await?;
        let refused = (&terminal_refused, &tunnel_refused);
        return terminal_session(&conn, ctrl_tx, ctrl_rx, gate, refused, off, &tell_permissions).await;
    }

    let keyframe = Arc::new(AtomicBool::new(true));
    let max_bps = config.bitrate_kbps * 1000;
    // With error correction the parity shards travel too: the adaptive bitrate is what
    // goes on the network (parity included) and the encoder gets its share of it,
    // otherwise parity pushes a saturated link past its rate.
    // Parity per data shard, in thousandths: it follows the shards the viewer reports
    // lost (NUVIRA_FEC_RATIO fixes it, for tests).
    let fixed_ratio: Option<f64> = std::env::var("NUVIRA_FEC_RATIO").ok().and_then(|r| r.parse().ok());
    let fec_permille = Arc::new(AtomicU32::new(match video_datagrams {
        true => (fixed_ratio.unwrap_or(FEC_START) * 1000.0) as u32,
        false => 0,
    }));
    // What goes on the network for each byte of video, in thousandths: measured, since
    // small frames get at least two parity shards and weigh more than the ratio says.
    let overhead_permille = Arc::new(AtomicU32::new(1000 + fec_permille.load(Ordering::Relaxed)));
    let encoder_bps = {
        let overhead_permille = overhead_permille.clone();
        move |bps: u32| (bps as f64 * 1000.0 / overhead_permille.load(Ordering::Relaxed) as f64) as u32
    };
    // Share of shards lost, smoothed over the last reports.
    let mut shard_loss = 0.0f64;
    // Start from half the maximum: the unknown network might not take more.
    let mut abr = (!config.fixed_bitrate).then(|| abr::Abr::new(max_bps / 2, MIN_BITRATE, max_bps));
    let bitrate = Arc::new(AtomicU32::new(encoder_bps(abr.as_ref().map_or(max_bps, |a| a.bitrate()))));
    let fps = Arc::new(AtomicU32::new(config.fps));
    // Area the viewer draws the image in (0 = full size until it says otherwise).
    let view = Arc::new(AtomicU64::new(0));
    let session_start = Instant::now();
    // Last activity from the viewer (keyboard and mouse, files, its requests), in
    // ms since the start of the session: for the idle limit.
    let activity = Arc::new(AtomicU64::new(0));
    let mark_activity = {
        let activity = activity.clone();
        move || activity.store(session_start.elapsed().as_millis() as u64, Ordering::Relaxed)
    };
    // First frame sent after the last feedback (ms from the start, 0 = none): tells
    // whether feedback is missing. Measuring from the last frame instead would make a
    // still screen (one frame every few seconds) look like a stuck network.
    let unanswered_since_ms = Arc::new(AtomicU64::new(0));
    // Video bytes sent since the start: the adaptive bitrate needs the rate actually used.
    let sent_bytes = Arc::new(AtomicU64::new(0));
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    // The statistics log, when NUVIRA_STATS asks for it (read at every session).
    let stats_config = nuvira_transport::stats::Config::from_env();
    let stats = stats_config.as_ref().map(|_| Arc::new(session_stats::AgentStats::default()));
    if let (Some(s), Some(abr)) = (&stats, abr.as_ref()) {
        s.abr_target_bps.store(abr.bitrate(), Ordering::Relaxed);
    }
    let source = VideoSource {
        config: config.clone(),
        frames: frame_tx,
        keyframe: keyframe.clone(),
        bitrate: bitrate.clone(),
        fps: fps.clone(),
        view: view.clone(),
        stats: stats.clone(),
    };
    let (mut display, first) = source.start(config.display).await?;
    ctrl_tx.send(&video_format(&display)).await?;
    if let Some(msg) = displays_message(config, &display) {
        ctrl_tx.send(&msg).await?;
    }
    // After the video format: older viewers want it right after AuthResult.
    ctrl_tx.send(&tell_permissions(off.mask())).await?;
    // Whether the computer will be locked at the end (after VideoFormat: the viewer
    // reads that first).
    if let Some(g) = gate.filter(|g| !g.request.join && lock::available()) {
        let (on, forced) = lock_wanted(g.ctx, g.request);
        ctrl_tx.send(&control(Msg::LockOnEndState(nuvira_proto::v1::LockOnEndState { on, forced }))).await?;
    }
    // The statistics log starts once the viewer has what it waits for first.
    if let (Some(cfg), Some(s)) = (&stats_config, &stats) {
        let src = session_stats::AgentSource {
            stats: s.clone(),
            conn: conn.clone(),
            plan: format!("{:?}", config.plan),
            fps_max: fps.clone(),
            sent_bytes: sent_bytes.clone(),
            fec_permille: video_datagrams.then(|| fec_permille.clone()),
            adaptive: abr.is_some(),
            last: Default::default(),
        };
        let session =
            gate.map_or_else(nuvira_transport::stats::local_session_id, |g| g.session_id.to_string());
        let path = gate.map_or(if reverse.is_some() { "nested" } else { "direct" }, |g| g.path);
        *recorder.lock().expect("stats recorder") =
            Some(nuvira_transport::stats::Recorder::start(cfg, "agent", &session, path, Box::new(src)));
    }
    // Capture in progress: the control channel replaces it when the screen changes.
    let pipeline = std::sync::Mutex::new(Some(first));
    let (screen_tx, mut screen_rx) = tokio::sync::watch::channel(screen_rect(&display));
    // Other viewers can join this session (sessions from a server only): the same
    // picture, and their keyboard and mouse into this session's input.
    let (guest_input_tx, mut guest_input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let shared = gate.map(|g| {
        let s = Arc::new(shared::Shared::new(
            keyframe.clone(),
            display.clone(),
            guest_input_tx,
            offered & switches::INPUT != 0,
            g.ctx.events.clone(),
            audio_offered,
            cursor_offered,
        ));
        *g.ctx.shared.lock().expect("shared") = Some(s.clone());
        s
    });
    let _end_shared = shared::EndShared(gate.map(|g| g.ctx));

    // Chat: messages from the person at the PC come from the app through the context.
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<String>();
    let events = gate.map(|g| {
        *g.ctx.chat.lock().expect("chat") = Some(chat_tx.clone());
        g.ctx.events.clone()
    });
    // Outcome of the received files, to report to the viewer.
    let (file_tx, mut file_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::FileReceived>();
    // Answers of the file manager, computed off the control loop.
    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::FsResponse>();
    // Shared clipboard: what is copied here goes to the viewer, what is copied there arrives here.
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<nuvira_clipboard::Content>();
    // Clipboard.paste: the keys go to the input once the text is in the clipboard.
    let (paste_tx, mut paste_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::Paste>();
    let clipboard = if offered & switches::CLIPBOARD == 0 {
        None
    } else {
        // As SYSTEM, what cannot be read here is read by a helper started as the person.
        #[cfg(windows)]
        let fallback: Option<nuvira_clipboard::Fallback> =
            nuvira_clipboard::running_as_system().then(|| Box::new(clip_user::read_as_user) as nuvira_clipboard::Fallback);
        #[cfg(not(windows))]
        let fallback = None;
        match nuvira_clipboard::spawn_sync_with(move |content| clip_tx.send(content).is_ok(), fallback) {
            Ok(tx) => Some(tx),
            Err(e) => {
                warn!("clipboard not shared: {e:#}");
                None
            }
        }
    };
    // Images arrive on their own streams, received with the files.
    let clipboard_in = clipboard.clone();
    // Files copied through the clipboard, both ways (the list at once, the
    // contents when pasted): with the clipboard and the files, here and there.
    let files_by_clipboard = viewer_files && clipboard_files_offered && clipboard.is_some();
    let (sharing, mut paste_rx_files) = nuvira_clipboard::Sharing::new();
    let (listed_tx, mut listed_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::ClipboardFiles>();
    let files_allowed = {
        let off = off.clone();
        move || files_by_clipboard && off.is_on(switches::CLIPBOARD) && off.is_on(switches::FILES)
    };

    // With datagrams there is no video stream (QUIC shows a stream to the other side
    // only once something is written on it: the viewer would wait for it forever).
    let mut video_tx = match video_datagrams {
        true => None,
        false => Some(FramedSend::new(conn.open_uni().await?)),
    };
    tracing::debug!(video_datagrams, "video channel open");
    let video = {
        let unanswered_since_ms = unanswered_since_ms.clone();
        let sent_bytes = sent_bytes.clone();
        let stats = stats.clone();
        let shared = shared.clone();
        let conn = conn.clone();
        let fec_permille = fec_permille.clone();
        let overhead_permille = overhead_permille.clone();
        async move {
            // Bytes on the network and bytes of video, decaying (the last few seconds).
            let (mut wire, mut video) = (0.0f64, 0.0f64);
            while let Some(frame) = frame_rx.recv().await {
                tracing::trace!(seq = frame.seq, bytes = frame.data.len(), "sending frame");
                let sent = match &mut video_tx {
                    Some(tx) => {
                        tx.send(&frame).await?;
                        frame.data.len()
                    }
                    None => {
                        let max = conn.max_datagram_size().context("datagrams no longer available")?;
                        let bytes = prost::Message::encode_to_vec(&frame);
                        let mut sent = 0;
                        let mut ratio = fec_permille.load(Ordering::Relaxed) as f64 / 1000.0;
                        // A keyframe lost freezes the image again: twice the parity.
                        if frame.keyframe {
                            ratio = (ratio * 2.0).max(FEC_MAX);
                        }
                        for packet in nuvira_transport::fec::encode(frame.seq, &bytes, max, ratio)? {
                            sent += packet.len();
                            conn.send_datagram_wait(packet.into()).await?;
                        }
                        wire = wire * 0.98 + sent as f64;
                        video = video * 0.98 + frame.data.len() as f64;
                        overhead_permille.store((wire / video.max(1.0) * 1000.0) as u32, Ordering::Relaxed);
                        sent
                    }
                };
                sent_bytes.fetch_add(sent as u64, Ordering::Relaxed);
                if let Some(s) = &stats {
                    s.frames_sent.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(shared) = &shared {
                    shared.frame(&frame);
                }
                let now_ms = session_start.elapsed().as_millis().max(1) as u64;
                let _ = unanswered_since_ms.compare_exchange(0, now_ms, Ordering::Relaxed, Ordering::Relaxed);
            }
            anyhow::Ok(())
        }
    };

    // The technician's microphone: datagrams from the viewer, played while it is on
    // (SetMicrophone) and the person at the PC did not mute it.
    let (mic_on_tx, mut mic_on_rx) = tokio::sync::watch::channel(false);
    let microphone = {
        let conn = conn.clone();
        let mut muted_rx = gate.map(|g| g.ctx.mic_muted.subscribe());
        let events = gate.map(|g| g.ctx.events.clone());
        async move {
            if !mic_offered {
                return std::future::pending::<Result<()>>().await;
            }
            let mut player: Option<nuvira_audio::player::Player> = None;
            let mut failed = false;
            loop {
                let datagram = tokio::select! {
                    d = conn.read_datagram() => match d {
                        Ok(d) => d,
                        // The connection ended: the other channels say why.
                        Err(_) => return std::future::pending::<Result<()>>().await,
                    },
                    Ok(()) = mic_on_rx.changed() => {
                        if !*mic_on_rx.borrow_and_update() && player.take().is_some() {
                            if let Some(events) = &events {
                                let _ = events.send(AgentEvent::Microphone { on: false });
                            }
                        }
                        continue;
                    }
                };
                let muted = muted_rx.as_mut().is_some_and(|m| *m.borrow_and_update());
                if !nuvira_audio::packet::is_audio(&datagram) || !*mic_on_rx.borrow() || muted || failed {
                    continue;
                }
                if player.is_none() {
                    // NUVIRA_MIC_DUMP (tests): written to that WAV file instead of played.
                    let dump = std::env::var_os("NUVIRA_MIC_DUMP").map(PathBuf::from);
                    match tokio::task::spawn_blocking(move || nuvira_audio::player::Player::start(dump)).await? {
                        Ok(p) => {
                            info!("technician's microphone played");
                            player = Some(p);
                            if let Some(events) = &events {
                                let _ = events.send(AgentEvent::Microphone { on: true });
                            }
                        }
                        Err(e) => {
                            warn!("technician's microphone not played: {e:#}");
                            failed = true;
                            continue;
                        }
                    }
                }
                if let (Some(p), Ok((seq, packet))) = (&player, nuvira_audio::packet::decode(&datagram)) {
                    p.push(seq, packet);
                }
            }
        }
    };
    // Ends with the session: the app's notice goes.
    struct MicNotice(Option<mpsc::UnboundedSender<AgentEvent>>);
    impl Drop for MicNotice {
        fn drop(&mut self) {
            if let Some(events) = &self.0 {
                let _ = events.send(AgentEvent::Microphone { on: false });
            }
        }
    }
    let _mic_notice = MicNotice(gate.filter(|_| mic_offered).map(|g| g.ctx.events.clone()));

    // The sound: captured while the viewer wants it (SetAudio), each packet a datagram.
    let (audio_on_tx, mut audio_on_rx) = tokio::sync::watch::channel(true);
    let audio = {
        let conn = conn.clone();
        // On while the viewer wants it (its mute button), or a guest of the session
        // does, and the person at the PC did not switch it off.
        let mut audio_off = off.clone();
        let shared = shared.clone();
        async move {
            if !audio_offered {
                return std::future::pending().await;
            }
            // Without a shared session no guest listens: a channel that never
            // changes (its sender kept here; a dropped one would wake the loop
            // at once, forever).
            let (_no_guests, idle) = tokio::sync::watch::channel(0);
            let mut listeners = match &shared {
                Some(s) => s.listeners(),
                None => idle,
            };
            let mut guests_gone = false;
            let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
            let mut capture: Option<nuvira_audio::capture::Capture> = None;
            let mut failed = false;
            let mut seq = 0u32;
            loop {
                let first = *audio_on_rx.borrow_and_update();
                let guests = *listeners.borrow_and_update() > 0;
                let want = (first || guests) && audio_off.is_on(switches::AUDIO);
                if want && capture.is_none() && !failed {
                    let tx = tx.clone();
                    match tokio::task::spawn_blocking(move || nuvira_audio::capture::Capture::start(move |p| {
                        let _ = tx.send(p);
                    }))
                    .await?
                    {
                        Ok(c) => capture = Some(c),
                        Err(e) => {
                            // No sound device: the session goes on without sound.
                            warn!("sound not sent: {e:#}");
                            failed = true;
                        }
                    }
                } else if !want && let Some(c) = capture.take() {
                    tokio::task::spawn_blocking(move || drop(c)).await?;
                }
                tokio::select! {
                    changed = audio_on_rx.changed() => {
                        if changed.is_err() {
                            return anyhow::Ok(());
                        }
                    }
                    _ = audio_off.changed() => {}
                    changed = listeners.changed(), if !guests_gone => {
                        if changed.is_err() {
                            guests_gone = true;
                        }
                    }
                    Some(packet) = rx.recv() => {
                        if capture.is_none() {
                            continue;
                        }
                        let datagram = Arc::new(nuvira_audio::packet::encode(seq, &packet));
                        seq = seq.wrapping_add(1);
                        if let Some(s) = &shared {
                            s.audio_packet(&datagram);
                        }
                        if !*audio_on_rx.borrow() {
                            continue;
                        }
                        // Not waiting for room: a packet that cannot leave now is late anyway.
                        match conn.send_datagram(datagram.as_ref().clone().into()) {
                            Ok(()) => {}
                            Err(quinn::SendDatagramError::ConnectionLost(e)) => return Err(e.into()),
                            Err(e) => tracing::debug!("audio packet not sent: {e}"),
                        }
                    }
                }
            }
        }
    };

    let control_loop = async {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        // The idle limit: checked every 5 s, a warning one minute before.
        let mut idle_tick = tokio::time::interval(Duration::from_secs(5));
        // The person at the PC mutes the microphone: the viewer is told.
        let (_never_muted, unmuted) = tokio::sync::watch::channel(false);
        let mut mic_muted_rx = match gate {
            Some(g) if mic_offered => g.ctx.mic_muted.subscribe(),
            _ => unmuted,
        };
        let mut idle_warned = false;
        let mut bytes_seen = 0u64;
        // Samples (time, bytes sent) of the last 2 s, for the rate actually used.
        let mut sent_samples = std::collections::VecDeque::<(Instant, u64)>::new();
        // Keyframes asked by the viewer: at most one a second. With the video as
        // datagrams on a 2 Mbit/s link every lost frame asked for one, each large and
        // with double parity, and they caused more losses: 2 Mbit/s went out while the
        // bitrate had come down to 0.5 (2026-11-01).
        let mut last_requested_key: Option<Instant> = None;
        // Privacy mode: on while this lives, at most until the end of the session.
        let mut privacy: Option<nuvira_input::privacy::Privacy> = None;
        // The viewer is recording (the person at the PC was told).
        let mut recording = false;
        let tell_app = |on: bool| {
            if let Some(g) = gate {
                let _ = g.ctx.events.send(AgentEvent::Recording { on });
            }
            // The recording is of the video: the pointer must be in it.
            pointer::set_recording(on);
        };
        // The pointer for this viewer, when it draws it.
        let mut pointer_sender = pointer::Sender::new(cursor_cache);
        let mut pointer_tick = tokio::time::interval(Duration::from_millis(16));
        pointer_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut changes = off.clone();
        // Lock state last told to the viewer (macOS only: elsewhere always false).
        let mut screen_locked = false;
        // Who joined the session, told to this viewer (none in LAN sessions).
        let mut guests_rx = match &shared {
            Some(s) => s.subscribe(),
            None => tokio::sync::watch::channel(nuvira_proto::v1::Guests::default()).1,
        };
        loop {
            let msg = tokio::select! {
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(msg) => msg,
                    None => return anyhow::Ok(()),
                },
                Ok(()) = guests_rx.changed() => {
                    let guests = guests_rx.borrow_and_update().clone();
                    ctrl_tx.send(&control(Msg::Guests(guests))).await?;
                    continue;
                }
                Some(state) = switch_rx.recv() => {
                    ctrl_tx.send(&control(Msg::SwitchSides(state))).await?;
                    continue;
                }
                Ok(()) = mic_muted_rx.changed() => {
                    let muted = *mic_muted_rx.borrow_and_update();
                    let wanted = *mic_on_tx.borrow();
                    let error = if muted { "@agent.microphone_muted".to_string() } else { String::new() };
                    let state = nuvira_proto::v1::MicrophoneState { on: wanted && !muted, error };
                    ctrl_tx.send(&control(Msg::MicrophoneState(state))).await?;
                    continue;
                }
                _ = idle_tick.tick(), if idle_limit > 0 => {
                    // Files moving count as activity.
                    let bytes =
                        stats::BYTES_IN.load(Ordering::Relaxed) + stats::BYTES_OUT.load(Ordering::Relaxed);
                    if bytes != bytes_seen {
                        bytes_seen = bytes;
                        mark_activity();
                    }
                    let idle_ms = (session_start.elapsed().as_millis() as u64)
                        .saturating_sub(activity.load(Ordering::Relaxed));
                    let limit_ms = u64::from(idle_limit) * 60_000;
                    if idle_ms >= limit_ms {
                        info!(minutes = idle_limit, "session ended after a time without activity");
                        bail!("@agent.idle_ended");
                    }
                    let warn_from = limit_ms.saturating_sub(60_000);
                    if idle_ms >= warn_from && !idle_warned {
                        idle_warned = true;
                        let seconds = ((limit_ms - idle_ms) / 1000) as u32;
                        let warning = nuvira_proto::v1::IdleWarning { seconds };
                        ctrl_tx.send(&control(Msg::IdleWarning(warning))).await?;
                    } else if idle_ms < warn_from && idle_warned {
                        idle_warned = false;
                        let over = nuvira_proto::v1::IdleWarning { seconds: 0 };
                        ctrl_tx.send(&control(Msg::IdleWarning(over))).await?;
                    }
                    continue;
                }
                _ = pointer_tick.tick(), if cursor_offered => {
                    for msg in pointer_sender.poll() {
                        ctrl_tx.send(&control(msg)).await?;
                    }
                    continue;
                }
                now = changes.changed() => {
                    ctrl_tx.send(&tell_permissions(now)).await?;
                    // Files or clipboard switched off: the viewer's files leave this
                    // clipboard (a paste would only fail), and ours are not served.
                    if files_by_clipboard && now & (switches::FILES | switches::CLIPBOARD) != 0 {
                        sharing.forget();
                        if let Some(clipboard) = &clipboard {
                            let _ = clipboard.send(nuvira_clipboard::Content::RemoteFiles(nuvira_clipboard::RemoteFiles::none()).into());
                        }
                    }
                    // Switched off by the person at the PC: the screen goes back at once.
                    if now & switches::RESOLUTION != 0 && nuvira_capture::resolution::adapted(owner, &display) {
                        let d = display.clone();
                        tokio::task::spawn_blocking(move || nuvira_capture::resolution::restore(owner, Some(&d))).await?;
                        display = reopen(&source, &pipeline, display.index, display.index).await?;
                        ctrl_tx.send(&video_format(&display)).await?;
                        if let Some(shared) = &shared {
                            shared.set_display(&display);
                        }
                        if let Some(msg) = displays_message(config, &display) {
                            ctrl_tx.send(&msg).await?;
                        }
                        let _ = screen_tx.send(screen_rect(&display));
                        let state = nuvira_proto::v1::ResolutionState {
                            width: display.width,
                            height: display.height,
                            adapted: false,
                            error: "@agent.resolution_switched_off".into(),
                        };
                        ctrl_tx.send(&control(Msg::ResolutionState(state))).await?;
                    }
                    continue;
                }
                Some(content) = clip_rx.recv() => {
                    if !off.is_on(switches::CLIPBOARD) {
                        sharing.forget();
                        continue;
                    }
                    match content {
                        nuvira_clipboard::Content::Text(text) => {
                            sharing.forget();
                            ctrl_tx.send(&control(Msg::Clipboard(nuvira_proto::v1::Clipboard { text, ..Default::default() }))).await?;
                        }
                        // Files: listed as the user signed in, offered to the viewer.
                        nuvira_clipboard::Content::Files(paths) => {
                            sharing.forget();
                            tracing::debug!(files = paths.len(), allowed = files_allowed(), "files copied here");
                            if files_allowed() {
                                let (sharing, listed_tx) = (sharing.clone(), listed_tx.clone());
                                tokio::spawn(async move {
                                    match fsops::as_user(move || nuvira_clipboard::list::expand(&paths)).await {
                                        Ok(listed) => {
                                            let _ = listed_tx.send(sharing.announce(listed));
                                        }
                                        Err(e) => info!("copied files not offered: {e:#}"),
                                    }
                                });
                            }
                        }
                        nuvira_clipboard::Content::RemoteFiles(_) => {}
                        // On its own stream, not to hold up the control channel.
                        nuvira_clipboard::Content::Image(png) => {
                            sharing.forget();
                            let conn = conn.clone();
                            tokio::spawn(async move {
                                let header = nuvira_proto::v1::FileChunk { clipboard_image: true, ..Default::default() };
                                if let Err(e) = nuvira_transport::send_stream(&conn, header, &png).await {
                                    warn!("clipboard image not sent: {e:#}");
                                }
                            });
                        }
                    }
                    continue;
                }
                Some(list) = listed_rx.recv() => {
                    ctrl_tx.send(&control(Msg::ClipboardFiles(list))).await?;
                    continue;
                }
                // Files of the viewer pasted here: their contents asked for.
                Some(request) = paste_rx_files.recv() => {
                    if files_allowed() {
                        ctrl_tx.send(&control(Msg::ClipboardFileRequest(request))).await?;
                    } else {
                        sharing.refuse(request.id, "files switched off");
                    }
                    continue;
                }
                Some(text) = chat_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Chat(nuvira_proto::v1::Chat { text }))).await?;
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
                    let locked = nuvira_capture::screen_locked();
                    if locked != screen_locked {
                        screen_locked = locked;
                        info!(locked, "screen lock state");
                        #[cfg(target_os = "macos")]
                        if locked {
                            nuvira_input::lockhid::prepare();
                        }
                        let input = nuvira_capture::lock_screen_takes_input();
                        let state = nuvira_proto::v1::ScreenState { locked, input };
                        ctrl_tx.send(&control(Msg::ScreenState(state))).await?;
                    }
                    let now = Instant::now();
                    sent_samples.push_back((now, sent_bytes.load(Ordering::Relaxed)));
                    while sent_samples.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(2)) {
                        sent_samples.pop_front();
                    }
                    // The person at the PC no longer allows it: it ends at once.
                    if privacy.is_some() && !privacy_allowed() {
                        if let Some(on) = privacy.take() {
                            tokio::task::spawn_blocking(move || on.stop()).await?;
                        }
                        let state = nuvira_proto::v1::PrivacyState { on: false, error: "@agent.privacy_not_allowed".into() };
                        ctrl_tx.send(&control(Msg::PrivacyState(state))).await?;
                    }
                    // Required by the rules: the viewer has a few seconds to start.
                    if recording_required && !recording && session_start.elapsed() > RECORDING_START {
                        warn!("session ended: not recorded as the server's rules want");
                        let state = nuvira_proto::v1::RecordingState { on: false, error: "@agent.recording_required".into() };
                        ctrl_tx.send(&control(Msg::RecordingState(state))).await?;
                        return anyhow::Ok(());
                    }
                    if recording && !recording_allowed() {
                        recording = false;
                        info!("recording stopped: no longer allowed by the person at the PC");
                        tell_app(false);
                        let state = nuvira_proto::v1::RecordingState { on: false, error: "@agent.recording_not_allowed".into() };
                        ctrl_tx.send(&control(Msg::RecordingState(state))).await?;
                    }
                    // The encoder's share follows the overhead measured.
                    if video_datagrams && let Some(abr) = abr.as_ref() {
                        bitrate.store(encoder_bps(abr.bitrate()), Ordering::Relaxed);
                    }
                    // A frame left long ago and no feedback since: the network is probably stuck.
                    let now_ms = session_start.elapsed().as_millis() as u64;
                    let since = unanswered_since_ms.load(Ordering::Relaxed);
                    if let Some(abr) = abr.as_mut()
                        && since != 0
                        && now_ms.saturating_sub(since) > abr::FEEDBACK_TIMEOUT.as_millis() as u64
                        && let Some(bps) = abr.on_silence(Instant::now())
                    {
                        bitrate.store(encoder_bps(bps), Ordering::Relaxed);
                        info!("bitrate {} kbit/s (no feedback from the viewer)", bps / 1000);
                    }
                    if let (Some(s), Some(abr)) = (&stats, abr.as_ref()) {
                        s.abr_target_bps.store(abr.bitrate(), Ordering::Relaxed);
                    }
                    continue;
                }
            };
            // What the viewer does counts as activity, not what it sends by itself.
            if !matches!(
                msg.msg,
                Some(Msg::Ping(_) | Msg::Feedback(_) | Msg::ViewSize(_) | Msg::RequestKeyframe(_)) | None
            ) {
                mark_activity();
            }
            match msg.msg {
                Some(Msg::Activity(_)) => {}
                Some(Msg::Ping(ping)) => {
                    let pong = Pong { ping_sent_us: ping.sent_us, agent_us: nuvira_proto::now_us() };
                    ctrl_tx.send(&control(Msg::Pong(pong))).await?;
                }
                Some(Msg::RequestKeyframe(_)) => {
                    if let Some(s) = &stats {
                        s.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    if last_requested_key.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                        last_requested_key = Some(Instant::now());
                        keyframe.store(true, Ordering::Relaxed);
                    }
                }
                Some(Msg::FsRequest(request)) => {
                    let fs_tx = fs_tx.clone();
                    if config.view_only || config.synthetic.is_some() || policy.no_files || !off.is_on(switches::FILES) {
                        let key = if policy.no_files {
                            "@agent.files_disabled"
                        } else if !off.is_on(switches::FILES) {
                            "@agent.files_switched_off"
                        } else {
                            "@agent.files_view_only"
                        };
                        let error = key.to_string();
                        let _ = fs_tx.send(nuvira_proto::v1::FsResponse {
                            id: request.id,
                            error,
                            ..Default::default()
                        });
                        continue;
                    }
                    let (conn, off) = (conn.clone(), off.clone());
                    tokio::spawn(async move {
                        let id = request.id;
                        let answer = switches::until_off(off, switches::FILES, fsops::handle(id, request.op, conn))
                            .await
                            .unwrap_or_else(|| nuvira_proto::v1::FsResponse {
                                id,
                                error: "@agent.files_switched_off".into(),
                                ..Default::default()
                            });
                        let _ = fs_tx.send(answer);
                    });
                }
                Some(Msg::Feedback(fb)) => {
                    unanswered_since_ms.store(0, Ordering::Relaxed);
                    if let Some(s) = &stats {
                        s.feedback(&fb);
                    }
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
                    if video_datagrams {
                        if let (Some(shards), Some(lost)) = (fb.shards, fb.shards_lost)
                            && shards > 0
                        {
                            shard_loss = 0.7 * shard_loss + 0.3 * (lost as f64 / shards as f64);
                            if fixed_ratio.is_none() {
                                let ratio = (FEC_MIN + 3.0 * shard_loss).clamp(FEC_MIN, FEC_MAX);
                                let new = (ratio * 1000.0) as u32;
                                if fec_permille.swap(new, Ordering::Relaxed).abs_diff(new) >= 20 {
                                    tracing::debug!(
                                        "parity {ratio:.2} (shards lost {:.1}%)",
                                        shard_loss * 100.0
                                    );
                                    if let Some(abr) = abr.as_ref() {
                                        bitrate.store(encoder_bps(abr.bitrate()), Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                        if let Some(abr) = abr.as_mut()
                            && let Some(bps) =
                                abr.on_loss(Instant::now(), fb.frames_lost.unwrap_or(0), sent_bps)
                        {
                            bitrate.store(encoder_bps(bps), Ordering::Relaxed);
                            info!(
                                "bitrate {} kbit/s ({} frames lost; sending {} kbit/s)",
                                bps / 1000,
                                fb.frames_lost.unwrap_or(0),
                                (sent_bps.min(1e12) / 1000.0) as u64
                            );
                        }
                    }
                    if let Some(abr) = abr.as_mut()
                        && fb.frames > 0
                        && let Some(bps) = abr.on_feedback(Instant::now(), p50 as f64, min as f64, sent_bps)
                    {
                        bitrate.store(encoder_bps(bps), Ordering::Relaxed);
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
                    // The screen left goes back to its own resolution.
                    let d = display.clone();
                    tokio::task::spawn_blocking(move || nuvira_capture::resolution::restore(owner, Some(&d))).await?;
                    display = reopen(&source, &pipeline, index, display.index).await?;
                    ctrl_tx.send(&video_format(&display)).await?;
                    if let Some(shared) = &shared {
                        shared.set_display(&display);
                    }
                    if let Some(msg) = displays_message(config, &display) {
                        ctrl_tx.send(&msg).await?;
                    }
                    let _ = screen_tx.send(screen_rect(&display));
                }
                Some(Msg::SetResolution(set)) => {
                    let refused = if !resolution_offered {
                        Some("@agent.resolution_disabled")
                    } else if !off.is_on(switches::RESOLUTION) {
                        Some("@agent.resolution_switched_off")
                    } else if display.index == nuvira_capture::ALL_DISPLAYS {
                        Some("@agent.resolution_all_screens")
                    } else {
                        None
                    };
                    let mut error = refused.map(String::from).unwrap_or_default();
                    if refused.is_none() {
                        let d = display.clone();
                        let target = nuvira_capture::resolution::Size { width: set.width, height: set.height };
                        let before = (display.width, display.height);
                        let result = tokio::task::spawn_blocking(move || match target.width == 0 || target.height == 0 {
                            true => Ok(nuvira_capture::resolution::restore(owner, Some(&d))),
                            false => nuvira_capture::resolution::fit(owner, &d, target).map(|s| (s.width, s.height) != before),
                        })
                        .await?;
                        match result {
                            // The capture starts again at the new size.
                            Ok(true) => {
                                display = reopen(&source, &pipeline, display.index, display.index).await?;
                                ctrl_tx.send(&video_format(&display)).await?;
                                if let Some(shared) = &shared {
                                    shared.set_display(&display);
                                }
                                if let Some(msg) = displays_message(config, &display) {
                                    ctrl_tx.send(&msg).await?;
                                }
                                let _ = screen_tx.send(screen_rect(&display));
                            }
                            Ok(false) => {}
                            Err(e) => {
                                warn!("screen resolution not changed: {e:#}");
                                error = e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@'))
                                    .unwrap_or_else(|| "@agent.resolution_failed".into());
                            }
                        }
                    }
                    let state = nuvira_proto::v1::ResolutionState {
                        width: display.width,
                        height: display.height,
                        adapted: nuvira_capture::resolution::adapted(owner, &display),
                        error,
                    };
                    ctrl_tx.send(&control(Msg::ResolutionState(state))).await?;
                }
                Some(Msg::Clipboard(clip)) => {
                    if let Some(clipboard) = clipboard.as_ref().filter(|_| off.is_on(switches::CLIPBOARD)) {
                        let paste = clip.paste();
                        let then: Option<Box<dyn FnOnce(bool) + Send>> = match paste {
                            nuvira_proto::v1::Paste::None => None,
                            _ => {
                                let paste_tx = paste_tx.clone();
                                Some(Box::new(move |written| {
                                    if written {
                                        let _ = paste_tx.send(paste);
                                    }
                                }))
                            }
                        };
                        let content = nuvira_clipboard::Content::Text(clip.text);
                        let _ = clipboard.send(nuvira_clipboard::Incoming { content, then });
                    }
                }
                // Files copied at the viewer: offered on this clipboard.
                Some(Msg::ClipboardFiles(list)) => {
                    if files_allowed() && let Some(clipboard) = &clipboard {
                        info!(entries = list.files.len(), "files copied at the viewer: on the clipboard");
                        let _ = clipboard.send(nuvira_clipboard::Content::RemoteFiles(sharing.remote(list)).into());
                    }
                }
                // Files copied here, pasted at the viewer: read as the user signed in.
                Some(Msg::ClipboardFileRequest(request)) => {
                    let allowed = files_allowed();
                    let (sharing, conn, off) = (sharing.clone(), conn.clone(), off.clone());
                    tokio::spawn(async move {
                        let opened = match (allowed, sharing.requested(&request)) {
                            (false, _) => Err(anyhow::anyhow!("@agent.files_switched_off")),
                            (true, Err(e)) => Err(e),
                            (true, Ok(path)) => fsops::as_user(move || {
                                std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))
                            })
                            .await,
                        };
                        let sent = nuvira_clipboard::Sharing::answer(&conn, &request, opened);
                        let sent = switches::until_off(off.clone(), switches::CLIPBOARD, sent);
                        match switches::until_off(off, switches::FILES, sent).await.flatten() {
                            Some(Err(e)) => warn!("pasted file not sent: {e:#}"),
                            Some(Ok(())) => {}
                            None => info!("pasted file cut: switched off at the PC"),
                        }
                    });
                }
                Some(Msg::Chat(chat)) => match &events {
                    Some(events) => {
                        let _ = events.send(AgentEvent::Chat { text: chat.text });
                    }
                    None => info!("chat from the viewer: {}", chat.text),
                },
                Some(Msg::ViewSize(size)) => {
                    let packed = crate::pipeline::pack_view(size.width, size.height);
                    if view.swap(packed, Ordering::Relaxed) != packed {
                        tracing::debug!("viewer area {}x{}", size.width, size.height);
                    }
                }
                Some(Msg::SetAudio(set)) => {
                    info!(on = set.on, "sound of the session");
                    let _ = audio_on_tx.send(set.on);
                }
                Some(Msg::SetMicrophone(set)) => {
                    let muted = gate.is_some_and(|g| *g.ctx.mic_muted.borrow());
                    let state = match (mic_offered, muted) {
                        (false, _) => nuvira_proto::v1::MicrophoneState {
                            on: false,
                            error: "@agent.microphone_unavailable".into(),
                        },
                        (true, true) if set.on => nuvira_proto::v1::MicrophoneState {
                            on: false,
                            error: "@agent.microphone_muted".into(),
                        },
                        _ => nuvira_proto::v1::MicrophoneState { on: set.on, error: String::new() },
                    };
                    info!(on = set.on, played = state.on, "technician's microphone asked");
                    let _ = mic_on_tx.send(set.on && mic_offered);
                    ctrl_tx.send(&control(Msg::MicrophoneState(state))).await?;
                }
                Some(Msg::SetWallpaper(set)) => {
                    if let Some(state) = wallpaper_state.clone() {
                        info!(hidden = set.hidden, "wallpaper asked by the viewer");
                        let done = tokio::task::spawn_blocking(move || match set.hidden {
                            true => nuvira_capture::wallpaper::hide(&state),
                            false => {
                                nuvira_capture::wallpaper::restore(&state);
                                Ok(())
                            }
                        })
                        .await?;
                        if let Err(e) = done {
                            warn!("wallpaper not hidden: {e:#}");
                        }
                    }
                }
                Some(Msg::SetLockOnEnd(set)) => {
                    if let Some(g) = gate.filter(|g| !g.request.join) {
                        info!(on = set.on, "lock at the end asked by the viewer");
                        g.ctx.lock_asked.store(set.on, Ordering::Relaxed);
                        let (on, forced) = lock_wanted(g.ctx, g.request);
                        let state = nuvira_proto::v1::LockOnEndState { on, forced };
                        ctrl_tx.send(&control(Msg::LockOnEndState(state))).await?;
                    }
                }
                // Handed over to a guest: its token goes to it, this session ends
                // (and the guests' with it), and it connects again as the first viewer.
                Some(Msg::HandOver(h)) => {
                    let from = gate.map(|g| match g.request.viewer_name.as_str() {
                        "" => g.request.viewer_id.clone(),
                        name => name.to_string(),
                    });
                    let handed = match (gate, &shared, from) {
                        (Some(g), Some(s), Some(from)) => s.hand_over(h.guest, &from).map(|t| (g, t)),
                        _ => None,
                    };
                    match handed {
                        Some((g, (token, viewer_id, name))) => {
                            *g.ctx.handover.lock().expect("handover") =
                                Some(shared::Handover { viewer_id, token, at: Instant::now() });
                            info!(to = name, "session handed over");
                            let told = nuvira_proto::v1::HandedOver { to: name, ..Default::default() };
                            ctrl_tx.send(&control(Msg::HandedOver(told))).await?;
                            // A moment for the guest to get its token before its session ends.
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            return anyhow::Ok(());
                        }
                        None => warn!(guest = h.guest, "session not handed over: no such guest, or it cannot take it"),
                    }
                }
                Some(Msg::SetPrivacy(set)) => {
                    let mut error = String::new();
                    if set.on && privacy.is_none() {
                        if policy.no_privacy || !off.is_on(switches::INPUT) {
                            error = "@agent.privacy_disabled".into();
                        } else if !privacy_allowed() {
                            error = "@agent.privacy_not_allowed".into();
                        } else if config.view_only || config.synthetic.is_some() {
                            error = "@agent.privacy_unsupported".into();
                        } else {
                            let message = nuvira_i18n::t("agent.privacy_screen");
                            match tokio::task::spawn_blocking(move || {
                                nuvira_input::privacy::start(&message, &nuvira_capture::hide_process)
                            }).await? {
                                Ok(on) => privacy = Some(on),
                                Err(e) => {
                                    warn!("privacy mode not turned on: {e:#}");
                                    error = e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@'))
                                        .unwrap_or_else(|| "@agent.privacy_failed".into());
                                }
                            }
                        }
                    } else if !set.on && let Some(on) = privacy.take() {
                        tokio::task::spawn_blocking(move || on.stop()).await?;
                    }
                    let state = nuvira_proto::v1::PrivacyState { on: privacy.is_some(), error };
                    ctrl_tx.send(&control(Msg::PrivacyState(state))).await?;
                }
                Some(Msg::SetRecording(set)) if !set.on && recording_required => {
                    warn!("session ended: the viewer stopped the recording the server's rules want");
                    tell_app(false);
                    let state = nuvira_proto::v1::RecordingState { on: false, error: "@agent.recording_required".into() };
                    ctrl_tx.send(&control(Msg::RecordingState(state))).await?;
                    return anyhow::Ok(());
                }
                Some(Msg::SetRecording(set)) => {
                    let mut error = String::new();
                    if set.on && !recording_allowed() {
                        error = "@agent.recording_not_allowed".into();
                    } else if set.on != recording {
                        recording = set.on;
                        info!(on = recording, "session recording");
                        tell_app(recording);
                    }
                    let state = nuvira_proto::v1::RecordingState { on: recording, error };
                    ctrl_tx.send(&control(Msg::RecordingState(state))).await?;
                }
                Some(Msg::RestartCheck(_)) => {
                    // Asked before the confirmation: the technician is told whose
                    // sessions a restart would close.
                    let others = if restart_available {
                        tokio::task::spawn_blocking(restart::other_users).await.unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    info!(others = others.len(), "other people signed in, asked before a restart");
                    let users = nuvira_proto::v1::RestartUsers { others };
                    ctrl_tx.send(&control(Msg::RestartUsers(users))).await?;
                }
                Some(Msg::Restart(asked)) => {
                    let mut state = nuvira_proto::v1::RestartState::default();
                    // Without keyboard and mouse (view only) no restart either.
                    let refused = !restart_available || (asked.safe_mode && !safe_mode_offered);
                    if refused || !off.is_on(switches::INPUT) {
                        state.error = "@agent.restart_unavailable".into();
                    } else {
                        info!(safe_mode = asked.safe_mode, "restart of the computer asked by the viewer");
                        let restart = move || {
                            if asked.safe_mode {
                                restart::prepare_safe_mode()?;
                            }
                            restart::restart()
                        };
                        match tokio::task::spawn_blocking(restart).await? {
                            Ok(()) => {
                                state.ok = true;
                                // Back without anyone at the computer: the permanent
                                // password, with no confirmation asked.
                                state.reconnect = gate.is_some_and(|g| {
                                    matches!(*g.access.lock().expect("access"), "permanent" | "profile")
                                });
                                if let Some(g) = gate {
                                    *g.ctx.restarting.lock().expect("restarting") = Some(Instant::now());
                                }
                            }
                            Err(e) => {
                                warn!("restart refused by the system: {e:#}");
                                state.error = e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@'))
                                    .unwrap_or_else(|| format!("{e:#}"));
                            }
                        }
                    }
                    let restarting = state.ok;
                    ctrl_tx.send(&control(Msg::RestartState(state))).await?;
                    if restarting {
                        // The answer leaves, then the session ends before the system stops it.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        info!("session ended: the computer is restarting");
                        return anyhow::Ok(());
                    }
                }
                Some(Msg::SetQuality(q)) => {
                    let (limit_bps, limit_fps) = quality_limits(config, q.quality());
                    fps.store(limit_fps, Ordering::Relaxed);
                    let bps = match abr.as_mut() {
                        Some(abr) => abr.set_max(limit_bps),
                        None => Some(limit_bps),
                    };
                    if let Some(bps) = bps {
                        bitrate.store(encoder_bps(bps), Ordering::Relaxed);
                    }
                    info!("quality {:?}: up to {} kbit/s and {limit_fps} fps", q.quality(), limit_bps / 1000);
                }
                _ => {}
            }
        }
    };

    let input = async {
        // Keyboard and mouse: while the session has them and they are not switched off.
        let open_injector = |screen| -> Result<_> { input_thread(nuvira_input::open(screen)?) };
        let closed = || anyhow!("input thread ended");
        let input_offered = offered & switches::INPUT != 0;
        // The viewer's input stream shows only with its first event; the events of
        // viewers who joined the session may come before.
        let mut injector = None;
        let stream = loop {
            tokio::select! {
                stream = conn.accept_uni() => break stream?,
                Some(event) = guest_input_rx.recv() => {
                    if !(input_offered && off.is_on(switches::INPUT)) {
                        // Ending the thread releases the keys still pressed.
                        injector = None;
                        continue;
                    }
                    if injector.is_none() {
                        injector = Some(open_injector(*screen_rx.borrow_and_update())?);
                    }
                    if let Some(injector) = &injector {
                        injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                    }
                }
                Ok(()) = screen_rx.changed() => if let Some(injector) = &injector {
                    let screen = *screen_rx.borrow_and_update();
                    injector.send(InputCmd::Screen(screen)).map_err(|_| closed())?;
                },
            }
        };
        let mut rx = FramedRecv::new(stream);
        // Switched off before the viewer's first event (the stream shows only then): none.
        if injector.is_none() && input_offered && off.is_on(switches::INPUT) {
            injector = Some(open_injector(*screen_rx.borrow_and_update())?);
        } else if !off.is_on(switches::INPUT) {
            injector = None;
        }
        let mut input_changes = off.clone();
        loop {
            tokio::select! {
                event = rx.recv::<InputEvent>() => match event? {
                    Some(event) => {
                        mark_activity();
                        if let Some(injector) = &injector {
                            injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                        }
                    }
                    None => break,
                },
                now = input_changes.changed(), if offered & switches::INPUT != 0 => {
                    match (now & switches::INPUT == 0, injector.is_some()) {
                        // Ending the thread releases the keys still pressed.
                        (false, true) => {
                            injector = None;
                            info!("keyboard and mouse switched off by the person at the PC");
                        }
                        (true, false) => {
                            injector = Some(open_injector(*screen_rx.borrow_and_update())?);
                            info!("keyboard and mouse switched on again by the person at the PC");
                        }
                        _ => {}
                    }
                }
                // The keyboard and mouse of viewers who joined the session.
                Some(event) = guest_input_rx.recv() => if let Some(injector) = &injector {
                    injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                },
                Ok(()) = screen_rx.changed() => if let Some(injector) = &injector {
                    let screen = *screen_rx.borrow_and_update();
                    injector.send(InputCmd::Screen(screen)).map_err(|_| closed())?;
                },
                // The text of a Clipboard.paste is in the clipboard: its keys now.
                Some(paste) = paste_rx.recv() => if let Some(injector) = &injector {
                    tracing::debug!(?paste, "clipboard written: paste keys");
                    for event in paste_keys(paste) {
                        injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                    }
                },
                // After the input one, every unidirectional stream of the viewer is a file
                // or an image copied to the clipboard.
                stream = conn.accept_uni() => {
                    let mut stream = stream?;
                    if config.view_only || config.synthetic.is_some() {
                        // View only: no files.
                        let _ = stream.stop(0u32.into());
                        continue;
                    }
                    let (file_tx, events, clipboard) = (file_tx.clone(), events.clone(), clipboard_in.clone());
                    let no_files = policy.no_files;
                    let off = off.clone();
                    let sharing = sharing.clone();
                    tokio::spawn(async move {
                        let mut rx = FramedRecv::new(stream);
                        let first = match rx.recv::<nuvira_proto::v1::FileChunk>().await {
                            Ok(Some(first)) => first,
                            _ => return,
                        };
                        // The answer to a paste here (asked only while allowed): cut as
                        // soon as files or the clipboard are switched off (the paste
                        // then fails instead of finishing).
                        if first.clipboard_file {
                            let receiving = sharing.receive(first, rx);
                            let receiving = switches::until_off(off.clone(), switches::CLIPBOARD, receiving);
                            if switches::until_off(off, switches::FILES, receiving).await.flatten().is_none() {
                                info!("pasted file cut: switched off at the PC");
                            }
                            return;
                        }
                        // Rules of the server: no files (images of the clipboard are refused
                        // by having no clipboard, see above).
                        let switched_off = !off.is_on(switches::FILES);
                        if (no_files || switched_off) && !first.clipboard_image {
                            let message = if no_files { "@agent.files_disabled" } else { "@agent.files_switched_off" };
                            let _ = file_tx.send(nuvira_proto::v1::FileReceived {
                                name: first.name,
                                transfer_id: first.transfer_id,
                                message: message.into(),
                                ..Default::default()
                            });
                            return;
                        }
                        if first.clipboard_image && !off.is_on(switches::CLIPBOARD) {
                            return;
                        }
                        // Switched off during the transfer: it stops there.
                        let stopped = |name: String, transfer_id: u32| nuvira_proto::v1::FileReceived {
                            name,
                            transfer_id,
                            message: "@agent.files_switched_off".into(),
                            ..Default::default()
                        };
                        // File manager: into the chosen folder, with resume.
                        if first.transfer_id != 0 {
                            let (name, transfer_id) = (first.name.clone(), first.transfer_id);
                            let Some(received) = switches::until_off(off, switches::FILES, fsops::receive(first, rx)).await else {
                                let _ = file_tx.send(stopped(name, transfer_id));
                                return;
                            };
                            let done = match received {
                                Ok(path) => nuvira_proto::v1::FileReceived {
                                    name,
                                    ok: true,
                                    transfer_id,
                                    path: path.display().to_string(),
                                    ..Default::default()
                                },
                                Err(e) => {
                                    warn!("file {name} not received: {e:#}");
                                    nuvira_proto::v1::FileReceived { name, transfer_id, message: format!("{e:#}"), ..Default::default() }
                                }
                            };
                            let _ = file_tx.send(done);
                            return;
                        }
                        if first.clipboard_image {
                            match nuvira_transport::read_stream(&mut rx, &first, nuvira_clipboard::MAX_IMAGE_PNG as u64).await {
                                Ok(png) => if let Some(clipboard) = clipboard {
                                    let _ = clipboard.send(nuvira_clipboard::Content::Image(png).into());
                                },
                                Err(e) => warn!("clipboard image not received: {e:#}"),
                            }
                            return;
                        }
                        let name = first.name.clone();
                        let Some(received) = switches::until_off(off, switches::FILES, files::receive(first, rx)).await else {
                            let _ = file_tx.send(stopped(name, 0));
                            return;
                        };
                        let done = match received {
                            Ok((name, path)) => {
                                if let Some(events) = &events {
                                    let path = path.display().to_string();
                                    let _ = events.send(AgentEvent::FileReceived { name: name.clone(), path });
                                }
                                nuvira_proto::v1::FileReceived { name, ok: true, message: "@agent.downloads".into(), ..Default::default() }
                            }
                            Err((name, e)) => {
                                warn!("file {name} not received: {e:#}");
                                nuvira_proto::v1::FileReceived { name, ok: false, message: format!("{e:#}"), ..Default::default() }
                            }
                        };
                        let _ = file_tx.send(done);
                    });
                }
            }
        }
        anyhow::Ok(())
    };

    // A failure of remote printing does not end the session.
    let printing = async {
        if let Some(printer) = printer
            && let Err(e) = printer.serve(conn.clone(), print_allowed).await
        {
            warn!("remote printing stopped: {e:#}");
        }
        std::future::pending::<Result<()>>().await
    };

    let result = tokio::select! {
        r = video => r.context("sending video"),
        r = control_loop => r.context("control channel"),
        r = input => r.context("input channel"),
        r = accept_streams(&conn, (&terminal_refused, &tunnel_refused), &off, switch_offered.then_some(&switch_tx)) => r,
        r = printing => r,
        r = audio => r.context("sound"),
        r = microphone => r.context("microphone"),
    };
    let last = pipeline.lock().expect("pipeline").take();
    let _ = tokio::task::spawn_blocking(move || drop(last)).await;
    if let Some(gate) = gate {
        *gate.ctx.chat.lock().expect("chat") = None;
    }
    drop(chat_tx);
    result
}

/// A session with terminals only: the control channel carries ping and chat, every
/// other bidirectional stream of the viewer is a terminal.
async fn terminal_session(
    conn: &Connection,
    mut ctrl_tx: FramedSend,
    mut ctrl_rx: FramedRecv,
    gate: Option<&Gate<'_>>,
    refused: Refusals<'_, impl Fn() -> Option<&'static str>, impl Fn() -> Option<&'static str>>,
    off: switches::Off,
    tell_permissions: &impl Fn(u8) -> ControlMessage,
) -> Result<()> {
    info!("terminal session (no desktop)");
    let mut changes = off.clone();
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<String>();
    let events = gate.map(|g| {
        *g.ctx.chat.lock().expect("chat") = Some(chat_tx.clone());
        g.ctx.events.clone()
    });
    let control = async {
        loop {
            tokio::select! {
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    None => return anyhow::Ok(()),
                    Some(ControlMessage { msg: Some(Msg::Ping(ping)) }) => {
                        let pong = Pong { ping_sent_us: ping.sent_us, agent_us: nuvira_proto::now_us() };
                        ctrl_tx.send(&control(Msg::Pong(pong))).await?;
                    }
                    Some(ControlMessage { msg: Some(Msg::Chat(chat)) }) => {
                        if let Some(events) = &events {
                            let _ = events.send(AgentEvent::Chat { text: chat.text });
                        }
                    }
                    Some(_) => {}
                },
                Some(text) = chat_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Chat(nuvira_proto::v1::Chat { text }))).await?;
                }
                now = changes.changed() => {
                    ctrl_tx.send(&tell_permissions(now)).await?;
                }
            }
        }
    };
    let result = tokio::select! {
        r = control => r.context("control channel"),
        r = accept_streams(conn, refused, &off, None) => r,
    };
    if let Some(gate) = gate {
        *gate.ctx.chat.lock().expect("chat") = None;
    }
    result
}

/// Why a terminal and a tunnel are refused, checked when each one opens.
type Refusals<'a, T, U> = (&'a T, &'a U);

/// Every bidirectional stream of the viewer after the control one is a terminal or a
/// TCP tunnel (its first message, `StreamOpen`, says which), served by a task of its
/// own. It never ends by itself: the end of the connection is seen by the other
/// channels of the session.
async fn accept_streams(
    conn: &Connection,
    (terminal_refused, tunnel_refused): Refusals<'_, impl Fn() -> Option<&'static str>, impl Fn() -> Option<&'static str>>,
    off: &switches::Off,
    switch_sides: Option<&mpsc::UnboundedSender<nuvira_proto::v1::SwitchSides>>,
) -> Result<()> {
    use nuvira_proto::v1::{StreamOpen, stream_open::Kind};
    while let Ok((send, mut recv)) = conn.accept_bi().await {
        let (terminal, tunnel) = (terminal_refused(), tunnel_refused());
        let off = off.clone();
        let switch_sides = switch_sides.cloned();
        tokio::spawn(async move {
            let limit = nuvira_transport::STREAM_HEADER_LIMIT;
            let open = match nuvira_transport::recv_one_with_limit::<StreamOpen>(&mut recv, limit).await {
                Ok(Some(open)) => open,
                Ok(None) => return,
                Err(e) => return warn!("stream from the viewer: {e:#}"),
            };
            let result = match open.kind {
                Some(Kind::Terminal(t)) => {
                    let size = t.size.unwrap_or_default();
                    let stop = off.switched_off(switches::TERMINAL);
                    terminal::serve(FramedSend::new(send), FramedRecv::new(recv), size, terminal, stop).await
                }
                // Switched off: the connection is cut.
                Some(Kind::Tunnel(t)) => {
                    switches::until_off(off, switches::TUNNEL, tunnel::serve(send, recv, t, tunnel)).await.unwrap_or(Ok(()))
                }
                // Switch sides: the window with the viewer's screen (an agent that
                // does not offer it closes the stream).
                Some(Kind::Reverse(open)) => match switch_sides {
                    Some(report) => reverse::open_window(send, recv, open, report).await,
                    None => Ok(()),
                },
                None => Ok(()),
            };
            if let Err(e) = result {
                warn!("stream from the viewer: {e:#}");
            }
        });
    }
    std::future::pending().await
}

/// What the viewer reads when the connection closes: the "@key" message of the
/// error that ended the session, if it has one (e.g. the screen of a Linux
/// computer is locked), translated on its side.
fn close_reason(result: &Result<()>) -> Vec<u8> {
    result
        .as_ref()
        .err()
        .and_then(|e| e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@')))
        .map_or_else(|| b"end of session".to_vec(), String::into_bytes)
}

/// Capture and encoding of a screen, restartable on another one during the session.
struct VideoSource {
    config: AgentConfig,
    /// Copy of the sender: keeps the video channel open while the screen changes.
    frames: mpsc::Sender<VideoFrame>,
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    fps: Arc<AtomicU32>,
    view: Arc<AtomicU64>,
    stats: Option<Arc<session_stats::AgentStats>>,
}

impl VideoSource {
    async fn start(&self, display: Option<usize>) -> Result<(nuvira_capture::Display, Pipeline)> {
        let mut cap_config = self.config.clone();
        cap_config.display = display;
        // The encoder starts from the current bitrate of the adaptive control.
        let settings = EncoderSettings {
            bitrate_bps: self.bitrate.load(Ordering::Relaxed),
            max_fps: self.fps.load(Ordering::Relaxed),
            ..Default::default()
        };
        let plan = self.config.plan;
        // A new stream must start with a keyframe.
        self.keyframe.store(true, Ordering::Relaxed);
        let (frames, keyframe, bitrate, fps, view, stats) = (
            self.frames.clone(),
            self.keyframe.clone(),
            self.bitrate.clone(),
            self.fps.clone(),
            self.view.clone(),
            self.stats.clone(),
        );
        tokio::task::spawn_blocking(move || {
            Pipeline::start(
                move || cap_config.open_capturer(),
                move |gpu| nuvira_codec::create_encoder(plan, settings, gpu),
                plan == EncoderPlan::HardwareGpu,
                frames,
                Controls { max_fps: fps, keyframe, bitrate, view, stats },
            )
        })
        .await?
        .context("starting capture and encoding")
    }
}

fn video_format(display: &nuvira_capture::Display) -> ControlMessage {
    control(Msg::VideoFormat(VideoFormat {
        codec: Codec::H264 as i32,
        width: display.width,
        height: display.height,
    }))
}

/// List of screens for the viewer (none with the tests' synthetic image).
fn displays_message(config: &AgentConfig, current: &nuvira_capture::Display) -> Option<ControlMessage> {
    if config.synthetic.is_some() {
        return None;
    }
    let list = nuvira_capture::displays().ok()?;
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

fn screen_rect(display: &nuvira_capture::Display) -> nuvira_input::ScreenRect {
    nuvira_input::ScreenRect {
        left: display.left,
        top: display.top,
        width: display.width,
        height: display.height,
    }
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

/// The keys of a Clipboard.paste, pressed and released in order.
fn paste_keys(paste: nuvira_proto::v1::Paste) -> Vec<InputEvent> {
    use nuvira_proto::v1::Paste;
    let combo: &[&str] = match paste {
        Paste::None => &[],
        Paste::V => &["KeyV"],
        Paste::Shortcut if cfg!(target_os = "macos") => &["MetaLeft", "KeyV"],
        Paste::Shortcut => &["ControlLeft", "KeyV"],
        Paste::Terminal => &["ControlLeft", "ShiftLeft", "KeyV"],
    };
    let key = |code: &str, down| InputEvent {
        event: Some(nuvira_proto::v1::input_event::Event::Key(nuvira_proto::v1::Key { code: code.into(), down })),
    };
    let downs = combo.iter().map(|c| key(c, true));
    let ups = combo.iter().rev().map(|c| key(c, false));
    downs.chain(ups).collect()
}

/// What the input thread receives.
enum InputCmd {
    Event(InputEvent),
    /// Screen change: mouse coordinates refer to this one.
    Screen(nuvira_input::ScreenRect),
}

/// Injects input on a dedicated thread, which can follow the active desktop
/// (UAC, lock screen) without touching tokio's threads. Closing the channel
/// ends the thread, which releases the keys still pressed.
fn input_thread(
    mut injector: Box<dyn nuvira_input::InputInjector>,
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
            if let Some(name) = nuvira_capture::desktop::follow_input_desktop() {
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

/// What the viewer may do: what the session has, less what is switched off.
/// The remote pointer apart from the picture (`Hello.cursor`; Linux, Windows, macOS):
/// what to ask the capture for, when it must be drawn into the picture after all,
/// and the messages for a viewer that draws it.
mod pointer {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use nuvira_pointer as cursor;
    use nuvira_proto::v1::control_message::Msg;

    static RECORDING: AtomicBool = AtomicBool::new(false);
    static PLAIN_GUESTS: AtomicUsize = AtomicUsize::new(0);

    /// The capture can give it apart. Linux: also with the DMA-BUF capture (the
    /// stream goes to shared memory while the pointer must be in the picture). Windows:
    /// DXGI gives it apart (the GDI capture draws it in and says so). macOS: the
    /// system gives its shape (otherwise ScreenCaptureKit draws it in and the
    /// capture says so).
    pub fn possible(plan: nuvira_codec::EncoderPlan) -> bool {
        let _ = plan;
        cfg!(any(windows, target_os = "linux", target_os = "macos"))
    }

    pub fn set_wanted(on: bool) {
        cursor::set_wanted(on);
    }

    /// At the end of the session: back to the pointer in the picture.
    pub struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_wanted(false);
            set_recording(false);
        }
    }

    fn update() {
        cursor::set_composite(RECORDING.load(Ordering::Relaxed) || PLAIN_GUESTS.load(Ordering::Relaxed) > 0);
    }

    /// The viewer records the session: the pointer goes into the picture.
    pub fn set_recording(on: bool) {
        RECORDING.store(on, Ordering::Relaxed);
        update();
    }

    /// A guest that cannot draw the pointer joins (or leaves): it needs it in the
    /// picture.
    pub fn plain_guest(joined: bool) {
        if joined {
            PLAIN_GUESTS.fetch_add(1, Ordering::Relaxed);
        } else {
            let _ = PLAIN_GUESTS.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        }
        update();
    }

    /// How many shapes a viewer with `Hello.cursor_cache` keeps at least.
    const KEPT: usize = 32;

    /// The pointer's messages for one viewer, when they change.
    #[derive(Default)]
    pub struct Sender {
        generation: Option<u64>,
        serial: u32,
        shown: bool,
        /// The viewer keeps the shapes it got (`Hello.cursor_cache`): their serials,
        /// the oldest first, and they go as PNG.
        cache: bool,
        sent: std::collections::VecDeque<u32>,
    }

    impl Sender {
        pub fn new(cache: bool) -> Self {
            Self { cache, ..Self::default() }
        }

        pub fn poll(&mut self) -> Vec<Msg> {
            let generation = cursor::generation();
            if self.generation == Some(generation) {
                return Vec::new();
            }
            self.generation = Some(generation);
            let mut out = Vec::new();
            // In the picture (a recording, a guest, a capture without it): the
            // viewer hides its own.
            if !cursor::active() || cursor::composite() {
                if self.shown {
                    self.shown = false;
                    out.push(Msg::CursorPosition(nuvira_proto::v1::CursorPosition::default()));
                }
                return out;
            }
            let (shape, pos) = cursor::latest();
            if let Some(s) = shape
                && s.serial != self.serial
            {
                self.serial = s.serial;
                // A shape the viewer keeps is named by the position alone.
                if !(self.cache && self.sent.contains(&s.serial)) {
                    let mut msg = nuvira_proto::v1::CursorShape {
                        serial: s.serial,
                        width: s.width,
                        height: s.height,
                        hot_x: s.hot_x,
                        hot_y: s.hot_y,
                        ..Default::default()
                    };
                    let png = if self.cache { cursor::png_of(&s) } else { Vec::new() };
                    if png.is_empty() {
                        msg.bgra = s.bgra.as_ref().clone();
                    } else {
                        msg.png = png;
                    }
                    out.push(Msg::CursorShape(msg));
                    if self.cache {
                        self.sent.push_back(s.serial);
                        if self.sent.len() > KEPT {
                            self.sent.pop_front();
                        }
                    }
                }
            }
            self.shown = pos.visible;
            out.push(Msg::CursorPosition(nuvira_proto::v1::CursorPosition {
                x: pos.x,
                y: pos.y,
                visible: pos.visible,
                serial: pos.serial,
            }));
            out
        }
    }
}
pub(crate) use pointer::{Sender as PointerSender, plain_guest};

fn permissions(offered: u8, off: u8) -> nuvira_proto::v1::Permissions {
    let on = |bit: u8| offered & bit != 0 && off & bit == 0;
    nuvira_proto::v1::Permissions {
        input: on(switches::INPUT),
        clipboard: on(switches::CLIPBOARD),
        files: on(switches::FILES),
        terminal: on(switches::TERMINAL),
        tunnel: on(switches::TUNNEL),
        print: on(switches::PRINT),
        resolution: on(switches::RESOLUTION),
        audio: on(switches::AUDIO),
        switched_off: switches::names(off & offered),
    }
}

/// Sessions, told apart by the screens they change.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Puts back the screens a session changed, however it ends.
struct RestoreScreens(u64);

impl Drop for RestoreScreens {
    fn drop(&mut self) {
        nuvira_capture::resolution::restore(self.0, None);
    }
}

fn recover_screens(dir: &std::path::Path) {
    nuvira_capture::resolution::recover(&dir.join("resolution.txt"));
    nuvira_capture::wallpaper::recover(&dir.join(WALLPAPER_STATE));
}

/// The alias the device keeps (next to the device key), sent at every registration.
const ALIAS_FILE: &str = "alias.txt";

/// Keeps the alias the server confirmed: a server that lost it gets it back at the
/// next registration. One that is not valid gives way to the server's (it never
/// will be); one taken stays, asked for again at every start.
fn keep_alias(dir: &std::path::Path, alias: &str, error: &str) {
    if !error.is_empty() && error != "@server.alias_invalid" {
        return;
    }
    let path = dir.join(ALIAS_FILE);
    if std::fs::read_to_string(&path).map(|a| a.trim().to_string()).unwrap_or_default() == alias {
        return;
    }
    let result = if alias.is_empty() { std::fs::remove_file(&path) } else { std::fs::write(&path, alias) };
    if let Err(e) = result {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("alias not saved: {e}");
        }
    }
}

/// What the wallpaper was, while a session keeps it hidden (`SetWallpaper`).
const WALLPAPER_STATE: &str = "wallpaper.txt";

/// Puts the wallpaper back at the end of the session, however it ends.
struct RestoreWallpaper(Option<PathBuf>);

impl Drop for RestoreWallpaper {
    fn drop(&mut self) {
        if let Some(state) = &self.0 {
            nuvira_capture::wallpaper::restore(state);
        }
    }
}

/// Stops the capture and opens screen `index` (`fallback` if it cannot be opened):
/// after a change of screen or of its resolution.
async fn reopen(
    source: &VideoSource,
    pipeline: &std::sync::Mutex<Option<Pipeline>>,
    index: usize,
    fallback: usize,
) -> Result<nuvira_capture::Display> {
    // First the current capture is stopped, then the new one is opened.
    let old = pipeline.lock().expect("pipeline").take();
    let _ = tokio::task::spawn_blocking(move || drop(old)).await;
    let (display, started) = match source.start(Some(index)).await {
        Ok(started) => started,
        Err(e) if index != fallback => {
            warn!("screen {index} not available: {e:#}; staying on the previous one");
            source.start(Some(fallback)).await?
        }
        Err(e) => return Err(e),
    };
    *pipeline.lock().expect("pipeline") = Some(started);
    Ok(display)
}

/// At the end of a session its switches are gone: the app's commands find nothing.
struct ClearSwitches<'a>(Option<&'a SessionContext>);

impl Drop for ClearSwitches<'_> {
    fn drop(&mut self) {
        if let Some(ctx) = self.0 {
            *ctx.switches.lock().expect("switches") = None;
        }
    }
}

fn local_hello() -> Hello {
    Hello {
        protocol_version: nuvira_proto::PROTOCOL_VERSION,
        device_name: nuvira_transport::device_name(),
        os: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        password_required: false,
        cursor: false,
        cursor_cache: false,
        clipboard_files: false,
        handover: false,
        switch_sides: false,
        lock_on_end: false,
        two_factor: false,
        idle_limit: false,
        wallpaper: false,
        restart_safe_mode: false,
        microphone: false,
        file_manager: false,
        video_datagrams: false,
        privacy_mode: false,
        terminal: false,
        terminal_only: false,
        tunnel: false,
        restart: false,
        restart_users: false,
        text: false,
        paste: false,
        audio: false,
        print: false,
        recording: false,
        recording_required: false,
        resolution: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_allowed_unless_said_otherwise() {
        assert!(serde_json::from_str::<Permissions>("{}").unwrap().privacy);
        assert!(!serde_json::from_str::<Permissions>(r#"{"privacy":false}"#).unwrap().privacy);
    }

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

/// Characterisation tests (Phase 2 of the full review) of the access gate of a
/// session (`Gate::authorize`, `Gate::check_code`): temporary and permanent
/// passwords, profiles, access rules, two-factor codes, hand-over tokens, support
/// tickets, confirmation by the person at the PC, restart. They pin the CURRENT
/// behaviour; the connection around it (Hello, AuthResult, pauses) is the end-to-end
/// tests' part.
#[cfg(test)]
mod gate_characterisation {
    use super::*;
    use nuvira_proto::v1::Policy;
    use std::sync::OnceLock;

    const TEMPORARY: &str = "abcd2345";
    const PERMANENT: &str = "the permanent one";
    const PROFILE: &str = "view only please";
    const VIEWER: &str = "111 222 333";

    /// The PBKDF2 hashes, made once (300 000 iterations each).
    fn hashes() -> &'static (String, String) {
        static H: OnceLock<(String, String)> = OnceLock::new();
        H.get_or_init(|| (nuvira_secret::hash(PERMANENT), nuvira_secret::hash(PROFILE)))
    }

    struct Fixture {
        ctx: SessionContext,
        events: mpsc::UnboundedReceiver<AgentEvent>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.ctx.access_dir);
        }
    }

    fn fixture(tag: &str, permanent: bool, confirm: bool, rules: Option<&str>) -> Fixture {
        let dir = std::env::temp_dir().join(format!("nuvira-p2-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(rules) = rules {
            std::fs::write(dir.join(access::FILE), rules).unwrap();
        }
        let (events, rx) = mpsc::unbounded_channel();
        let ctx = SessionContext {
            config: AgentConfig {
                display: None,
                synthetic: Some((64, 64, 30)),
                bitrate_kbps: 1000,
                fps: 30,
                plan: EncoderPlan::Software,
                fixed_bitrate: false,
                view_only: false,
            },
            password: Arc::new(Mutex::new(TEMPORARY.to_string())),
            permanent: permanent.then(|| hashes().0.clone()),
            confirm,
            pending: Mutex::new(HashMap::new()),
            events,
            chat: Mutex::new(None),
            busy: AtomicU32::new(0),
            attempts: AtomicU32::new(0),
            claims: Mutex::new(()),
            waiting: Mutex::new(None),
            end_session: Notify::new(),
            shared: Mutex::new(None),
            handover: Mutex::new(None),
            policy: Default::default(),
            support_ticket: Mutex::new(None),
            support_request: Mutex::new(None),
            allow_privacy: AtomicBool::new(true),
            allow_terminal: AtomicBool::new(true),
            allow_tunnel: AtomicBool::new(true),
            allow_print: AtomicBool::new(true),
            allow_recording: AtomicBool::new(true),
            lock_on_end: AtomicBool::new(false),
            lock_asked: AtomicBool::new(false),
            idle_minutes: AtomicU32::new(0),
            mic_muted: tokio::sync::watch::channel(false).0,
            access_dir: dir,
            last_code_step: AtomicU64::new(0),
            wrong_codes: Mutex::new(Vec::new()),
            features: Mutex::new(Vec::new()),
            unattended: permanent,
            restarting: Mutex::new(None),
            switches: Mutex::new(None),
        };
        Fixture { ctx, events: rx }
    }

    fn request(viewer: &str, join: bool) -> Request {
        Request { viewer_id: viewer.into(), viewer_name: "Tech".into(), limits: Policy::default(), join }
    }

    fn password(p: &str) -> Auth {
        Auth { password: p.into(), ..Default::default() }
    }

    /// What `authorize` said, the access noted for the report, the profile's switches off.
    struct Verdict {
        result: Result<(), String>,
        access: &'static str,
        profile_off: u8,
    }

    fn gate<'a>(
        ctx: &'a SessionContext,
        request: &'a Request,
        access: &'a std::sync::Mutex<&'static str>,
        session_id: &'a str,
    ) -> Gate<'a> {
        Gate {
            ctx,
            session_id,
            request,
            direct: true,
            path: "direct",
            access,
            terminal_only: AtomicBool::new(false),
            profile_off: AtomicU8::new(0),
            claimed: AtomicBool::new(false),
        }
    }

    async fn authorize(ctx: &SessionContext, request: &Request, auth: &Auth) -> Verdict {
        let access = std::sync::Mutex::new("");
        let gate = gate(ctx, request, &access, "s1");
        let result = gate.authorize(auth, None).await;
        let profile_off = gate.profile_off.load(Ordering::Relaxed);
        drop(gate);
        Verdict { result, access: *access.lock().unwrap(), profile_off }
    }

    /// `authorize` while answering the confirmation request with `answer` (`None`:
    /// no answer). Returns the verdict and the events seen.
    async fn authorize_answering(
        f: &mut Fixture,
        request: &Request,
        auth: &Auth,
        answer: Option<bool>,
    ) -> (Verdict, Vec<String>) {
        let (ctx, events) = (&f.ctx, &mut f.events);
        let seen = std::cell::RefCell::new(Vec::new());
        let respond = async {
            while let Some(event) = events.recv().await {
                let name = serde_json::to_value(&event).unwrap()["event"].as_str().unwrap().to_string();
                seen.borrow_mut().push(name);
                match event {
                    AgentEvent::Incoming { session_id, .. } => {
                        if let Some(answer) = answer {
                            let tx = ctx.pending.lock().unwrap().remove(&session_id).unwrap();
                            let _ = tx.send(answer);
                        }
                    }
                    AgentEvent::IncomingClosed { .. } => break,
                    _ => {}
                }
            }
        };
        let (verdict, ()) = tokio::join!(authorize(ctx, request, auth), async {
            // Stops once authorize is done and nothing more comes.
            let _ = tokio::time::timeout(CONFIRM_TIMEOUT + Duration::from_secs(5), respond).await;
        });
        while let Ok(event) = f.events.try_recv() {
            seen.borrow_mut().push(serde_json::to_value(&event).unwrap()["event"].as_str().unwrap().to_string());
        }
        (verdict, seen.into_inner())
    }

    fn drain(f: &mut Fixture) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = f.events.try_recv() {
            out.push(serde_json::to_value(&event).unwrap()["event"].as_str().unwrap().to_string());
        }
        out
    }

    fn code_now(secret: &str) -> String {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        nuvira_secret::totp_code(secret, now / nuvira_secret::TOTP_STEP).unwrap()
    }

    #[tokio::test]
    async fn temporary_password() {
        let mut f = fixture("temp", false, false, None);
        let req = request(VIEWER, false);
        let v = authorize(&f.ctx, &req, &password(TEMPORARY)).await;
        assert_eq!((v.result, v.access), (Ok(()), "temporary"));
        assert_eq!(drain(&mut f), ["session_started"]);
        for wrong in ["", "ABCD2345", "abcd234", "abcd23456", " abcd2345"] {
            let v = authorize(&f.ctx, &req, &password(wrong)).await;
            assert_eq!((v.result, v.access), (Err(WRONG_PASSWORD.into()), "wrong_password"), "{wrong:?}");
        }
        assert!(drain(&mut f).is_empty());
    }

    #[tokio::test]
    async fn characterises_wrong_passwords_are_never_locked_out() {
        // KNOWN LIMIT (Phase 2): the gate counts wrong two-factor codes, not wrong
        // passwords: after any number of wrong temporary or permanent passwords
        // the right one still works. The only brake is the 2 s pause before
        // `session()` closes a connection after a wrong password.
        // Expected after a fix (if one is decided): a lockout or growing delay.
        let f = fixture("nolock", true, false, None);
        let req = request(VIEWER, false);
        // 20 here: each wrong guess also costs a PBKDF2 check (300 000 iterations)
        // against the permanent hash, slow in debug builds.
        for i in 0..20 {
            let v = authorize(&f.ctx, &req, &password(&format!("guess{i}"))).await;
            assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
        }
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
        assert_eq!(authorize(&f.ctx, &req, &password(PERMANENT)).await.result, Ok(()));
    }

    #[tokio::test]
    async fn permanent_password_and_profiles() {
        let rules = serde_json::json!({
            "profiles": [{ "name": "View", "password_hash": hashes().1, "off": ["input", "files"] }]
        })
        .to_string();
        let mut f = fixture("perm", true, true, Some(&rules));
        let req = request(VIEWER, false);
        // The permanent password is never confirmed, also when ctx.confirm is set.
        let v = authorize(&f.ctx, &req, &password(PERMANENT)).await;
        assert_eq!((v.result, v.access, v.profile_off), (Ok(()), "permanent", 0));
        let v = authorize(&f.ctx, &req, &password(PROFILE)).await;
        let off = switches::bit("input").unwrap() | switches::bit("files").unwrap();
        assert_eq!((v.result, v.access, v.profile_off), (Ok(()), "profile", off));
        assert_eq!(drain(&mut f), ["session_started", "session_started"]);
        // Without a main permanent password, a profile's password is not one: it is
        // compared with the temporary password.
        let f2 = fixture("perm-none", false, false, Some(&rules));
        let v = authorize(&f2.ctx, &req, &password(PROFILE)).await;
        assert_eq!((v.result, v.access), (Err(WRONG_PASSWORD.into()), "wrong_password"));
        let v = authorize(&f2.ctx, &req, &password(PERMANENT)).await;
        assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
    }

    #[tokio::test]
    async fn server_rule_confirm_always_asks_also_for_the_permanent_password() {
        let mut f = fixture("always", true, false, None);
        *f.ctx.policy.lock().unwrap() = Policy { confirm_always: true, ..Default::default() };
        let req = request(VIEWER, false);
        let (v, events) = authorize_answering(&mut f, &req, &password(PERMANENT), Some(true)).await;
        assert_eq!((v.result, v.access), (Ok(()), "confirmed"));
        assert_eq!(events, ["incoming", "incoming_closed", "session_started"]);
        // The same rule from the session's limits (Incoming.limits).
        let mut f = fixture("always-limits", true, false, None);
        let req = Request { limits: Policy { confirm_always: true, ..Default::default() }, ..request(VIEWER, false) };
        let (v, _) = authorize_answering(&mut f, &req, &password(TEMPORARY), Some(false)).await;
        assert_eq!((v.result, v.access), (Err("@agent.not_accepted".into()), "not_accepted"));
    }

    #[tokio::test(start_paused = true)]
    async fn confirmation_by_the_person_at_the_pc() {
        let req = request(VIEWER, false);
        let mut f = fixture("confirm", false, true, None);
        let (v, events) = authorize_answering(&mut f, &req, &password(TEMPORARY), Some(true)).await;
        assert_eq!((v.result, v.access), (Ok(()), "confirmed"));
        assert_eq!(events, ["incoming", "incoming_closed", "session_started"]);
        let (v, events) = authorize_answering(&mut f, &req, &password(TEMPORARY), Some(false)).await;
        assert_eq!((v.result, v.access), (Err("@agent.not_accepted".into()), "not_accepted"));
        assert_eq!(events, ["incoming", "incoming_closed"]);
        // No answer within CONFIRM_TIMEOUT (the clock is paused: no real wait).
        let start = tokio::time::Instant::now();
        let (v, events) = authorize_answering(&mut f, &req, &password(TEMPORARY), None).await;
        assert_eq!((v.result, v.access), (Err("@agent.no_answer".into()), "no_answer"));
        assert_eq!(events, ["incoming", "incoming_closed"]);
        assert!(start.elapsed() >= CONFIRM_TIMEOUT);
        assert!(f.ctx.pending.lock().unwrap().is_empty());
        // A wrong password is refused before anyone is asked.
        let (v, events) = authorize_answering(&mut f, &req, &password("nope"), Some(true)).await;
        assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn allow_list() {
        let only = r#"{"allow":[{"id":"999 888 777"}]}"#;
        let attended_too = r#"{"allow":[{"id":"999888777"}],"allow_attended":true}"#;
        let (other, listed) = (request(VIEWER, false), request("999-888-777", false));

        let f = fixture("allow", true, false, Some(only));
        let v = authorize(&f.ctx, &other, &password(PERMANENT)).await;
        assert_eq!((v.result, v.access), (Err(NOT_ALLOWED.into()), "not_allowed"));
        assert_eq!(authorize(&f.ctx, &listed, &password(PERMANENT)).await.result, Ok(()));
        // Without allow_attended the temporary password works from any computer.
        assert_eq!(authorize(&f.ctx, &other, &password(TEMPORARY)).await.result, Ok(()));

        let f = fixture("allow-att", true, false, Some(attended_too));
        let v = authorize(&f.ctx, &other, &password(TEMPORARY)).await;
        assert_eq!((v.result, v.access), (Err(NOT_ALLOWED.into()), "not_allowed"));
        // The password is checked first: a wrong one from such a computer is "wrong".
        let v = authorize(&f.ctx, &other, &password("nope")).await;
        assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
        assert_eq!(authorize(&f.ctx, &listed, &password(TEMPORARY)).await.result, Ok(()));
        // An empty viewer ID is never on the list.
        let v = authorize(&f.ctx, &request("", false), &password(PERMANENT)).await;
        assert_eq!(v.result, Err(NOT_ALLOWED.into()));
    }

    #[tokio::test]
    async fn characterises_unreadable_rules_refuse_only_permanent_passwords() {
        // An access.json that does not parse refuses every permanent password
        // (documented in access.rs). The temporary password is still accepted from
        // any computer, even if the broken file meant to restrict it
        // (`allow_attended`): `attended_allowed` falls back to true.
        let f = fixture("broken", true, false, Some(r#"{"allow":[{"id":"999888777"}],"allow_attended":true,"#));
        let req = request(VIEWER, false);
        let v = authorize(&f.ctx, &req, &password(PERMANENT)).await;
        assert_eq!((v.result, v.access), (Err("@agent.access_rules_invalid".into()), "not_allowed"));
        let v = authorize(&f.ctx, &req, &password(TEMPORARY)).await;
        assert_eq!((v.result, v.access), (Ok(()), "temporary"));
        // A UTF-8 BOM at the start is accepted (Notepad).
        let f = fixture("bom", true, false, Some("\u{feff}{\"allow\":[{\"id\":\"999888777\"}]}"));
        assert_eq!(authorize(&f.ctx, &req, &password(PERMANENT)).await.result, Err(NOT_ALLOWED.into()));
    }

    #[tokio::test]
    async fn two_factor_codes() {
        let secret = nuvira_secret::totp_secret();
        let rules = serde_json::json!({ "totp": secret }).to_string();
        let f = fixture("totp", true, false, Some(&rules));
        let req = request(VIEWER, false);
        let with_code = |code: &str| Auth { password: PERMANENT.into(), code: code.into(), ..Default::default() };
        // No code: asked for (not counted as wrong).
        for _ in 0..10 {
            assert_eq!(authorize(&f.ctx, &req, &with_code(" ")).await.result, Err(CODE_REQUIRED.into()));
        }
        assert!(f.ctx.wrong_codes.lock().unwrap().is_empty());
        // The temporary password needs no code.
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
        // A right code works once.
        let code = code_now(&secret);
        let v = authorize(&f.ctx, &req, &with_code(&code)).await;
        assert_eq!((v.result, v.access), (Ok(()), "permanent"));
        let v = authorize(&f.ctx, &req, &with_code(&code)).await;
        assert_eq!((v.result, v.access), (Err("@agent.code_wrong".into()), "wrong_code"), "already used");
        // A wrong password with a code: the password is wrong, the code not looked at.
        let v = authorize(&f.ctx, &req, &Auth { password: "nope".into(), code: "000000".into(), ..Default::default() })
            .await;
        assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
        assert_eq!(f.ctx.wrong_codes.lock().unwrap().len(), 1, "only the reused code");
    }

    #[tokio::test]
    async fn five_wrong_codes_lock_the_permanent_passwords() {
        let secret = nuvira_secret::totp_secret();
        let rules = serde_json::json!({ "totp": secret }).to_string();
        let f = fixture("totp-lock", true, false, Some(&rules));
        let req = request(VIEWER, false);
        let with_code = |code: &str| Auth { password: PERMANENT.into(), code: code.into(), ..Default::default() };
        let right = code_now(&secret);
        let wrong = if right == "123456" { "654321" } else { "123456" };
        for _ in 0..CODE_MAX_WRONG {
            assert_eq!(authorize(&f.ctx, &req, &with_code(wrong)).await.result, Err("@agent.code_wrong".into()));
        }
        // Locked: even the right code, and even an empty one, is refused.
        let v = authorize(&f.ctx, &req, &with_code(&right)).await;
        assert_eq!((v.result, v.access), (Err("@agent.code_locked".into()), "code_locked"));
        assert_eq!(authorize(&f.ctx, &req, &with_code("")).await.result, Err("@agent.code_locked".into()));
        // The lock does not grow while locked.
        assert_eq!(f.ctx.wrong_codes.lock().unwrap().len(), CODE_MAX_WRONG);
        // The temporary password is not locked.
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
        // After CODE_WINDOW the wrong codes are forgotten.
        let long_ago = Instant::now().checked_sub(CODE_WINDOW + Duration::from_secs(1)).unwrap();
        f.ctx.wrong_codes.lock().unwrap().iter_mut().for_each(|t| *t = long_ago);
        assert_eq!(authorize(&f.ctx, &req, &with_code(&right)).await.result, Ok(()));
    }

    #[tokio::test]
    async fn hand_over_tokens() {
        let mut f = fixture("handover", false, true, None);
        let token = "token-0123456789";
        let set = |f: &Fixture, viewer: &str, age: Duration| {
            *f.ctx.handover.lock().unwrap() = Some(shared::Handover {
                viewer_id: viewer.into(),
                token: token.into(),
                at: Instant::now().checked_sub(age).unwrap(),
            });
        };
        let with = |t: &str| Auth { handover: t.into(), ..Default::default() };
        let req = request(VIEWER, false);
        // Right viewer, right token, in time: no confirmation (ctx.confirm is set),
        // and only once.
        set(&f, VIEWER, Duration::ZERO);
        let v = authorize(&f.ctx, &req, &with(token)).await;
        assert_eq!((v.result, v.access), (Ok(()), "handover"));
        assert_eq!(drain(&mut f), ["session_started"]);
        assert!(f.ctx.handover.lock().unwrap().is_none());
        assert_eq!(authorize(&f.ctx, &req, &with(token)).await.result, Err(WRONG_PASSWORD.into()));
        // Wrong token, another viewer (IDs compared as given), too late, or a join.
        for (viewer, age, auth, join) in [
            (VIEWER, Duration::ZERO, with("token-x"), false),
            ("111222333", Duration::ZERO, with(token), false),
            (VIEWER, shared::HANDOVER_VALID, with(token), false),
            (VIEWER, Duration::ZERO, with(token), true),
        ] {
            set(&f, viewer, age);
            let v = authorize(&f.ctx, &request(VIEWER, join), &auth).await;
            assert_eq!((v.result, v.access), (Err(WRONG_PASSWORD.into()), "wrong_password"), "{viewer} {age:?}");
            // A failed attempt leaves the token there.
            assert!(f.ctx.handover.lock().unwrap().is_some());
        }
        // A token beats the password: a right temporary password with a wrong token is refused.
        set(&f, VIEWER, Duration::ZERO);
        let auth = Auth { password: TEMPORARY.into(), handover: "bad".into(), ..Default::default() };
        assert_eq!(authorize(&f.ctx, &req, &auth).await.result, Err(WRONG_PASSWORD.into()));
    }

    #[tokio::test]
    async fn hand_over_and_ticket_follow_allow_attended() {
        let f = fixture("handover-allow", false, false, Some(r#"{"allow":[{"id":"999888777"}],"allow_attended":true}"#));
        *f.ctx.handover.lock().unwrap() =
            Some(shared::Handover { viewer_id: VIEWER.into(), token: "t".into(), at: Instant::now() });
        let req = request(VIEWER, false);
        let v = authorize(&f.ctx, &req, &Auth { handover: "t".into(), ..Default::default() }).await;
        assert_eq!((v.result, v.access), (Err(NOT_ALLOWED.into()), "not_allowed"));
        let v = authorize(&f.ctx, &req, &Auth { support_ticket: "x".into(), ..Default::default() }).await;
        assert_eq!(v.result, Err(NOT_ALLOWED.into()));
    }

    #[tokio::test]
    async fn support_tickets_still_ask_the_person_at_the_pc() {
        let mut f = fixture("ticket", false, false, None);
        *f.ctx.support_ticket.lock().unwrap() = Some("ticket-1".into());
        let req = request(VIEWER, false);
        let with = |t: &str| Auth { support_ticket: t.into(), ..Default::default() };
        let v = authorize(&f.ctx, &req, &with("ticket-2")).await;
        assert_eq!((v.result, v.access), (Err("@agent.support_ticket_invalid".into()), "wrong_password"));
        // The right ticket: asked even though ctx.confirm is off; used up at once.
        let (v, events) = authorize_answering(&mut f, &req, &with("ticket-1"), Some(true)).await;
        assert_eq!((v.result, v.access), (Ok(()), "confirmed"));
        assert_eq!(events, ["incoming", "incoming_closed", "session_started"]);
        assert!(f.ctx.support_ticket.lock().unwrap().is_none());
        let v = authorize(&f.ctx, &req, &with("ticket-1")).await;
        assert_eq!(v.result, Err("@agent.support_ticket_invalid".into()));
    }

    #[tokio::test]
    async fn a_restart_under_way_refuses_everyone_for_three_minutes() {
        let f = fixture("restart", true, false, None);
        let req = request(VIEWER, false);
        *f.ctx.restarting.lock().unwrap() = Some(Instant::now());
        for p in [TEMPORARY, PERMANENT, "wrong"] {
            assert_eq!(authorize(&f.ctx, &req, &password(p)).await.result, Err("@agent.restarting".into()));
        }
        let given_up = Instant::now().checked_sub(RESTART_GIVEN_UP).unwrap();
        *f.ctx.restarting.lock().unwrap() = Some(given_up);
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
    }

    #[tokio::test]
    async fn one_session_is_claimed_at_a_time_once_the_password_is_right() {
        // Review AGT-1 (Phase 6 lot 2): a viewer is not counted as busy before its
        // credentials are right; the first right one takes the computer, a second
        // one is refused as busy until the first ends; a guest is not counted.
        let mut f = fixture("claim", false, false, None);
        let (req, other) = (request(VIEWER, false), request("444 555 666", false));
        let (a1, a2, a3) = (Mutex::new(""), Mutex::new(""), Mutex::new(""));
        let first = gate(&f.ctx, &req, &a1, "s1");
        assert_eq!(first.authorize(&password("nope"), None).await, Err(WRONG_PASSWORD.into()));
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0, "a wrong password claims nothing");
        assert_eq!(first.authorize(&password(TEMPORARY), None).await, Ok(()));
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 1);
        let second = gate(&f.ctx, &other, &a2, "s2");
        assert_eq!(second.authorize(&password(TEMPORARY), None).await, Err("@agent.busy".into()));
        let guest_req = request("444 555 666", true);
        let guest = gate(&f.ctx, &guest_req, &a3, "s3");
        assert_eq!(guest.authorize(&password(TEMPORARY), None).await, Ok(()));
        drop(guest);
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 1);
        // The first session ends: the next one is let in.
        drop(first);
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0);
        assert_eq!(second.authorize(&password(TEMPORARY), None).await, Ok(()));
        drop(second);
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0);
        assert_eq!(drain(&mut f), ["session_started", "session_started"]);
    }

    #[tokio::test]
    async fn the_person_at_the_pc_is_asked_about_one_viewer_at_a_time() {
        // Review AGT-1: the session is claimed before the confirmation, so a second
        // viewer with the right password while the first waits is refused as busy
        // without asking the person at the PC again.
        let mut f = fixture("claim-confirm", false, true, None);
        let (req, other) = (request(VIEWER, false), request("444 555 666", false));
        let (a1, a2) = (Mutex::new(""), Mutex::new(""));
        let first = gate(&f.ctx, &req, &a1, "s1");
        let second = gate(&f.ctx, &other, &a2, "s2");
        let (ctx, events) = (&f.ctx, &mut f.events);
        let right = password(TEMPORARY);
        let (r1, r2) = tokio::join!(first.authorize(&right, None), async {
            let Some(AgentEvent::Incoming { session_id, .. }) = events.recv().await else {
                panic!("the first viewer was not asked about");
            };
            assert_eq!(session_id, "s1");
            let r2 = second.authorize(&right, None).await;
            ctx.pending.lock().unwrap().remove("s1").unwrap().send(true).unwrap();
            r2
        });
        assert_eq!((r1, r2), (Ok(()), Err("@agent.busy".into())));
        drop((first, second));
        assert_eq!(drain(&mut f), ["incoming_closed", "session_started"]);
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_guest_joining_is_not_announced_as_a_session() {
        let mut f = fixture("join", false, false, None);
        let v = authorize(&f.ctx, &request(VIEWER, true), &password(TEMPORARY)).await;
        assert_eq!((v.result, v.access), (Ok(()), "temporary"));
        assert!(drain(&mut f).is_empty(), "no SessionStarted for a guest");
    }
}
