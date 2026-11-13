//! Privacy mode on KDE Plasma (KWin 6.6 or later, Wayland).
//!
//! - Black screen: a black full-screen window on every output, opened by the
//!   privacy child process as a Wayland client. A KWin script, loaded before the
//!   windows open, gives them KWin's `excludeFromCapture` (Plasma 6.6: the
//!   screencast leaves them out, so the technician sees the desktop underneath),
//!   keeps them above the other windows and gives the focus back to the window
//!   that had it; a second one sets the exclusion again once they are shown, which
//!   the screencast already running needs. Their input region is empty: the
//!   technician's clicks go to the windows below. "Show desktop" is turned off as
//!   soon as it turns on (it would hide them). The compositor removes them when
//!   the child ends, even killed: no black screen can be left behind.
//! - Notifications and on-screen displays (volume) are in KWin layers above the
//!   windows: they can show on the monitor.
//! - With the display off (power saving) KWin sends the screencast no picture:
//!   the technician sees black until the screen wakes, as without privacy mode.
//! - Local mouse and keyboard: grabbed by root (`local_input`), as on GNOME.
//!
//! No message: drawing text would need a font renderer, as on the other Linux
//! desktops the screen is plain black.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::debug;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use crate::local_input;

/// The windows' Wayland app id: KWin's `resourceClass`, what the script looks for.
const APP_ID: &str = "dev.nuvirahub.privacy";
/// The name the script is loaded under (one at a time; a stale one is replaced).
const SCRIPT_NAME: &str = "nuvirahub-privacy";
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Loaded before the windows open: sets them up as they appear and keeps them above.
const SCRIPT: &str = r#"
const APP = "dev.nuvirahub.privacy";
let previous = workspace.activeWindow;
function ours(w) { return w && w.resourceClass == APP; }
function apply(w) {
    if (!ours(w)) return;
    w.excludeFromCapture = true;
    w.keepAbove = true;
    w.skipTaskbar = true;
    w.skipPager = true;
    w.skipSwitcher = true;
    w.onAllDesktops = true;
    w.keepAboveChanged.connect(function () { if (!w.keepAbove) w.keepAbove = true; });
    if (previous && !ours(previous)) workspace.activeWindow = previous;
}
workspace.windowActivated.connect(function (w) { if (w && !ours(w)) previous = w; });
workspace.windowList().forEach(apply);
workspace.windowAdded.connect(apply);
"#;

/// Run once the windows are drawn: a screencast already running when they
/// appeared (the technician's) leaves them out only once the property changes
/// while they are shown (KWin 6.6; set as they appear, it stayed in that stream).
const SHOWN_SCRIPT: &str = r#"
for (const w of workspace.windowList()) {
    if (w.resourceClass != "dev.nuvirahub.privacy") continue;
    w.excludeFromCapture = false;
    w.excludeFromCapture = true;
    w.keepAbove = true;
}
"#;
const SHOWN_SCRIPT_NAME: &str = "nuvirahub-privacy-shown";

/// KWin's major and minor version, if KWin runs this session.
fn kwin_version() -> Option<(u32, u32)> {
    static VERSION: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *VERSION.get_or_init(|| {
        // Own thread: the caller may be inside the agent's async runtime.
        std::thread::spawn(|| -> Result<(u32, u32)> {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(async {
                let conn = zbus::Connection::session().await?;
                let reply = conn
                    .call_method(
                        Some("org.kde.KWin"),
                        "/KWin",
                        Some("org.kde.KWin"),
                        "supportInformation",
                        &(),
                    )
                    .await?;
                let text: String = reply.body().deserialize()?;
                parse_version(&text).context("no version in KWin's support information")
            })
        })
        .join()
        .ok()
        .and_then(Result::ok)
    })
}

/// "KWin version: 6.6.6" in KWin's support information.
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.lines().find_map(|l| l.trim().strip_prefix("KWin version:"))?;
    let mut parts = rest.trim().split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// This is a KDE Plasma session whose KWin can leave windows out of the capture.
pub fn supported() -> bool {
    kwin_version().is_some_and(|v| v >= (6, 6))
}

