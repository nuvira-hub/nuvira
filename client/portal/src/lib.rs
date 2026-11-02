//! Remote desktop session through xdg-desktop-portal, for Wayland desktops, where
//! programs cannot read the screen or move the mouse by themselves. One session
//! gives both: the screen as a PipeWire stream and mouse and keyboard injection.
//!
//! The person at the computer allows it in a system dialog. The portal then gives
//! a restore token, kept in the data folder: later sessions start without asking,
//! until the person revokes the permission in the system settings.
//!
//! The portal is asynchronous (D-Bus): the session lives in its own thread with a
//! small Tokio runtime, and the rest of the program talks to it through a channel,
//! from any thread, with or without a runtime of its own.
#![cfg(target_os = "linux")]

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::remote_desktop::{
    Axis, DeviceType, KeyState, NotifyKeyboardKeycodeOptions, NotifyPointerAxisDiscreteOptions,
    NotifyPointerAxisOptions, NotifyPointerButtonOptions, NotifyPointerMotionAbsoluteOptions, RemoteDesktop,
    SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub use ashpd::desktop::remote_desktop::Axis as WheelAxis;

/// The screen the person chose to share.
#[derive(Debug, Clone, Copy)]
pub struct Screen {
    /// PipeWire node of its stream.
    pub node_id: u32,
    /// Position and size in the desktop's logical coordinates (those of the mouse).
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

enum Cmd {
    PipeWire(std::sync::mpsc::Sender<Result<OwnedFd>>),
    Motion(f64, f64),
    Button(i32, bool),
    Wheel(Axis, i32),
    Scroll(f64, f64),
    Key(i32, bool),
}

/// An open session. It closes when the last `Arc` is dropped.
pub struct Session {
    cmds: mpsc::UnboundedSender<Cmd>,
    screen: Screen,
    /// False once the system closed the session (the person stopped sharing, or
    /// the screen stream ended).
    open: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

static CURRENT: Mutex<Weak<Session>> = Mutex::new(Weak::new());

/// The open session, or a new one. With `ask` the system dialog is shown when
/// there is no saved permission or it is not accepted, and the person's answer is
/// awaited; without it only the saved permission is tried (resuming after the
/// screen was locked: GNOME refuses it while locked, and a dialog at every lock
/// would be a nuisance).
pub fn session(token_file: PathBuf, ask: bool) -> Result<Arc<Session>> {
    let mut current = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(session) = current.upgrade().filter(|s| s.is_open()) {
        debug!("portal session reused");
        return Ok(session);
    }
    let session = Arc::new(Session::start(token_file, ask)?);
    *current = Arc::downgrade(&session);
    Ok(session)
}

/// The open session, if there is one.
pub fn current() -> Option<Arc<Session>> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).upgrade().filter(|s| s.is_open())
}

impl Session {
    fn start(token_file: PathBuf, ask: bool) -> Result<Self> {
        let (cmds, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let open = Arc::new(AtomicBool::new(true));
        let thread_open = open.clone();
        let thread = std::thread::Builder::new()
            .name("portal".into())
            .spawn(move || run(token_file, ask, ready_tx, rx, &thread_open))?;
        let screen = ready_rx.recv().map_err(|_| anyhow!("portal thread ended"))??;
        Ok(Self { cmds, screen, open, thread: Mutex::new(Some(thread)) })
    }

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// A new connection to PipeWire that can see only the shared screen.
    pub fn pipewire(&self) -> Result<OwnedFd> {
        let closed = || anyhow!("portal session closed");
        let (tx, rx) = std::sync::mpsc::channel();
        self.cmds.send(Cmd::PipeWire(tx)).map_err(|_| closed())?;
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| closed())?
    }

    /// Mouse position in logical coordinates of the shared screen.
    pub fn pointer_motion(&self, x: f64, y: f64) {
        let _ = self.cmds.send(Cmd::Motion(x, y));
    }

    /// `button` is a Linux input code (`BTN_LEFT` = 0x110...).
    pub fn pointer_button(&self, button: i32, down: bool) {
        let _ = self.cmds.send(Cmd::Button(button, down));
    }

    /// Wheel notches; positive is down or right.
    pub fn wheel(&self, axis: WheelAxis, steps: i32) {
        let _ = self.cmds.send(Cmd::Wheel(axis, steps));
    }

    /// Smooth scrolling in pixels; positive is down or right.
    pub fn scroll(&self, dx: f64, dy: f64) {
        let _ = self.cmds.send(Cmd::Scroll(dx, dy));
    }

    /// `keycode` is a Linux input code (`KEY_A` = 30...).
    pub fn key(&self, keycode: i32, down: bool) {
        let _ = self.cmds.send(Cmd::Key(keycode, down));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        debug!("portal session released");
        // Closing the channel ends the thread, which closes the session.
        let (closed, _) = mpsc::unbounded_channel();
        drop(std::mem::replace(&mut self.cmds, closed));
        if let Some(thread) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = thread.join();
        }
    }
}

