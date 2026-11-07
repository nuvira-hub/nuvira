//! Network side of the viewer: connection to the agent, video reception and
//! decoding, input sending and measurement of RTT and clock difference.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_codec::YuvFrame;
use nuvira_proto::v1::{
    ControlMessage, DisplayInfo, FileChunk, Hello, InputEvent, Ping, RequestKeyframe, VideoFrame,
    control_message::Msg,
};
use nuvira_transport::{Fingerprint, FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::{info, warn};
use winit::event_loop::EventLoopProxy;

/// Events from the network to the window.
pub enum NetEvent {
    /// The person at the remote computer is being asked to accept.
    ConfirmationPending,
    Connected {
        agent: String,
        /// Direct path; otherwise through the server's relay.
        direct: bool,
        width: u32,
        height: u32,
        /// The agent answers the file manager.
        file_manager: bool,
        /// Ctrl+Alt+Del means something there: only on Windows.
        secure_attention: bool,
        /// The remote computer is a Mac: from a Mac, Cmd and Ctrl stay what they are.
        remote_mac: bool,
        /// The agent can turn on privacy mode.
        privacy: bool,
        /// The agent opens remote terminals.
        terminal: bool,
        /// The agent opens TCP tunnels (NuviraHub Pro).
        tunnel: bool,
        /// The agent can restart the computer.
        restart: bool,
        /// What is printed on the remote computer's NuviraHub printer arrives here.
        print: bool,
        /// The person at the remote computer allows recording the session.
        recording: bool,
        /// The server's rules want the session recorded: from the start, no stopping.
        recording_required: bool,
    },
    /// A document printed on the remote computer (PDF).
    PrintJob {
        name: String,
        data: Vec<u8>,
    },
    /// Message of the print window's page.
    PrintPage(String),
    /// A remote document was printed here (`error` empty) or not.
    Printed {
        name: String,
        printer: String,
        error: String,
    },
    /// A tunnel listens on `local_port` of this computer, or could not (`error`).
    TunnelListening {
        id: u32,
        local_port: u16,
        error: String,
    },
    /// A connection through a tunnel opened (`error` empty) or was refused by the agent.
    TunnelConnection {
        id: u32,
        error: String,
    },
    /// Message of the tunnels window's page.
    TunnelPage(String),
    /// Output of a remote terminal.
    TerminalOutput {
        id: u32,
        data: Vec<u8>,
    },
    /// A remote terminal ended; `error` (translated) when it did not start or broke.
    TerminalExit {
        id: u32,
        code: i32,
        error: String,
    },
    /// Message of a terminal window's page.
    TerminalPage {
        window: winit::window::WindowId,
        body: String,
    },
    /// What the session may do now (Permissions from the agent): at the start and
    /// whenever the person at the remote computer switches something off or on.
    Permissions {
        input: bool,
        files: bool,
        terminal: bool,
        tunnel: bool,
        /// Switched off by the person there ("input", "clipboard", "files", …).
        switched_off: Vec<String>,
    },
    /// The remote screen is locked and cannot be shown (macOS), or no longer.
    ScreenLocked(bool),
    /// Privacy mode reached by the agent; `error` ("@key" or text) when it did not turn on.
    Privacy {
        on: bool,
        error: String,
    },
    /// Recording state from the agent (the person there was told); `error` ("@key"
    /// or text) when it refused or stopped it.
    Recording {
        on: bool,
        error: String,
    },
    /// The decoding thread closed a recording: the files written, or why it broke.
    RecordingSaved {
        files: Vec<std::path::PathBuf>,
        error: Option<String>,
    },
    /// Answer of the agent to a restart request: with `ok` the computer is restarting,
    /// with `reconnect` the viewer can connect again by itself once it is back.
    Restart {
        ok: bool,
        error: String,
        reconnect: bool,
    },
    /// There is a new frame to show in the [`FrameSlot`].
    FrameReady,
    Clock {
        rtt_us: u64,
        offset_us: i64,
    },
    /// The session ended, with the reason; `retry` when the computer may only be
    /// unreachable for now (e.g. restarting), not when the agent refused the access.
    Disconnected {
        reason: String,
        retry: bool,
    },
    /// Time to try again to reach a computer that was restarting.
    Reconnect,
    /// The duration set with `--duration` is over.
    TestFinished,
    /// Screens of the agent and the one being streamed.
    Displays {
        displays: Vec<DisplayInfo>,
        current: u32,
        /// The agent can stream all the screens at once.
        all_available: bool,
    },
    /// Progress of a file being sent.
    FileProgress {
        name: String,
        sent: u64,
        total: u64,
    },
    /// Outcome of a file sent (from the agent, or an error while sending).
    FileDone {
        name: String,
        ok: bool,
        message: String,
    },
    /// Files picked with the "Send files" button.
    FilesPicked(Vec<std::path::PathBuf>),
    /// Chat message from the person at the remote PC.
    Chat(String),
    /// Click on the toolbar.
    Toolbar(crate::toolbar::Action),
    /// File manager: answer of the agent to a request.
    Fs(nuvira_proto::v1::FsResponse),
    /// File manager: progress of a transfer (both directions).
    Transfer {
        id: u32,
        done: u64,
        total: u64,
    },
    /// File manager: end of a transfer; with `ok` the final path, otherwise the reason.
    TransferDone {
        id: u32,
        ok: bool,
        message: String,
    },
    /// Message of the file manager page.
    FileManager(String),
}

/// What to send to the agent on its own stream.
pub enum Outgoing {
    /// "Send files": into the remote Downloads folder.
    File(std::path::PathBuf),
    /// File manager: into `dir`, from `offset` on (resume).
    Upload { id: u32, local: std::path::PathBuf, dir: String, offset: u64 },
}

/// Downloads in progress: request id → local file to write.
pub type Downloads = Arc<std::sync::Mutex<std::collections::HashMap<u32, std::path::PathBuf>>>;

pub struct DecodedFrame {
    pub yuv: YuvFrame,
    /// Capture time according to the agent's clock.
    pub capture_us: u64,
    pub encode_us: u32,
    /// End of encoding according to the agent's clock.
    pub encoded_us: u64,
    /// Arrival time of the complete frame, on our clock.
    pub received_us: u64,
    pub decode_us: u32,
    pub bytes: usize,
}

/// Last decoded frame, waiting to be shown.
///
/// If the window is slower than decoding (modest graphics card, vsync), the frame
/// not yet shown is replaced by the new one instead of queueing: a frame is
/// skipped but latency does not grow.
#[derive(Default)]
pub struct FrameSlot {
    frame: Mutex<Option<Box<DecodedFrame>>>,
    notified: AtomicBool,
    dropped: AtomicU64,
}

impl FrameSlot {
    /// Stores a frame; `true` if the window must be notified.
    fn put(&self, frame: Box<DecodedFrame>) -> bool {
        if self.frame.lock().expect("slot").replace(frame).is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        !self.notified.swap(true, Ordering::AcqRel)
    }

    pub fn take(&self) -> Option<Box<DecodedFrame>> {
        self.notified.store(false, Ordering::Release);
        self.frame.lock().expect("slot").take()
    }

    /// Frames decoded but never shown because a more recent one overtook them.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// How to reach the agent.
#[derive(Clone)]
pub enum Target {
    /// Direct LAN connection (phase 0), with the agent's fingerprint.
    Direct { addr: SocketAddr, fingerprint: Option<Fingerprint> },
    /// By ID through nuvira-server and its relay.
    Server {
        server: SocketAddr,
        server_name: String,
        server_key: Option<Fingerprint>,
        transport: nuvira_transport::signal::TransportMode,
        server_ws: Option<String>,
        device_id: String,
        identity: std::path::PathBuf,
    },
}

/// What the window sends to the session: input, control messages, files, where the
/// downloads go, terminals and tunnels.
type SessionChannels = (
    mpsc::UnboundedReceiver<InputEvent>,
    mpsc::UnboundedReceiver<Msg>,
    mpsc::UnboundedReceiver<Outgoing>,
    Downloads,
    (mpsc::UnboundedReceiver<TermCmd>, mpsc::UnboundedReceiver<TunnelCmd>),
);

/// Commands for the remote terminals, from the windows to the network.
pub enum TermCmd {
    Open { id: u32, cols: u16, rows: u16 },
    Input { id: u32, data: Vec<u8> },
    Resize { id: u32, cols: u16, rows: u16 },
    /// Closing the stream ends the shell on the agent.
    Close { id: u32 },
}

/// TCP tunnels: a local port of this computer forwarded to `host`:`port` of the
/// remote computer's network.
pub enum TunnelCmd {
    /// `local_port` 0: any free port.
    Add { id: u32, local_port: u16, host: String, port: u16 },
    Remove { id: u32 },
}

/// What travels from the network to one terminal's stream.
enum TermIn {
    Data(Vec<u8>),
    Resize(u16, u16),
}

/// Channels towards the session started by [`start`].
pub struct Session {
    /// Closing it ends the session.
    pub input: mpsc::UnboundedSender<InputEvent>,
    /// Control messages towards the agent (feedback, screen, quality, chat).
    pub control: mpsc::UnboundedSender<Msg>,
    /// Files to send to the agent, one at a time.
    pub files: mpsc::UnboundedSender<Outgoing>,
    /// Where the downloads requested from the file manager go.
    pub downloads: Downloads,
    /// Remote terminals.
    pub terminals: mpsc::UnboundedSender<TermCmd>,
    /// TCP tunnels.
    pub tunnels: mpsc::UnboundedSender<TunnelCmd>,
    /// To be joined before exiting.
    pub thread: std::thread::JoinHandle<()>,
}

/// Starts the session on a dedicated thread.
pub fn start(
    target: Target,
    password: Option<String>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
    recording: Arc<crate::recorder::Switch>,
    terminal_only: bool,
) -> Session {
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (terminals_tx, terminals_rx) = mpsc::unbounded_channel();
    let (tunnels_tx, tunnels_rx) = mpsc::unbounded_channel();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (files_tx, files_rx) = mpsc::unbounded_channel();
    let downloads = Downloads::default();
    let session_downloads = downloads.clone();
    let thread = std::thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let channels = (input_rx, control_rx, files_rx, session_downloads, (terminals_rx, tunnels_rx));
            let (reason, retry) = match rt.block_on(session(target, password, proxy.clone(), (slot, recording), channels, terminal_only)) {
                Ok(()) => ("session ended".to_string(), true),
                // A computer still restarting refuses with "@agent.restarting": it will be back.
                Err(e) => {
                    let retry = e.downcast_ref::<Denied>().is_none_or(|d| d.0 == "@agent.restarting");
                    (format!("{e:#}"), retry)
                }
            };
            let _ = proxy.send_event(NetEvent::Disconnected { reason, retry });
        })
        .expect("network thread");
    Session { input: input_tx, control: control_tx, files: files_tx, downloads, terminals: terminals_tx, tunnels: tunnels_tx, thread }
}

