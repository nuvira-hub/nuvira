//! Headless viewer (plan 7.2, WP3): the viewer's side of a session without a
//! window, speaking the frozen handshake of `client/viewer/src/net.rs`
//! (`handshake` :535, `session` :692, `receive_datagram_video` :1663,
//! `decode_loop` :1774) and the feedback of `viewer/src/lib.rs` `send_feedback`
//! :829, so the agent's adaptive bitrate behaves as with the real viewer.
//!
//! Interface (plan section 11):
//! ```ignore
//! let mut s = HeadlessViewer::connect_direct(addr, Some(fp), ViewerOptions::default()).await?;
//! // or HeadlessViewer::from_connection(conn, opts) on a connection made elsewhere
//! s.first_frame(Duration::from_secs(10)).await?;
//! tokio::time::sleep(warm_up).await;
//! let stats: SessionStats = s.run(Duration::from_secs(20)).await?; // measured window
//! s.control().send(&msg).await?;   // ControlMessage to the agent (queued, in order)
//! s.input().send(&event).await?;   // InputEvent on the input stream
//! s.events().recv().await;         // every ControlMessage from the agent (Pong too)
//! let last = s.close().await;
//! ```
//!
//! Everything runs in background tasks of the **current** runtime from the moment
//! the session is open (ping every 1 s, feedback every 250 ms, keyframe requests,
//! video reception), plus one decoder thread; `run`/`mark`/`snapshot` only choose
//! the measured window. Run it on [`super::viewer_runtime`] so allocations and CPU
//! are attributed to the viewer side; the decoder thread marks itself.
//!
//! Differences from the real viewer, all on the viewer's own side: frames are
//! "shown" when decoded (no `FrameSlot`, no renderer, no recorder), no audio, no
//! clipboard, no file manager; latency uses the shared clock of an in-process
//! session ([`ViewerOptions::same_clock`], default) instead of the best-RTT clock
//! offset (still computed and recorded).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::control_message::Msg;
use nuvira_proto::v1::{
    Auth, ControlMessage, Feedback, Hello, Ping, RequestKeyframe, VideoFormat, VideoFrame,
};
use nuvira_transport::{Connection, Fingerprint, FramedRecv, FramedSend};
use serde::Serialize;
use tokio::sync::{Notify, mpsc};

use super::{CpuMark, SideUsage};
use crate::measure::{Distribution, UsHistogram};

/// A longer pause between two decoded frames is a freeze (`viewer/src/report.rs` FREEZE).
pub const FREEZE: Duration = Duration::from_millis(200);

/// Which decoder the headless viewer uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum DecoderChoice {
    /// `nuvira_codec::software_decoder()` (OpenH264).
    Software,
    /// `nuvira_codec::best_decoder()` (`NUVIRA_DECODER=hardware` selects MF/VT).
    Best,
}

#[derive(Clone, Debug)]
pub struct ViewerOptions {
    /// Ask for the video as datagrams with FEC (`Hello.video_datagrams`).
    pub video_datagrams: bool,
    /// Session password, sent when the agent asks for one.
    pub password: Option<String>,
    /// Two-factor code, when the agent asks for one (`CodeRequired`).
    pub code: Option<String>,
    pub decoder: DecoderChoice,
    /// Agent and viewer share one clock (in-process or same machine): latency is
    /// `decoded - capture` directly. False: the best-RTT offset is applied, as the
    /// real viewer does.
    pub same_clock: bool,
    /// Decoded frames kept (in the measured window) for quality scoring.
    pub quality_samples: usize,
    /// Keep one decoded frame every this many (in the measured window).
    pub sample_every: u32,
    pub device_name: String,
    /// Send `Feedback` (the agent's adaptive bitrate needs it). Default true.
    pub feedback: bool,
    /// Keep a record of every decoded frame of the window (`SessionStats::per_frame`,
    /// exact percentiles). Default true. False for long sessions (B-RES-01): the window
    /// is summed up in fixed memory instead ([`FrameAgg`]), so the bench's own records
    /// (72 B per frame, ~15 MiB/h at 60 fps) do not show up as the session's memory
    /// growth (VWR-1).
    pub keep_frames: bool,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        ViewerOptions {
            video_datagrams: false,
            password: None,
            code: None,
            decoder: DecoderChoice::Software,
            same_clock: true,
            quality_samples: 0,
            sample_every: 15,
            device_name: "nuvira-bench headless".into(),
            feedback: true,
            keep_frames: true,
        }
    }
}

/// Times of the handshake, from the start of `connect_direct` / `from_connection`.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct HandshakeTimes {
    /// QUIC connection established (connect_direct only).
    pub connected_ms: Option<f64>,
    /// Agent `Hello` received.
    pub hello_ms: f64,
    /// `AuthResult` received (when a password was asked).
    pub auth_ms: Option<f64>,
    /// `VideoFormat` received.
    pub video_format_ms: f64,
    /// Video stream accepted (stream mode: the first frame's data is on its way).
    pub video_channel_ms: f64,
}

/// One decoded (or dropped) frame.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct FrameRec {
    pub seq: u64,
    pub keyframe: bool,
    pub bytes: u32,
    /// Agent clock at capture, encode end (µs since 1970).
    pub capture_us: u64,
    pub encoded_us: u64,
    pub encode_us: u32,
    /// Viewer clock when received and when decoded.
    pub received_us: u64,
    pub decoded_us: u64,
    pub decode_us: u32,
    /// Capture -> decoded with the clock offset in force, µs (None before it is known).
    pub latency_us: Option<i64>,
}

/// A decoded frame kept for quality scoring.
pub struct Sample {
    pub seq: u64,
    pub capture_us: u64,
    pub frame: nuvira_codec::YuvFrame,
}

