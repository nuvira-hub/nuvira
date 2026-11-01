//! NuviraHub viewer — the window of the remote session.
//!
//! Connects to an agent by ID through nuvira-server (or directly in a LAN),
//! shows its screen with wgpu (or the CPU renderer) and sends mouse and keyboard.
//! The window title shows frames per second, capture→display latency, RTT and
//! bandwidth; the toolbar is at the top (`toolbar.rs`).

mod cpu;
mod filemanager;
mod net;
mod render;
mod report;
mod toolbar;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use nuvira_i18n::{t, tf};
use nuvira_proto::v1::{
    Button, InputEvent, Key, MouseButton, MouseMove, MouseWheel, SecureAttention, input_event::Event,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::net::{FrameSlot, NetEvent};
use crate::render::Renderer;
use crate::report::Report;

const COPYRIGHT: &str =
    "NuviraHub © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/nuvira-hub/nuvira";

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum TransportArg {
    Auto,
    Udp,
    Websocket,
}

impl From<TransportArg> for nuvira_transport::signal::TransportMode {
    fn from(arg: TransportArg) -> Self {
        match arg {
            TransportArg::Auto => Self::Auto,
            TransportArg::Udp => Self::Udp,
            TransportArg::Websocket => Self::WebSocket,
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about = "NuviraHub viewer: controls a PC running NuviraHub", after_help = COPYRIGHT)]
struct Args {
    /// Agent ID (with --server) or LAN address, e.g. 192.168.1.20[:21500].
    agent: String,

    /// nuvira-server used to connect by ID (host[:port]).
    #[arg(long, value_name = "HOST[:PORT]")]
    server: Option<String>,

    /// Server certificate fingerprint. Without it, the one seen at the first connection is remembered.
    #[arg(long, value_name = "FINGERPRINT")]
    server_key: Option<String>,

    /// Transport towards the server: auto (UDP, then WebSocket if UDP is blocked), udp, websocket.
    #[arg(long, value_enum, default_value_t = TransportArg::Auto)]
    transport: TransportArg,

    /// WebSocket URL of the server for the fallback (default: ws://<server>:<port>/nuvira),
    /// e.g. wss://nuvira.example.com/nuvira behind a reverse proxy.
    #[arg(long, value_name = "URL")]
    server_ws: Option<String>,

    /// Session password shown by the agent (also from the NUVIRA_PASSWORD variable,
    /// which, unlike the command line, other programs cannot see).
    #[arg(long, env = "NUVIRA_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    /// File with the device key (default: viewer.key in the data folder; different
    /// from the agent's, which the server would otherwise disconnect).
    #[arg(long, value_name = "FILE")]
    identity: Option<PathBuf>,

    /// Certificate fingerprint shown by the agent (recommended).
    #[arg(long)]
    fingerprint: Option<String>,

    /// View only: does not send mouse and keyboard.
    #[arg(long)]
    view_only: bool,

    /// Starts at real size (1:1) instead of fitting the window (also from the toolbar).
    #[arg(long)]
    actual_size: bool,

    /// Closes the session after this many seconds (automated tests).
    #[arg(long, value_name = "SECONDS")]
    duration: Option<u64>,

    /// Writes the session results as JSON on exit (automated tests).
    #[arg(long, value_name = "FILE")]
    stats_file: Option<PathBuf>,

    /// Saves the last frame shown as PNG on exit (colour checks in tests).
    #[arg(long, value_name = "FILE")]
    snapshot: Option<PathBuf>,

    /// Sends this file to the agent once connected (automated tests; repeatable).
    #[arg(long, value_name = "FILE")]
    send_file: Vec<PathBuf>,

    /// File manager, automated tests: downloads this remote file into --download-dir
    /// once connected (repeatable), then exits when all transfers are over.
    #[arg(long, value_name = "REMOTE_FILE")]
    download: Vec<String>,

    #[arg(long, value_name = "DIR", default_value = ".")]
    download_dir: PathBuf,

    /// File manager, automated tests: sends this file into --upload-dir on the remote PC.
    #[arg(long, value_name = "FILE")]
    upload: Vec<PathBuf>,

    #[arg(long, value_name = "REMOTE_DIR", default_value = "")]
    upload_dir: String,

    /// With --download/--upload: continue what an interrupted transfer left (the `.part`).
    #[arg(long)]
    resume: bool,
}

/// Colours in the logs only if the terminal understands them (the old Windows
/// prompt would show them as `←[2m`).
fn ansi_colors() -> bool {
    #[cfg(windows)]
    return std::io::IsTerminal::is_terminal(&std::io::stderr())
        && nu_ansi_term::enable_ansi_support().is_ok();
    #[cfg(not(windows))]
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// The viewer's command line: `args` as from `std::env::args_os()` (program name first).
pub fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_ansi(ansi_colors())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
    let args = Args::parse_from(args);
    // Language passed by the app (NUVIRA_LANG), else the system one.
    nuvira_i18n::init(None);
    let target = match &args.server {
        Some(server_name) => net::Target::Server {
            server: nuvira_transport::resolve_addr(server_name, nuvira_transport::signal::DEFAULT_PORT)?,
            server_name: server_name.clone(),
            server_key: args.server_key.as_deref().map(nuvira_transport::parse_fingerprint).transpose()?,
            transport: args.transport.into(),
            server_ws: args.server_ws.clone(),
            device_id: args.agent.chars().filter(|c| c.is_ascii_digit()).collect(),
            identity: args
                .identity
                .clone()
                .unwrap_or_else(|| nuvira_transport::identity::data_dir().join("viewer.key")),
        },
        None => net::Target::Direct {
            addr: nuvira_transport::resolve_addr(&args.agent, nuvira_transport::DEFAULT_PORT)?,
            fingerprint: args.fingerprint.as_deref().map(nuvira_transport::parse_fingerprint).transpose()?,
        },
    };
    println!("{COPYRIGHT}");

    let started = Instant::now();
    let event_loop = EventLoop::<NetEvent>::with_user_event().build()?;
    if let Some(secs) = args.duration {
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            let _ = proxy.send_event(NetEvent::TestFinished);
        });
    }
    let mut app = App {
        keep_last_frame: args.snapshot.is_some(),
        last_frame: None,
        slot: Arc::new(FrameSlot::default()),
        started,
        report: Report::default(),
        connect: Some((target, args.password.clone())),
        proxy: event_loop.create_proxy(),
        input: None,
        net_thread: None,
        control: None,
        sent_view_size: None,
        files: None,
        pending_files: args.send_file.clone(),
        fm: Default::default(),
        cli_transfers: (
            args.download.clone(),
            args.download_dir.clone(),
            args.upload.clone(),
            args.upload_dir.clone(),
            args.resume,
        ),
        cli_started: false,
        feedback_window: (Instant::now(), Vec::new(), Vec::new()),
        view_only: args.view_only,
        view: render::View { actual: args.actual_size, ..Default::default() },
        window: None,
        renderer: None,
        toolbar: None,
        chat: None,
        quality: nuvira_proto::v1::Quality::Balanced,
        agent: args.agent.clone(),
        stats: Stats::new(),
        clock_offset_us: None,
        best_rtt_us: u64::MAX,
        rtt_us: 0,
        pressed_keys: HashSet::new(),
        exit_reason: None,
        closed_message: None,
    };
    event_loop.run_app(&mut app)?;
    // Closing the input channel ends the session: wait (briefly) for the close to
    // reach the agent.
    app.input = None;
    if let Some(thread) = app.net_thread.take() {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if let Some(reason) = &app.exit_reason {
        println!("{reason}");
    }
    // For the NuviraHub app, which shows it to the user (already translated).
    if let Some(message) = &app.closed_message {
        println!("@closed\t{message}");
    }
    if let Some(path) = &args.stats_file {
        app.report.exit_reason = app.exit_reason.clone();
        app.report.dropped = app.slot.dropped();
        std::fs::write(path, app.report.to_json(app.started))
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if let (Some(path), Some(frame)) = (&args.snapshot, &app.last_frame) {
        cpu::write_png(frame, path).with_context(|| format!("writing {}", path.display()))?;
    }
    // Orderly shutdown: toolbar and wgpu surface, then the window, then the rest.
    // Left to the implicit order, the viewer sometimes crashed on exit.
    drop(app.chat.take());
    drop(app.toolbar.take());
    drop(app.renderer.take());
    drop(app.window.take());
    drop(app);
    Ok(())
}

struct App {
    /// Keeps a copy of the last frame for `--snapshot`.
    keep_last_frame: bool,
    last_frame: Option<nuvira_codec::YuvFrame>,
    slot: Arc<FrameSlot>,
    started: Instant,
    report: Report,
    /// Agent to connect to as soon as the window is ready.
    connect: Option<(net::Target, Option<String>)>,
    proxy: EventLoopProxy<NetEvent>,
    input: Option<UnboundedSender<InputEvent>>,
    net_thread: Option<std::thread::JoinHandle<()>>,
    /// Control messages towards the agent (feedback, screen, quality, chat).
    control: Option<UnboundedSender<nuvira_proto::v1::control_message::Msg>>,
    /// Drawing area last told to the agent (it encodes the image at that size).
    sent_view_size: Option<(u32, u32)>,
    /// Files to send to the agent.
    files: Option<UnboundedSender<net::Outgoing>>,
    /// File manager window and transfers.
    fm: filemanager::FileManager,
    /// `--download`/`--upload`: transfers to start once connected, and resume.
    cli_transfers: (Vec<String>, PathBuf, Vec<PathBuf>, String, bool),
    cli_started: bool,
    /// `--send-file`: to send as soon as connected.
    pending_files: Vec<PathBuf>,
    /// Feedback for the adaptive bitrate: window start, total latencies and network
    /// latencies (ms) of the frames shown since then.
    feedback_window: (Instant, Vec<u32>, Vec<u32>),
    view_only: bool,
    /// Fit or real size, and the pan at real size.
    view: render::View,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    toolbar: Option<toolbar::Toolbar>,
    chat: Option<toolbar::ChatPanel>,
    quality: nuvira_proto::v1::Quality,
    agent: String,
    stats: Stats,
    /// Agent clock minus ours (from the measurement with the lowest RTT).
    clock_offset_us: Option<i64>,
    best_rtt_us: u64,
    rtt_us: u64,
    pressed_keys: HashSet<String>,
    exit_reason: Option<String>,
    /// Why the session ended, for the user (translated); printed as `@closed` for the app.
    closed_message: Option<String>,
}

impl App {
    fn send(&self, event: Event) {
        if let (false, Some(input)) = (self.view_only, &self.input) {
            let _ = input.send(InputEvent { event: Some(event) });
        }
    }

    /// Every 250 ms sends the agent the median and minimum latency of the frames shown.
    /// Called for every frame and for every clock measurement (once a second), so that
    /// on a still screen the last frames do not wait for the next one to be reported.
    fn send_feedback(&mut self) {
        let (since, latencies, transits) = &mut self.feedback_window;
        let window = since.elapsed();
        if window < Duration::from_millis(250) || latencies.is_empty() {
            return;
        }
        latencies.sort_unstable();
        transits.sort_unstable();
        let feedback = nuvira_proto::v1::Feedback {
            latency_ms_p50: latencies[latencies.len() / 2],
            latency_ms_min: latencies[0],
            frames: latencies.len() as u32,
            window_ms: window.as_millis() as u32,
            transit_ms_p50: transits.get(transits.len() / 2).copied(),
            transit_ms_min: transits.first().copied(),
            // Losses of the video as datagrams: added by the network side.
            ..Default::default()
        };
        latencies.clear();
        transits.clear();
        *since = Instant::now();
        self.send_control(nuvira_proto::v1::control_message::Msg::Feedback(feedback));
    }

    fn send_control(&self, msg: nuvira_proto::v1::control_message::Msg) {
        if let Some(tx) = &self.control {
            let _ = tx.send(msg);
        }
    }

    /// Tells the agent the pixels of the window, so that it encodes no more than are
    /// drawn; real size wants the screen's own resolution (0x0).
    fn send_view_size(&mut self) {
        let Some(window) = &self.window else { return };
        let size = if self.view.actual {
            (0, 0)
        } else {
            let s = window.inner_size();
            (s.width, s.height)
        };
        if size.0 > 0 && size.1 == 0 || self.sent_view_size == Some(size) {
            return;
        }
        self.sent_view_size = Some(size);
        self.send_control(nuvira_proto::v1::control_message::Msg::ViewSize(nuvira_proto::v1::ViewSize {
            width: size.0,
            height: size.1,
        }));
    }

    fn ctrl_alt_held(&self) -> bool {
        let held = |a: &str, b: &str| self.pressed_keys.contains(a) || self.pressed_keys.contains(b);
        held("ControlLeft", "ControlRight") && held("AltLeft", "AltRight")
    }

    fn release_keys(&mut self) {
        for code in std::mem::take(&mut self.pressed_keys) {
            self.send(Event::Key(Key { code, down: false }));
        }
    }

    fn update_title(&self) {
        let Some(window) = &self.window else { return };
        let s = &self.stats;
        let latency = if self.clock_offset_us.is_some() && s.frames > 0 {
            format!("{:.0} ms", s.latency_us as f64 / s.frames as f64 / 1000.0)
        } else {
            "…".into()
        };
        let title = tf(
            "viewer.title",
            &[
                ("agent", &self.agent),
                ("fps", &format!("{:.0}", s.fps())),
                ("latency", &latency),
                ("rtt", &format!("{:.1}", self.rtt_us as f64 / 1000.0)),
                ("mbps", &format!("{:.1}", s.mbps())),
                ("enc", &format!("{:.1}", s.encode_us as f64 / s.frames.max(1) as f64 / 1000.0)),
                ("dec", &format!("{:.1}", s.decode_us as f64 / s.frames.max(1) as f64 / 1000.0)),
            ],
        );
        debug!("{title}");
        window.set_title(&title);
        if let Some(toolbar) = &self.toolbar {
            let stats = format!("{:.0} fps · {latency}", s.fps());
            toolbar.update(&format!("{{stats:{}}}", toolbar::js_string(&stats)));
        }
    }

    fn handle_toolbar(&mut self, event_loop: &ActiveEventLoop, action: toolbar::Action) {
        use nuvira_proto::v1::control_message::Msg;
        use toolbar::Action;
        match action {
            Action::SecureAttention => self.send(Event::SecureAttention(SecureAttention {})),
            Action::ViewOnly => {
                if !self.view_only {
                    // Before stopping sending, whatever is pressed is released.
                    self.release_keys();
                }
                self.view_only = !self.view_only;
                info!("view only: {}", self.view_only);
            }
            Action::ActualSize => {
                self.view.actual = !self.view.actual;
                info!("real size: {}", self.view.actual);
                self.send_view_size();
                if let Some(r) = self.renderer.as_mut() {
                    r.set_view(self.view);
                    r.render();
                }
                self.update_toolbar_state();
            }
            Action::Fullscreen => {
                if let Some(window) = &self.window {
                    let full = window.fullscreen().is_none();
                    window.set_fullscreen(full.then_some(winit::window::Fullscreen::Borderless(None)));
                }
            }
            Action::Display(index) => {
                self.send_control(Msg::SelectDisplay(nuvira_proto::v1::SelectDisplay { index }));
            }
            Action::Quality(quality) => {
                self.quality = quality;
                info!("quality {quality:?}");
                self.send_control(Msg::SetQuality(nuvira_proto::v1::SetQuality { quality: quality as i32 }));
            }
            Action::Chat => {
                let open = !self.chat.as_ref().is_some_and(|c| c.visible());
                self.show_chat(open);
                // The open panel keeps the keyboard for typing.
                if open {
                    return;
                }
            }
            Action::ChatSend(text) => {
                if let Some(chat) = &self.chat {
                    chat.add(true, &text);
                }
                self.send_control(Msg::Chat(nuvira_proto::v1::Chat { text }));
                return;
            }
            Action::FileManager => {
                if let Err(e) = self.fm.open(event_loop, self.proxy.clone(), self.window.as_deref()) {
                    error!("file manager not available: {e:#}");
                }
                // The keyboard stays with the file manager: giving it back to the session
                // brought the session window in front of the new one.
                return;
            }
            Action::SendFile => {
                // The file picker blocks its own thread, not the session.
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let picked = rfd::FileDialog::new().set_title(t("viewer.file_dialog")).pick_files();
                    if let Some(paths) = picked {
                        let _ = proxy.send_event(NetEvent::FilesPicked(paths));
                    }
                });
            }
            Action::Collapse | Action::Expand => {
                if let Some(toolbar) = self.toolbar.as_mut() {
                    toolbar.set_collapsed(action == Action::Collapse);
                }
            }
            Action::Close => {
                self.release_keys();
                self.exit_reason = Some("session closed by the user".into());
                event_loop.exit();
                return;
            }
        }
        self.update_toolbar_state();
        if let Some(toolbar) = &self.toolbar {
            toolbar.focus_session();
        }
    }

    fn send_file(&self, path: PathBuf) {
        if let Some(files) = &self.files {
            info!("file to send: {}", path.display());
            let _ = files.send(net::Outgoing::File(path));
        }
    }

    fn notice(&self, text: &str, sticky: bool) {
        if let Some(toolbar) = &self.toolbar {
            toolbar.update(&format!("{{notice:{},sticky:{sticky}}}", toolbar::js_string(text)));
        }
    }

    fn show_chat(&mut self, open: bool) {
        if let Some(chat) = self.chat.as_mut() {
            chat.set_visible(open);
        }
        if let Some(toolbar) = &self.toolbar {
            toolbar.update(&format!("{{chat:{open}}}"));
        }
    }

    fn update_toolbar_state(&self) {
        let (Some(toolbar), Some(window)) = (&self.toolbar, &self.window) else { return };
        toolbar.update(&format!(
            "{{viewOnly:{},actualSize:{},fullscreen:{},quality:{}}}",
            self.view_only,
            self.view.actual,
            window.fullscreen().is_some(),
            toolbar::js_string(toolbar::quality_name(self.quality))
        ));
    }
}