/// The agent refused the access, with its reason ("@key" or text).
#[derive(Debug)]
struct Denied(String);

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = nuvira_i18n::message(&self.0);
        f.write_str(&nuvira_i18n::tf("viewer.access_denied", &[("reason", &reason)]))
    }
}

impl std::error::Error for Denied {}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

/// Connection to the agent: direct, or asking nuvira-server for the route.
/// Also returns whether the path is direct (`false`: through the server's relay).
async fn open_connection(
    target: Target,
) -> Result<(nuvira_transport::Connection, bool, Option<crate::upload::Target>)> {
    use nuvira_proto::v1::{Connect, Role, client_message, server_message};
    use nuvira_transport::{identity::Identity, p2p, signal};
    match target {
        Target::Direct { addr, fingerprint } => {
            Ok((nuvira_transport::connect(addr, fingerprint).await?, true, None))
        }
        Target::Server { server, server_name, server_key, transport, server_ws, device_id, identity } => {
            let identity_path = identity.clone();
            let identity = Identity::load_or_create(&identity)?;
            let expected = server_key.or_else(|| signal::known_server::get(&server_name));
            let name = nuvira_transport::device_name();
            // The viewer receives no connections: several session windows can coexist.
            let device = signal::DeviceInfo { name: &name, reachable: false };
            let ws_url = server_ws.unwrap_or_else(|| nuvira_transport::ws::default_url(&server_name, server));
            let mut sig =
                signal::connect_auto(server, &ws_url, transport, expected, &identity, &device).await?;
            if let nuvira_transport::ServerTransport::WebSocket(url) = &sig.transport {
                info!("connected to nuvira-server through the WebSocket {url}");
            }
            if expected.is_none() {
                signal::known_server::remember(&server_name, &sig.server_fingerprint)?;
            }
            info!(id = sig.device_id, "registered on {server_name}; requesting {device_id}");
            // The app's address book sign-in, for the session log (who worked on it).
            let account_token = std::env::var("NUVIRA_ACCOUNT_TOKEN").unwrap_or_default();
            sig.tx.send(client_message::Msg::Connect(Connect { device_id: device_id.clone(), account_token })).await?;
            let route = loop {
                match sig.rx.recv().await? {
                    Some(server_message::Msg::Route(r)) if r.role() == Role::Viewer => break r,
                    Some(server_message::Msg::Error(e)) => bail!("{}", nuvira_i18n::message(&e.message)),
                    Some(_) => {}
                    None => bail!("nuvira-server closed the connection"),
                }
            };
            let session_id = route.session_id.clone();
            let route = p2p::Route {
                relay: server,
                transport: sig.transport.clone(),
                token: route.relay_token.as_slice().try_into().context("invalid relay token")?,
                peer_public_key: route.peer_public_key.as_slice().try_into().context("invalid agent key")?,
                role: p2p::Role::Viewer,
            };
            // NuviraHub Pro: the recordings of this session go to the server too.
            let upload = sig.features.iter().any(|f| f == "pro.recording").then(|| crate::upload::Target {
                server: server_name.clone(),
                server_key: nuvira_transport::fingerprint_hex(&sig.server_fingerprint),
                transport,
                server_ws: Some(ws_url.clone()),
                identity: identity_path.clone(),
                session: session_id.clone(),
            });
            let (conn, path) = p2p::viewer_connect(&identity, &route).await?;
            info!(?path, remote = %conn.remote_address(), "path");
            Ok((conn, path == p2p::Path::Direct, upload))
        }
    }
}

