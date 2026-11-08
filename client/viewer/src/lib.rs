//! NuviraHub viewer — the window of the remote session.
//!
//! Connects to an agent by ID through nuvira-server (or directly in a LAN),
//! shows its screen with wgpu (or the CPU renderer) and sends mouse and keyboard.
//! The window title shows frames per second, capture→display latency, RTT and
//! bandwidth; the toolbar is at the top (`toolbar.rs`).

mod cpu;
mod filemanager;
mod net;
mod mp4;
mod printing;
mod recorder;
mod render;
mod report;
mod terminal;
mod tunnels;
mod toolbar;
mod upload;

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
use tracing::{debug, error, info, warn};
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

/// In place of a password, this prefix carries the ticket of a support request
/// (support queue): the viewer sends it as `Auth.support_ticket`.
pub const SUPPORT_TICKET: &str = "\u{1}support-ticket:";

/// After a remote restart: the first attempt to reconnect (the computer takes a while
/// to go down), the pause between the following ones and how long to keep trying.
const RESTART_FIRST_TRY: Duration = Duration::from_secs(20);
const RESTART_RETRY: Duration = Duration::from_secs(5);
const RESTART_WAIT: Duration = Duration::from_secs(10 * 60);

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

    /// Ticket of a support request taken from the queue (support queue), instead of
    /// the password: the person at the PC is asked to accept.
    #[arg(long, conflicts_with = "password")]
    support_ticket: Option<String>,
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

    /// Turns on privacy mode as soon as connected, when the agent has it (also from the toolbar).
    #[arg(long)]
    privacy: bool,

    /// Records the session from the start, when the person at the remote computer
    /// allows it (also from the toolbar). The video is saved as MP4 files.
    #[arg(long)]
    record: bool,

    /// Folder of the recordings (default: Videos/NuviraHub, Movies/NuviraHub on macOS).
    #[arg(long, value_name = "DIR")]
    record_dir: Option<PathBuf>,

    /// Tests: restarts the remote computer as soon as connected, without asking, then
    /// waits for it and connects again (the toolbar button asks first).
    #[arg(long, hide = true)]
    restart: bool,

    /// Terminal only: a shell of the remote computer, without its desktop.
    #[arg(long)]
    terminal: bool,

    /// Automated tests: text typed in the first terminal as soon as its shell starts
    /// ("\n" becomes Enter).
    #[arg(long, value_name = "TEXT", hide = true)]
    terminal_type: Option<String>,

    /// Automated tests: moves the remote mouse every second, between a quarter and three
    /// quarters of the width of the screen, as the mouse over the window would.
    #[arg(long, hide = true)]
    input_test: bool,
    /// Automated tests: types this text (letters, digits, space, "\n" for Enter, "\b"
    /// for Backspace) on the remote computer 3 s after connecting, key by key: for
    /// passwords on a lock or sign-in screen.
    #[arg(long, value_name = "TEXT", hide = true)]
    type_keys: Option<String>,
    /// Automated tests: clicks there (fractions of the remote screen, e.g. 0.5,0.44)
    /// 3 s after connecting, before `--type-keys`.
    #[arg(long, value_name = "X,Y", hide = true)]
    click_at: Option<String>,

    /// TCP tunnel through the session (NuviraHub Pro): LOCAL:HOST:PORT, e.g.
    /// 13389:192.168.1.20:3389 (LOCAL 0 = any free port); HOST must be in the
    /// remote computer's local network. Repeatable; also from the toolbar.
    #[arg(long, value_name = "LOCAL:HOST:PORT")]
    tunnel: Vec<String>,

    /// Automated tests: writes the output of the terminals to this file.
    #[arg(long, value_name = "FILE", hide = true)]
    terminal_log: Option<PathBuf>,

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

    /// Automated tests: documents printed on the remote computer go to this printer
    /// without asking (remote printing).
    #[arg(long, value_name = "PRINTER", hide = true)]
    print_to: Option<String>,

    /// Automated tests: documents printed on the remote computer are saved in this
    /// folder without asking.
    #[arg(long, value_name = "DIR", hide = true, conflicts_with = "print_to")]
    print_save: Option<PathBuf>,
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

/// On Linux the WebViews can be children of the window only under X11 (wry), so
/// the window goes through XWayland on a Wayland desktop; GTK, which draws them,
/// is started here.
fn event_loop() -> Result<EventLoop<NetEvent>> {
    let mut builder = EventLoop::<NetEvent>::with_user_event();
    #[cfg(target_os = "linux")]
    {
        use winit::platform::x11::EventLoopBuilderExtX11;
        // WebKit's GPU path (DMA-BUF) fails without DRI3 (virtual machines, some
        // drivers): the toolbar is small and is drawn just as well by the CPU.
        // SAFETY: at startup, before the network thread exists.
        unsafe {
            std::env::set_var("GDK_BACKEND", "x11");
            if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
                std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            }
        }
        gtk::init().context("GTK not available")?;
        builder.with_x11();
        let event_loop = builder.build()?;
        keep_gtk_x11_errors(&event_loop);
        return Ok(event_loop);
    }
    #[allow(unreachable_code)]
    Ok(builder.build()?)
}

