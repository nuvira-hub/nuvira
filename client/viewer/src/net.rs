//! Network side of the viewer: connection to the agent, video reception and
//! decoding, input sending and measurement of RTT and clock difference.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use nuvira_codec::YuvFrame;
use nuvira_proto::v1::{ControlMessage, DisplayInfo, InputEvent, control_message::Msg};
use nuvira_transport::Fingerprint;
use tokio::sync::mpsc;
use winit::event_loop::EventLoopProxy;

mod audio;
mod connect;
mod session;
mod streams;
mod transfer;
mod video;

use session::session;
pub use transfer::part_path;

/// Events from the network to the window.
pub enum NetEvent {
    /// The statistics log of the session that starts (`NUVIRA_STATS`), only when it is on.
    Stats(Arc<crate::session_stats::ViewerStats>),
    /// The person at the remote computer is being asked to accept.
    ConfirmationPending,
    /// The session ends in this many seconds without activity (0: warning over).
    IdleWarning(u32),
    /// The remote computer wants the two-factor code: the window asks for it and
    /// answers here (`None`: not given).
    CodeRequired(tokio::sync::oneshot::Sender<Option<String>>),
    /// The remote pointer's image (RGBA, not premultiplied), when it changes.
    CursorShape { serial: u32, width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: std::sync::Arc<Vec<u8>> },
    /// Where the remote pointer is, in pixels of the remote screen; not visible:
    /// hidden, or drawn into the picture again.
    CursorPosition { x: i32, y: i32, visible: bool, serial: u32 },
    /// The remote screen's size changed (another screen, another resolution).
    ScreenSize { width: u32, height: u32 },
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
        /// The remote computer runs Windows (see `remote_is_windows`): from Windows
        /// towards any other, AltGr needs the viewer's AltGr fix.
        remote_windows: bool,
        /// The agent can turn on privacy mode.
        privacy: bool,
        /// The agent opens remote terminals.
        terminal: bool,
        /// The agent opens TCP tunnels (NuviraHub Pro).
        tunnel: bool,
        /// The agent can restart the computer.
        restart: bool,
        /// The agent tells who else is signed in before a restart (RestartCheck).
        restart_users: bool,
        /// What is printed on the remote computer's NuviraHub printer arrives here.
        print: bool,
        /// The person at the remote computer allows recording the session.
        recording: bool,
        /// The server's rules want the session recorded: from the start, no stopping.
        recording_required: bool,
        /// It can adapt its screen's resolution to the window (SetResolution).
        resolution: bool,
        /// Switch sides: it can open a window with this computer's screen.
        switch_sides: bool,
        /// Its sound is played here (SetAudio switches it).
        audio: bool,
        /// It can lock the computer when the session ends (SetLockOnEnd).
        lock_on_end: bool,
        /// It can hide its wallpaper (SetWallpaper).
        wallpaper: bool,
        /// It can restart into safe mode with networking (Windows).
        restart_safe_mode: bool,
        /// It plays this computer's microphone (SetMicrophone).
        microphone: bool,
    },
    /// Whether the microphone is played there, or why not ("@key").
    Microphone {
        on: bool,
        error: String,
    },
    /// Whether the remote computer will be locked when the session ends, and
    /// whether its setting or the server's rules want it (no turning it off).
    LockOnEnd {
        on: bool,
        forced: bool,
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
        /// Adapting the remote resolution to the window.
        resolution: bool,
        /// The remote computer's sound (off: the person there switched it off).
        audio: bool,
        /// Switched off by the person there ("input", "clipboard", "files", …).
        switched_off: Vec<String>,
    },
    /// The remote screen is locked and cannot be shown (macOS, GNOME), or no longer;
    /// whether the keyboard still reaches it.
    ScreenLocked { locked: bool, input: bool },
    /// Answer of the agent to SetResolution: the remote screen's size now, whether
    /// the session changed it; `error` ("@key" or text) when it could not.
    Resolution {
        width: u32,
        height: u32,
        adapted: bool,
        error: String,
    },
    /// The window kept its size for a while: time to ask for the remote resolution
    /// (only if `0` is still the latest request).
    AdaptSettled(u64),
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
    /// A screenshot of the session was written (`Ok`: the file), or why not.
    ScreenshotSaved(Result<std::path::PathBuf, String>),
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
    /// Who else is signed in on the remote computer (answer to RestartCheck).
    RestartUsers { others: Vec<String> },
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
    /// Who joined this session (shared sessions), at every change.
    Guests(nuvira_proto::v1::Guests),
    /// The session was handed over: to this viewer (a token to connect again
    /// with), or by it to a guest.
    HandedOver(nuvira_proto::v1::HandedOver),
    /// Switch sides: the window with this screen opened at the other computer
    /// (`on`), or it ended (`error` empty when closed normally).
    SwitchSides { on: bool, error: String },
    /// Click on the toolbar.
    Toolbar(crate::toolbar::Action),
    /// The dialog with why the session ended was answered: the viewer closes.
    ClosedDialogAnswered,
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
    /// To be joined before exiting; `None` when the thread could not be started (the
    /// window was told with `Disconnected`).
    pub thread: Option<std::thread::JoinHandle<()>>,
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
    let failed = proxy.clone();
    let thread = std::thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            // Without a runtime (threads or handles exhausted) the window is told, as for any
            // other end of the session, instead of waiting forever on "connecting".
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    let reason = format!("cannot start the network runtime: {e}");
                    let _ = proxy.send_event(NetEvent::Disconnected { reason, retry: false });
                    return;
                }
            };
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
        .map_err(|e| {
            let reason = format!("cannot start the network thread: {e}");
            let _ = failed.send_event(NetEvent::Disconnected { reason, retry: false });
        })
        .ok();
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

/// The agent's system is Windows: older agents send none, and they are all Windows.
pub(crate) fn remote_is_windows(os: &str) -> bool {
    os.is_empty() || os == "windows"
}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}