impl ApplicationHandler<NetEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(tf("viewer.title_connecting", &[("agent", &self.agent)]))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!("window not created: {e}");
                event_loop.exit();
                return;
            }
        };
        match Renderer::new(window.clone()) {
            Ok((mut r, info)) => {
                info!("rendering with {info}");
                self.report.renderer = Some(info);
                r.set_view(self.view);
                self.renderer = Some(r);
            }
            Err(e) => {
                error!("rendering not available: {e:#}");
                self.exit_reason = Some(format!("rendering not available: {e:#}"));
                event_loop.exit();
                return;
            }
        }
        match toolbar::Toolbar::new(window.clone(), self.proxy.clone()) {
            Ok(toolbar) => {
                toolbar.update(&format!("{{name:{}}}", toolbar::js_string(&self.agent)));
                self.toolbar = Some(toolbar);
            }
            // The session stays usable (Ctrl+Alt+End, closing the window).
            Err(e) => error!("toolbar not available: {e:#}"),
        }
        match toolbar::ChatPanel::new(window.clone(), self.proxy.clone()) {
            Ok(chat) => self.chat = Some(chat),
            Err(e) => error!("chat not available: {e:#}"),
        }
        self.window = Some(window);
        self.update_toolbar_state();
        // The connection starts only once window and rendering are ready: when the
        // network came first, the video channel was sometimes never accepted.
        if let Some((target, password)) = self.connect.take() {
            let session = net::start(target, password, self.proxy.clone(), self.slot.clone());
            self.input = Some(session.input);
            self.control = Some(session.control.clone());
            self.fm.connect(filemanager::Link {
                control: session.control,
                files: session.files.clone(),
                downloads: session.downloads.clone(),
            });
            self.files = Some(session.files);
            self.net_thread = Some(session.thread);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: NetEvent) {
        match event {
            NetEvent::ConfirmationPending => {
                // For the NuviraHub app, which shows it too.
                println!("@pending");
                if let Some(window) = &self.window {
                    window.set_title(&tf("viewer.title_pending", &[("agent", &self.agent)]));
                }
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{name:{}}}", toolbar::js_string(&t("viewer.pending"))));
                }
            }
            NetEvent::Connected { agent, direct, width, height, file_manager } => {
                info!("remote screen {width}x{height}");
                self.sent_view_size = None;
                self.send_view_size();
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{fileManager:{file_manager}}}"));
                }
                let (downloads, download_dir, uploads, upload_dir, resume) =
                    std::mem::take(&mut self.cli_transfers);
                self.cli_started = !downloads.is_empty() || !uploads.is_empty();
                for remote in downloads {
                    // The size comes with the data.
                    self.fm.download(remote, 0, download_dir.clone(), resume);
                }
                for local in uploads {
                    self.fm.upload(local, upload_dir.clone(), resume);
                }
                // For the NuviraHub app, which reads the output: the session started (address book).
                println!("@connected\t{agent}");
                for path in std::mem::take(&mut self.pending_files) {
                    self.send_file(path);
                }
                if let Some(toolbar) = &self.toolbar {
                    let name = if agent.is_empty() { &self.agent } else { &agent };
                    let path = if direct { "direct" } else { "relay" };
                    toolbar.update(&format!("{{name:{},path:\"{path}\"}}", toolbar::js_string(name)));
                }
                self.report.agent = Some(agent.clone());
                self.report.resolution = Some((width, height));
                if !agent.is_empty() {
                    self.agent = agent;
                }
                self.update_title();
            }
            NetEvent::FrameReady => {
                let Some(frame) = self.slot.take() else { return };
                if self.keep_last_frame {
                    let last = self.last_frame.get_or_insert_with(Default::default);
                    last.width = frame.yuv.width;
                    last.height = frame.yuv.height;
                    last.y.clone_from(&frame.yuv.y);
                    last.u.clone_from(&frame.yuv.u);
                    last.v.clone_from(&frame.yuv.v);
                }
                let Some(renderer) = self.renderer.as_mut() else { return };
                let t = Instant::now();
                renderer.upload(&frame.yuv);
                renderer.render();
                let render_us = t.elapsed().as_micros() as u64;
                tracing::trace!("frame shown in {render_us} µs");
                self.report.render_us += render_us;
                if let Some(offset) = self.clock_offset_us {
                    // Capture time translated to our clock.
                    let captured_here = frame.capture_us as i64 - offset;
                    let latency = nuvira_proto::now_us() as i64 - captured_here;
                    self.stats.latency_us += latency.max(0) as u64;
                    self.report.latencies_us.push(latency.clamp(0, u32::MAX as i64) as u32);
                    self.feedback_window.1.push((latency.max(0) / 1000) as u32);
                    // From the end of encoding to arrival: the network alone.
                    let transit = frame.received_us as i64 - (frame.encoded_us as i64 - offset);
                    self.feedback_window.2.push((transit.max(0) / 1000) as u32);
                }
                self.send_feedback();
                let r = &mut self.report;
                r.first_frame_ms.get_or_insert(self.started.elapsed().as_millis() as u64);
                if let Some(gap) = r.shown(Instant::now()) {
                    tracing::debug!(
                        gap_ms = gap.as_millis() as u64,
                        at_ms = self.started.elapsed().as_millis() as u64,
                        "freeze"
                    );
                }
                r.frames += 1;
                r.bytes += frame.bytes as u64;
                r.encode_us += frame.encode_us as u64;
                r.decode_us += frame.decode_us as u64;
                self.stats.frames += 1;
                self.stats.bytes += frame.bytes;
                self.stats.encode_us += frame.encode_us as u64;
                self.stats.decode_us += frame.decode_us as u64;
                if self.stats.since.elapsed().as_secs_f64() >= 1.0 {
                    self.update_title();
                    self.stats = Stats::new();
                }
            }
            NetEvent::Clock { rtt_us, offset_us } => {
                self.send_feedback();
                self.rtt_us = rtt_us;
                self.report.rtt_us.push(rtt_us.min(u32::MAX as u64) as u32);
                // The measurement with the lowest RTT is the least disturbed by network queues.
                if rtt_us <= self.best_rtt_us {
                    self.best_rtt_us = rtt_us;
                    self.clock_offset_us = Some(offset_us);
                }
            }
            NetEvent::TestFinished => {
                info!("test duration over");
                self.report.completed = true;
                self.exit_reason = Some("test duration over".into());
                self.release_keys();
                event_loop.exit();
            }
            NetEvent::Displays { displays, current, all_available } => {
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&toolbar::displays_state(&displays, current, all_available));
                }
            }
            NetEvent::Fs(answer) => self.fm.response(answer),
            NetEvent::Transfer { id, done, total } => self.fm.progress(id, done, total),
            NetEvent::TransferDone { id, ok, message } => {
                self.fm.finished(id, ok, message);
                // --download/--upload: done when nothing is running any more.
                if self.cli_started && self.fm.idle() {
                    info!("transfers over");
                    self.exit_reason = Some("transfers over".into());
                    event_loop.exit();
                }
            }
            NetEvent::FileManager(body) => self.fm.message(&body),
            NetEvent::FilesPicked(paths) => {
                for path in paths {
                    self.send_file(path);
                }
            }
            NetEvent::FileProgress { name, sent, total } => {
                let percent = (sent * 100).checked_div(total).unwrap_or(100);
                let text = if sent < total {
                    tf("viewer.file_sending", &[("name", &name), ("percent", &percent.to_string())])
                } else {
                    tf("viewer.file_sent_waiting", &[("name", &name)])
                };
                self.notice(&text, true);
            }
            NetEvent::FileDone { name, ok, message } => {
                let text = if ok {
                    tf("viewer.file_arrived", &[("name", &name), ("folder", &nuvira_i18n::message(&message))])
                } else {
                    tf("viewer.file_failed", &[("name", &name), ("reason", &nuvira_i18n::message(&message))])
                };
                info!("{text}");
                self.notice(&text, false);
                if let Some(chat) = &self.chat {
                    chat.notice(&text);
                }
            }
            NetEvent::Chat(text) => {
                info!("chat message received");
                if !self.chat.as_ref().is_some_and(|c| c.visible()) {
                    self.show_chat(true);
                }
                if let Some(chat) = &self.chat {
                    chat.add(false, &text);
                }
            }
            NetEvent::Toolbar(action) => self.handle_toolbar(event_loop, action),
            NetEvent::Disconnected(reason) => {
                info!("disconnected: {reason}");
                self.exit_reason = Some(format!("disconnected: {reason}"));
                self.closed_message = Some(tf("viewer.session_closed", &[("reason", &reason)]));
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.fm.owns(id) {
            self.fm.window_event(&event);
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.release_keys();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                    r.render();
                }
                if let Some(toolbar) = &self.toolbar {
                    toolbar.layout();
                }
                if let Some(chat) = &self.chat {
                    chat.layout();
                }
                self.update_toolbar_state();
                self.send_view_size();
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    r.render();
                }
            }
            WindowEvent::Focused(false) => self.release_keys(),
            WindowEvent::DroppedFile(path) => self.send_file(path),
            WindowEvent::CursorMoved { position, .. } => {
                // Real size: the image follows the mouse, so that moving it from one edge
                // of the window to the other crosses the whole remote screen.
                if self.view.actual
                    && let (Some(r), Some(window)) = (self.renderer.as_mut(), &self.window)
                {
                    let size = window.inner_size();
                    let pan = (
                        (position.x / size.width.max(1) as f64) as f32,
                        (position.y / size.height.max(1) as f64) as f32,
                    );
                    let before = r.viewport();
                    self.view.pan = pan;
                    r.set_view(self.view);
                    let after = r.viewport();
                    if (before.x, before.y) != (after.x, after.y) {
                        window.request_redraw();
                    }
                }
                let Some(r) = &self.renderer else { return };
                if let Some((x, y)) = r.viewport().normalize(position.x, position.y) {
                    self.send(Event::MouseMove(MouseMove { x, y }));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Click on the image while typing in the chat: the keyboard goes back to the session.
                if state == ElementState::Pressed
                    && let Some(chat) = &self.chat
                {
                    chat.focus_session();
                }
                let button = match button {
                    winit::event::MouseButton::Left => Button::Left,
                    winit::event::MouseButton::Right => Button::Right,
                    winit::event::MouseButton::Middle => Button::Middle,
                    winit::event::MouseButton::Back => Button::Back,
                    winit::event::MouseButton::Forward => Button::Forward,
                    winit::event::MouseButton::Other(_) => return,
                };
                self.send(Event::MouseButton(MouseButton {
                    button: button as i32,
                    down: state == ElementState::Pressed,
                }));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Windows units: 120 per notch. Touchpads give pixels: ~40 px per notch.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 * 3.0, p.y as f32 * 3.0),
                };
                self.send(Event::MouseWheel(MouseWheel { dx: dx as i32, dy: dy as i32 }));
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let code = w3c_code(code);
                let down = event.state == ElementState::Pressed;
                // Ctrl+Alt+End (as in Remote Desktop) = Ctrl+Alt+Del on the controlled
                // device: the real Ctrl+Alt+Del is caught by the local system.
                if code == "End" && self.ctrl_alt_held() {
                    if down && !event.repeat {
                        info!("Ctrl+Alt+End: sending Ctrl+Alt+Del");
                        self.send(Event::SecureAttention(SecureAttention {}));
                    }
                    return;
                }
                if down {
                    self.pressed_keys.insert(code.clone());
                } else {
                    self.pressed_keys.remove(&code);
                }
                self.send(Event::Key(Key { code, down }));
            }
            _ => {}
        }
    }
}