/// Xlib has one error handler per process, and winit's takes the errors of GTK's
/// own X connection too (a WebView's window already destroyed, GLX without a
/// GPU): it would report them at its next call, and some of its calls panic on
/// an error. Errors of other connections are only logged.
#[cfg(target_os = "linux")]
fn keep_gtk_x11_errors(event_loop: &EventLoop<NetEvent>) {
    use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    let Some(ours) = event_loop.display_handle().ok().and_then(|h| match h.as_raw() {
        RawDisplayHandle::Xlib(xlib) => xlib.display.map(|d| d.as_ptr() as usize),
        _ => None,
    }) else {
        return;
    };
    winit::platform::x11::register_xlib_error_hook(Box::new(move |display, _event| {
        let other = display as usize != ours;
        if other {
            debug!("X11 error of GTK's connection, ignored");
        }
        other
    }));
}

/// Icon of the session windows: the N inside a screen, distinct from the app's.
#[cfg(not(target_os = "linux"))]
const ICON: &[u8] = include_bytes!("../icons/viewer-64.png");
#[cfg(any(target_os = "linux", target_os = "macos"))]
const ICON_LARGE: &[u8] = include_bytes!("../icons/viewer-256.png");

/// On macOS the session window belongs to the same bundle as the app, so the
/// Dock would show the app's icon twice: the viewer sets its own at start.
#[cfg(target_os = "macos")]
fn dock_icon() {
    use objc2::AnyThread as _;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(ICON_LARGE);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else { return };
    #[allow(unused_unsafe)]
    unsafe {
        NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
    }
}

fn icon(png_bytes: &[u8]) -> Option<winit::window::Icon> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes)).read_info().ok()?;
    let mut rgba = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut rgba).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    rgba.truncate(info.buffer_size());
    winit::window::Icon::from_rgba(rgba, info.width, info.height).ok()
}

/// Attributes shared by the viewer's windows (session, file manager): their own
/// icon and, on Linux, their own window class, so that the desktop does not take
/// them for the app's windows.
fn window_attributes() -> winit::window::WindowAttributes {
    let attrs = Window::default_attributes();
    #[cfg(windows)]
    let attrs = {
        use winit::platform::windows::WindowAttributesExtWindows;
        attrs.with_window_icon(icon(ICON)).with_taskbar_icon(icon(ICON))
    };
    #[cfg(target_os = "linux")]
    let attrs = {
        use winit::platform::x11::WindowAttributesExtX11;
        attrs.with_window_icon(icon(ICON_LARGE)).with_name("nuvira-viewer", "nuvira-viewer")
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    let attrs = attrs.with_window_icon(icon(ICON));
    attrs
}

/// On Windows the session windows form their own group in the taskbar, with their
/// icon, instead of joining the app's (same program).
#[cfg(windows)]
fn own_taskbar_group() {
    // SAFETY: a static wide string; called once at start-up.
    let result = unsafe {
        windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::w!(
            "NuviraHub.Viewer"
        ))
    };
    if let Err(e) = result {
        debug!("taskbar group not set: {e}");
    }
}

/// Where the session window saves recordings unless told otherwise.
pub fn default_recording_dir() -> PathBuf {
    recorder::default_dir()
}

/// The viewer's command line: `args` as from `std::env::args_os()` (program name first).
/// `upload ...` sends a recording to the server instead (see `upload.rs`).
pub fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<()> {
    let args: Vec<std::ffi::OsString> = args.into_iter().collect();
    if args.get(1).is_some_and(|a| a == "upload") {
        return upload::run(args.into_iter().skip(1));
    }
    let _ = tracing_subscriber::fmt()
        .with_ansi(ansi_colors())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
    let args = Args::parse_from(args);
    // Language passed by the app (NUVIRA_LANG), else the system one.
    nuvira_i18n::init(None);
    // Empty: NuviraHub's public server, as for the agent.
    let server = args.server.as_deref().map(|s| nuvira_transport::signal::server_or_public(s).to_string());
    let target = match &server {
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
    #[cfg(windows)]
    own_taskbar_group();
    let event_loop = event_loop()?;
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
        connect: Some((
            target,
            args.support_ticket.as_ref().map(|t| format!("{SUPPORT_TICKET}{t}")).or_else(|| args.password.clone()),
        )),
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
        privacy: false,
        privacy_at_start: args.privacy,
        recording: Recording {
            switch: Default::default(),
            dir: args.record_dir.clone().unwrap_or_else(recorder::default_dir),
            available: false,
            on: false,
            at_start: args.record,
            asked: false,
            required: false,
        },
        restart_at_start: args.restart,
        terminal_only: args.terminal,
        terminals: Default::default(),
        term_link: None,
        tunnels: Default::default(),
        tunnel_link: None,
        printing: printing::Printing::new(
            match (&args.print_to, &args.print_save) {
                (Some(printer), _) => Some(printing::Test::PrintTo(printer.clone())),
                (None, Some(dir)) => Some(printing::Test::SaveIn(dir.clone())),
                (None, None) => None,
            },
            args.agent.clone(),
        ),
        terminal_type: args.terminal_type.as_ref().map(|t| t.replace("\\n", "\r").into_bytes()),
        input_test: args.input_test.then_some(0),
        type_keys: (args.type_keys.is_some() || args.click_at.is_some()).then(|| {
            (0, args.type_keys.as_deref().unwrap_or("").replace("\\n", "\n").replace("\\b", "\u{8}"))
        }),
        click_at: args.click_at.as_deref().and_then(|t| {
            let (x, y) = t.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        }),
        terminal_log: args.terminal_log.as_ref().and_then(|p| match std::fs::File::create(p) {
            Ok(f) => Some(f),
            Err(e) => {
                error!("{}: {e}", p.display());
                None
            }
        }),
        view: render::View { actual: args.actual_size, ..Default::default() },
        window: None,
        renderer: None,
        toolbar: None,
        chat: None,
        notice: None,
        quality: nuvira_proto::v1::Quality::Balanced,
        agent: args.agent.clone(),
        stats: Stats::new(),
        clock_offset_us: None,
        best_rtt_us: u64::MAX,
        rtt_us: 0,
        pressed_keys: HashSet::new(),
        remote_mac: false,
        altgr: AltGrFix::default(),
        input_blocked: false,
        switched_off: Vec::new(),
        exit_reason: None,
        closed_message: None,
        restarting: None,
        restarted: false,
    };
    for spec in &args.tunnel {
        app.tunnels.add_spec(spec).with_context(|| format!("--tunnel {spec}"))?;
    }
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
    drop(app.notice.take());
    drop(app.chat.take());
    drop(app.toolbar.take());
    drop(app.renderer.take());
    drop(app.window.take());
    drop(app);
    Ok(())
}

