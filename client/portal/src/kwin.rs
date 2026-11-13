//! KWin's own Wayland protocols, for the sign-in screen of SDDM on Wayland, where
//! no portal runs: the screen through `zkde_screencast_unstable_v1` (a PipeWire
//! stream of the output) and input through `org_kde_kwin_fake_input`.
//!
//! KWin offers the two to a program only when a `.desktop` file names it in
//! `X-KDE-Wayland-Interfaces` (looked up in the KDE service cache of the
//! compositor's user): the system service installs one for a dedicated path of
//! the program and starts the greeter's agent from there (`nuvira-service`,
//! `signin_linux`). Feasibility test and reasons: nuvira-pro
//! docs/internal/36-sddm-wayland-greeter.md.
//!
//! Only for a system user (the greeter's): in a person's session the portal asks
//! them, and a program authorised this way must not skip that.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{info, warn};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput as FakeInput;
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::{
    self as stream, ZkdeScreencastStreamUnstableV1 as Stream,
};
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1 as Screencast;

use crate::{Cmd, Screen, WheelAxis};

/// `pointer` of `stream_output`: the cursor drawn into the picture.
const POINTER_EMBEDDED: u32 = 2;
/// `pointer` of `stream_output`: the pointer as stream metadata.
const POINTER_METADATA: u32 = 4;
/// `fake_input.axis`: vertical and horizontal wheel.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;
/// One wheel notch, in the units of `axis` (libinput's 15 degrees).
const NOTCH: f64 = 15.0;

