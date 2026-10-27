//! Network side of the viewer: connection to the agent, video reception and
//! decoding, input sending and measurement of RTT and clock difference.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nexodesk_codec::YuvFrame;
use nexodesk_proto::v1::{
    ControlMessage, DisplayInfo, FileChunk, Hello, InputEvent, Ping, RequestKeyframe, VideoFrame,
    control_message::Msg,
};
use nexodesk_transport::{Fingerprint, FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::{info, warn};
use winit::event_loop::EventLoopProxy;

/// Events from the network to the window.
pub enum NetEvent {
    Connected {
        agent: String,
        /// Direct path; otherwise through the server's relay.
        direct: bool,
        width: u32,
        height: u32,
    },
    /// There is a new frame to show in the [`FrameSlot`].
    FrameReady,
    Clock {
        rtt_us: u64,
        offset_us: i64,
    },
    Disconnected(String),
    /// The duration set with `--duration` is over.
    TestFinished,
    /// Screens of the agent and the one being streamed.
    Displays {
        displays: Vec<DisplayInfo>,
        current: u32,
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
}

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
    /// By ID through nexodesk-server and its relay.
    Server {
        server: SocketAddr,
        server_name: String,
        server_key: Option<Fingerprint>,
        transport: nexodesk_transport::signal::TransportMode,
        server_ws: Option<String>,
        device_id: String,
        identity: std::path::PathBuf,
    },
}

/// Channels towards the session started by [`start`].
pub struct Session {
    /// Closing it ends the session.
    pub input: mpsc::UnboundedSender<InputEvent>,
    /// Control messages towards the agent (feedback, screen, quality, chat).
    pub control: mpsc::UnboundedSender<Msg>,
    /// Files to send to the agent, one at a time.
    pub files: mpsc::UnboundedSender<std::path::PathBuf>,
    /// To be joined before exiting.
    pub thread: std::thread::JoinHandle<()>,
}

/// Starts the session on a dedicated thread.
pub fn start(
    target: Target,
    password: Option<String>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
) -> Session {
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (files_tx, files_rx) = mpsc::unbounded_channel();
    let thread = std::thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let reason =
                match rt.block_on(session(target, password, proxy.clone(), slot, input_rx, control_rx, files_rx)) {
                    Ok(()) => "session ended".to_string(),
                    Err(e) => format!("{e:#}"),
                };
            let _ = proxy.send_event(NetEvent::Disconnected(reason));
        })
        .expect("network thread");
    Session { input: input_tx, control: control_tx, files: files_tx, thread }
}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

/// Connection to the agent: direct, or asking nexodesk-server for the route.
/// Also returns whether the path is direct (`false`: through the server's relay).
async fn open_connection(target: Target) -> Result<(nexodesk_transport::Connection, bool)> {
    use nexodesk_proto::v1::{Connect, Role, client_message, server_message};
    use nexodesk_transport::{identity::Identity, p2p, signal};
    match target {
        Target::Direct { addr, fingerprint } => Ok((nexodesk_transport::connect(addr, fingerprint).await?, true)),
        Target::Server { server, server_name, server_key, transport, server_ws, device_id, identity } => {
            let identity = Identity::load_or_create(&identity)?;
            let expected = server_key.or_else(|| signal::known_server::get(&server_name));
            let name = std::env::var("COMPUTERNAME").unwrap_or_default();
            // The viewer receives no connections: several session windows can coexist.
            let device = signal::DeviceInfo { name: &name, reachable: false };
            let ws_url = server_ws.unwrap_or_else(|| nexodesk_transport::ws::default_url(&server_name, server));
            let mut sig =
                signal::connect_auto(server, &ws_url, transport, expected, &identity, &device).await?;
            if let nexodesk_transport::ServerTransport::WebSocket(url) = &sig.transport {
                info!("connected to nexodesk-server through the WebSocket {url}");
            }
            if expected.is_none() {
                signal::known_server::remember(&server_name, &sig.server_fingerprint)?;
            }
            info!(id = sig.device_id, "registered on {server_name}; requesting {device_id}");
            sig.tx.send(client_message::Msg::Connect(Connect { device_id: device_id.clone() })).await?;
            let route = loop {
                match sig.rx.recv().await? {
                    Some(server_message::Msg::Route(r)) if r.role() == Role::Viewer => break r,
                    Some(server_message::Msg::Error(e)) => bail!("{}", nexodesk_i18n::message(&e.message)),
                    Some(_) => {}
                    None => bail!("nexodesk-server closed the connection"),
                }
            };
            let route = p2p::Route {
                relay: server,
                transport: sig.transport.clone(),
                token: route.relay_token.as_slice().try_into().context("invalid relay token")?,
                peer_public_key: route
                    .peer_public_key
                    .as_slice()
                    .try_into()
                    .context("invalid agent key")?,
                role: p2p::Role::Viewer,
            };
            let (conn, path) = p2p::viewer_connect(&identity, &route).await?;
            info!(?path, remote = %conn.remote_address(), "path");
            Ok((conn, path == p2p::Path::Direct))
        }
    }
}