/// Recording of the session (see `recorder.rs`).
struct Recording {
    /// Read by the decoding thread, which writes the files.
    switch: Arc<recorder::Switch>,
    dir: PathBuf,
    /// The person at the remote computer allows it.
    available: bool,
    /// On at the agent (as it last said): the person there was told.
    on: bool,
    /// `--record`: to start as soon as connected.
    at_start: bool,
    /// Asked to the agent, no answer yet.
    asked: bool,
    /// The server's rules want every session recorded.
    required: bool,
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
    /// Keyboard and mouse not allowed in this session (limits, or switched off by the
    /// person at the remote computer): nothing is sent, whatever view_only says.
    input_blocked: bool,
    /// What the person at the remote computer switched off, as the agent last said.
    switched_off: Vec<String>,
    /// Privacy mode on at the agent (as it last said).
    privacy: bool,
    /// `--privacy`: to turn on as soon as connected.
    privacy_at_start: bool,
    recording: Recording,
    /// `--restart`: to ask as soon as connected.
    restart_at_start: bool,
    /// `--terminal`: no session window, only terminals.
    terminal_only: bool,
    terminals: terminal::Terminals,
    tunnels: tunnels::Tunnels,
    /// Towards the network for the tunnels.
    tunnel_link: Option<UnboundedSender<net::TunnelCmd>>,
    /// Documents printed on the remote computer, and the print window.
    printing: printing::Printing,
    /// Towards the network for the terminals, once connected to an agent that has them.
    term_link: Option<UnboundedSender<net::TermCmd>>,
    /// `--terminal-type`: still to type in the first terminal.
    terminal_type: Option<Vec<u8>>,
    /// `--input-test`: moves sent so far.
    input_test: Option<u32>,
    /// `--type-keys`: clock measurements seen so far (one a second) and the text.
    type_keys: Option<(u32, String)>,
    /// `--click-at`.
    click_at: Option<(f32, f32)>,
    /// `--terminal-log`.
    terminal_log: Option<std::fs::File>,
    /// Fit or real size, and the pan at real size.
    view: render::View,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    toolbar: Option<toolbar::Toolbar>,
    chat: Option<toolbar::ChatPanel>,
    /// Message in the middle of the window until the first image.
    notice: Option<toolbar::Notice>,
    quality: nuvira_proto::v1::Quality,
    agent: String,
    stats: Stats,
    /// Agent clock minus ours (from the measurement with the lowest RTT).
    clock_offset_us: Option<i64>,
    best_rtt_us: u64,
    rtt_us: u64,
    pressed_keys: HashSet<String>,
    /// The controlled computer is a Mac (see [`w3c_code`]).
    remote_mac: bool,
    /// Windows towards a Mac: when the left Ctrl went down, and whether it was the one
    /// Windows adds to AltGr (see [`AltGrFix`]).
    altgr: AltGrFix,
    exit_reason: Option<String>,
    /// Why the session ended, for the user (translated); printed as `@closed` for the app.
    closed_message: Option<String>,
    /// The remote computer is restarting since then: the viewer waits and reconnects.
    restarting: Option<Instant>,
    /// It restarted, but the viewer cannot connect again by itself (no permanent password).
    restarted: bool,
}