/// Waits for the viewer to drop the input channel (the events sent meanwhile, before
/// the session starts, are discarded).
async fn viewer_gone(input_rx: &mut mpsc::UnboundedReceiver<InputEvent>) {
    while input_rx.recv().await.is_some() {}
}

/// Closes the connection and gives the close message time to leave: without it the
/// agent would notice only at the timeout and stay "busy" in the meantime.
/// Why the agent closed the connection, when it says it ("@key", translated):
/// e.g. the screen of a Linux computer is locked.
async fn agent_reason(conn: &nuvira_transport::Connection) -> Option<String> {
    let closed = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await.ok()?;
    let quinn::ConnectionError::ApplicationClosed(close) = closed else { return None };
    let reason = std::str::from_utf8(&close.reason).ok()?;
    reason.starts_with('@').then(|| nuvira_i18n::message(reason))
}

async fn close(conn: &nuvira_transport::Connection) {
    conn.close(0u32.into(), b"end of session");
    let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
    // With the WebSocket tunnel datagrams go through a queue emptied by a task:
    // a moment for the close message to really leave.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// From the connection to the start of the session: introductions, password (and the
/// confirmation at the remote PC), video format, input and video channels.
async fn handshake(
    conn: &nuvira_transport::Connection,
    direct: bool,
    password: Option<String>,
    proxy: &EventLoopProxy<NetEvent>,
    terminal_only: bool,
) -> Result<(FramedSend, FramedRecv, Option<FramedSend>, Option<FramedRecv>)> {
    let (send, recv) = conn.open_bi().await?;
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    ctrl_tx
        .send(&control(Msg::Hello(Hello {
            protocol_version: nuvira_proto::PROTOCOL_VERSION,
            device_name: nuvira_transport::device_name(),
            os: std::env::consts::OS.into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            // Prototype: video as datagrams with error correction (nuvira_transport::fec).
            video_datagrams: std::env::var_os("NUVIRA_VIDEO_FEC").is_some()
                && conn.max_datagram_size().is_some(),
            terminal_only,
            print: !terminal_only && crate::printing::available(),
            recording: !terminal_only,
            ..Default::default()
        })))
        .await?;
    let Some(ControlMessage { msg: Some(Msg::Hello(agent)) }) = ctrl_rx.recv().await? else {
        bail!("the agent did not introduce itself");
    };
    if agent.password_required {
        let password = password.with_context(|| nuvira_i18n::t("viewer.password_required"))?;
        let auth = match password.strip_prefix(crate::SUPPORT_TICKET) {
            Some(ticket) => nuvira_proto::v1::Auth { support_ticket: ticket.into(), ..Default::default() },
            None => nuvira_proto::v1::Auth { password, ..Default::default() },
        };
        ctrl_tx.send(&control(Msg::Auth(auth))).await?;
        let mut answer = ctrl_rx.recv().await?;
        if let Some(ControlMessage { msg: Some(Msg::ConfirmationPending(_)) }) = answer {
            info!("waiting for the person at the remote computer to accept");
            let _ = proxy.send_event(NetEvent::ConfirmationPending);
            answer = ctrl_rx.recv().await?;
        }
        match answer {
            Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) if r.ok => {}
            Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) => return Err(Denied(r.message).into()),
            _ => bail!("no answer to the password"),
        }
    }
    // Terminal only: no video and no input channel.
    if terminal_only {
        if !agent.terminal {
            bail!("{}", nuvira_i18n::t("viewer.terminal_unavailable"));
        }
        info!(agent = agent.device_name, "connected (terminal)");
        let _ = proxy.send_event(NetEvent::Connected {
            agent: agent.device_name,
            direct,
            width: 0,
            height: 0,
            file_manager: false,
            secure_attention: false,
            remote_mac: agent.os == "macos",
            privacy: false,
            terminal: true,
            tunnel: agent.tunnel,
            restart: false,
            print: false,
            recording: false,
            recording_required: false,
        });
        return Ok((ctrl_tx, ctrl_rx, None, None));
    }
    let Some(ControlMessage { msg: Some(Msg::VideoFormat(format)) }) = ctrl_rx.recv().await? else {
        bail!("missing video format");
    };
    info!(
        agent = agent.device_name,
        w = format.width,
        h = format.height,
        video_datagrams = agent.video_datagrams,
        "connected"
    );
    let video_datagrams = agent.video_datagrams;
    let _ = proxy.send_event(NetEvent::Connected {
        agent: agent.device_name,
        direct,
        width: format.width,
        height: format.height,
        file_manager: agent.file_manager,
        // Older agents send no system: they are all Windows.
        secure_attention: agent.os.is_empty() || agent.os == "windows",
        remote_mac: agent.os == "macos",
        privacy: agent.privacy_mode,
        terminal: agent.terminal,
        tunnel: agent.tunnel,
        restart: agent.restart,
        print: agent.print,
        recording: agent.recording,
        recording_required: agent.recording_required,
    });

    let input_tx = FramedSend::new(conn.open_uni().await?);
    tracing::debug!("input channel open");
    // With datagrams there is no video stream.
    let video_rx = match video_datagrams {
        true => None,
        false => Some(FramedRecv::new(conn.accept_uni().await?)),
    };
    tracing::debug!("video channel accepted");
    Ok((ctrl_tx, ctrl_rx, Some(input_tx), video_rx))
}

