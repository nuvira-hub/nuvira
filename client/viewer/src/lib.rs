//! NuviraHub viewer — the window of the remote session.
//!
//! Connects to an agent by ID through nuvira-server (or directly in a LAN),
//! shows its screen with wgpu (or the CPU renderer) and sends mouse and keyboard.
//! The window title shows frames per second, capture→display latency, RTT and
//! bandwidth; the toolbar is at the top (`toolbar.rs`).

mod cli;
mod cpu;
mod filemanager;
mod keys;
mod net;
mod net_events;
mod mp4;
mod printing;
mod recorder;
mod render;
mod report;
mod session_stats;
mod terminal;
mod tunnels;
mod toolbar;
mod toolbar_actions;
mod upload;
mod window_events;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nuvira_i18n::{t, tf};
use nuvira_proto::v1::{InputEvent, Key, input_event::Event};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::keys::AltGrFix;
use crate::net::{FrameSlot, NetEvent};
use crate::render::Renderer;

pub use cli::run;

/// The remote pointer as this viewer knows it (see `update_pointer`).
#[derive(Default)]
struct Pointer {
    shape: Option<render::CursorShape>,
    os: Option<winit::window::CustomCursor>,
    /// The shapes received, newest last (`Hello.cursor_cache`: the agent sends a
    /// shape once and then names it by its serial).
    kept: std::collections::VecDeque<(render::CursorShape, Option<winit::window::CustomCursor>)>,
    position: Option<(i32, i32, bool, u32)>,
    screen: (u32, u32),
    local: Option<(f32, f32)>,
}

/// The pointer `s` drawn with its hot spot at (`x`, `y`) on a remote screen of
/// `sw` x `sh` pixels: its rectangle as fractions of the image.
fn cursor_overlay(s: &render::CursorShape, x: i32, y: i32, sw: f32, sh: f32) -> render::CursorOverlay {
    render::CursorOverlay {
        shape: s.clone(),
        rect: [
            (x - s.hot_x as i32) as f32 / sw,
            (y - s.hot_y as i32) as f32 / sh,
            s.width as f32 / sw,
            s.height as f32 / sh,
        ],
    }
}

use crate::report::Report;