impl App {
    fn send(&self, event: Event) {
        if let (false, false, Some(input)) = (self.view_only, self.input_blocked, &self.input) {
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
        // While recording the agent sends the full screen (the window shows it reduced):
        // readable text in the video, and resizing the window does not cut the file.
        let size = if self.view.actual || self.recording.on || self.recording.asked {
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

    /// Connects to the agent (again, after a restart) with the window already open.
    fn start_session(&mut self) {
        let Some((target, password)) = self.connect.clone() else { return };
        let session =
            net::start(target, password, self.proxy.clone(), self.slot.clone(), self.recording.switch.clone(), false);
        self.input = Some(session.input);
        self.control = Some(session.control.clone());
        self.term_link = Some(session.terminals);
        self.tunnel_link = Some(session.tunnels);
        self.fm.connect(filemanager::Link {
            control: session.control,
            files: session.files.clone(),
            downloads: session.downloads.clone(),
        });
        self.files = Some(session.files);
        self.net_thread = Some(session.thread);
    }

    /// The session ended while the remote computer restarts: the window stays open
    /// with a message, and a new attempt starts after a pause.
    fn wait_for_restart(&mut self, reason: &str, pause: Duration) {
        info!("waiting for the remote computer to restart ({reason}); trying again in {} s", pause.as_secs());
        self.release_keys();
        self.input = None;
        self.control = None;
        self.files = None;
        self.term_link = None;
        self.tunnel_link = None;
        self.net_thread = None;
        let text = tf("viewer.waiting_restart", &[("agent", &self.agent)]);
        match (&self.notice, &self.window) {
            (Some(notice), _) => notice.set_text(&text),
            (None, Some(window)) => match toolbar::Notice::new(window.clone(), self.proxy.clone(), &text) {
                Ok(notice) => self.notice = Some(notice),
                Err(e) => debug!("notice not available: {e:#}"),
            },
            _ => {}
        }
        if let Some(window) = &self.window {
            window.set_title(&tf("viewer.title_restarting", &[("agent", &self.agent)]));
        }
        if let Some(toolbar) = &self.toolbar {
            toolbar.update("{restart:false}");
        }
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(pause);
            let _ = proxy.send_event(NetEvent::Reconnect);
        });
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
            Action::Tunnels => {
                if let Err(e) = self.tunnels.open(event_loop, self.proxy.clone()) {
                    error!("tunnels not available: {e:#}");
                }
                return;
            }
            Action::Terminal => {
                let title = tf("term.title", &[("agent", &self.agent)]);
                if let Err(e) = self.terminals.open(event_loop, self.proxy.clone(), &title, "") {
                    error!("terminal not available: {e:#}");
                }
                // The keyboard stays with the new window.
                return;
            }
            Action::Restart => {
                // The dialog blocks its own thread, not the session.
                let proxy = self.proxy.clone();
                let text = tf("viewer.restart_confirm", &[("agent", &self.agent)]);
                std::thread::spawn(move || {
                    let answer = rfd::MessageDialog::new()
                        .set_level(rfd::MessageLevel::Warning)
                        .set_title(t("viewer.restart_title"))
                        .set_description(text)
                        .set_buttons(rfd::MessageButtons::YesNo)
                        .show();
                    if answer == rfd::MessageDialogResult::Yes {
                        let _ = proxy.send_event(NetEvent::Toolbar(toolbar::Action::RestartConfirmed));
                    }
                });
            }
            Action::RestartConfirmed => {
                info!("restart of the remote computer requested");
                self.send_control(Msg::Restart(nuvira_proto::v1::Restart {}));
            }
            // Required by the server's rules: it cannot be stopped (the button is disabled).
            Action::Record if self.recording.required => {}
            Action::Record => {
                info!("recording requested: {}", !self.recording.on);
                self.recording.asked = !self.recording.on;
                self.send_view_size();
                self.send_control(Msg::SetRecording(nuvira_proto::v1::SetRecording { on: !self.recording.on }));
            }
            Action::Privacy => {
                info!("privacy mode requested: {}", !self.privacy);
                self.send_control(Msg::SetPrivacy(nuvira_proto::v1::SetPrivacy { on: !self.privacy }));
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
            "{{viewOnly:{},inputBlocked:{},privacy:{},recording:{},recordingRequired:{},actualSize:{},fullscreen:{},quality:{}}}",
            self.view_only || self.input_blocked,
            self.input_blocked,
            self.privacy,
            self.recording.on,
            self.recording.required,
            self.view.actual,
            window.fullscreen().is_some(),
            toolbar::js_string(toolbar::quality_name(self.quality))
        ));
    }
}