/// Why privacy mode cannot start here, if it cannot.
pub fn check() -> Result<()> {
    if !local_input::available() {
        bail!("@agent.privacy_needs_system_linux");
    }
    Ok(())
}

/// KWin's scripting interface, on its own runtime.
struct Kwin {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
}

impl Kwin {
    fn connect() -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let conn = rt.block_on(zbus::Connection::session()).context("session bus")?;
        Ok(Self { rt, conn })
    }

    fn scripting(&self, method: &str, args: &[&str]) -> Result<zbus::Message> {
        let call = async {
            let (dest, path, iface) = (Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"));
            match args {
                [a] => self.conn.call_method(dest, path, iface, method, &(*a,)).await,
                [a, b] => self.conn.call_method(dest, path, iface, method, &(*a, *b)).await,
                _ => self.conn.call_method(dest, path, iface, method, &()).await,
            }
        };
        Ok(self.rt.block_on(call)?)
    }

    fn unload(&self) {
        let _ = self.scripting("unloadScript", &[SCRIPT_NAME]);
        let _ = self.scripting("unloadScript", &[SHOWN_SCRIPT_NAME]);
    }

    /// Loads and runs the main script.
    fn load(&self) -> Result<()> {
        self.unload();
        self.run(SCRIPT_NAME, SCRIPT)
    }

    /// Runs the script for windows already shown (unloaded first if it ran before).
    fn shown(&self) -> Result<()> {
        let _ = self.scripting("unloadScript", &[SHOWN_SCRIPT_NAME]);
        self.run(SHOWN_SCRIPT_NAME, SHOWN_SCRIPT)
    }

    /// Loads and runs a script from a file KWin can read (it is this user's).
    fn run(&self, name: &str, source: &str) -> Result<()> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR")?;
        let path = std::path::Path::new(&dir).join(format!("{name}.js"));
        std::fs::write(&path, source).context("script file")?;
        let path = path.to_string_lossy().into_owned();
        let reply = self.scripting("loadScript", &[&path, name])?;
        let id: i32 = reply.body().deserialize()?;
        if id < 0 {
            bail!("KWin did not load the script");
        }
        let script = format!("/Scripting/Script{id}");
        self.rt.block_on(self.conn.call_method(
            Some("org.kde.KWin"),
            script.as_str(),
            Some("org.kde.kwin.Script"),
            "run",
            &(),
        ))?;
        Ok(())
    }
}

/// Unloads a script left by a killed privacy process (it changes nothing by
/// itself: only our windows, which went with the process).
pub fn recover() {
    let _ = std::thread::spawn(|| Kwin::connect().map(|k| k.unload())).join();
}

fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// "Show desktop" (or peeking at it from the panel) hides every window, the black
/// ones too: a thread turns it off again as soon as KWin says it is on.
fn keep_desktop_hidden() {
    let _ = std::thread::Builder::new().name("privacy-show-desktop".into()).spawn(|| {
        let result = (|| -> Result<()> {
            use futures_util::StreamExt;
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(async {
                let conn = zbus::Connection::session().await?;
                let hide = || {
                    conn.call_method(
                        Some("org.kde.KWin"),
                        "/KWin",
                        Some("org.kde.KWin"),
                        "showDesktop",
                        &(false,),
                    )
                };
                let rule = zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .sender("org.kde.KWin")?
                    .path("/KWin")?
                    .interface("org.kde.KWin")?
                    .member("showingDesktopChanged")?
                    .build();
                let mut signals = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
                hide().await?;
                while let Some(message) = signals.next().await {
                    if message?.body().deserialize::<bool>()? {
                        hide().await?;
                    }
                }
                Ok(())
            })
        })();
        if let Err(e) = result {
            eprintln!("privacy: show desktop not watched: {e:#}");
        }
    });
}