/// The measured window of a session.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionStats {
    pub window_s: f64,
    /// Since the start of the session (first decoded frame).
    pub first_frame_ms: Option<f64>,
    pub frames: u64,
    pub bytes: u64,
    pub fps: f64,
    pub mbit_s: f64,
    /// Capture -> decoded, ms.
    pub latency_ms: Distribution,
    /// Capture -> received (before decoding), ms.
    pub latency_received_ms: Distribution,
    /// Encode end -> received: the network part, ms.
    pub transit_ms: Distribution,
    pub encode_ms: Distribution,
    pub decode_ms: Distribution,
    pub rtt_ms: Distribution,
    /// Agent clock minus viewer clock from the best-RTT Pong, µs.
    pub clock_offset_us: Option<i64>,
    pub freezes: u64,
    pub freeze_ms: f64,
    pub keyframes: u64,
    pub keyframe_requests: u64,
    pub decode_errors: u64,
    /// Frames dropped while waiting for a keyframe.
    pub dropped_waiting_key: u64,
    /// Datagram mode: shards expected and lost, frames lost, frames that needed parity.
    pub shards: u64,
    pub shards_lost: u64,
    pub frames_lost: u64,
    pub frames_rebuilt: u64,
    pub feedback_sent: u64,
    /// Received video kbit/s per 250 ms of the window.
    pub bitrate_trace_kbps: Vec<f64>,
    /// Control messages received from the agent in the window, by kind.
    pub control_messages: BTreeMap<String, u64>,
    pub usage: SideUsage,
    #[serde(skip)]
    pub per_frame: Vec<FrameRec>,
    #[serde(skip)]
    pub samples: Arc<Vec<Sample>>,
}

impl std::fmt::Debug for Sample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sample(seq {}, {}x{})", self.seq, self.frame.width, self.frame.height)
    }
}

impl SessionStats {
    /// Time for latency to settle after each freeze: from the frame that ends the
    /// gap until 10 consecutive frames are within 1.5 x the window median + 5 ms.
    pub fn settle_after_freezes_ms(&self) -> Vec<f64> {
        let lat: Vec<(u64, f64)> = self
            .per_frame
            .iter()
            .filter_map(|f| f.latency_us.map(|l| (f.decoded_us, l as f64 / 1000.0)))
            .collect();
        if lat.len() < 20 {
            return Vec::new();
        }
        let limit = self.latency_ms.p50 * 1.5 + 5.0;
        let mut out = Vec::new();
        for i in 1..lat.len() {
            if lat[i].0.saturating_sub(lat[i - 1].0) <= FREEZE.as_micros() as u64 {
                continue;
            }
            let mut run = 0;
            for j in i..lat.len() {
                run = if lat[j].1 <= limit { run + 1 } else { 0 };
                if run >= 10 {
                    out.push(lat[j + 1 - 10].0.saturating_sub(lat[i].0) as f64 / 1000.0);
                    break;
                }
            }
        }
        out
    }
}

/// The decoded frames of a window summed up in fixed memory (`keep_frames: false`):
/// the same figures as [`frames_stats`] computes from the records, with the
/// percentiles of [`UsHistogram`] (within 0.1 %).
#[derive(Default)]
struct FrameAgg {
    frames: u64,
    bytes: u64,
    keyframes: u64,
    /// Viewer clock at the window start, µs (the origin of `trace`).
    start_us: u64,
    last_decoded_us: Option<u64>,
    freezes: u64,
    freeze_ms: f64,
    /// Received kbit/s per 250 ms.
    trace: Vec<f64>,
    latency_us: UsHistogram,
    /// Received minus capture / minus encode end, µs, without the clock offset (applied
    /// at the end, as `frames_stats` applies the offset in force then).
    recv_capture_us: UsHistogram,
    recv_encoded_us: UsHistogram,
    encode_us: UsHistogram,
    decode_us: UsHistogram,
}

impl FrameAgg {
    fn add(&mut self, f: &FrameRec) {
        self.frames += 1;
        self.bytes += f.bytes as u64;
        self.keyframes += u64::from(f.keyframe);
        if let Some(last) = self.last_decoded_us {
            let gap = f.decoded_us.saturating_sub(last);
            if gap > FREEZE.as_micros() as u64 {
                self.freezes += 1;
                self.freeze_ms += gap as f64 / 1000.0;
            }
        }
        self.last_decoded_us = Some(self.last_decoded_us.map_or(f.decoded_us, |l| l.max(f.decoded_us)));
        let k = (f.received_us.saturating_sub(self.start_us) / 250_000) as usize;
        // A frame received days after the start is no bitrate sample (clock jump).
        if k < 1 << 20 {
            if self.trace.len() <= k {
                self.trace.resize(k + 1, 0.0);
            }
            self.trace[k] += f.bytes as f64 * 8.0 / 0.25 / 1000.0;
        }
        if let Some(l) = f.latency_us {
            self.latency_us.add(l);
        }
        self.recv_capture_us.add(f.received_us as i64 - f.capture_us as i64);
        self.recv_encoded_us.add(f.received_us as i64 - encoded_us_of(f) as i64);
        self.encode_us.add(f.encode_us as i64);
        self.decode_us.add(f.decode_us as i64);
    }
}

/// Encode end of a frame on the agent's clock (older agents send only `encode_us`).
fn encoded_us_of(f: &FrameRec) -> u64 {
    if f.encoded_us == 0 { f.capture_us + f.encode_us as u64 } else { f.encoded_us }
}