impl ApplicationHandler<NetEvent> for App {
    /// GTK has its own connection to the X server: its events are handled here, and
    /// the loop wakes up at least every 16 ms so that the WebViews answer even
    /// while no frames arrive.
    #[cfg(target_os = "linux")]
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(16),
        ));
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || !self.terminals.is_empty() {
            return;
        }
        if self.terminal_only {
            #[cfg(target_os = "macos")]
            dock_icon();
            let title = tf("term.title", &[("agent", &self.agent)]);
            let connecting = tf("viewer.connecting", &[("agent", &self.agent)]);
            if let Err(e) = self.terminals.open(event_loop, self.proxy.clone(), &title, &connecting) {
                error!("terminal not available: {e:#}");
                self.exit_reason = Some(format!("terminal not available: {e:#}"));
                event_loop.exit();
                return;
            }
            if let Some((target, password)) = self.connect.take() {
                let session = net::start(
                    target,
                    password,
                    self.proxy.clone(),
                    self.slot.clone(),
                    self.recording.switch.clone(),
                    true,
                );
                self.input = Some(session.input);
                self.control = Some(session.control);
                self.term_link = Some(session.terminals);
                self.tunnel_link = Some(session.tunnels);
                self.net_thread = Some(session.thread);
            }
            return;
        }
        #[cfg(target_os = "macos")]
        dock_icon();
        let attrs = window_attributes()
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
        let connecting = tf("viewer.connecting", &[("agent", &self.agent)]);
        match toolbar::Notice::new(window.clone(), self.proxy.clone(), &connecting) {
            Ok(notice) => self.notice = Some(notice),
            Err(e) => debug!("notice not available: {e:#}"),
        }
        self.window = Some(window);
        self.update_toolbar_state();
        // The connection starts only once window and rendering are ready: when the
        // network came first, the video channel was sometimes never accepted.
        // `connect` is kept: after a remote restart the viewer connects again the same way.
        self.start_session();
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
                if let Some(notice) = &self.notice {
                    notice.set_text(&t("viewer.pending"));
                }
            }
            NetEvent::Connected {
                agent,
                direct,
                width,
                height,
                file_manager,
                secure_attention,
                remote_mac,
                privacy,
                terminal,
                tunnel,
                restart,
                print,
                recording,
                recording_required,
            } => {
                self.remote_mac = remote_mac;
                if self.restarting.take().is_some() {
                    info!("connected again after the restart");
                    self.notice(&t("viewer.restart_back"), false);
                }
                if terminal && let Some(link) = self.term_link.clone() {
                    self.terminals.connect(link);
                }
                match (tunnel, self.tunnel_link.clone()) {
                    (true, Some(link)) => self.tunnels.connect(link),
                    _ if self.tunnels.has_any() => warn!("tunnels not available on this agent"),
                    _ => {}
                }
                if self.terminal_only {
                    // For the NuviraHub app, which reads the output: the session started (address book).
                    println!("@connected\t{agent}");
                    if !agent.is_empty() {
                        self.agent = agent;
                    }
                    self.terminals.set_title(&tf("term.title", &[("agent", &self.agent)]));
                    return;
                }
                info!("remote screen {width}x{height}");
                if print {
                    info!("remote printing: what is printed on the NuviraHub printer there arrives here");
                }
                if !agent.is_empty() {
                    self.printing.agent = agent.clone();
                }
                // After a reconnection the recording goes on in a new file.
                self.recording.available = recording;
                self.recording.required = recording_required;
                if recording_required {
                    info!("the server's rules want this session recorded");
                }
                if std::mem::take(&mut self.recording.at_start) || self.recording.on || recording_required {
                    // Nothing is written before the agent says the person there was told.
                    self.recording.switch.stop();
                    self.recording.on = false;
                    match recording {
                        true => {
                            self.recording.asked = true;
                            self.send_control(nuvira_proto::v1::control_message::Msg::SetRecording(
                                nuvira_proto::v1::SetRecording { on: true },
                            ));
                        }
                        false => self.notice(&t("viewer.recording_not_allowed"), true),
                    }
                }
                self.sent_view_size = None;
                self.send_view_size();
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!(
                        "{{fileManager:{file_manager},secureAttention:{secure_attention},privacyAvailable:{privacy},terminal:{terminal},tunnel:{tunnel},restart:{restart},recordingAvailable:{recording}}}"
                    ));
                }

                if std::mem::take(&mut self.restart_at_start) {
                    match restart {
                        true => self.send_control(nuvira_proto::v1::control_message::Msg::Restart(
                            nuvira_proto::v1::Restart {},
                        )),
                        false => warn!("restart not available on this agent"),
                    }
                }
                if std::mem::take(&mut self.privacy_at_start) {
                    match privacy {
                        true => self.send_control(nuvira_proto::v1::control_message::Msg::SetPrivacy(
                            nuvira_proto::v1::SetPrivacy { on: true },
                        )),
                        false => warn!("privacy mode not available on this agent"),
                    }
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
                // The image is there: no more waiting message.
                self.notice = None;
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
                if let Some(n) = self.input_test.as_mut() {
                    *n += 1;
                    let x = if *n % 2 == 0 { 0.25 } else { 0.75 };
                    self.send(Event::MouseMove(MouseMove { x, y: 0.5 }));
                }
                if let Some((seen, _)) = self.type_keys.as_mut() {
                    *seen += 1;
                    if *seen >= 3
                        && let Some((_, text)) = self.type_keys.take()
                    {
                        if let Some((x, y)) = self.click_at.take() {
                            self.send(Event::MouseMove(MouseMove { x, y }));
                            for down in [true, false] {
                                self.send(Event::MouseButton(MouseButton { button: Button::Left as i32, down }));
                            }
                        }
                        // At a person's pace (40 ms a key): in a burst the input method of
                        // a sign-in screen (ibus) mixed up the order of the characters.
                        if let Some(input) = self.input.clone() {
                            std::thread::spawn(move || {
                                let key = |code: &str, down| {
                                    let event = Event::Key(Key { code: code.into(), down });
                                    let _ = input.send(InputEvent { event: Some(event) });
                                    std::thread::sleep(Duration::from_millis(20));
                                };
                                for (code, shift) in text.chars().filter_map(test_key_code) {
                                    if shift {
                                        key("ShiftLeft", true);
                                    }
                                    key(&code, true);
                                    key(&code, false);
                                    if shift {
                                        key("ShiftLeft", false);
                                    }
                                }
                                info!("test text typed");
                            });
                        }
                    }
                }
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
            NetEvent::TerminalPage { window, body } => self.terminals.message(window, &body),
            NetEvent::TunnelPage(body) => self.tunnels.message(&body),
            NetEvent::PrintJob { name, data } => {
                if let Some(text) = self.printing.arrived(name, data, event_loop, &self.proxy) {
                    self.notice(&text, false);
                }
            }
            NetEvent::PrintPage(body) => {
                if let Some(text) = self.printing.message(&body, &self.proxy) {
                    self.notice(&text, false);
                }
            }
            NetEvent::Printed { name, printer, error } => {
                let text = match error.is_empty() {
                    true => tf("viewer.print_done", &[("name", &name), ("printer", &printer)]),
                    false => tf("viewer.print_failed", &[("name", &name), ("reason", &error)]),
                };
                self.notice(&text, false);
            }
            NetEvent::TunnelListening { id, local_port, error } => self.tunnels.listening(id, local_port, error),
            NetEvent::TunnelConnection { id, error } => self.tunnels.connection_error(id, error),
            NetEvent::TerminalOutput { id, data } => {
                if let Some(log) = self.terminal_log.as_mut() {
                    use std::io::Write;
                    let _ = log.write_all(&data);
                }
                self.terminals.output(id, &data);
                // `--terminal-type`: after the first output (the prompt) of the first terminal.
                if id == 1
                    && let (Some(text), Some(link)) = (self.terminal_type.take(), &self.term_link)
                {
                    let _ = link.send(net::TermCmd::Input { id, data: text });
                }
            }
            NetEvent::TerminalExit { id, code, error } => {
                self.terminals.exited(id, code, &error);
                if let Some(log) = self.terminal_log.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(log, "\n@exit {code} {error}");
                }
            }
            NetEvent::Recording { on, error } => {
                info!(on, error, "recording");
                if on && !self.recording.on {
                    let stem = recorder::file_stem(&self.printing.agent);
                    self.recording.switch.start(self.recording.dir.clone(), stem);
                    let dir = self.recording.dir.display().to_string();
                    self.notice(&tf("viewer.recording_on", &[("dir", &dir)]), false);
                } else if !on && self.recording.on {
                    self.recording.switch.stop();
                }
                if !error.is_empty() {
                    let reason = nuvira_i18n::message(&error);
                    self.notice(&tf("viewer.recording_failed", &[("reason", &reason)]), true);
                }
                self.recording.on = on;
                self.recording.asked = false;
                self.send_view_size();
                self.update_toolbar_state();
            }
            NetEvent::RecordingSaved { files, error } => {
                match error {
                    Some(e) => {
                        error!("recording: {e}");
                        self.notice(&tf("viewer.recording_failed", &[("reason", &e)]), true);
                        // The person at the remote computer is no longer told it is recorded.
                        if self.recording.on {
                            self.recording.on = false;
                            self.send_view_size();
                            self.send_control(nuvira_proto::v1::control_message::Msg::SetRecording(
                                nuvira_proto::v1::SetRecording { on: false },
                            ));
                            self.update_toolbar_state();
                        }
                    }
                    None if !files.is_empty() => {
                        let dir = self.recording.dir.display().to_string();
                        self.notice(&tf("viewer.recording_saved", &[("dir", &dir)]), false);
                    }
                    None => {}
                }
            }
            NetEvent::Permissions { input, files, terminal, tunnel, switched_off } => {
                if !input && !self.input_blocked {
                    // Whatever is pressed is released before sending stops.
                    self.release_keys();
                }
                self.input_blocked = !input;
                let names = |list: Vec<&String>| {
                    list.iter().map(|n| t(&format!("perm.{n}"))).collect::<Vec<_>>().join(", ")
                };
                let off: Vec<&String> = switched_off.iter().filter(|n| !self.switched_off.contains(n)).collect();
                let on: Vec<&String> = self.switched_off.iter().filter(|n| !switched_off.contains(n)).collect();
                // What went off stays on screen; what came back only for a while.
                let sticky = !off.is_empty();
                let mut said = Vec::new();
                if !off.is_empty() {
                    let what = names(off);
                    info!("switched off at the remote computer: {what}");
                    said.push(tf("viewer.switched_off", &[("what", &what)]));
                }
                if !on.is_empty() {
                    let what = names(on);
                    info!("switched on again at the remote computer: {what}");
                    said.push(tf("viewer.switched_on", &[("what", &what)]));
                }
                if !said.is_empty() {
                    self.notice(&said.join(" · "), sticky);
                }
                self.switched_off = switched_off;
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{fileManager:{files},terminal:{terminal},tunnel:{tunnel}}}"));
                }
                self.update_toolbar_state();
            }
            NetEvent::ScreenLocked { locked, input } => {
                info!(locked, input, "remote screen lock state");
                // Kept on screen while locked: the image is only a dark placeholder.
                let text = match (locked, input) {
                    (true, true) => "viewer.screen_locked",
                    (true, false) => "viewer.screen_locked_no_input",
                    (false, _) => "viewer.screen_unlocked",
                };
                self.notice(&t(text), locked);
            }
            NetEvent::Privacy { on, error } => {
                info!(on, error, "privacy mode");
                let changed = on != self.privacy;
                self.privacy = on;
                if !error.is_empty() {
                    let reason = nuvira_i18n::message(&error);
                    self.notice(&tf("viewer.privacy_failed", &[("reason", &reason)]), false);
                } else if changed {
                    self.notice(&t(if on { "viewer.privacy_on" } else { "viewer.privacy_off" }), false);
                }
                self.update_toolbar_state();
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
            NetEvent::Restart { ok, error, reconnect } => {
                info!(ok, error, reconnect, "restart");
                if !ok {
                    let reason = nuvira_i18n::message(&error);
                    self.notice(&tf("viewer.restart_failed", &[("reason", &reason)]), false);
                } else if reconnect {
                    self.restarting = Some(Instant::now());
                    self.notice(&t("viewer.restarting"), true);
                } else {
                    self.restarted = true;
                }
            }
            NetEvent::Reconnect => {
                if self.restarting.is_some() && self.input.is_none() {
                    self.start_session();
                }
            }
            NetEvent::Disconnected { reason, retry }
                if retry && self.restarting.is_some_and(|since| since.elapsed() < RESTART_WAIT) =>
            {
                // The first time the computer has only just started going down.
                let first = self.input.is_some() && self.notice.is_none();
                self.wait_for_restart(&reason, if first { RESTART_FIRST_TRY } else { RESTART_RETRY });
            }
            NetEvent::Disconnected { reason, .. } if self.restarting.is_some() || self.restarted => {
                info!("disconnected after the restart: {reason}");
                self.exit_reason = Some(format!("disconnected after the restart: {reason}"));
                self.closed_message = Some(match self.restarting {
                    Some(_) => tf("viewer.restart_not_back", &[("agent", &self.agent), ("reason", &reason)]),
                    None => tf("viewer.restarted", &[("agent", &self.agent)]),
                });
                event_loop.exit();
            }
            NetEvent::Disconnected { reason, .. } if self.terminal_only && !self.terminals.is_empty() => {
                // The windows stay open with the reason; closing them exits.
                info!("disconnected: {reason}");
                self.exit_reason = Some(format!("disconnected: {reason}"));
                self.terminals.status(&tf("viewer.session_closed", &[("reason", &reason)]));
            }
            NetEvent::Disconnected { reason, .. } => {
                info!("disconnected: {reason}");
                self.exit_reason = Some(format!("disconnected: {reason}"));
                self.closed_message = Some(tf("viewer.session_closed", &[("reason", &reason)]));
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.terminals.owns(id) {
            self.terminals.window_event(id, &event);
            if self.terminal_only && self.terminals.is_empty() {
                self.exit_reason.get_or_insert_with(|| "session closed by the user".into());
                event_loop.exit();
            }
            return;
        }
        if self.fm.owns(id) {
            self.fm.window_event(&event);
            return;
        }
        if self.tunnels.owns(id) {
            self.tunnels.window_event(&event);
            return;
        }
        if self.printing.owns(id) {
            self.printing.window_event(&event);
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
                if let Some(notice) = &self.notice {
                    notice.layout();
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
                let code = w3c_code(code, self.remote_mac);
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
                let keys = if cfg!(windows) && self.remote_mac {
                    self.altgr.filter(&code, down, Instant::now())
                } else {
                    vec![(code, down)]
                };
                for (code, down) in keys {
                    if down {
                        self.pressed_keys.insert(code.clone());
                    } else {
                        self.pressed_keys.remove(&code);
                    }
                    self.send(Event::Key(Key { code, down }));
                }
            }
            _ => {}
        }
    }
}