fn run(
    token_file: PathBuf,
    ask: bool,
    ready: std::sync::mpsc::Sender<Result<Screen>>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    open_flag: &AtomicBool,
) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(e.into()));
            return;
        }
    };
    rt.block_on(async {
        let (remote, screencast, session, screen) = match open(&token_file, ask).await {
            Ok(v) => v,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let mut closed = match session.receive_closed().await {
            Ok(closed) => closed.boxed(),
            Err(e) => {
                warn!("portal session: {e}");
                futures_util::stream::pending().boxed()
            }
        };
        let _ = ready.send(Ok(screen));
        let node = screen.node_id;
        let mut failed = false;
        loop {
            let cmd = tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => cmd,
                    None => break,
                },
                _ = closed.next() => {
                    info!("the system closed the remote desktop session");
                    open_flag.store(false, Ordering::Relaxed);
                    return;
                }
            };
            let result = match cmd {
                Cmd::PipeWire(reply) => {
                    let fd = screencast.open_pipe_wire_remote(&session, Default::default()).await;
                    let _ = reply.send(fd.context("PipeWire connection of the portal"));
                    continue;
                }
                Cmd::Motion(x, y) => {
                    let options = NotifyPointerMotionAbsoluteOptions::default();
                    remote.notify_pointer_motion_absolute(&session, node, x, y, options).await
                }
                Cmd::Button(button, down) => {
                    let options = NotifyPointerButtonOptions::default();
                    remote.notify_pointer_button(&session, button, state(down), options).await
                }
                Cmd::Wheel(axis, steps) => {
                    let options = NotifyPointerAxisDiscreteOptions::default();
                    remote.notify_pointer_axis_discrete(&session, axis, steps, options).await
                }
                Cmd::Scroll(dx, dy) => {
                    let options = NotifyPointerAxisOptions::default();
                    remote.notify_pointer_axis(&session, dx, dy, options).await
                }
                Cmd::Key(keycode, down) => {
                    let options = NotifyKeyboardKeycodeOptions::default();
                    remote.notify_keyboard_keycode(&session, keycode, state(down), options).await
                }
            };
            // Once is enough in the log: the same error comes for every event.
            if let Err(e) = result
                && !std::mem::replace(&mut failed, true)
            {
                warn!("portal input: {e}");
            }
        }
        open_flag.store(false, Ordering::Relaxed);
        let _ = session.close().await;
        info!("portal session closed");
    });
}

fn state(down: bool) -> KeyState {
    if down { KeyState::Pressed } else { KeyState::Released }
}

type Opened = (RemoteDesktop, Screencast, ashpd::desktop::Session<RemoteDesktop>, Screen);

/// How long a session with the saved permission may take to start.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(15);

/// Opens the session with the saved permission; if the system no longer accepts
/// it (revoked, or the token is stale), asks again.
async fn open(token_file: &std::path::Path, ask: bool) -> Result<Opened> {
    let token = std::fs::read_to_string(token_file).ok();
    let Some(token) = token.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
        if !ask {
            bail!("no saved remote desktop permission");
        }
        return open_with(token_file, None).await;
    };
    // With a saved permission there is no dialog: the answer comes at once, or
    // never (seen right after the screen locked): then it is given up, and the
    // capture asks again later.
    match tokio::time::timeout(RESTORE_TIMEOUT, open_with(token_file, Some(token))).await {
        Err(_) => Err(anyhow!("no answer from the remote desktop portal")),
        Ok(Err(e)) if !ask => Err(e),
        // Not deleted: GNOME refuses it also while the screen is locked. A new
        // permission replaces it.
        Ok(Err(e)) => {
            info!("saved remote desktop permission not accepted ({e:#}): asking again");
            open_with(token_file, None).await
        }
        Ok(opened) => opened,
    }
}

async fn open_with(token_file: &std::path::Path, token: Option<&str>) -> Result<Opened> {
    // A connection of its own: ashpd's shared one belongs to the runtime that
    // made it, the thread of the first session, and once that ended every later
    // request went unanswered.
    let bus = ashpd::zbus::Connection::session().await.context("D-Bus session bus")?;
    let remote = RemoteDesktop::with_connection(bus.clone()).await.context("remote desktop portal not available")?;
    let screencast = Screencast::with_connection(bus).await.context("screen cast portal not available")?;
    let session = remote.create_session(CreateSessionOptions::default()).await?;
    remote
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token),
        )
        .await?
        .response()?;
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                .set_multiple(false),
        )
        .await?
        .response()?;
    info!(restored = token.is_some(), "waiting for the remote desktop permission");
    let started = remote
        .start(&session, None, StartOptions::default())
        .await?
        .response()
        .map_err(|e| anyhow!("the remote session was not allowed on this computer ({e})"))?;
    if let Some(token) = started.restore_token()
        && let Err(e) = write_private(token_file, token)
    {
        warn!("cannot keep the remote desktop permission: {e}");
    }
    let stream = started.streams().first().context("no screen shared")?;
    let (width, height) = stream.size().unwrap_or((0, 0));
    let (left, top) = stream.position().unwrap_or((0, 0));
    let screen = Screen {
        node_id: stream.pipe_wire_node_id(),
        left,
        top,
        width: width.max(0) as u32,
        height: height.max(0) as u32,
    };
    info!(?screen, "remote desktop session started");
    Ok((remote, screencast, session, screen))
}

fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(text.as_bytes())
}