#[derive(Default)]
struct Window {
    started: Option<Instant>,
    /// Every decoded frame (`keep_frames`); empty otherwise, see `agg`.
    frames: Vec<FrameRec>,
    agg: FrameAgg,
    rtt_us: Vec<u64>,
    keyframe_requests: u64,
    decode_errors: u64,
    dropped_waiting_key: u64,
    shards: u64,
    shards_lost: u64,
    frames_lost: u64,
    frames_rebuilt: u64,
    feedback_sent: u64,
    control: BTreeMap<String, u64>,
    samples: Vec<Sample>,
    decoded: u64,
}

/// Losses of the video as datagrams since the last feedback (as the viewer's `VideoLoss`).
#[derive(Default)]
struct VideoLoss {
    shards: AtomicU64,
    shards_lost: AtomicU64,
    frames_lost: AtomicU64,
}

impl VideoLoss {
    fn fill(&self, f: &mut Feedback) {
        let take = |n: &AtomicU64| Some(n.swap(0, Relaxed).min(u32::MAX as u64) as u32);
        f.shards = take(&self.shards);
        f.shards_lost = take(&self.shards_lost);
        f.frames_lost = take(&self.frames_lost);
    }
}

struct FeedbackWindow {
    since: Instant,
    latencies: Vec<u32>,
    transits: Vec<u32>,
}

struct Shared {
    same_clock: bool,
    keep_frames: bool,
    datagrams: bool,
    quality_samples: usize,
    sample_every: u32,
    window: Mutex<Window>,
    mark: Mutex<CpuMark>,
    first_frame: Mutex<Option<Instant>>,
    first_frame_notify: Notify,
    offset_us: AtomicI64,
    offset_known: AtomicBool,
    best_rtt_us: AtomicU64,
    loss: VideoLoss,
    feedback: Mutex<FeedbackWindow>,
    closed: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Shared {
    fn fail(&self, e: &anyhow::Error) {
        if !self.closed.load(Relaxed) {
            self.error.lock().expect("error").get_or_insert_with(|| format!("{e:#}"));
        }
    }
}

/// Sends control messages to the agent through the session's control task (it
/// also sends the pings and feedback, in order with these).
pub struct ControlTx {
    tx: mpsc::UnboundedSender<ControlMessage>,
}

impl ControlTx {
    pub async fn send(&mut self, msg: &ControlMessage) -> Result<()> {
        self.tx.send(msg.clone()).map_err(|_| anyhow::anyhow!("session closed"))
    }

    /// Sends one `Msg`.
    pub async fn send_msg(&mut self, msg: Msg) -> Result<()> {
        self.send(&ControlMessage { msg: Some(msg) }).await
    }
}

/// Entry points.
pub struct HeadlessViewer;

/// An open session.
pub struct HeadlessSession {
    pub conn: Connection,
    /// The agent's `Hello`.
    pub agent: Hello,
    pub format: VideoFormat,
    pub times: HandshakeTimes,
    /// Start of `connect_direct` / `from_connection`.
    pub started: Instant,
    control: ControlTx,
    input: FramedSend,
    events: mpsc::UnboundedReceiver<ControlMessage>,
    shared: Arc<Shared>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    decoder: Option<std::thread::JoinHandle<()>>,
}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

fn msg_kind(m: &ControlMessage) -> String {
    match &m.msg {
        None => "none".into(),
        Some(msg) => {
            let s = format!("{msg:?}");
            s.split(['(', ' ', '{']).next().unwrap_or("?").to_string()
        }
    }
}

impl HeadlessViewer {
    /// Connects to an agent at `addr` (127.0.0.1-bound client, production
    /// transport config: [`super::loopback_connect`]) and opens the session.
    pub async fn connect_direct(
        addr: SocketAddr,
        fp: Option<Fingerprint>,
        opts: ViewerOptions,
    ) -> Result<HeadlessSession> {
        let t0 = Instant::now();
        let conn = super::loopback_connect(addr, fp).await?;
        let connected = t0.elapsed();
        let mut s = Self::open(conn, opts, t0).await?;
        s.times.connected_ms = Some(ms(connected));
        Ok(s)
    }

    /// Opens the session on a connection made elsewhere (server mode, WP4).
    pub async fn from_connection(conn: Connection, opts: ViewerOptions) -> Result<HeadlessSession> {
        Self::open(conn, opts, Instant::now()).await
    }

    async fn open(conn: Connection, opts: ViewerOptions, t0: Instant) -> Result<HeadlessSession> {
        let r = Self::handshake(&conn, opts, t0).await;
        if r.is_err() {
            conn.close(0u32.into(), b"handshake failed");
        }
        r
    }