/// `--type-keys`: the W3C code of a character on a US layout, and whether it needs Shift.
fn test_key_code(c: char) -> Option<(String, bool)> {
    Some(match c {
        'a'..='z' => (format!("Key{}", c.to_ascii_uppercase()), false),
        'A'..='Z' => (format!("Key{c}"), true),
        '0'..='9' => (format!("Digit{c}"), false),
        ' ' => ("Space".into(), false),
        '\n' => ("Enter".into(), false),
        '\u{8}' => ("Backspace".into(), false),
        _ => return None,
    })
}

/// W3C name (`KeyboardEvent.code`) of a winit key. The names are the same except
/// for the Windows/Cmd key, which winit calls "Super". From a Mac to a Windows or
/// Linux computer Cmd and Ctrl are swapped ([`mac_swap`]); from a Mac to a Mac
/// they stay what they are.
fn w3c_code(code: winit::keyboard::KeyCode, remote_mac: bool) -> String {
    let name = match format!("{code:?}").as_str() {
        "SuperLeft" => "MetaLeft".into(),
        "SuperRight" => "MetaRight".into(),
        other => other.to_string(),
    };
    if cfg!(target_os = "macos") && !remote_mac { mac_swap(name) } else { name }
}

/// AltGr from Windows towards a Mac. Windows reports AltGr as a left Ctrl followed at
/// once by the right Alt; a Mac would get Ctrl+Option, which types nothing, where
/// Option alone types what AltGr types on Windows (`[`, `{`, `@`, `#` on Italian
/// layouts). When the right Alt goes down within a few milliseconds of the left Ctrl,
/// the Ctrl is let go on the Mac and its own release is dropped.
#[derive(Default)]
struct AltGrFix {
    ctrl_down: Option<Instant>,
    swallow_ctrl_up: bool,
}