/// W3C name (`KeyboardEvent.code`) of a winit key. The names are the same except
/// for the Windows/Cmd key, which winit calls "Super".
fn w3c_code(code: winit::keyboard::KeyCode) -> String {
    match format!("{code:?}").as_str() {
        "SuperLeft" => "MetaLeft".into(),
        "SuperRight" => "MetaRight".into(),
        other => other.into(),
    }
}

struct Stats {
    since: Instant,
    frames: u32,
    bytes: usize,
    latency_us: u64,
    encode_us: u64,
    decode_us: u64,
}

impl Stats {
    fn new() -> Self {
        Self { since: Instant::now(), frames: 0, bytes: 0, latency_us: 0, encode_us: 0, decode_us: 0 }
    }

    fn fps(&self) -> f64 {
        self.frames as f64 / self.since.elapsed().as_secs_f64().max(0.001)
    }

    fn mbps(&self) -> f64 {
        self.bytes as f64 * 8.0 / self.since.elapsed().as_secs_f64().max(0.001) / 1e6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn super_becomes_meta() {
        assert_eq!(w3c_code(winit::keyboard::KeyCode::SuperLeft), "MetaLeft");
        assert_eq!(w3c_code(winit::keyboard::KeyCode::KeyA), "KeyA");
    }
}