    async fn handshake(conn: &Connection, opts: ViewerOptions, t0: Instant) -> Result<HeadlessSession> {
        let (send, recv) = conn.open_bi().await?;
        let mut ctrl_tx = FramedSend::new(send);
        let mut ctrl_rx = FramedRecv::new(recv);
        let datagrams_wanted = opts.video_datagrams && conn.max_datagram_size().is_some();
        ctrl_tx
            .send(&control(Msg::Hello(Hello {
                protocol_version: nuvira_proto::PROTOCOL_VERSION,
                device_name: opts.device_name.clone(),
                os: std::env::consts::OS.into(),
                app_version: format!("nuvira-bench {}", env!("CARGO_PKG_VERSION")),
                video_datagrams: datagrams_wanted,
                terminal_only: false,
                print: false,
                recording: false,
                audio: false,
                cursor: true,
                cursor_cache: true,
                clipboard_files: false,
                handover: false,
                switch_sides: false,
                two_factor: true,
                microphone: false,
                ..Default::default()
            })))
            .await?;
        let Some(ControlMessage { msg: Some(Msg::Hello(agent)) }) = ctrl_rx.recv().await? else {
            bail!("the agent did not introduce itself");
        };
        let mut times = HandshakeTimes { hello_ms: ms(t0.elapsed()), ..Default::default() };
        if agent.password_required {
            let password = opts.password.clone().context("the agent asks for a password")?;
            let auth = Auth { password, ..Default::default() };
            ctrl_tx.send(&control(Msg::Auth(auth.clone()))).await?;
            let mut answer = ctrl_rx.recv().await?;
            if let Some(ControlMessage { msg: Some(Msg::CodeRequired(_)) }) = answer {
                let code = opts.code.clone().context("the agent asks for a two-factor code")?;
                ctrl_tx.send(&control(Msg::Auth(Auth { code, ..auth }))).await?;
                answer = ctrl_rx.recv().await?;
            }
            if let Some(ControlMessage { msg: Some(Msg::ConfirmationPending(_)) }) = answer {
                answer = ctrl_rx.recv().await?;
            }
            match answer {
                Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) if r.ok => {}
                Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) => bail!("refused: {}", r.message),
                other => bail!("no answer to the password: {other:?}"),
            }
            times.auth_ms = Some(ms(t0.elapsed()));
        }
        // Control messages before VideoFormat are kept for `events()`.
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ControlMessage>();
        let format = loop {
            match ctrl_rx.recv::<ControlMessage>().await? {
                Some(ControlMessage { msg: Some(Msg::VideoFormat(f)) }) => break f,
                Some(other) => {
                    let _ = events_tx.send(other);
                }
                None => bail!("missing video format"),
            }
        };
        times.video_format_ms = ms(t0.elapsed());
        let datagrams = agent.video_datagrams;
        let input = FramedSend::new(conn.open_uni().await?);
        let video_rx = if datagrams { None } else { Some(FramedRecv::new(conn.accept_uni().await?)) };
        times.video_channel_ms = ms(t0.elapsed());

        let shared = Arc::new(Shared {
            same_clock: opts.same_clock,
            keep_frames: opts.keep_frames,
            datagrams,
            quality_samples: opts.quality_samples,
            sample_every: opts.sample_every.max(1),
            window: Mutex::new(Window::default()),
            mark: Mutex::new(CpuMark::now()),
            first_frame: Mutex::new(None),
            first_frame_notify: Notify::new(),
            offset_us: AtomicI64::new(0),
            offset_known: AtomicBool::new(opts.same_clock),
            best_rtt_us: AtomicU64::new(u64::MAX),
            loss: VideoLoss::default(),
            feedback: Mutex::new(FeedbackWindow {
                since: Instant::now(),
                latencies: Vec::new(),
                transits: Vec::new(),
            }),
            closed: AtomicBool::new(false),
            error: Mutex::new(None),
        });

        // Decoder thread (viewer side), fed as the viewer feeds `decode_loop`.
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<(VideoFrame, u64)>(4);
        let (key_tx, key_rx) = mpsc::unbounded_channel::<()>();
        let (shown_tx, shown_rx) = mpsc::unbounded_channel::<()>();
        let decoder = {
            let shared = shared.clone();
            let key_tx = key_tx.clone();
            let choice = opts.decoder;
            std::thread::Builder::new()
                .name("bench-decoding".into())
                .spawn(move || decode_loop(frame_rx, choice, shared, key_tx, shown_tx))?
        };

        let mut tasks = Vec::new();
        {
            let shared = shared.clone();
            let conn = conn.clone();
            tasks.push(tokio::spawn(async move {
                let r = match video_rx {
                    Some(rx) => receive_stream(rx, &frame_tx).await,
                    None => receive_datagrams(&conn, &frame_tx, &key_tx, &shared).await,
                };
                if let Err(e) = r {
                    shared.fail(&e);
                }
            }));
        }
        let (user_tx, user_rx) = mpsc::unbounded_channel::<ControlMessage>();
        {
            let shared = shared.clone();
            let feedback = opts.feedback;
            tasks.push(tokio::spawn(async move {
                let r =
                    control_loop(ctrl_tx, ctrl_rx, &shared, feedback, (user_rx, key_rx, shown_rx), events_tx)
                        .await;
                if let Err(e) = r {
                    shared.fail(&e);
                }
            }));
        }
        Ok(HeadlessSession {
            conn: conn.clone(),
            agent,
            format,
            times,
            started: t0,
            control: ControlTx { tx: user_tx },
            input,
            events: events_rx,
            shared,
            tasks,
            decoder: Some(decoder),
        })
    }
}

impl HeadlessSession {
    pub fn control(&mut self) -> &mut ControlTx {
        &mut self.control
    }

    pub fn input(&mut self) -> &mut FramedSend {
        &mut self.input
    }

    /// Every control message from the agent (also `Pong`, `Displays`, `Permissions`,
    /// cursor shapes and positions, chat).
    pub fn events(&mut self) -> &mut mpsc::UnboundedReceiver<ControlMessage> {
        &mut self.events
    }

    /// Whether the video arrives as datagrams.
    pub fn datagrams(&self) -> bool {
        self.shared.datagrams
    }