/// In place of a password, this prefix carries the ticket of a support request
/// (support queue): the viewer sends it as `Auth.support_ticket`.
pub const SUPPORT_TICKET: &str = "\u{1}support-ticket:";
/// Switch sides: serves a session of this computer's screen on the endpoint, with
/// the password, until it ends (the app sets it with its agent code; without it the
/// viewer does not offer switching sides).
pub type ReverseHost =
    fn(nuvira_transport::Endpoint, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;
pub static REVERSE_HOST: std::sync::OnceLock<ReverseHost> = std::sync::OnceLock::new();

/// In place of the password: the token of a session handed over to this viewer.
pub const HANDOVER_TOKEN: &str = "\u{1}handover:";
/// How long a viewer a session was handed over to keeps trying to take it.
const TAKEOVER_WAIT: Duration = Duration::from_secs(30);

/// After a remote restart: the first attempt to reconnect (the computer takes a while
/// to go down), the pause between the following ones and how long to keep trying.
const RESTART_FIRST_TRY: Duration = Duration::from_secs(20);
const RESTART_RETRY: Duration = Duration::from_secs(5);
const RESTART_WAIT: Duration = Duration::from_secs(10 * 60);

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

/// Locking the remote computer when the session ends (`SetLockOnEnd`).
#[derive(Default)]
struct LockOnEnd {
    /// The agent can.
    available: bool,
    /// Asked for (`--lock-on-end`, the toolbar): asked again after a reconnection.
    wanted: bool,
    /// What the agent says: it will lock, and its setting or the rules want it.
    on: bool,
    forced: bool,
}

struct App {
    /// Keeps a copy of the last frame for `--snapshot`.
    keep_last_frame: bool,
    last_frame: Option<nuvira_codec::YuvFrame>,
    /// The picture on screen now, for a screenshot (the toolbar's button).
    shown: Option<nuvira_codec::YuvFrame>,
    /// A screenshot is being written (one at a time).
    saving_screenshot: bool,
    /// `--screenshot-after`.
    screenshot_after: Option<u64>,
    /// Locking the remote computer at the end of the session.
    lock: LockOnEnd,
    /// `--hide-wallpaper`: asked again at every connection.
    hide_wallpaper: bool,
    /// The agent can restart into safe mode with networking.
    restart_safe_mode: bool,
    /// The technician's microphone: (offered, played there now).
    microphone: (bool, bool),
    /// `--microphone`.
    microphone_at_start: bool,
    /// The agent warned that the session ends without activity: the next local
    /// mouse or key event answers (`Activity`).
    idle_warned: bool,
    slot: Arc<FrameSlot>,
    /// Statistics log of the current session (`NUVIRA_STATS`), only when it is on.
    session_stats: Option<Arc<session_stats::ViewerStats>>,
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
    /// Who joined this session (shared sessions), as the agent last said.
    guests: Vec<String>,
    /// The guests' numbers in the session and whether each can take it over.
    guest_ids: Vec<u64>,
    guest_can_take_over: Vec<bool>,
    /// The session was handed over to this viewer (since then): it connects again.
    taking_over: Option<Instant>,
    /// This viewer handed the session over (to whom): it ends.
    handed_to: Option<String>,
    /// The remote pointer drawn here (Hello.cursor): its shape, as this
    /// computer's pointer too, and position; the remote screen's size; where this
    /// mouse last pointed in the picture (0..1).
    pointer: Pointer,
    /// Privacy mode on at the agent (as it last said).
    privacy: bool,
    /// The remote computer's sound: None when this session has none, else on or muted.
    sound: Option<bool>,
    /// `--privacy`: to turn on as soon as connected.
    privacy_at_start: bool,
    /// The remote resolution adapted to the window.
    adapt: Adapt,
    recording: Recording,
    /// `--restart`: to ask as soon as connected.
    /// `--safe-mode` with `--restart`.
    restart_safe_mode_at_start: bool,
    restart_at_start: bool,
    restart_check_at_start: bool,
    /// The agent tells who else is signed in (Hello.restart_users).
    restart_users: bool,
    /// RestartCheck sent: its answer opens the confirmation.
    restart_check_pending: bool,
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
    /// `--handover-after`: the wait, and when the session started.
    handover_after: Option<(Duration, Option<Instant>)>,
    /// This window shows the screen of this technician (switch sides).
    reverse_of: Option<String>,
    /// The agent can open a window with this computer's screen (switch sides).
    switch_offered: bool,
    /// This computer's screen is shown there now.
    switching: bool,
    /// `--switch-after`, `--switch-back-after`.
    switch_test: Option<SwitchTest>,
    /// `--terminal-log`.
    terminal_log: Option<std::fs::File>,
    /// Fit or real size, and the pan at real size.
    view: render::View,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    toolbar: Option<toolbar::Toolbar>,
    chat: Option<toolbar::ChatPanel>,
    menu: Option<toolbar::Menu>,
    /// Message in the middle of the window until the first image.
    notice: Option<toolbar::Notice>,
    /// Where the two-factor code goes, while the message in the middle asks for it.
    code_reply: Option<tokio::sync::oneshot::Sender<Option<String>>>,
    quality: nuvira_proto::v1::Quality,
    agent: String,
    stats: Stats,
    /// Agent clock minus ours (from the measurement with the lowest RTT).
    clock_offset_us: Option<i64>,
    best_rtt_us: u64,
    rtt_us: u64,
    pressed_keys: HashSet<String>,
    /// The controlled computer is a Mac (see [`w3c_code`](keys::w3c_code)).
    remote_mac: bool,
    /// The controlled computer runs Windows: AltGr needs no [`AltGrFix`] there.
    remote_windows: bool,
    /// Windows towards a Mac or Linux: when the left Ctrl went down, and whether it was
    /// the one Windows adds to AltGr (see [`AltGrFix`]).
    altgr: AltGrFix,
    exit_reason: Option<String>,
    /// Why the session ended, for the user (translated); printed as `@closed` for the app.
    closed_message: Option<String>,
    /// `--error-dialog`: that reason in a dialog before closing.
    error_dialog: bool,
    /// The dialog is open: the viewer closes when it is answered.
    closing: bool,
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

    /// The size to ask the agent for: the window's when adapting, 0x0 (its own)
    /// otherwise; `None` while the window has no size (minimised).
    fn adapt_target(&self) -> Option<(u32, u32)> {
        let a = &self.adapt;
        if !(a.wanted && a.allowed && a.display != ALL_DISPLAYS) {
            return Some((0, 0));
        }
        let s = self.window.as_ref()?.inner_size();
        (s.width > 0 && s.height > 0).then_some((s.width, s.height))
    }

    /// Asks the agent for the remote resolution, if it changed since the last request.
    fn send_adapt(&mut self) {
        if !self.adapt.available {
            return;
        }
        let Some(target) = self.adapt_target() else { return };
        // Nothing to put back if the session never changed it.
        let unchanged = self.adapt.sent == Some(target) || (target == (0, 0) && self.adapt.sent.is_none());
        if unchanged {
            self.adapt.asked_by_click = false;
            return;
        }
        info!("remote resolution for a {}x{} area asked", target.0, target.1);
        self.adapt.sent = Some(target);
        self.adapt.waiting = true;
        self.send_control(nuvira_proto::v1::control_message::Msg::SetResolution(nuvira_proto::v1::SetResolution {
            width: target.0,
            height: target.1,
        }));
    }

    /// The request leaves once the window has stopped changing size (while it is
    /// dragged, the remote screen would change at every step).
    fn schedule_adapt(&mut self) {
        if !self.adapt.wanted || !self.adapt.available {
            return;
        }
        self.adapt.generation += 1;
        let (generation, proxy) = (self.adapt.generation, self.proxy.clone());
        std::thread::spawn(move || {
            std::thread::sleep(ADAPT_SETTLE);
            let _ = proxy.send_event(NetEvent::AdaptSettled(generation));
        });
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
        self.net_thread = session.thread;
    }

    /// Restart button: an agent that can tell is asked first who else is signed in,
    /// and its answer opens the confirmation (an older agent: the confirmation at once).
    fn ask_restart(&mut self) {
        if self.restart_users {
            self.restart_check_pending = true;
            self.send_control(nuvira_proto::v1::control_message::Msg::RestartCheck(nuvira_proto::v1::RestartCheck {}));
        } else {
            self.confirm_restart(&[]);
        }
    }

    /// The confirmation, with the other people signed in whose sessions a restart closes.
    fn confirm_restart(&self, others: &[String]) {
        let mut text = tf("viewer.restart_confirm", &[("agent", &self.agent)]);
        if !others.is_empty() {
            text += "\n\n";
            text += &tf("viewer.restart_others", &[("users", &others.join(", "))]);
        }
        // The dialog blocks its own thread, not the session.
        let proxy = self.proxy.clone();
        let safe_mode = self.restart_safe_mode;
        std::thread::spawn(move || {
            let (normal, safe) = (t("viewer.restart_normal"), t("viewer.restart_safe_mode"));
            let cancel = t("viewer.cancel");
            let mut dialog = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title(t("viewer.restart_title"))
                .set_description(text);
            dialog = match safe_mode {
                true => dialog.set_buttons(rfd::MessageButtons::YesNoCancelCustom(
                    normal.clone(),
                    safe.clone(),
                    cancel,
                )),
                false => dialog.set_buttons(rfd::MessageButtons::YesNo),
            };
            let action = match dialog.show() {
                rfd::MessageDialogResult::Yes => Some(toolbar::Action::RestartConfirmed),
                rfd::MessageDialogResult::Custom(c) if c == normal => Some(toolbar::Action::RestartConfirmed),
                rfd::MessageDialogResult::No if safe_mode => Some(toolbar::Action::RestartSafeModeConfirmed),
                rfd::MessageDialogResult::Custom(c) if c == safe => {
                    Some(toolbar::Action::RestartSafeModeConfirmed)
                }
                _ => None,
            };
            if let Some(action) = action {
                let _ = proxy.send_event(NetEvent::Toolbar(action));
            }
        });
    }

    /// The session is over and the viewer closes, with message as its reason (for
    /// the app's Connect page, `@closed`). With `--error-dialog` the message is
    /// shown first in a dialog (information when the person at the remote
    /// computer ended it, a warning otherwise) and the window closes when it is
    /// answered: the technician sees why, not only a window that goes away.
    fn close_saying(&mut self, event_loop: &ActiveEventLoop, message: String, information: bool) {
        self.closed_message = Some(message.clone());
        if !self.error_dialog {
            event_loop.exit();
            return;
        }
        if self.closing {
            return;
        }
        self.closing = true;
        // The dialog blocks its own thread, not the window (as the restart's).
        let proxy = self.proxy.clone();
        let title = tf("viewer.closed_title", &[("agent", &self.agent)]);
        std::thread::spawn(move || {
            let level = if information { rfd::MessageLevel::Info } else { rfd::MessageLevel::Warning };
            let _ = rfd::MessageDialog::new()
                .set_level(level)
                .set_title(title)
                .set_description(message)
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            let _ = proxy.send_event(NetEvent::ClosedDialogAnswered);
        });
    }

    /// The session ended while the remote computer restarts: the window stays open
    /// with a message, and a new attempt starts after a pause.
    /// Switch sides ended (taken back, closed there, or failed): the window back.
    fn switch_ended(&mut self, text: &str) {
        self.switching = false;
        if let Some(toolbar) = &self.toolbar {
            toolbar.update("{switching:false}");
        }
        if let Some(window) = &self.window {
            window.set_minimized(false);
            window.focus_window();
        }
        self.notice(text, false);
    }

    /// The session's links are dropped (it ended), before connecting again.
    fn drop_session(&mut self) {
        self.release_keys();
        self.input = None;
        self.control = None;
        self.files = None;
        self.term_link = None;
        self.tunnel_link = None;
        self.net_thread = None;
    }

    /// Connects again after `pause` (NetEvent::Reconnect).
    fn reconnect_after(&self, pause: Duration) {
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(pause);
            let _ = proxy.send_event(NetEvent::Reconnect);
        });
    }

    /// The handover menu: the guests that can take the session over.
    fn handover_items(&self) -> Vec<(String, String)> {
        self.guests
            .iter()
            .zip(&self.guest_ids)
            .zip(&self.guest_can_take_over)
            .filter(|(_, can)| **can)
            .map(|((name, id), _)| (format!("handover:{id}"), name.clone()))
            .collect()
    }

    /// Asks before handing the session over to guest `id`.
    fn confirm_handover(&self, id: u64) {
        let Some(name) = self.guest_ids.iter().position(|g| *g == id).and_then(|i| self.guests.get(i)).cloned() else {
            return;
        };
        let text = tf("viewer.handover_confirm", &[("name", &name)]);
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let answer = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Info)
                .set_title(t("viewer.handover_title"))
                .set_description(text)
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if answer == rfd::MessageDialogResult::Ok {
                let _ = proxy.send_event(NetEvent::Toolbar(toolbar::Action::HandOverConfirmed(id)));
            }
        });
    }

    /// The message in the middle of the window (until the first picture).
    fn show_notice(&mut self, text: String) {
        match (&self.notice, &self.window) {
            (Some(notice), _) => notice.set_text(&text),
            (None, Some(window)) => match toolbar::Notice::new(window.clone(), self.proxy.clone(), &text) {
                Ok(notice) => self.notice = Some(notice),
                Err(e) => debug!("notice not available: {e:#}"),
            },
            _ => {}
        }
    }

    fn wait_for_restart(&mut self, reason: &str, pause: Duration) {
        info!("waiting for the remote computer to restart ({reason}); trying again in {} s", pause.as_secs());
        self.drop_session();
        self.show_notice(tf("viewer.waiting_restart", &[("agent", &self.agent)]));
        if let Some(window) = &self.window {
            window.set_title(&tf("viewer.title_restarting", &[("agent", &self.agent)]));
        }
        if let Some(toolbar) = &self.toolbar {
            toolbar.update("{restart:false}");
        }
        self.reconnect_after(pause);
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
        // Switch sides: the window says whose screen it is, not the statistics.
        if let Some(name) = &self.reverse_of {
            window.set_title(&tf("viewer.reverse_title", &[("name", name)]));
            return;
        }
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

    fn send_file(&self, path: PathBuf) {
        if let Some(files) = &self.files {
            info!("file to send: {}", path.display());
            let _ = files.send(net::Outgoing::File(path));
        }
    }

    /// Answers the agent's idle warning.
    fn still_here(&mut self) {
        self.idle_warned = false;
        info!("activity after the idle warning");
        self.send_control(nuvira_proto::v1::control_message::Msg::Activity(nuvira_proto::v1::Activity {}));
        self.notice(&t("viewer.idle_over"), false);
    }

    /// Saves the picture on screen as PNG in Pictures/NuviraHub, with the remote
    /// pointer drawn where it is, on a thread of its own (a 4K picture takes a
    /// moment to convert and compress).
    fn screenshot(&mut self) {
        if self.saving_screenshot {
            return;
        }
        let Some(frame) = &self.shown else {
            self.notice(&t("viewer.screenshot_none"), false);
            return;
        };
        let frame = frame.clone();
        let cursor = self.pointer_overlay();
        let dir = recorder::pictures_dir();
        let stem = recorder::file_stem(&self.agent);
        let proxy = self.proxy.clone();
        self.saving_screenshot = true;
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<PathBuf> {
                std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
                let path = (1..)
                    .map(|n| match n {
                        1 => dir.join(format!("{stem}.png")),
                        n => dir.join(format!("{stem} ({n}).png")),
                    })
                    .find(|p| !p.exists())
                    .expect("a free name");
                let planes = frame.with_planes();
                cpu::write_screenshot(&planes, cursor.as_ref(), &path)
                    .with_context(|| path.display().to_string())?;
                Ok(path)
            })();
            let _ = proxy.send_event(NetEvent::ScreenshotSaved(result.map_err(|e| format!("{e:#}"))));
        });
    }

    /// The remote pointer as it is now, wherever it is (for a screenshot).
    fn pointer_overlay(&self) -> Option<render::CursorOverlay> {
        let p = &self.pointer;
        let (x, y, visible, serial) = p.position?;
        let s = p.shape.as_ref().filter(|s| s.serial == serial && visible)?;
        let (sw, sh) = (p.screen.0.max(1) as f32, p.screen.1.max(1) as f32);
        Some(cursor_overlay(s, x, y, sw, sh))
    }

    /// The remote pointer: this computer's pointer takes its shape while this
    /// viewer controls; it is drawn into the picture where the remote computer moves
    /// it (its own mouse, a program) or while only watching. Hidden by the agent
    /// (drawn in the picture again, a recording): the usual pointer.
    fn update_pointer(&mut self) {
        let p = &self.pointer;
        let (Some(window), Some(r)) = (&self.window, self.renderer.as_mut()) else { return };
        let Some((x, y, visible, serial)) = p.position else { return };
        let shape = p.shape.as_ref().filter(|s| s.serial == serial && visible);
        let (sw, sh) = (p.screen.0.max(1) as f32, p.screen.1.max(1) as f32);
        let controlling = !self.view_only && !self.input_blocked;
        match (shape, &p.os) {
            (Some(_), Some(os)) if controlling => window.set_cursor(os.clone()),
            _ => window.set_cursor(winit::window::CursorIcon::Default),
        }
        // Drawn unless it is where this mouse put it (within 3 pixels).
        let here = p.local.is_some_and(|(lx, ly)| (lx * sw - x as f32).abs() <= 3.0 && (ly * sh - y as f32).abs() <= 3.0);
        let overlay = shape.filter(|_| !(controlling && here)).map(|s| cursor_overlay(s, x, y, sw, sh));
        if r.set_cursor(overlay) {
            window.request_redraw();
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
            "{{viewOnly:{},inputBlocked:{},privacy:{},recording:{},recordingRequired:{},actualSize:{},fullscreen:{},quality:{},resolutionAvailable:{},adaptResolution:{},sound:{}}}",
            self.view_only || self.input_blocked,
            self.input_blocked,
            self.privacy,
            self.recording.on,
            self.recording.required,
            self.view.actual,
            window.fullscreen().is_some(),
            toolbar::js_string(toolbar::quality_name(self.quality)),
            self.adapt.available && self.adapt.allowed,
            self.adapt.wanted,
            self.sound.unwrap_or(false)
        ));
    }
}