async fn session(
    target: Target,
    password: Option<String>,
    proxy: EventLoopProxy<NetEvent>,
    (slot, recording): (Arc<FrameSlot>, Arc<crate::recorder::Switch>),
    (mut input_rx, mut control_rx, mut files_rx, downloads, (mut terminals_rx, mut tunnels_rx)): SessionChannels,
    terminal_only: bool,
) -> Result<()> {
    // Until the session starts the viewer may close (window closed while connecting
    // or waiting for the confirmation): then nothing more is done, and a connection
    // already open is closed, or the agent would stay "busy" until the timeout.
    let (conn, direct, upload) = tokio::select! {
        r = open_connection(target) => r?,
        () = viewer_gone(&mut input_rx) => return Ok(()),
    };
    recording.set_upload(upload);
    let (mut ctrl_tx, mut ctrl_rx, mut input_tx, video_rx) = tokio::select! {
        r = handshake(&conn, direct, password, &proxy, terminal_only) => match r {
            Ok(channels) => channels,
            Err(e) => {
                // The agent's own reason, unless it already gave it in its answer.
                let e = match agent_reason(&conn).await {
                    Some(reason) if e.downcast_ref::<Denied>().is_none() => anyhow::anyhow!(reason),
                    _ => e,
                };
                close(&conn).await;
                return Err(e);
            }
        },
        () = viewer_gone(&mut input_rx) => {
            info!("viewer closed while connecting");
            close(&conn).await;
            return Ok(());
        }
    };

    // Decoding happens on a separate thread so as not to block the network.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<()>();
    let key_tx_for_datagrams = key_tx.clone();
    // Losses of the video as datagrams, reported to the agent with the feedback.
    let datagrams = video_rx.is_none() && !terminal_only;
    let loss = Arc::new(VideoLoss::default());
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<(VideoFrame, u64)>(4);
    let decoder_proxy = proxy.clone();
    let decoder = std::thread::Builder::new()
        .name("decoding".into())
        .spawn(move || decode_loop(frame_rx, decoder_proxy, slot, key_tx, &recording))?;

    let video = async {
        if terminal_only {
            return std::future::pending().await;
        }
        let Some(mut video_rx) = video_rx else {
            return receive_datagram_video(&conn, &frame_tx, &key_tx_for_datagrams, &loss).await;
        };
        while let Some(frame) = video_rx.recv::<VideoFrame>().await? {
            if frame_tx.send((frame, nuvira_proto::now_us())).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    };

    // Closing the input channel (the viewer exits) ends the session, also terminal only.
    let input = async {
        while let Some(event) = input_rx.recv().await {
            if let Some(tx) = input_tx.as_mut() {
                tx.send(&event).await?;
            }
        }
        anyhow::Ok(())
    };

    // Each terminal on a stream of its own, served by a task.
    let terminals = {
        let conn = conn.clone();
        let proxy = proxy.clone();
        async move {
            let mut open = std::collections::HashMap::<u32, mpsc::UnboundedSender<TermIn>>::new();
            while let Some(cmd) = terminals_rx.recv().await {
                match cmd {
                    TermCmd::Open { id, cols, rows } => {
                        let (tx, rx) = mpsc::unbounded_channel();
                        open.insert(id, tx);
                        let (conn, proxy) = (conn.clone(), proxy.clone());
                        tokio::spawn(async move {
                            let (code, error) = match run_terminal(&conn, id, cols, rows, rx, &proxy).await {
                                Ok(exit) => exit,
                                Err(e) => (0, format!("{e:#}")),
                            };
                            let _ = proxy.send_event(NetEvent::TerminalExit { id, code, error });
                        });
                    }
                    TermCmd::Input { id, data } => {
                        if let Some(tx) = open.get(&id) {
                            let _ = tx.send(TermIn::Data(data));
                        }
                    }
                    TermCmd::Resize { id, cols, rows } => {
                        if let Some(tx) = open.get(&id) {
                            let _ = tx.send(TermIn::Resize(cols, rows));
                        }
                    }
                    TermCmd::Close { id } => {
                        open.remove(&id);
                    }
                }
            }
            std::future::pending::<anyhow::Result<()>>().await
        }
    };

    // TCP tunnels: a listener on this computer for each one, a stream for each connection.
    let tunnels = {
        let conn = conn.clone();
        let proxy = proxy.clone();
        async move {
            let mut open = std::collections::HashMap::<u32, tokio::task::JoinHandle<()>>::new();
            while let Some(cmd) = tunnels_rx.recv().await {
                match cmd {
                    TunnelCmd::Add { id, local_port, host, port } => {
                        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", local_port)).await {
                            Ok(listener) => listener,
                            Err(e) => {
                                warn!(id, local_port, "tunnel not opened: {e}");
                                let error = format!("{e}");
                                let _ = proxy.send_event(NetEvent::TunnelListening { id, local_port, error });
                                continue;
                            }
                        };
                        let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(local_port);
                        info!(id, local_port, host, port, "tunnel listening");
                        let _ = proxy.send_event(NetEvent::TunnelListening { id, local_port, error: String::new() });
                        let (conn, proxy) = (conn.clone(), proxy.clone());
                        open.insert(id, tokio::spawn(async move {
                            while let Ok((socket, _)) = listener.accept().await {
                                let (conn, proxy, host) = (conn.clone(), proxy.clone(), host.clone());
                                tokio::spawn(async move {
                                    let error = match forward(&conn, socket, &host, port).await {
                                        Ok(()) => return,
                                        Err(e) => format!("{e:#}"),
                                    };
                                    let _ = proxy.send_event(NetEvent::TunnelConnection { id, error });
                                });
                            }
                        }));
                    }
                    TunnelCmd::Remove { id } => {
                        if let Some(task) = open.remove(&id) {
                            info!(id, "tunnel closed");
                            task.abort();
                        }
                    }
                }
            }
            std::future::pending::<anyhow::Result<()>>().await
        }
    };

    // Shared clipboard: what is copied here goes to the agent, what is copied there arrives here.
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<nuvira_clipboard::Content>();
    let clipboard = match terminal_only {
        true => None,
        false => match nuvira_clipboard::spawn_sync(move |content| clip_tx.send(content).is_ok()) {
            Ok(tx) => Some(tx),
            Err(e) => {
                warn!("clipboard not shared: {e:#}");
                None
            }
        },
    };

    // After the video one, the agent's unidirectional streams carry images copied to
    // its clipboard, the files downloaded with the file manager and printed documents.
    let streams = {
        let clipboard = clipboard.clone();
        let conn = conn.clone();
        let proxy = proxy.clone();
        async move {
            loop {
                let mut rx = FramedRecv::new(conn.accept_uni().await?);
                let (clipboard, proxy, downloads) = (clipboard.clone(), proxy.clone(), downloads.clone());
                tokio::spawn(async move {
                    let Ok(Some(first)) = rx.recv::<FileChunk>().await else { return };
                    if first.transfer_id != 0 {
                        let id = first.transfer_id;
                        let target = downloads.lock().expect("downloads").get(&id).cloned();
                        let Some(target) = target else { return };
                        let outcome = receive_download(first, rx, &target, &proxy).await;
                        downloads.lock().expect("downloads").remove(&id);
                        let (ok, message) = match outcome {
                            Ok(path) => (true, path.display().to_string()),
                            Err(e) => (false, format!("{e:#}")),
                        };
                        let _ = proxy.send_event(NetEvent::TransferDone { id, ok, message });
                        return;
                    }
                    if first.print {
                        match nuvira_transport::read_stream(&mut rx, &first, crate::printing::MAX_DOCUMENT).await {
                            Ok(data) => {
                                let _ = proxy.send_event(NetEvent::PrintJob { name: first.name, data });
                            }
                            Err(e) => warn!("printed document not received: {e:#}"),
                        }
                        return;
                    }
                    if !first.clipboard_image {
                        return;
                    }
                    match nuvira_transport::read_stream(
                        &mut rx,
                        &first,
                        nuvira_clipboard::MAX_IMAGE_PNG as u64,
                    )
                    .await
                    {
                        Ok(png) => {
                            if let Some(clipboard) = clipboard {
                                let _ = clipboard.send(nuvira_clipboard::Content::Image(png));
                            }
                        }
                        Err(e) => warn!("clipboard image not received: {e:#}"),
                    }
                });
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        }
    };

    let control_loop = async {
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        // While frames are lost none is shown and the usual feedback stops: the losses
        // are then reported on their own, which is when the agent needs them most.
        let mut loss_tick = tokio::time::interval(Duration::from_millis(250));
        let mut last_feedback = Instant::now();
        // What is copied here goes to the agent while the session allows it (older
        // agents send no Permissions: always).
        let mut clipboard_allowed = true;
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    ctrl_tx.send(&control(Msg::Ping(Ping { sent_us: nuvira_proto::now_us() }))).await?;
                }
                _ = loss_tick.tick(), if datagrams => {
                    if last_feedback.elapsed() >= Duration::from_millis(250)
                        && loss.frames_lost.load(Ordering::Relaxed) > 0
                    {
                        let mut f = nuvira_proto::v1::Feedback {
                            window_ms: last_feedback.elapsed().as_millis() as u32,
                            ..Default::default()
                        };
                        loss.fill(&mut f);
                        last_feedback = Instant::now();
                        ctrl_tx.send(&control(Msg::Feedback(f))).await?;
                    }
                }
                Some(mut msg) = control_rx.recv() => {
                    if let Msg::Feedback(f) = &mut msg
                        && datagrams
                    {
                        loss.fill(f);
                        last_feedback = Instant::now();
                    }
                    ctrl_tx.send(&control(msg)).await?;
                }
                Some(content) = clip_rx.recv(), if clipboard_allowed => match content {
                    nuvira_clipboard::Content::Text(text) => {
                        ctrl_tx.send(&control(Msg::Clipboard(nuvira_proto::v1::Clipboard { text }))).await?;
                    }
                    // On its own stream, not to hold up the control channel.
                    nuvira_clipboard::Content::Image(png) => {
                        let conn = conn.clone();
                        tokio::spawn(async move {
                            let header = FileChunk { clipboard_image: true, ..Default::default() };
                            if let Err(e) = nuvira_transport::send_stream(&conn, header, &png).await {
                                warn!("clipboard image not sent: {e:#}");
                            }
                        });
                    }
                },
                Some(()) = key_rx.recv() => {
                    ctrl_tx.send(&control(Msg::RequestKeyframe(RequestKeyframe {}))).await?;
                }
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(ControlMessage { msg: Some(Msg::Pong(pong)) }) => {
                        let now = nuvira_proto::now_us();
                        let rtt_us = now.saturating_sub(pong.ping_sent_us);
                        // Agent clock minus ours, assuming a symmetric round trip.
                        let midpoint = (pong.ping_sent_us + now) / 2;
                        let offset_us = pong.agent_us as i64 - midpoint as i64;
                        let _ = proxy.send_event(NetEvent::Clock { rtt_us, offset_us });
                    }
                    Some(ControlMessage { msg: Some(Msg::Displays(d)) }) => {
                        let _ = proxy.send_event(NetEvent::Displays {
                            displays: d.displays,
                            current: d.current,
                            all_available: d.all_available,
                        });
                    }
                    Some(ControlMessage { msg: Some(Msg::FileReceived(f)) }) if f.transfer_id != 0 => {
                        let message = if f.ok { f.path } else { f.message };
                        let _ = proxy.send_event(NetEvent::TransferDone { id: f.transfer_id, ok: f.ok, message });
                    }
                    Some(ControlMessage { msg: Some(Msg::FileReceived(f)) }) => {
                        let _ = proxy.send_event(NetEvent::FileDone { name: f.name, ok: f.ok, message: f.message });
                    }
                    Some(ControlMessage { msg: Some(Msg::FsResponse(answer)) }) => {
                        let _ = proxy.send_event(NetEvent::Fs(answer));
                    }
                    Some(ControlMessage { msg: Some(Msg::Chat(chat)) }) => {
                        let _ = proxy.send_event(NetEvent::Chat(chat.text));
                    }
                    Some(ControlMessage { msg: Some(Msg::ScreenState(s)) }) => {
                        let _ = proxy.send_event(NetEvent::ScreenLocked(s.locked));
                    }
                    Some(ControlMessage { msg: Some(Msg::PrivacyState(p)) }) => {
                        let _ = proxy.send_event(NetEvent::Privacy { on: p.on, error: p.error });
                    }
                    Some(ControlMessage { msg: Some(Msg::Permissions(p)) }) => {
                        info!(?p, "permissions of the session");
                        clipboard_allowed = p.clipboard;
                        let _ = proxy.send_event(NetEvent::Permissions {
                            input: p.input,
                            files: p.files,
                            terminal: p.terminal,
                            tunnel: p.tunnel,
                            switched_off: p.switched_off,
                        });
                    }
                    Some(ControlMessage { msg: Some(Msg::RecordingState(r)) }) => {
                        let _ = proxy.send_event(NetEvent::Recording { on: r.on, error: r.error });
                    }
                    Some(ControlMessage { msg: Some(Msg::RestartState(r)) }) => {
                        let _ = proxy.send_event(NetEvent::Restart { ok: r.ok, error: r.error, reconnect: r.reconnect });
                    }
                    Some(ControlMessage { msg: Some(Msg::Clipboard(clip)) }) => {
                        if let Some(clipboard) = &clipboard {
                            let _ = clipboard.send(nuvira_clipboard::Content::Text(clip.text));
                        }
                    }
                    Some(ControlMessage { msg: Some(Msg::VideoFormat(f)) }) => {
                        info!(w = f.width, h = f.height, "new video format");
                    }
                    Some(_) => {}
                    None => return anyhow::Ok(()),
                },
            }
        }
    };

    // Outgoing files, one at a time, each on its own stream.
    let files = async {
        while let Some(outgoing) = files_rx.recv().await {
            match outgoing {
                Outgoing::File(path) => {
                    if let Err(e) = send_file(&conn, &path, &proxy).await {
                        warn!("sending {} failed: {e:#}", path.display());
                        let name = file_name(&path);
                        let _ = proxy.send_event(NetEvent::FileDone {
                            name,
                            ok: false,
                            message: format!("{e:#}"),
                        });
                    }
                }
                Outgoing::Upload { id, local, dir, offset } => {
                    // The outcome comes from the agent (FileReceived with the id).
                    if let Err(e) = send_upload(&conn, id, &local, dir, offset, &proxy).await {
                        warn!("sending {} failed: {e:#}", local.display());
                        let _ = proxy.send_event(NetEvent::TransferDone {
                            id,
                            ok: false,
                            message: format!("{e:#}"),
                        });
                    }
                }
            }
        }
        // The channel closes only on exit: the input ends the session.
        std::future::pending::<anyhow::Result<()>>().await
    };

    let result = tokio::select! {
        r = files => r.context("sending files"),
        r = video => r.context("receiving video"),
        r = input => r.context("sending input"),
        r = control_loop => r.context("control channel"),
        r = streams => r.context("streams from the agent"),
        r = terminals => r,
        r = tunnels => r,
    };
    close(&conn).await;
    // The decoder ends when its channel closes: it is waited for, so that on exit
    // none of our threads is still working.
    drop(frame_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || decoder.join()))
        .await;
    result
}