    /// The first error of a background task (connection lost, protocol error).
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().expect("error").clone()
    }

    /// Waits for the first decoded frame; time since the start of the session.
    pub async fn first_frame(&self, timeout: Duration) -> Result<Duration> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.shared.first_frame_notify.notified();
            if let Some(t) = *self.shared.first_frame.lock().expect("first") {
                return Ok(t.duration_since(self.started));
            }
            if let Some(e) = self.error() {
                bail!("no first frame: {e}");
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                bail!("no frame decoded in {timeout:?}");
            }
        }
    }

    /// Starts a new measured window now (everything before is discarded).
    pub fn mark(&self) {
        let mut w = self.shared.window.lock().expect("window");
        *w = Window { started: Some(Instant::now()), ..Default::default() };
        w.agg.start_us = nuvira_proto::now_us();
        *self.shared.mark.lock().expect("mark") = CpuMark::now();
    }

    /// The window from the last [`mark`](Self::mark) to now.
    pub fn snapshot(&self) -> SessionStats {
        let usage = CpuMark::now().since(&self.shared.mark.lock().expect("mark"));
        let mut w = self.shared.window.lock().expect("window");
        let samples = Arc::new(std::mem::take(&mut w.samples));
        let mut s = stats_of(&w, usage, &self.shared);
        s.samples = samples;
        s.first_frame_ms =
            self.shared.first_frame.lock().expect("first").map(|t| ms(t.duration_since(self.started)));
        s
    }

    /// Measures `duration` from now: [`mark`](Self::mark), wait, [`snapshot`](Self::snapshot).
    /// Fails if the session broke meanwhile.
    pub async fn run(&mut self, duration: Duration) -> Result<SessionStats> {
        self.mark();
        tokio::time::sleep(duration).await;
        let s = self.snapshot();
        if let Some(e) = self.error() {
            bail!("session failed during the measurement: {e}");
        }
        Ok(s)
    }

    /// Closes the session (as the viewer does: the connection closed with code 0)
    /// and stops the tasks and the decoder thread. Returns the stats of the last
    /// window.
    pub async fn close(mut self) -> SessionStats {
        let stats = self.snapshot();
        self.shared.closed.store(true, Relaxed);
        self.conn.close(0u32.into(), b"bench viewer closed");
        for t in self.tasks.drain(..) {
            t.abort();
            let _ = t.await;
        }
        if let Some(d) = self.decoder.take() {
            let _ = tokio::task::spawn_blocking(move || d.join()).await;
        }
        stats
    }
}