/// The privacy child process on KDE; returns its exit code.
pub fn run_child() -> i32 {
    let signals = match super::x11::signal_fd() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("privacy: signals: {e}");
            say("error @agent.privacy_failed");
            return 2;
        }
    };
    let kwin = match Kwin::connect().and_then(|k| k.load().map(|()| k)) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("privacy: KWin script: {e:#}");
            say("error @agent.privacy_unsupported_linux");
            return 2;
        }
    };
    let mut block = match local_input::block() {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("privacy: local input: {e:#}");
            kwin.unload();
            say(&format!("error {e}"));
            return 2;
        }
    };
    // The windows live on their own thread: they go when the process ends.
    let (ready_tx, ready_rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("privacy-windows".into()).spawn(move || {
        if let Err(e) = windows(ready_tx.clone()) {
            let _ = ready_tx.send(Err(format!("{e:#}")));
        }
    });
    let screens = match ready_rx.recv_timeout(READY_TIMEOUT) {
        Ok(Ok(n)) => n,
        other => {
            let why = match other {
                Ok(Err(e)) => e,
                _ => "no answer".into(),
            };
            eprintln!("privacy: black windows: {why}");
            drop(block);
            kwin.unload();
            say("error @agent.privacy_failed");
            return 2;
        }
    };
    if let Err(e) = kwin.shown() {
        eprintln!("privacy: KWin script for the shown windows: {e:#}");
    }
    // Again a second later, in case KWin had not shown them yet the first time.
    let mut shown_again = Some(std::time::Instant::now() + Duration::from_secs(1));
    keep_desktop_hidden();
    say(&format!("ready devices={} screens={screens}", block.as_ref().map_or(0, |b| b.devices)));

    let stdin = std::io::stdin();
    let mut fds = [
        libc::pollfd { fd: stdin.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: signals, events: libc::POLLIN, revents: 0 },
    ];
    loop {
        // SAFETY: two valid pollfd entries.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, 500) };
        if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            eprintln!("privacy: poll: {}", std::io::Error::last_os_error());
            break;
        }
        if fds[1].revents != 0 {
            eprintln!("privacy: ended by a signal");
            break;
        }
        if fds[0].revents != 0 {
            let mut buf = [0u8; 64];
            // SAFETY: reading into a local buffer.
            let read = unsafe { libc::read(fds[0].fd, buf.as_mut_ptr().cast(), buf.len()) };
            if read <= 0 {
                eprintln!("privacy: agent gone or done");
                break;
            }
        }
        match ready_rx.try_recv() {
            // A window thread that ended means the compositor went away (sign-out).
            Ok(Err(e)) => {
                eprintln!("privacy: black windows: {e}");
                break;
            }
            // A window for a new screen.
            Ok(Ok(n)) => {
                eprintln!("privacy: {n} black windows");
                shown_again = Some(std::time::Instant::now() + Duration::from_millis(300));
            }
            Err(_) => {}
        }
        if shown_again.is_some_and(|at| std::time::Instant::now() >= at) {
            shown_again = None;
            if let Err(e) = kwin.shown() {
                eprintln!("privacy: KWin script for the shown windows: {e:#}");
            }
        }
        // Root's service started again (an update, a crash): block again.
        if !block.as_ref().is_some_and(local_input::Block::alive) {
            block = match local_input::block() {
                Ok(b) => {
                    eprintln!("privacy: local input blocked again ({} devices)", b.devices);
                    Some(b)
                }
                Err(e) => {
                    eprintln!("privacy: local input not blocked: {e:#}");
                    None
                }
            };
        }
    }
    drop(block);
    kwin.unload();
    eprintln!("privacy: devices restored, black windows closed");
    // The windows close with the process.
    0
}

/// One black window.
struct Window {
    surface: wl_surface::WlSurface,
    _xdg: xdg_surface::XdgSurface,
    _toplevel: xdg_toplevel::XdgToplevel,
    /// Size asked by the last toplevel configure.
    asked: (i32, i32),
    /// Size of the buffer attached.
    drawn: (i32, i32),
    buffer: Option<wl_buffer::WlBuffer>,
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    outputs: Vec<wl_output::WlOutput>,
    windows: Vec<Window>,
    /// Windows are created for new outputs as they come.
    started: bool,
}