/// One remote terminal: opens its stream, then keys and size go out and the output
/// comes back, until the agent says the shell ended or the window closes (`rx`
/// closed: the stream is finished and the agent ends the shell). Returns the exit
/// code and the reason ("" when the shell ended by itself or the window closed).
async fn run_terminal(
    conn: &nuvira_transport::Connection,
    id: u32,
    cols: u16,
    rows: u16,
    mut rx: mpsc::UnboundedReceiver<TermIn>,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<(i32, String)> {
    use nuvira_proto::v1::{StreamOpen, TerminalMessage, TerminalOpen, TerminalSize, stream_open::Kind, terminal_message::Msg as T};

    let (send, recv) = conn.open_bi().await?;
    let (mut tx, mut stream_rx) = (FramedSend::new(send), FramedRecv::new(recv));
    let size = |cols: u16, rows: u16| TerminalSize { cols: cols as u32, rows: rows as u32 };
    tx.send(&StreamOpen { kind: Some(Kind::Terminal(TerminalOpen { size: Some(size(cols, rows)) })) }).await?;
    info!(id, cols, rows, "terminal opened");
    loop {
        tokio::select! {
            input = rx.recv() => match input {
                Some(TermIn::Data(data)) => tx.send(&TerminalMessage { msg: Some(T::Data(data)) }).await?,
                Some(TermIn::Resize(cols, rows)) => {
                    tx.send(&TerminalMessage { msg: Some(T::Resize(size(cols, rows))) }).await?;
                }
                None => {
                    let _ = tx.finish();
                    return Ok((0, String::new()));
                }
            },
            msg = stream_rx.recv::<TerminalMessage>() => match msg? {
                Some(TerminalMessage { msg: Some(T::Data(data)) }) => {
                    let _ = proxy.send_event(NetEvent::TerminalOutput { id, data });
                }
                Some(TerminalMessage { msg: Some(T::Exit(exit)) }) => {
                    return Ok((exit.code, nuvira_i18n::message(&exit.error)));
                }
                Some(_) => {}
                None => return Ok((0, String::new())),
            },
        }
    }
}

/// One connection through a tunnel: a stream to the agent, which connects to
/// `host`:`port` of its network; then the bytes are copied both ways. `Err` with the
/// reason when the agent refused it (translated) or the connection broke.
async fn forward(conn: &nuvira_transport::Connection, socket: tokio::net::TcpStream, host: &str, port: u16) -> Result<()> {
    use nuvira_proto::v1::{StreamOpen, TunnelOpen, TunnelResult, stream_open::Kind};
    use tokio::io::AsyncWriteExt;

    let _ = socket.set_nodelay(true);
    let (mut send, mut recv) = conn.open_bi().await?;
    let open = StreamOpen { kind: Some(Kind::Tunnel(TunnelOpen { host: host.into(), port: port as u32 })) };
    send.write_all(&nuvira_proto::framing::encode(&open)).await?;
    match nuvira_transport::recv_one::<TunnelResult>(&mut recv).await? {
        Some(TunnelResult { ok: true, .. }) => {}
        Some(TunnelResult { error, .. }) => bail!("{}", nuvira_i18n::message(&error)),
        None => bail!("the agent closed the tunnel"),
    }
    let (mut socket_rx, mut socket_tx) = socket.into_split();
    let up = async {
        let _ = tokio::io::copy(&mut socket_rx, &mut send).await;
        let _ = send.finish();
    };
    let down = async {
        let _ = tokio::io::copy(&mut recv, &mut socket_tx).await;
        let _ = socket_tx.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}

/// File manager upload: `local` from `offset` on, into `dir` on the remote PC.
async fn send_upload(
    conn: &nuvira_transport::Connection,
    id: u32,
    local: &std::path::Path,
    dir: String,
    offset: u64,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file =
        tokio::fs::File::open(local).await.with_context(|| format!("opening {}", local.display()))?;
    let total = file.metadata().await?.len();
    if offset > total {
        bail!("resume point beyond the end of the file");
    }
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut tx = FramedSend::new(conn.open_uni().await?);
    tx.send(&FileChunk {
        name: file_name(local),
        size: total,
        dir,
        offset,
        transfer_id: id,
        ..Default::default()
    })
    .await?;
    let mut buf = vec![0u8; 256 * 1024];
    let (mut done, mut last) = (offset, Instant::now());
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        tx.send(&FileChunk { data: buf[..n].to_vec(), ..Default::default() }).await?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            let _ = proxy.send_event(NetEvent::Transfer { id, done, total });
        }
    }
    tx.finish()?;
    let _ = proxy.send_event(NetEvent::Transfer { id, done, total });
    Ok(())
}