impl Drop for HeadlessSession {
    fn drop(&mut self) {
        self.shared.closed.store(true, Relaxed);
        for t in &self.tasks {
            t.abort();
        }
        self.conn.close(0u32.into(), b"bench viewer dropped");
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn stats_of(w: &Window, usage: SideUsage, shared: &Shared) -> SessionStats {
    let window_s = w.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
    let offset = shared.offset_known.load(Relaxed).then(|| shared.offset_us.load(Relaxed));
    let mut s = if shared.keep_frames {
        frames_stats(w, window_s, offset, shared.same_clock)
    } else {
        agg_stats(&w.agg, window_s, offset, shared.same_clock)
    };
    s.window_s = window_s;
    s.fps = s.frames as f64 / window_s.max(1e-3);
    s.mbit_s = s.bytes as f64 * 8.0 / window_s.max(1e-3) / 1e6;
    s.rtt_ms = Distribution::of(&w.rtt_us.iter().map(|&r| r as f64 / 1000.0).collect::<Vec<_>>());
    s.clock_offset_us = offset.filter(|_| shared.best_rtt_us.load(Relaxed) != u64::MAX);
    s.keyframe_requests = w.keyframe_requests;
    s.decode_errors = w.decode_errors;
    s.dropped_waiting_key = w.dropped_waiting_key;
    s.shards = w.shards;
    s.shards_lost = w.shards_lost;
    s.frames_lost = w.frames_lost;
    s.frames_rebuilt = w.frames_rebuilt;
    s.feedback_sent = w.feedback_sent;
    s.control_messages = w.control.clone();
    s.usage = usage;
    s
}

/// The frame figures of a window from its records (`keep_frames`).
fn frames_stats(w: &Window, window_s: f64, offset: Option<i64>, same_clock: bool) -> SessionStats {
    let frames = &w.frames;
    let bytes: u64 = frames.iter().map(|f| f.bytes as u64).sum();
    let dist = |f: &dyn Fn(&FrameRec) -> Option<f64>| {
        Distribution::of(&frames.iter().filter_map(f).collect::<Vec<_>>())
    };
    let mut decoded: Vec<u64> = frames.iter().map(|f| f.decoded_us).collect();
    decoded.sort_unstable();
    let (mut freezes, mut freeze_ms) = (0u64, 0f64);
    for g in decoded.windows(2) {
        let gap = g[1].saturating_sub(g[0]);
        if gap > FREEZE.as_micros() as u64 {
            freezes += 1;
            freeze_ms += gap as f64 / 1000.0;
        }
    }
    // Received bitrate per 250 ms from the window start.
    let mut trace = Vec::new();
    if let (Some(start), Some(_)) = (w.started, frames.first()) {
        let start_us = nuvira_proto::now_us().saturating_sub(start.elapsed().as_micros() as u64);
        let buckets = ((window_s / 0.25).ceil() as usize).max(1);
        trace = vec![0f64; buckets];
        for f in frames {
            let k = (f.received_us.saturating_sub(start_us) / 250_000) as usize;
            if let Some(b) = trace.get_mut(k) {
                *b += f.bytes as f64 * 8.0 / 0.25 / 1000.0;
            }
        }
    }
    let transit = |f: &FrameRec| {
        let off = if same_clock { 0 } else { offset.unwrap_or(0) };
        Some((f.received_us as i64 - (encoded_us_of(f) as i64 - off)) as f64 / 1000.0)
    };
    SessionStats {
        frames: frames.len() as u64,
        bytes,
        latency_ms: dist(&|f| f.latency_us.map(|l| l as f64 / 1000.0)),
        latency_received_ms: dist(&|f| {
            let off = if same_clock { 0 } else { offset? };
            Some((f.received_us as i64 - (f.capture_us as i64 - off)) as f64 / 1000.0)
        }),
        transit_ms: dist(&transit),
        encode_ms: dist(&|f| Some(f.encode_us as f64 / 1000.0)),
        decode_ms: dist(&|f| Some(f.decode_us as f64 / 1000.0)),
        freezes,
        freeze_ms,
        keyframes: frames.iter().filter(|f| f.keyframe).count() as u64,
        bitrate_trace_kbps: trace,
        per_frame: frames.clone(),
        ..Default::default()
    }
}

/// The same figures from the fixed-memory summary (`keep_frames: false`).
fn agg_stats(a: &FrameAgg, window_s: f64, offset: Option<i64>, same_clock: bool) -> SessionStats {
    let mut trace = Vec::new();
    if a.frames > 0 {
        trace = a.trace.clone();
        trace.resize(((window_s / 0.25).ceil() as usize).max(1), 0.0);
    }
    let off_ms = if same_clock { 0.0 } else { offset.unwrap_or(0) as f64 / 1000.0 };
    // Without a known offset the records give no `latency_received` value at all.
    let latency_received_ms = if same_clock || offset.is_some() {
        a.recv_capture_us.distribution(1e-3, off_ms)
    } else {
        Distribution::default()
    };
    SessionStats {
        frames: a.frames,
        bytes: a.bytes,
        latency_ms: a.latency_us.distribution(1e-3, 0.0),
        latency_received_ms,
        transit_ms: a.recv_encoded_us.distribution(1e-3, off_ms),
        encode_ms: a.encode_us.distribution(1e-3, 0.0),
        decode_ms: a.decode_us.distribution(1e-3, 0.0),
        freezes: a.freezes,
        freeze_ms: a.freeze_ms,
        keyframes: a.keyframes,
        bitrate_trace_kbps: trace,
        ..Default::default()
    }
}

async fn receive_stream(
    mut rx: FramedRecv,
    frames: &std::sync::mpsc::SyncSender<(VideoFrame, u64)>,
) -> Result<()> {
    while let Some(frame) = rx.recv::<VideoFrame>().await? {
        // As the viewer: a blocking hand-off to the decoder (4 frames of slack).
        if frames.send((frame, nuvira_proto::now_us())).is_err() {
            break;
        }
    }
    Ok(())
}

/// Which data shards of a frame's blocks arrived, to count frames rebuilt by parity.
#[derive(Default)]
pub struct ShardSeen {
    /// seq -> per block (data shards, data indices received).
    frames: BTreeMap<u64, Vec<(usize, Vec<bool>)>>,
}

impl ShardSeen {
    pub fn push(&mut self, p: &[u8]) {
        let h = nuvira_transport::fec::HEADER;
        if p.len() <= h {
            return;
        }
        let seq = u64::from_le_bytes(p[1..9].try_into().expect("8 bytes"));
        let (block, blocks, index, data) = (p[9] as usize, p[10] as usize, p[11] as usize, p[12] as usize);
        if block >= blocks {
            return;
        }
        let e = self.frames.entry(seq).or_insert_with(|| vec![(0, Vec::new()); blocks]);
        if let Some(b) = e.get_mut(block) {
            if b.1.is_empty() {
                *b = (data, vec![false; data]);
            }
            if index < b.0 {
                b.1[index] = true;
            }
        }
    }

    /// Frame `seq` completed: whether some data shard was missing (parity used).
    pub fn completed(&mut self, seq: u64) -> bool {
        let rebuilt =
            self.frames.get(&seq).is_some_and(|blocks| blocks.iter().any(|(_, got)| got.iter().any(|g| !g)));
        self.frames.retain(|&s, _| s > seq);
        while self.frames.len() > 128 {
            let first = *self.frames.keys().next().expect("non-empty");
            self.frames.remove(&first);
        }
        rebuilt
    }
}

/// `receive_datagram_video` of the viewer, with the rebuilt-by-parity count.
async fn receive_datagrams(
    conn: &Connection,
    frames: &std::sync::mpsc::SyncSender<(VideoFrame, u64)>,
    request_keyframe: &mpsc::UnboundedSender<()>,
    shared: &Shared,
) -> Result<()> {
    use prost::Message;
    let mut rx = nuvira_transport::fec::Receiver::default();
    let mut seen = ShardSeen::default();
    let mut last = 0u64;
    let mut waiting_key = false;
    let mut asked: Option<Instant> = None;
    loop {
        let packet = match conn.read_datagram().await {
            Ok(p) => p,
            Err(quinn::ConnectionError::ApplicationClosed(_) | quinn::ConnectionError::LocallyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        seen.push(&packet);
        let pushed = rx.push(&packet);
        let (shards, lost) = rx.take_loss();
        shared.loss.shards.fetch_add(shards, Relaxed);
        shared.loss.shards_lost.fetch_add(lost, Relaxed);
        let (seq, bytes) = match pushed {
            Ok(Some(done)) => done,
            Ok(None) => {
                if shards > 0 {
                    let mut w = shared.window.lock().expect("window");
                    w.shards += shards;
                    w.shards_lost += lost;
                }
                continue;
            }
            Err(_) => continue,
        };
        let rebuilt = seen.completed(seq);
        let frame = VideoFrame::decode(bytes.as_slice()).context("video frame")?;
        let mut newly_lost = 0;
        if seq != last + 1 {
            newly_lost = seq - last - 1;
            shared.loss.frames_lost.fetch_add(newly_lost, Relaxed);
            waiting_key = true;
        }
        {
            let mut w = shared.window.lock().expect("window");
            w.shards += shards;
            w.shards_lost += lost;
            w.frames_lost += newly_lost;
            w.frames_rebuilt += u64::from(rebuilt);
        }
        last = seq;
        if waiting_key {
            if frame.keyframe {
                waiting_key = false;
                asked = None;
            } else {
                shared.window.lock().expect("window").dropped_waiting_key += 1;
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

type ControlChannels =
    (mpsc::UnboundedReceiver<ControlMessage>, mpsc::UnboundedReceiver<()>, mpsc::UnboundedReceiver<()>);

/// Feedback every 250 ms with frames, as `App::send_feedback`.
fn take_feedback(shared: &Shared) -> Option<Feedback> {
    let mut fw = shared.feedback.lock().expect("feedback");
    let window = fw.since.elapsed();
    if window < Duration::from_millis(250) || fw.latencies.is_empty() {
        return None;
    }
    fw.latencies.sort_unstable();
    fw.transits.sort_unstable();
    let f = Feedback {
        latency_ms_p50: fw.latencies[fw.latencies.len() / 2],
        latency_ms_min: fw.latencies[0],
        frames: fw.latencies.len() as u32,
        window_ms: window.as_millis() as u32,
        transit_ms_p50: fw.transits.get(fw.transits.len() / 2).copied(),
        transit_ms_min: fw.transits.first().copied(),
        ..Default::default()
    };
    fw.latencies.clear();
    fw.transits.clear();
    fw.since = Instant::now();
    Some(f)
}

async fn control_loop(
    mut ctrl_tx: FramedSend,
    mut ctrl_rx: FramedRecv,
    shared: &Shared,
    feedback: bool,
    (mut user_rx, mut key_rx, mut shown_rx): ControlChannels,
    events: mpsc::UnboundedSender<ControlMessage>,
) -> Result<()> {
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    let mut loss_tick = tokio::time::interval(Duration::from_millis(250));
    let mut last_feedback = Instant::now();
    let datagrams = shared.datagrams;
    let send_feedback = async |tx: &mut FramedSend, last: &mut Instant, mut f: Feedback| -> Result<()> {
        if datagrams {
            shared.loss.fill(&mut f);
        }
        *last = Instant::now();
        tx.send(&control(Msg::Feedback(f))).await?;
        let mut w = shared.window.lock().expect("window");
        if w.started.is_some() {
            w.feedback_sent += 1;
        }
        Ok(())
    };
    loop {
        tokio::select! {
            _ = ping.tick() => {
                ctrl_tx.send(&control(Msg::Ping(Ping { sent_us: nuvira_proto::now_us() }))).await?;
            }
            _ = loss_tick.tick(), if datagrams && feedback => {
                if last_feedback.elapsed() >= Duration::from_millis(250)
                    && shared.loss.frames_lost.load(Relaxed) > 0
                {
                    let f = Feedback { window_ms: last_feedback.elapsed().as_millis() as u32, ..Default::default() };
                    send_feedback(&mut ctrl_tx, &mut last_feedback, f).await?;
                }
            }
            Some(()) = shown_rx.recv() => {
                if feedback && let Some(f) = take_feedback(shared) {
                    send_feedback(&mut ctrl_tx, &mut last_feedback, f).await?;
                }
            }
            Some(()) = key_rx.recv() => {
                ctrl_tx.send(&control(Msg::RequestKeyframe(RequestKeyframe {}))).await?;
                let mut w = shared.window.lock().expect("window");
                if w.started.is_some() {
                    w.keyframe_requests += 1;
                }
            }
            Some(mut msg) = user_rx.recv() => {
                if let Some(Msg::Feedback(f)) = &mut msg.msg && datagrams {
                    shared.loss.fill(f);
                    last_feedback = Instant::now();
                }
                ctrl_tx.send(&msg).await?;
            }
            msg = ctrl_rx.recv::<ControlMessage>() => {
                let Some(msg) = msg? else { return Ok(()) };
                if let Some(Msg::Pong(pong)) = &msg.msg {
                    let now = nuvira_proto::now_us();
                    let rtt = now.saturating_sub(pong.ping_sent_us);
                    let midpoint = (pong.ping_sent_us + now) / 2;
                    let offset = pong.agent_us as i64 - midpoint as i64;
                    // As the viewer: feedback first, then the offset of the best RTT.
                    if feedback && let Some(f) = take_feedback(shared) {
                        send_feedback(&mut ctrl_tx, &mut last_feedback, f).await?;
                    }
                    if rtt <= shared.best_rtt_us.load(Relaxed) {
                        shared.best_rtt_us.store(rtt, Relaxed);
                        if !shared.same_clock {
                            shared.offset_us.store(offset, Relaxed);
                            shared.offset_known.store(true, Relaxed);
                        } else {
                            // Recorded only (latency uses the shared clock).
                            shared.offset_us.store(offset, Relaxed);
                        }
                    }
                    let mut w = shared.window.lock().expect("window");
                    if w.started.is_some() {
                        w.rtt_us.push(rtt);
                    }
                }
                {
                    let mut w = shared.window.lock().expect("window");
                    if w.started.is_some() {
                        *w.control.entry(msg_kind(&msg)).or_default() += 1;
                    }
                }
                let _ = events.send(msg);
            }
        }
    }
}

fn decode_loop(
    frames: std::sync::mpsc::Receiver<(VideoFrame, u64)>,
    choice: DecoderChoice,
    shared: Arc<Shared>,
    request_keyframe: mpsc::UnboundedSender<()>,
    shown: mpsc::UnboundedSender<()>,
) {
    super::viewer_thread_start();
    let decoder = match choice {
        DecoderChoice::Software => nuvira_codec::software_decoder(),
        DecoderChoice::Best => nuvira_codec::best_decoder(),
    };
    let mut decoder = match decoder {
        Ok(d) => d,
        Err(e) => {
            shared.fail(&e.context("decoder not available"));
            return;
        }
    };
    let mut waiting_key = false;
    while let Ok((frame, received_us)) = frames.recv() {
        if waiting_key && !frame.keyframe {
            shared.window.lock().expect("window").dropped_waiting_key += 1;
            super::viewer_checkpoint();
            continue;
        }
        let t = Instant::now();
        let result = decoder.decode(&frame.data);
        let decode_us = t.elapsed().as_micros() as u32;
        match result {
            Ok(Some(yuv)) => {
                waiting_key = false;
                let decoded_us = nuvira_proto::now_us();
                let first = {
                    let mut f = shared.first_frame.lock().expect("first");
                    let was = f.is_none();
                    f.get_or_insert_with(Instant::now);
                    was
                };
                if first {
                    shared.first_frame_notify.notify_waiters();
                }
                let known = shared.offset_known.load(Relaxed);
                let offset = if shared.same_clock { 0 } else { shared.offset_us.load(Relaxed) };
                let latency_us = known.then(|| decoded_us as i64 - (frame.capture_us as i64 - offset));
                let encoded_us = match frame.encoded_us {
                    0 => frame.capture_us + frame.encode_us as u64,
                    t => t,
                };
                if let Some(l) = latency_us {
                    let transit = received_us as i64 - (encoded_us as i64 - offset);
                    let mut fw = shared.feedback.lock().expect("feedback");
                    fw.latencies.push((l.max(0) / 1000) as u32);
                    fw.transits.push((transit.max(0) / 1000) as u32);
                }
                let rec = FrameRec {
                    seq: frame.seq,
                    keyframe: frame.keyframe,
                    bytes: frame.data.len() as u32,
                    capture_us: frame.capture_us,
                    encoded_us: frame.encoded_us,
                    encode_us: frame.encode_us,
                    received_us,
                    decoded_us,
                    decode_us,
                    latency_us,
                };
                // A sample is copied outside the window lock.
                let want_sample = {
                    let mut w = shared.window.lock().expect("window");
                    if w.started.is_some() {
                        w.decoded += 1;
                        if shared.keep_frames {
                            w.frames.push(rec);
                        } else {
                            w.agg.add(&rec);
                        }
                        shared.quality_samples > w.samples.len()
                            && (w.decoded - 1).is_multiple_of(shared.sample_every as u64)
                    } else {
                        false
                    }
                };
                if want_sample {
                    let copy = yuv.with_planes().into_owned();
                    let mut w = shared.window.lock().expect("window");
                    if w.started.is_some() {
                        w.samples.push(Sample { seq: frame.seq, capture_us: frame.capture_us, frame: copy });
                    }
                }
                let _ = shown.send(());
            }
            Ok(None) => {}
            Err(_) => {
                shared.window.lock().expect("window").decode_errors += 1;
                if decoder.name() != "openh264"
                    && let Ok(d) = nuvira_codec::software_decoder()
                {
                    decoder = d;
                }
                waiting_key = true;
                let _ = request_keyframe.send(());
            }
        }
        super::viewer_checkpoint();
    }
    super::viewer_checkpoint();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_memory_summary_matches_the_records() {
        // A 60 fps window with a keyframe every 120 frames, one 500 ms freeze, a latency
        // spread and a tail: the summary (B-RES-01) gives the records' figures (VWR-1).
        let start_us = nuvira_proto::now_us();
        let mut w = Window { started: Some(Instant::now()), ..Default::default() };
        w.agg.start_us = start_us;
        let mut t = start_us + 1000;
        for i in 0..3000u64 {
            t += if i == 1500 { 500_000 } else { 16_667 };
            let lat = 9_000 + (i * 7919 % 13_000) as i64 + if i % 97 == 0 { 80_000 } else { 0 };
            let rec = FrameRec {
                seq: i + 1,
                keyframe: i % 120 == 0,
                bytes: 2_000 + (i * 31 % 900) as u32,
                capture_us: t - lat as u64,
                encoded_us: t - lat as u64 + 4_000,
                encode_us: 4_000 + (i % 50) as u32,
                received_us: t - 2_000,
                decoded_us: t,
                decode_us: 1_500 + (i % 40) as u32,
                latency_us: Some(lat),
            };
            w.frames.push(rec);
            w.agg.add(&rec);
        }
        let window_s = (t - start_us) as f64 / 1e6 + 0.1;
        let a = frames_stats(&w, window_s, None, true);
        let b = agg_stats(&w.agg, window_s, None, true);
        assert_eq!((a.frames, a.bytes, a.keyframes, a.freezes), (b.frames, b.bytes, b.keyframes, b.freezes));
        assert!((a.freeze_ms - b.freeze_ms).abs() < 1e-6);
        for (x, y) in [
            (a.latency_ms, b.latency_ms),
            (a.latency_received_ms, b.latency_received_ms),
            (a.transit_ms, b.transit_ms),
            (a.encode_ms, b.encode_ms),
            (a.decode_ms, b.decode_ms),
        ] {
            assert_eq!(x.n, y.n);
            for (u, v) in [(x.mean, y.mean), (x.min, y.min), (x.max, y.max)] {
                assert!((u - v).abs() < 1e-9, "{u} vs {v}");
            }
            for (u, v) in [(x.p50, y.p50), (x.p95, y.p95), (x.p99, y.p99)] {
                assert!((u - v).abs() <= u.abs() * 1e-3, "{u} vs {v}");
            }
        }
        assert_eq!(a.bitrate_trace_kbps.len(), b.bitrate_trace_kbps.len());
        let (sa, sb): (f64, f64) = (a.bitrate_trace_kbps.iter().sum(), b.bitrate_trace_kbps.iter().sum());
        assert!((sa - sb).abs() < 1e-6 * sa, "{sa} vs {sb}");
        assert!(b.per_frame.is_empty());
    }

    #[test]
    fn shard_tracking_sees_parity_use() {
        let frame: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let packets = nuvira_transport::fec::encode(3, &frame, 1200, 0.25).unwrap();
        let mut seen = ShardSeen::default();
        for p in &packets {
            seen.push(p);
        }
        assert!(!seen.completed(3));
        let mut seen = ShardSeen::default();
        for p in packets.iter().skip(1) {
            seen.push(p);
        }
        assert!(seen.completed(3));
    }
}
