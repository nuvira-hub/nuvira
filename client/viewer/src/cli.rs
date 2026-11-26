//! The viewer's command line (`Args`) and `run`, which opens the session window
//! or sends a recording (`upload ...`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::error;

use crate::keys::AltGrFix;
use crate::net::{FrameSlot, NetEvent};
use crate::report::Report;
#[cfg(windows)]
use crate::own_taskbar_group;
use crate::{
    Adapt, App, LockOnEnd, Pointer, Recording, SUPPORT_TICKET, Stats, SwitchTest, cpu, event_loop, net, prefs,
    printing, recorder, render, upload,
};

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
    /// Agent ID or alias (with --server), or LAN address, e.g. 192.168.1.20[:21500].
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

    /// When the session ends without the user closing it (refused, not reached,
    /// connection lost, ended at the remote computer), the reason is shown in a
    /// dialog before the window closes. The app passes it; scripts and tests do
    /// not, so the viewer never waits for a click there.
    #[arg(long, hide = true)]
    error_dialog: bool,

    /// View only: does not send mouse and keyboard.
    #[arg(long)]
    view_only: bool,

    /// Starts at real size (1:1) instead of fitting the window (also from the toolbar).
    #[arg(long)]
    actual_size: bool,

    /// Turns on privacy mode as soon as connected, when the agent has it (also from the toolbar).
    #[arg(long)]
    privacy: bool,

    /// The remote screen takes the size of the window, as close as its modes allow
    /// (also from the toolbar, which remembers the choice).
    #[arg(long)]
    adapt_resolution: bool,

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

    /// Tests, with --restart: into safe mode with networking (Windows agents).
    #[arg(long, hide = true)]
    safe_mode: bool,

    /// Tests: as soon as connected, asks who else is signed in and shows the restart
    /// confirmation as the toolbar button does (nothing is restarted unless confirmed).
    #[arg(long, hide = true)]
    restart_check: bool,

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
    /// Switch sides, at the controlled computer: the window shows the screen of the
    /// technician named here (started by the agent).
    #[arg(long, value_name = "NAME", env = "NUVIRA_REVERSE_OF", hide = true, hide_env_values = true)]
    reverse_of: Option<String>,
    /// Automated tests: hands the session over, without asking, to the first guest
    /// that can take it, as soon as there is one this many seconds after connecting.
    #[arg(long, value_name = "SECONDS", hide = true)]
    handover_after: Option<u64>,
    /// Automated tests: switches sides this many seconds after connecting.
    #[arg(long, value_name = "SECONDS", hide = true)]
    switch_after: Option<u64>,
    /// Automated tests: takes control back this many seconds after switching.
    #[arg(long, value_name = "SECONDS", hide = true)]
    switch_back_after: Option<u64>,

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

    /// Locks the remote computer when the session ends (also from the toolbar).
    #[arg(long)]
    lock_on_end: bool,

    /// Tests: the microphone on as soon as connected (the toolbar's button).
    #[arg(long, hide = true)]
    microphone: bool,

    /// Hides the remote computer's wallpaper during the session (put back at the
    /// end): a plain background needs less bandwidth.
    #[arg(long)]
    hide_wallpaper: bool,

    /// Tests: this many seconds after connecting, takes a screenshot as the
    /// toolbar's button does (Pictures/NuviraHub).
    #[arg(long, hide = true, value_name = "SECONDS")]
    screenshot_after: Option<u64>,

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
            device_id: nuvira_transport::signal::target(&args.agent),
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
        shown: None,
        saving_screenshot: false,
        screenshot_after: args.screenshot_after,
        lock: LockOnEnd { wanted: args.lock_on_end, ..LockOnEnd::default() },
        idle_warned: false,
        hide_wallpaper: args.hide_wallpaper,
        restart_safe_mode: false,
        microphone: (false, false),
        microphone_at_start: args.microphone,
        slot: Arc::new(FrameSlot::default()),
        session_stats: None,
        started,
        report: Report::new(args.stats_file.is_some()),
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
        sound: None,
        privacy_at_start: args.privacy,
        adapt: Adapt { wanted: args.adapt_resolution || prefs::load().adapt_resolution, ..Default::default() },
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
        restart_safe_mode_at_start: args.safe_mode,
        restart_check_at_start: args.restart_check,
        restart_users: false,
        restart_check_pending: false,
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
            let text = args.type_keys.as_deref().unwrap_or("").replace("\\n", "\n").replace("\\b", "\u{8}");
            // Shortcuts: \A, \C, \V for Ctrl+A, C, V; \w waits a second.
            let text = text
                .replace("\\A", "\u{1}")
                .replace("\\C", "\u{3}")
                .replace("\\V", "\u{16}")
                .replace("\\w", "\u{17}");
            (0, text)
        }),
        handover_after: args.handover_after.map(|s| (Duration::from_secs(s), None)),
        reverse_of: args.reverse_of.clone().filter(|n| !n.is_empty()),
        switch_test: args.switch_after.map(|a| SwitchTest {
            after: Duration::from_secs(a),
            back_after: args.switch_back_after.map(Duration::from_secs),
            since: None,
        }),
        switch_offered: false,
        switching: false,
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
        menu: None,
        notice: None,
        code_reply: None,
        quality: nuvira_proto::v1::Quality::Balanced,
        agent: args.agent.clone(),
        stats: Stats::new(),
        clock_offset_us: None,
        best_rtt_us: u64::MAX,
        rtt_us: 0,
        pressed_keys: HashSet::new(),
        remote_mac: false,
        remote_windows: true,
        altgr: AltGrFix::default(),
        input_blocked: false,
        switched_off: Vec::new(),
        guests: Vec::new(),
        guest_ids: Vec::new(),
        guest_can_take_over: Vec::new(),
        taking_over: None,
        handed_to: None,
        pointer: Pointer::default(),
        exit_reason: None,
        closed_message: None,
        error_dialog: args.error_dialog,
        closing: false,
        restarting: None,
        restarted: false,
    };
    for spec in &args.tunnel {
        app.tunnels.add_spec(spec).with_context(|| format!("--tunnel {spec}"))?;
    }
    event_loop.run_app(&mut app)?;
    // The session's end for the report, before the exit wait below.
    let ended = Instant::now();
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
        std::fs::write(path, app.report.to_json(app.started, ended))
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