/// Automated tests of switching sides: when to switch and when to take control back.
struct SwitchTest {
    after: Duration,
    back_after: Option<Duration>,
    /// When the session started, then when it switched.
    since: Option<Instant>,
}

/// "All the screens" in SelectDisplay and Displays.
const ALL_DISPLAYS: u32 = 65535;
/// How long the window must keep its size before the remote resolution follows it.
const ADAPT_SETTLE: Duration = Duration::from_millis(800);

/// The remote resolution adapted to the window (toolbar button, `--adapt-resolution`).
#[derive(Default)]
struct Adapt {
    /// The technician wants it (remembered in the preferences).
    wanted: bool,
    /// The agent can do it (Hello.resolution).
    available: bool,
    /// The session allows it now (Permissions: the person there may switch it off).
    allowed: bool,
    /// Last size asked for (0x0: put back), `None` if nothing was asked in this session
    /// or on this screen.
    sent: Option<(u32, u32)>,
    /// Screen being shown.
    display: u32,
    /// Latest scheduled request (see [`App::schedule_adapt`]).
    generation: u64,
    /// A request is waiting for its answer: what comes back is said on screen.
    waiting: bool,
    /// The last request came from the button: going back is said on screen too.
    asked_by_click: bool,
}

/// Choices of the viewer kept between sessions (`viewer.json` in the data folder).
mod prefs;

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
                self.net_thread = session.thread;
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
        match toolbar::Menu::new(window.clone(), self.proxy.clone()) {
            Ok(menu) => self.menu = Some(menu),
            Err(e) => error!("menu not available: {e:#}"),
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
        self.net_event(event_loop, event);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        self.window_input(event_loop, id, event);
    }
}

/// What the window and the app say when the session closed with `reason`: "Session
/// closed: …", or, when the person at the PC ended it, only that (as the web page
/// says it).
fn closed_text(reason: &str) -> String {
    match reason == t("agent.ended_at_computer") {
        true => reason.to_string(),
        false => tf("viewer.session_closed", &[("reason", reason)]),
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

    /// The person at the PC ended the session (Phase 7 F-END-MSG): the window and
    /// the app say only that, as the web page does; any other end "Session
    /// closed: …" as before.
    #[test]
    fn the_end_at_the_computer_is_said_alone() {
        let at_computer = nuvira_i18n::message("@agent.ended_at_computer");
        assert_ne!(at_computer, "@agent.ended_at_computer", "the key is translated");
        assert_eq!(closed_text(&at_computer), at_computer);
        let other = nuvira_i18n::message("@agent.busy");
        assert_eq!(closed_text(&other), tf("viewer.session_closed", &[("reason", &other)]));
        assert_eq!(closed_text("session ended"), tf("viewer.session_closed", &[("reason", "session ended")]));
    }
}