/// File manager download into `target`: written to `target.part` (continued from
/// the header's offset, if an earlier attempt left it), renamed once complete. If
/// `target` exists a free name is used. Returns the final path.
async fn receive_download(
    first: FileChunk,
    mut rx: FramedRecv,
    target: &std::path::Path,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<std::path::PathBuf> {
    use tokio::io::AsyncWriteExt;
    let (id, size, offset) = (first.transfer_id, first.size, first.offset);
    let part = part_path(target);
    let mut file = if offset == 0 {
        tokio::fs::File::create(&part).await.with_context(|| format!("creating {}", part.display()))?
    } else {
        let file =
            tokio::fs::OpenOptions::new().append(true).open(&part).await.context("nothing to resume")?;
        let have = file.metadata().await?.len();
        if have != offset {
            bail!("cannot resume: {have} bytes there, not {offset}");
        }
        file
    };
    let mut done = offset + first.data.len() as u64;
    file.write_all(&first.data).await?;
    let mut last = Instant::now();
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        file.write_all(&chunk.data).await?;
        done += chunk.data.len() as u64;
        if done > size {
            bail!("more data than announced");
        }
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            let _ = proxy.send_event(NetEvent::Transfer { id, done, total: size });
        }
    }
    file.flush().await?;
    drop(file);
    let _ = proxy.send_event(NetEvent::Transfer { id, done, total: size });
    if done != size {
        // The part stays: the download can be resumed from here.
        bail!("interrupted at {done} bytes out of {size}");
    }
    let final_path = free_path(target);
    tokio::fs::rename(&part, &final_path)
        .await
        .with_context(|| format!("saving {}", final_path.display()))?;
    info!("downloaded {} ({size} bytes, from {offset})", final_path.display());
    Ok(final_path)
}