async fn session(
    target: Target,
    password: Option<String>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    mut control_rx: mpsc::UnboundedReceiver<Msg>,
    mut files_rx: mpsc::UnboundedReceiver<std::path::PathBuf>,
) -> Result<()> {
    let (conn, direct) = open_connection(target).await?;
    let (send, recv) = conn.open_bi().await?;
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    ctrl_tx
        .send(&control(Msg::Hello(Hello {
            protocol_version: nexodesk_proto::PROTOCOL_VERSION,
            device_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
            os: std::env::consts::OS.into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            password_required: false,
        })))
        .await?;
    let Some(ControlMessage { msg: Some(Msg::Hello(agent)) }) = ctrl_rx.recv().await? else {
        bail!("the agent did not introduce itself");
    };
    if agent.password_required {
        let password = password.with_context(|| nexodesk_i18n::t("viewer.password_required"))?;
        ctrl_tx.send(&control(Msg::Auth(nexodesk_proto::v1::Auth { password }))).await?;
        match ctrl_rx.recv().await? {
            Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) if r.ok => {}
            Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) => {
                bail!("{}", nexodesk_i18n::tf("viewer.access_denied", &[("reason", &nexodesk_i18n::message(&r.message))]))
            }
            _ => bail!("no answer to the password"),
        }
    }
    let Some(ControlMessage { msg: Some(Msg::VideoFormat(format)) }) = ctrl_rx.recv().await? else {
        bail!("missing video format");
    };
    info!(agent = agent.device_name, w = format.width, h = format.height, "connected");
    let _ = proxy.send_event(NetEvent::Connected {
        agent: agent.device_name,
        direct,
        width: format.width,
        height: format.height,
    });

    let mut input_tx = FramedSend::new(conn.open_uni().await?);
    tracing::debug!("input channel open");
    let mut video_rx = FramedRecv::new(conn.accept_uni().await?);
    tracing::debug!("video channel accepted");

    // Decoding happens on a separate thread so as not to block the network.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<()>();
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<(VideoFrame, u64)>(4);
    let decoder_proxy = proxy.clone();
    let decoder = std::thread::Builder::new()
        .name("decoding".into())
        .spawn(move || decode_loop(frame_rx, decoder_proxy, slot, key_tx))?;

    let video = async {
        while let Some(frame) = video_rx.recv::<VideoFrame>().await? {
            if frame_tx.send((frame, nexodesk_proto::now_us())).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    };

    let input = async {
        while let Some(event) = input_rx.recv().await {
            input_tx.send(&event).await?;
        }
        anyhow::Ok(())
    };

    // Shared clipboard: text copied here goes to the agent, text copied there arrives here.
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<String>();
    let clipboard = match nexodesk_clipboard::spawn_sync(move |text| clip_tx.send(text).is_ok()) {
        Ok(tx) => Some(tx),
        Err(e) => {
            warn!("clipboard not shared: {e:#}");
            None
        }
    };

    let control_loop = async {
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    ctrl_tx.send(&control(Msg::Ping(Ping { sent_us: nexodesk_proto::now_us() }))).await?;
                }
                Some(msg) = control_rx.recv() => {
                    ctrl_tx.send(&control(msg)).await?;
                }
                Some(text) = clip_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Clipboard(nexodesk_proto::v1::Clipboard { text }))).await?;
                }
                Some(()) = key_rx.recv() => {
                    ctrl_tx.send(&control(Msg::RequestKeyframe(RequestKeyframe {}))).await?;
                }
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(ControlMessage { msg: Some(Msg::Pong(pong)) }) => {
                        let now = nexodesk_proto::now_us();
                        let rtt_us = now.saturating_sub(pong.ping_sent_us);
                        // Agent clock minus ours, assuming a symmetric round trip.
                        let midpoint = (pong.ping_sent_us + now) / 2;
                        let offset_us = pong.agent_us as i64 - midpoint as i64;
                        let _ = proxy.send_event(NetEvent::Clock { rtt_us, offset_us });
                    }
                    Some(ControlMessage { msg: Some(Msg::Displays(d)) }) => {
                        let _ = proxy.send_event(NetEvent::Displays { displays: d.displays, current: d.current });
                    }
                    Some(ControlMessage { msg: Some(Msg::FileReceived(f)) }) => {
                        let _ = proxy.send_event(NetEvent::FileDone { name: f.name, ok: f.ok, message: f.message });
                    }
                    Some(ControlMessage { msg: Some(Msg::Chat(chat)) }) => {
                        let _ = proxy.send_event(NetEvent::Chat(chat.text));
                    }
                    Some(ControlMessage { msg: Some(Msg::Clipboard(clip)) }) => {
                        if let Some(clipboard) = &clipboard {
                            let _ = clipboard.send(clip.text);
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
        while let Some(path) = files_rx.recv().await {
            if let Err(e) = send_file(&conn, &path, &proxy).await {
                warn!("sending {} failed: {e:#}", path.display());
                let name = file_name(&path);
                let _ = proxy.send_event(NetEvent::FileDone { name, ok: false, message: format!("{e:#}") });
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
    };
    conn.close(0u32.into(), b"end of session");
    // Give the close message time to leave: without it the agent would notice only
    // at the timeout and stay "busy" in the meantime.
    let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
    // With the WebSocket tunnel datagrams go through a queue emptied by a task:
    // a moment for the close message to really leave.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The decoder ends when its channel closes: it is waited for, so that on exit
    // none of our threads is still working.
    drop(frame_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || decoder.join()))
        .await;
    result
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Sends a file on a new stream; the outcome then comes from the agent (`FileReceived`).
async fn send_file(
    conn: &nexodesk_transport::Connection,
    path: &std::path::Path,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let name = file_name(path);
    if tokio::fs::metadata(path).await.is_ok_and(|m| m.is_dir()) {
        bail!("{}", nexodesk_i18n::t("viewer.folder_not_supported"));
    }
    let mut file = tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
    let total = file.metadata().await?.len();
    info!("sending {name} ({total} bytes)");
    let mut tx = FramedSend::new(conn.open_uni().await?);
    tx.send(&FileChunk { name: name.clone(), size: total, data: Vec::new() }).await?;
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

fn decode_loop(
    frames: std::sync::mpsc::Receiver<(VideoFrame, u64)>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
    request_keyframe: mpsc::UnboundedSender<()>,
) {
    let mut decoder = match nexodesk_codec::best_decoder() {
        Ok(d) => d,
        Err(e) => {
            let _ = proxy.send_event(NetEvent::Disconnected(format!("decoder not available: {e:#}")));
            return;
        }
    };
    info!("decoder: {}", decoder.name());
    let mut waiting_key = false;
    while let Ok((frame, received_us)) = frames.recv() {
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
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!("frame {} not decodable: {e:#}; requesting a keyframe", frame.seq);
                waiting_key = true;
                let _ = request_keyframe.send(());
            }
        }
    }
}