impl AltGrFix {
    /// Windows sends the two together: well under this.
    const WINDOW: Duration = Duration::from_millis(30);

    /// The keys to send for this one.
    fn filter(&mut self, code: &str, down: bool, now: Instant) -> Vec<(String, bool)> {
        match (code, down) {
            ("ControlLeft", true) => {
                // Repeats of a held Ctrl keep the first time.
                if self.ctrl_down.is_none() && !self.swallow_ctrl_up {
                    self.ctrl_down = Some(now);
                }
                if self.swallow_ctrl_up {
                    return Vec::new();
                }
            }
            ("ControlLeft", false) => {
                self.ctrl_down = None;
                if std::mem::take(&mut self.swallow_ctrl_up) {
                    return Vec::new();
                }
            }
            ("AltRight", true) => {
                if let Some(at) = self.ctrl_down.take()
                    && now.duration_since(at) <= Self::WINDOW
                {
                    self.swallow_ctrl_up = true;
                    return vec![("ControlLeft".into(), false), ("AltRight".into(), true)];
                }
            }
            _ => self.ctrl_down = None,
        }
        vec![(code.to_string(), down)]
    }
}

/// On a Mac, Cmd does what Ctrl does on Windows and Linux (Cmd+C, Cmd+V, Cmd+Z):
/// towards the remote computer the two are swapped. Option is already Alt.
fn mac_swap(name: String) -> String {
    match name.as_str() {
        "MetaLeft" => "ControlLeft".into(),
        "MetaRight" => "ControlRight".into(),
        "ControlLeft" => "MetaLeft".into(),
        "ControlRight" => "MetaRight".into(),
        _ => name,
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

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn super_becomes_meta() {
        assert_eq!(w3c_code(winit::keyboard::KeyCode::SuperLeft, false), "MetaLeft");
        assert_eq!(w3c_code(winit::keyboard::KeyCode::KeyA, false), "KeyA");
    }

    #[test]
    fn mac_to_mac_keeps_cmd() {
        assert_eq!(w3c_code(winit::keyboard::KeyCode::SuperLeft, true), "MetaLeft");
        assert_eq!(w3c_code(winit::keyboard::KeyCode::ControlLeft, true), "ControlLeft");
    }

    #[test]
    fn altgr_towards_mac_is_option() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let mut f = AltGrFix::default();
        let k = |c: &str, d| vec![(c.to_string(), d)];
        // AltGr: Ctrl then right Alt together → Ctrl let go, its release dropped.
        assert_eq!(f.filter("ControlLeft", true, ms(0)), k("ControlLeft", true));
        assert_eq!(
            f.filter("AltRight", true, ms(1)),
            vec![("ControlLeft".to_string(), false), ("AltRight".to_string(), true)]
        );
        assert!(f.filter("ControlLeft", true, ms(500)).is_empty(), "repeat while AltGr is held");
        assert_eq!(f.filter("BracketLeft", true, ms(600)), k("BracketLeft", true));
        assert_eq!(f.filter("AltRight", false, ms(700)), k("AltRight", false));
        assert!(f.filter("ControlLeft", false, ms(700)).is_empty());
        // A real Ctrl, then the right Alt later: both go through.
        assert_eq!(f.filter("ControlLeft", true, ms(1000)), k("ControlLeft", true));
        assert_eq!(f.filter("AltRight", true, ms(1300)), k("AltRight", true));
        assert_eq!(f.filter("ControlLeft", false, ms(1400)), k("ControlLeft", false));
        // Ctrl+C is untouched.
        assert_eq!(f.filter("ControlLeft", true, ms(2000)), k("ControlLeft", true));
        assert_eq!(f.filter("KeyC", true, ms(2001)), k("KeyC", true));
        assert_eq!(f.filter("ControlLeft", false, ms(2100)), k("ControlLeft", false));
    }

    #[test]
    fn mac_cmd_is_ctrl() {
        assert_eq!(mac_swap("MetaLeft".into()), "ControlLeft");
        assert_eq!(mac_swap("ControlRight".into()), "MetaRight");
        assert_eq!(mac_swap("KeyC".into()), "KeyC");
    }
}