/// `file.ext.part`, where a download is written until complete.
pub fn part_path(target: &std::path::Path) -> std::path::PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

/// `target`, or `name (1).ext`, `name (2).ext`… if it exists.
fn free_path(target: &std::path::Path) -> std::path::PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let stem = target.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = target.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|i| target.with_file_name(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("a free name")
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Sends a file on a new stream; the outcome then comes from the agent (`FileReceived`).
async fn send_file(
    conn: &nuvira_transport::Connection,
    path: &std::path::Path,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let name = file_name(path);
    if tokio::fs::metadata(path).await.is_ok_and(|m| m.is_dir()) {
        bail!("{}", nuvira_i18n::t("viewer.folder_not_supported"));
    }
    let mut file =
        tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
    let total = file.metadata().await?.len();
    info!("sending {name} ({total} bytes)");
    let mut tx = FramedSend::new(conn.open_uni().await?);
    tx.send(&FileChunk { name: name.clone(), size: total, ..Default::default() }).await?;
    let mut buf = vec![0u8; 256 * 1024];
    let (mut sent, mut last) = (0u64, Instant::now());
    let _ = proxy.send_event(NetEvent::FileProgress { name: name.clone(), sent, total });
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        tx.send(&FileChunk { data: buf[..n].to_vec(), ..Default::default() }).await?;
        sent += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            let _ = proxy.send_event(NetEvent::FileProgress { name: name.clone(), sent, total });
        }
    }
    tx.finish()?;
    let _ = proxy.send_event(NetEvent::FileProgress { name, sent, total });
    Ok(())
}

/// Losses of the video as datagrams since the last feedback.
#[derive(Default)]
struct VideoLoss {
    shards: AtomicU64,
    shards_lost: AtomicU64,
    frames_lost: AtomicU64,
}

