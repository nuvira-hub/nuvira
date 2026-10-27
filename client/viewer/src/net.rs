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
    Fs(nexodesk_proto::v1::FsResponse),
    /// File manager: progress of a transfer (both directions).
    Transfer { id: u32, done: u64, total: u64 },
    /// File manager: end of a transfer; with `ok` the final path, otherwise the reason.
    TransferDone { id: u32, ok: bool, message: String },
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
    pub files: mpsc::UnboundedSender<Outgoing>,
    /// Where the downloads requested from the file manager go.
    pub downloads: Downloads,
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
    let downloads = Downloads::default();
    let session_downloads = downloads.clone();
    let thread = std::thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let channels = (input_rx, control_rx, files_rx, session_downloads);
            let reason =
                match rt.block_on(session(target, password, proxy.clone(), slot, channels)) {
                    Ok(()) => "session ended".to_string(),
                    Err(e) => format!("{e:#}"),
                };
            let _ = proxy.send_event(NetEvent::Disconnected(reason));
        })
        .expect("network thread");
    Session { input: input_tx, control: control_tx, files: files_tx, downloads, thread }
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
    (mut input_rx, mut control_rx, mut files_rx, downloads): (
        mpsc::UnboundedReceiver<InputEvent>,
        mpsc::UnboundedReceiver<Msg>,
        mpsc::UnboundedReceiver<Outgoing>,
        Downloads,
    ),
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
            ..Default::default()
        })))
        .await?;
    let Some(ControlMessage { msg: Some(Msg::Hello(agent)) }) = ctrl_rx.recv().await? else {
        bail!("the agent did not introduce itself");
    };
    if agent.password_required {
        let password = password.with_context(|| nexodesk_i18n::t("viewer.password_required"))?;
        ctrl_tx.send(&control(Msg::Auth(nexodesk_proto::v1::Auth { password }))).await?;
        let mut answer = ctrl_rx.recv().await?;
        if let Some(ControlMessage { msg: Some(Msg::ConfirmationPending(_)) }) = answer {
            info!("waiting for the person at the remote computer to accept");
            let _ = proxy.send_event(NetEvent::ConfirmationPending);
            answer = ctrl_rx.recv().await?;
        }
        match answer {
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
        file_manager: agent.file_manager,
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

    // Shared clipboard: what is copied here goes to the agent, what is copied there arrives here.
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<nexodesk_clipboard::Content>();
    let clipboard = match nexodesk_clipboard::spawn_sync(move |content| clip_tx.send(content).is_ok()) {
        Ok(tx) => Some(tx),
        Err(e) => {
            warn!("clipboard not shared: {e:#}");
            None
        }
    };

    // After the video one, the agent's unidirectional streams carry images copied to
    // its clipboard and the files downloaded with the file manager.
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
                    if !first.clipboard_image {
                        return;
                    }
                    match nexodesk_transport::read_stream(&mut rx, &first, nexodesk_clipboard::MAX_IMAGE_PNG as u64).await {
                        Ok(png) => {
                            if let Some(clipboard) = clipboard {
                                let _ = clipboard.send(nexodesk_clipboard::Content::Image(png));
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
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    ctrl_tx.send(&control(Msg::Ping(Ping { sent_us: nexodesk_proto::now_us() }))).await?;
                }
                Some(msg) = control_rx.recv() => {
                    ctrl_tx.send(&control(msg)).await?;
                }
                Some(content) = clip_rx.recv() => match content {
                    nexodesk_clipboard::Content::Text(text) => {
                        ctrl_tx.send(&control(Msg::Clipboard(nexodesk_proto::v1::Clipboard { text }))).await?;
                    }
                    // On its own stream, not to hold up the control channel.
                    nexodesk_clipboard::Content::Image(png) => {
                        let conn = conn.clone();
                        tokio::spawn(async move {
                            let header = FileChunk { clipboard_image: true, ..Default::default() };
                            if let Err(e) = nexodesk_transport::send_stream(&conn, header, &png).await {
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
                        let now = nexodesk_proto::now_us();
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
                    Some(ControlMessage { msg: Some(Msg::Clipboard(clip)) }) => {
                        if let Some(clipboard) = &clipboard {
                            let _ = clipboard.send(nexodesk_clipboard::Content::Text(clip.text));
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
                        let _ = proxy.send_event(NetEvent::FileDone { name, ok: false, message: format!("{e:#}") });
                    }
                }
                Outgoing::Upload { id, local, dir, offset } => {
                    // The outcome comes from the agent (FileReceived with the id).
                    if let Err(e) = send_upload(&conn, id, &local, dir, offset, &proxy).await {
                        warn!("sending {} failed: {e:#}", local.display());
                        let _ = proxy.send_event(NetEvent::TransferDone { id, ok: false, message: format!("{e:#}") });
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

/// File manager upload: `local` from `offset` on, into `dir` on the remote PC.
async fn send_upload(
    conn: &nexodesk_transport::Connection,
    id: u32,
    local: &std::path::Path,
    dir: String,
    offset: u64,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(local).await.with_context(|| format!("opening {}", local.display()))?;
    let total = file.metadata().await?.len();
    if offset > total {
        bail!("resume point beyond the end of the file");
    }
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut tx = FramedSend::new(conn.open_uni().await?);
    tx.send(&FileChunk { name: file_name(local), size: total, dir, offset, transfer_id: id, ..Default::default() }).await?;
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
        let file = tokio::fs::OpenOptions::new().append(true).open(&part).await.context("nothing to resume")?;
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
    tokio::fs::rename(&part, &final_path).await.with_context(|| format!("saving {}", final_path.display()))?;
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
    (1..).map(|i| target.with_file_name(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).expect("a free name")
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
