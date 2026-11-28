//! NuviraHub agent — the controlled device.
//!
//! Library used by the `nuvira` app and by the `nuvira-agent` command-line
//! program: it registers with nuvira-server (or waits for direct LAN
//! connections), streams the screen and applies the mouse and keyboard input it
//! receives. With the server it talks to its user through [`AgentEvent`] and [`AgentCommand`].

mod abr;
pub mod access;
pub mod cli;
mod connected;
#[cfg(any(windows, test))]
mod display_awake;
#[cfg(windows)]
pub mod clip_user;
mod ending;
mod files;
mod fsops;
mod gate;
mod guesses;
mod handshake;
mod input;
#[cfg(any(windows, unix))]
pub mod ipc;
mod inventory;
mod lan;
mod netscan;
mod lock;
mod pipeline;
mod policy;
mod printer;
mod pty;
mod reopen;
mod restart;
pub mod reverse;
mod session_stats;
mod shared;
mod signalling;
mod switches;
mod terminal;
mod tunnel;
mod video;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nuvira_capture::{Capturer, SyntheticCapturer};
use nuvira_codec::{EncoderPlan, EncoderSettings};
use nuvira_proto::v1::{ControlMessage, control_message::Msg};
use nuvira_transport::signal::TransportMode;
use nuvira_transport::Fingerprint;
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use connected::{permissions, session};
use ending::{ENDED_AT_COMPUTER, close_reason, normal_end};
use gate::{Gate, authorize_with_code};
use handshake::{control_channel, local_hello, read_auth, read_hello, watched};
use policy::merge_limits;
use signalling::signal_loop;
use video::{recover_screens, reopen, video_format};

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
/// Largest message accepted on the control stream before the viewer has
/// authenticated (a Hello is about 100 bytes) and on the input stream (events are
/// a few bytes, a typed text a few characters); after authentication the control
/// stream takes [`nuvira_proto::framing::MAX_MESSAGE_LEN`] (clipboard texts).
pub const HANDSHAKE_MESSAGE_LIMIT: usize = 64 * 1024;

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
                let capturer = nuvira_capture::open(self.display)?;
                // Windows: the display stays on while the session captures it (VID-10).
                #[cfg(windows)]
                let capturer = display_awake::wrap(capturer);
                Ok(capturer)
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
        /// The computer is never announced on the local network.
        #[serde(default)]
        lan_off: bool,
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
    /// The person at the PC lets this computer be announced on the local network
    /// or not ("On this network" in the apps there; kept like `AllowPrivacy`).
    LanVisible {
        visible: bool,
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
    /// Announced on the local network ("On this network" in the apps there).
    #[serde(default = "yes")]
    lan: bool,
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
                lan: true,
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

/// Random 8-character password that is easy to dictate (no 0/O, 1/l/I).
pub use nuvira_secret::random_password;

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
        let result = normal_end(result);
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
    // On a blocking thread: it waits while the system keeps the key locked (a
    // keyring not opened yet at boot), which must not hold a runtime worker
    // (review NET-14).
    let identity_path = opts.identity_path.clone();
    let identity = tokio::task::spawn_blocking(move || {
        nuvira_transport::identity::Identity::load_or_create(&identity_path)
    })
    .await??;
    let identity = Arc::new(identity);
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
        ends: Default::default(),
        hosts: Default::default(),
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
        allow_lan: AtomicBool::new(permissions.lan),
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
        guesses: Mutex::new(Default::default()),
        reports: Default::default(),
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
pub(crate) mod stats;

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
    /// How each viewer's last session ended: an automatic reopen after a normal end
    /// is refused (`Hello.reopen`, see `reopen`).
    ends: reopen::Ends,
    /// The viewers whose session is in progress, and when each was last heard.
    hosts: reopen::Hosts,
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
    /// The person at the PC lets the computer be announced on the local network
    /// ([`AgentCommand::LanVisible`]).
    allow_lan: AtomicBool,
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
    /// Wrong passwords, by viewer and in all ([`guesses`]).
    guesses: Mutex<guesses::Guesses>,
    /// End-of-session reports not given to the server yet ([`Reports`]).
    reports: Reports,
}

/// The end-of-session reports waiting for the connection to the server. They
/// outlive it: a session goes on while the server is unreachable (a network cut,
/// the server restarted), and its report is sent once the agent is registered
/// again; the server takes it for 48 hours.
struct Reports {
    tx: mpsc::UnboundedSender<nuvira_proto::v1::SessionReport>,
    /// Taken by the one connection to the server in use.
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<nuvira_proto::v1::SessionReport>>,
}

impl Default for Reports {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Reports { tx, rx: tokio::sync::Mutex::new(rx) }
    }
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
            lan: self.allow_lan.load(Ordering::Relaxed),
        }
        .save();
    }
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

/// See [`SessionContext::restarting`].
const RESTART_GIVEN_UP: Duration = Duration::from_secs(180);

/// Switch sides: the technician's computer shows its screen to the person at the
/// controlled one, inside the session already open (`nuvira_transport::nested`).
/// The one viewer that connects must give `password`; it gets the picture, the
/// keyboard and the mouse, nothing else (`reverse_policy`).
pub struct Reverse {
    pub password: String,
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
    normal_end(result)
}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

/// What the viewer may do: what the session has, less what is switched off.
mod pointer;
pub(crate) use pointer::{Sender as PointerSender, plain_guest};

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