impl VideoLoss {
    fn fill(&self, feedback: &mut nuvira_proto::v1::Feedback) {
        let take = |n: &AtomicU64| Some(n.swap(0, Ordering::Relaxed).min(u32::MAX as u64) as u32);
        feedback.shards = take(&self.shards);
        feedback.shards_lost = take(&self.shards_lost);
        feedback.frames_lost = take(&self.frames_lost);
    }
}

/// Video as QUIC datagrams with error correction (`nuvira_transport::fec`, prototype):
/// frames go to decoding as they are rebuilt. After a lost frame the next ones cannot
/// be decoded: they are dropped until a keyframe, asked for at most once a second.
async fn receive_datagram_video(
    conn: &nuvira_transport::Connection,
    frames: &std::sync::mpsc::SyncSender<(VideoFrame, u64)>,
    request_keyframe: &mpsc::UnboundedSender<()>,
    loss: &VideoLoss,
) -> Result<()> {
    use prost::Message;
    let mut rx = nuvira_transport::fec::Receiver::default();
    let mut last = 0u64;
    let mut waiting_key = false;
    let mut asked: Option<Instant> = None;
    info!("receiving the video as datagrams");
    loop {
        let packet = match conn.read_datagram().await {
            Ok(p) => {
                tracing::trace!(bytes = p.len(), "video datagram");
                p
            }
            Err(quinn::ConnectionError::ApplicationClosed(_) | quinn::ConnectionError::LocallyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let pushed = rx.push(&packet);
        let (shards, lost) = rx.take_loss();
        loss.shards.fetch_add(shards, Ordering::Relaxed);
        loss.shards_lost.fetch_add(lost, Ordering::Relaxed);
        let (seq, bytes) = match pushed {
            Ok(Some(done)) => done,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("video datagram dropped: {e:#}");
                continue;
            }
        };
        let frame = VideoFrame::decode(bytes.as_slice()).context("video frame")?;
        if seq != last + 1 {
            tracing::debug!(lost = seq - last - 1, "video frames lost");
            loss.frames_lost.fetch_add(seq - last - 1, Ordering::Relaxed);
            waiting_key = true;
        }
        last = seq;
        if waiting_key {
            if frame.keyframe {
                waiting_key = false;
                asked = None;
            } else {
                if asked.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                    asked = Some(Instant::now());
                    let _ = request_keyframe.send(());
                }
                continue;
            }
        }
        if frames.send((frame, nuvira_proto::now_us())).is_err() {
            return Ok(());
        }
    }
}

fn decode_loop(
    frames: std::sync::mpsc::Receiver<(VideoFrame, u64)>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
    request_keyframe: mpsc::UnboundedSender<()>,
    recording: &crate::recorder::Switch,
) {
    let mut decoder = match nuvira_codec::best_decoder() {
        Ok(d) => d,
        Err(e) => {
            let _ = proxy.send_event(NetEvent::Disconnected {
                reason: format!("decoder not available: {e:#}"),
                retry: false,
            });
            return;
        }
    };
    info!("decoder: {}", decoder.name());
    let mut waiting_key = false;
    // The recording being written, with its number (see recorder::Switch).
    let mut recorder: Option<(u64, crate::recorder::Recorder)> = None;
    let close = |recorder: Option<(u64, crate::recorder::Recorder)>, error: Option<String>| {
        if let Some((_, r)) = recorder {
            let (files, error) = match r.finish() {
                Ok(files) => (files, error),
                Err(e) => (Vec::new(), Some(error.unwrap_or_else(|| format!("{e:#}")))),
            };
            let _ = proxy.send_event(NetEvent::RecordingSaved { files, error });
        }
    };
    loop {
        // Also without frames (still screen), so that a stop closes the file at once.
        let next = frames.recv_timeout(Duration::from_millis(200));
        let wanted = recording.wanted();
        if recorder.as_ref().map(|r| r.0) != wanted.as_ref().map(|w| w.0) {
            close(recorder.take(), None);
            if let Some((n, dir, stem)) = wanted {
                recorder = Some((n, crate::recorder::Recorder::new(dir, stem, recording.upload())));
                // It starts at a keyframe.
                let _ = request_keyframe.send(());
            }
        }
        let (frame, received_us) = match next {
            Ok(f) => f,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some((_, r)) = recorder.as_mut()
            && let Err(e) = r.push(&frame)
        {
            warn!("recording stopped: {e:#}");
            recording.stop();
            close(recorder.take(), Some(format!("{e:#}")));
        }
        // After an error frames are dropped until the next keyframe.
        if waiting_key && !frame.keyframe {
            continue;
        }
        let t = Instant::now();
        tracing::trace!(seq = frame.seq, bytes = frame.data.len(), "frame received");
        match decoder.decode(&frame.data) {
            Ok(Some(yuv)) => {
                waiting_key = false;
                let decoded = DecodedFrame {
                    yuv: YuvFrame {
                        width: yuv.width,
                        height: yuv.height,
                        y: yuv.y.clone(),
                        u: yuv.u.clone(),
                        v: yuv.v.clone(),
                    },
                    capture_us: frame.capture_us,
                    encode_us: frame.encode_us,
                    // Older agents: estimated from capture and encoding times.
                    encoded_us: match frame.encoded_us {
                        0 => frame.capture_us + frame.encode_us as u64,
                        t => t,
                    },
                    received_us,
                    decode_us: t.elapsed().as_micros() as u32,
                    bytes: frame.data.len(),
                };
                if slot.put(Box::new(decoded)) && proxy.send_event(NetEvent::FrameReady).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!("frame {} not decodable: {e:#}; requesting a keyframe", frame.seq);
                // A failing hardware decoder is not trusted again: OpenH264 from the keyframe on.
                if decoder.name() != "openh264" {
                    match nuvira_codec::software_decoder() {
                        Ok(d) => {
                            warn!("hardware decoder dropped: {}", d.name());
                            decoder = d;
                        }
                        Err(e) => warn!("OpenH264 not available: {e:#}"),
                    }
                }
                waiting_key = true;
                let _ = request_keyframe.send(());
            }
        }
    }
    // End of the session: the recording is closed as it is.
    close(recorder.take(), None);
}