#[derive(Default)]
struct State {
    output: Option<wl_output::WlOutput>,
    /// Position (compositor space), mode size in pixels and scale of the output.
    position: (i32, i32),
    mode: (i32, i32),
    scale: i32,
    screencast: Option<Screencast>,
    fake: Option<(FakeInput, u32)>,
    node: Option<u32>,
    failed: Option<String>,
    closed: bool,
    /// The compositor is KWin (it offers its own interfaces to everyone).
    kwin: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        if interface.starts_with("org_kde_kwin_") {
            state.kwin = true;
        }
        match interface.as_str() {
            // The first output: the sign-in screen is on it (one per greeter).
            "wl_output" if state.output.is_none() => {
                state.output = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "zkde_screencast_unstable_v1" => {
                state.screencast = Some(registry.bind(name, version.min(5), qh, ()));
            }
            "org_kde_kwin_fake_input" => {
                let version = version.min(6);
                state.fake = Some((registry.bind(name, version, qh, ()), version));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Geometry { x, y, .. } => state.position = (x, y),
            wl_output::Event::Mode { width, height, .. } => state.mode = (width, height),
            wl_output::Event::Scale { factor } => state.scale = factor,
            _ => {}
        }
    }
}

impl Dispatch<Screencast, ()> for State {
    fn event(
        _: &mut Self,
        _: &Screencast,
        _: <Screencast as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<Stream, ()> for State {
    fn event(state: &mut Self, _: &Stream, event: stream::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            stream::Event::Created { node } => state.node = Some(node),
            stream::Event::Failed { error } => state.failed = Some(error),
            stream::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<FakeInput, ()> for State {
    fn event(
        _: &mut Self,
        _: &FakeInput,
        _: <FakeInput as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// A system account: the greeter's (`sddm` and the like), never a person's.
fn system_user() -> bool {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    uid != 0 && uid < 1000
}

struct Opened {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    _stream: Stream,
    screen: Screen,
}

fn open() -> Result<Opened> {
    if !system_user() {
        bail!("KWin's remote control protocols only for the sign-in screen");
    }
    std::env::var_os("WAYLAND_DISPLAY").context("not a Wayland session")?;
    // KWin reads the KDE service cache with its authorisation a few seconds after
    // the system service rebuilt it (at the start of the greeter's agent), and
    // decides for each connection when it is made: a new one until then.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let (conn, mut queue, mut state, screencast) = loop {
        let conn = Connection::connect_to_env().context("Wayland connection")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State { scale: 1, ..Default::default() };
        // Globals, then the output's properties.
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        if let (Some(screencast), Some(_)) = (state.screencast.clone(), &state.fake) {
            break (conn, queue, state, screencast);
        }
        if !state.kwin {
            bail!("the compositor is not KWin");
        }
        if std::time::Instant::now() > deadline {
            bail!("KWin's screen cast and fake input protocols not offered (not KWin, or the program is not authorised)");
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let qh = queue.handle();
    let output = state.output.clone().context("no screen")?;
    let metadata = crate::cursor::wanted();
    crate::cursor::set_active(metadata);
    let stream =
        screencast.stream_output(&output, if metadata { POINTER_METADATA } else { POINTER_EMBEDDED }, &qh, ());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while state.node.is_none() && state.failed.is_none() && !state.closed {
        if std::time::Instant::now() > deadline {
            bail!("no screen stream from KWin");
        }
        queue.roundtrip(&mut state)?;
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(error) = &state.failed {
        bail!("KWin's screen stream failed: {error}");
    }
    let node_id = state.node.context("KWin closed the screen stream")?;
    let scale = state.scale.max(1);
    let screen = Screen {
        node_id,
        left: state.position.0,
        top: state.position.1,
        width: (state.mode.0 / scale).max(0) as u32,
        height: (state.mode.1 / scale).max(0) as u32,
    };
    if let Some((fake, _)) = &state.fake {
        fake.authenticate("NuviraHub".into(), "Remote control of the sign-in screen".into());
        // SDDM's Breeze theme hides the sign-in form after a while with no input,
        // leaving the wallpaper: a nudge of the pointer brings it back for the viewer.
        fake.pointer_motion(1.0, 0.0);
        fake.pointer_motion(-1.0, 0.0);
    }
    conn.flush()?;
    info!(?screen, "KWin remote control of the sign-in screen started");
    Ok(Opened { conn, queue, state, _stream: stream, screen })
}

pub(crate) fn run(
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
        let Opened { conn, mut queue, mut state, _stream, screen } = match open() {
            Ok(opened) => opened,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let wayland = match conn.backend().poll_fd().try_clone_to_owned().map_err(anyhow::Error::from).and_then(
            // SAFETY: an OwnedFd, owned by the AsyncFd for its whole life.
            |fd: OwnedFd| unsafe { AsyncFd::register(fd) }.map_err(|e| anyhow!("Wayland socket: {e}")),
        ) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(screen));
        let Some((fake, version)) = state.fake.clone() else { return };
        loop {
            tokio::select! {
                cmd = cmds.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        Cmd::PipeWire(reply) => {
                            let _ = reply.send(crate::mutter::pipewire_socket());
                            continue;
                        }
                        // KWin wants positions in the whole desktop's logical space.
                        Cmd::Motion(x, y) => {
                            fake.pointer_motion_absolute(x + f64::from(screen.left), y + f64::from(screen.top));
                        }
                        Cmd::Button(button, down) => fake.button(button as u32, u32::from(down)),
                        Cmd::Wheel(axis, steps) => {
                            let axis = if matches!(axis, WheelAxis::Horizontal) { AXIS_HORIZONTAL } else { AXIS_VERTICAL };
                            fake.axis(axis, f64::from(steps) * NOTCH);
                        }
                        Cmd::Scroll(dx, dy) => {
                            if dx != 0.0 {
                                fake.axis(AXIS_HORIZONTAL, dx);
                            }
                            if dy != 0.0 {
                                fake.axis(AXIS_VERTICAL, dy);
                            }
                        }
                        // keyboard_key since version 4.
                        Cmd::Key(keycode, down) if version >= 4 => fake.keyboard_key(keycode as u32, u32::from(down)),
                        Cmd::Key(..) => {}
                        // fake_input has no keysyms: text is left out on this sign-in screen.
                        Cmd::Keysym(..) => {}
                    }
                    if let Err(e) = conn.flush() {
                        warn!("KWin input: {e}");
                        break;
                    }
                }
                guard = wayland.readable() => {
                    let Ok(mut guard) = guard else { break };
                    if let Some(read) = queue.prepare_read() {
                        if let Err(e) = read.read() {
                            if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                                warn!("KWin connection: {e}");
                                break;
                            }
                        }
                    }
                    guard.clear_ready();
                    if let Err(e) = queue.dispatch_pending(&mut state) {
                        warn!("KWin connection: {e}");
                        break;
                    }
                    if state.closed {
                        info!("KWin closed the screen stream");
                        break;
                    }
                }
            }
        }
        open_flag.store(false, Ordering::Relaxed);
        _stream.close();
        if version >= 5 {
            fake.destroy();
        }
        let _ = conn.flush();
        info!("KWin remote control of the sign-in screen closed");
    });
}