impl State {
    fn open(&mut self, output: &wl_output::WlOutput, qh: &QueueHandle<Self>) {
        let (Some(compositor), Some(wm)) = (&self.compositor, &self.wm) else { return };
        let surface = compositor.create_surface(qh, ());
        // Empty input region: clicks go to the windows underneath.
        let region = compositor.create_region(qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let index = self.windows.len();
        let xdg = wm.get_xdg_surface(&surface, qh, index);
        let toplevel = xdg.get_toplevel(qh, index);
        toplevel.set_app_id(APP_ID.into());
        toplevel.set_title("NuviraHub".into());
        toplevel.set_fullscreen(Some(output));
        surface.commit();
        self.windows.push(Window {
            surface,
            _xdg: xdg,
            _toplevel: toplevel,
            asked: (0, 0),
            drawn: (0, 0),
            buffer: None,
        });
    }

    /// Attaches a black buffer of the asked size (zeroed memory is black XRGB).
    fn draw(&mut self, index: usize, qh: &QueueHandle<Self>) -> Result<()> {
        let Some(shm) = self.shm.clone() else { return Ok(()) };
        let w = &mut self.windows[index];
        let (width, height) = if w.asked.0 > 0 && w.asked.1 > 0 { w.asked } else { (64, 64) };
        if w.drawn == (width, height) && w.buffer.is_some() {
            w.surface.commit();
            return Ok(());
        }
        let size = width as usize * height as usize * 4;
        // SAFETY: memfd_create with a static name; the descriptor is owned below.
        let fd = unsafe { libc::memfd_create(c"nuvirahub-privacy".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            bail!("memfd: {}", std::io::Error::last_os_error());
        }
        // SAFETY: a new descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: resizing our own memfd (zeroed memory: black in XRGB).
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
            bail!("memfd size: {}", std::io::Error::last_os_error());
        }
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, width, height, width * 4, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();
        w.surface.attach(Some(&buffer), 0, 0);
        w.surface.damage_buffer(0, 0, width, height);
        w.surface.commit();
        if let Some(old) = w.buffer.replace(buffer) {
            old.destroy();
        }
        w.drawn = (width, height);
        Ok(())
    }
}

/// Opens a black window on every output and serves them until the compositor
/// goes; sends the number of windows once all are drawn.
fn windows(ready: mpsc::Sender<Result<usize, String>>) -> Result<()> {
    let conn = Connection::connect_to_env().context("Wayland display")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    if state.compositor.is_none() || state.shm.is_none() || state.wm.is_none() {
        bail!("compositor without xdg-shell or shm");
    }
    if state.outputs.is_empty() {
        bail!("no output");
    }
    state.started = true;
    for output in state.outputs.clone() {
        state.open(&output, &qh);
    }
    // The configures, then the buffers.
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let drawn = state.windows.iter().filter(|w| w.buffer.is_some()).count();
    debug!(windows = drawn, "privacy windows");
    if drawn == 0 {
        bail!("no window drawn");
    }
    let _ = ready.send(Ok(drawn));
    let mut last = drawn;
    loop {
        queue.blocking_dispatch(&mut state).context("compositor gone")?;
        // A screen plugged in: its window must be left out of the capture too.
        let drawn = state.windows.iter().filter(|w| w.buffer.is_some()).count();
        if drawn > last {
            last = drawn;
            let _ = ready.send(Ok(drawn));
        }
    }
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
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(4), qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => state.wm = Some(registry.bind(name, version.min(2), qh, ())),
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(2), qh, ());
                    if state.started {
                        state.open(&output, qh);
                    }
                    state.outputs.push(output);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, usize> for State {
    fn event(
        state: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            if let Err(e) = state.draw(*index, qh) {
                eprintln!("privacy: window {index}: {e:#}");
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, usize> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Close requests are ignored: the windows go with privacy mode.
        if let xdg_toplevel::Event::Configure { width, height, .. } = event
            && let Some(w) = state.windows.get_mut(*index)
        {
            w.asked = (width, height);
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_output::WlOutput);

#[cfg(test)]
mod tests {
    #[test]
    fn kwin_version_line() {
        let text = "KWin Support Information:\n==========================\n\nVersion\n=======\nKWin version: 6.6.6\nQt Version: 6.10.1\n";
        assert_eq!(super::parse_version(text), Some((6, 6)));
        assert_eq!(super::parse_version("nothing"), None);
    }
}
