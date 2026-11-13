//! Privacy mode on Linux, X11 sessions.
//!
//! X11 has no way to leave one window out of the capture of the root window, so
//! there are no black windows as on Windows and macOS:
//! - Black screen: the gamma ramp of every CRTC (RandR) goes to zero. The capture
//!   reads the frame buffer before the ramp, so the technician still sees the real
//!   image; the monitor (and the hardware cursor) shows black. No message can be
//!   shown. Needs a driver with a gamma ramp: virtual machines often have none
//!   (virtio-gpu), then privacy mode is not offered.
//! - Local mouse and keyboard: every physical slave device is switched off
//!   (XInput 2 "Device Enabled" = 0, like `xinput disable`). The XTest devices,
//!   through which NuviraHub injects, stay on.
//! - Wayland: see `privacy_wayland.rs` (Mutter's gamma, input grabbed by root).
//!   The [`Overlay`] here starts the child process for both.
//!
//! The work is done by a child process (the same program started with
//! `--privacy-overlay`), so it is undone even if the agent dies: the child
//! restores everything when its standard input closes or it gets SIGTERM, SIGINT
//! or SIGHUP (systemd stopping the user service), and re-applies it every
//! [`ENFORCE_EVERY`] (devices plugged in, night light changing the gamma). The
//! devices it switched off are written to [`state_path`] and the file is removed
//! once restored: if both processes are killed, [`recover`] (called when the
//! agent starts) switches them on again.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xinput::{ConnectionExt as _, DeviceType, XIChangePropertyAux, XIGetPropertyItems};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
use x11rb::rust_connection::RustConnection;

/// Argument that turns the program into the privacy child process.
const OVERLAY_ARG: &str = "--privacy-overlay";
/// Virtual machine tests: privacy mode without the black screen (no gamma ramp).
const NO_BLACK_ENV: &str = "NUVIRA_PRIVACY_NO_BLACK";
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the child applies the black screen and switches devices off again.
const ENFORCE_EVERY: Duration = Duration::from_millis(500);
/// All devices (`XIAllDevices`).
const ALL_DEVICES: u16 = 0;

fn black_optional() -> bool {
    std::env::var_os(NO_BLACK_ENV).is_some_and(|v| v == "1")
}

/// An X11 session with XInput 2 and at least one screen whose gamma can be
/// changed (or [`NO_BLACK_ENV`]).
pub fn available() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    let Ok(x) = X::connect() else { return false };
    black_optional() || x.gamma_crtcs().is_ok_and(|c| !c.is_empty())
}

pub struct Overlay {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl Overlay {
    pub fn start(message: &str) -> Result<Self> {
        // Nothing can be written on a screen whose gamma is zero.
        let _ = message;
        super::check()?;
        let exe = std::env::current_exe().context("program path")?;
        let mut child = Command::new(exe)
            .arg(OVERLAY_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("privacy process not started")?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("output of the privacy process")?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("privacy-x11".into()).spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        })?;
        match rx.recv_timeout(READY_TIMEOUT) {
            Ok(line) if line.starts_with("ready") => {
                info!(pid = child.id(), state = line.trim(), "privacy mode on");
                Ok(Self { child: Some(child), stdin })
            }
            other => {
                drop(stdin);
                let _ = child.wait();
                super::recover();
                let error = other.ok().and_then(|l| l.trim().strip_prefix("error ").map(str::to_owned));
                bail!(error.unwrap_or_else(|| "@agent.privacy_failed".into()));
            }
        }
    }

    pub fn stop(&mut self) {
        // Closing its input makes the child restore everything and end.
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    _ => {
                        warn!("privacy process did not end: stopped");
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        // The child died without restoring (killed): do it from its notes.
        super::recover();
        info!("privacy mode off");
    }
}

/// File with the devices switched off by the child, per X display.
fn state_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let display: String = std::env::var("DISPLAY")
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Some(PathBuf::from(dir).join(format!("nuvira-privacy{display}")))
}

/// Undoes a privacy mode whose process was killed: switches its devices on and
/// gives the screens a linear gamma if they are still black.
pub fn recover() {
    let Some(path) = state_path() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let Ok(x) = X::connect() else { return };
    let devices: Vec<u16> = text.split_whitespace().filter_map(|w| w.parse().ok()).collect();
    for &id in &devices {
        // Only devices still off: the ids may belong to another X server by now.
        if x.device_enabled(id) == Some(false) {
            let _ = x.set_enabled(id, true);
        }
    }
    if let Ok(crtcs) = x.gamma_crtcs() {
        for (crtc, size) in crtcs {
            if x.gamma(crtc).is_ok_and(|g| g.iter().all(|ramp| ramp.iter().all(|&v| v == 0))) {
                let _ = x.set_gamma(crtc, &linear_ramp(size));
            }
        }
    }
    let _ = x.sync();
    let _ = std::fs::remove_file(&path);
    warn!(?devices, "privacy mode of a killed process undone");
}

fn linear_ramp(size: u16) -> Vec<u16> {
    let last = u32::from(size.max(2) - 1);
    (0..u32::from(size)).map(|i| (i * 65535 / last) as u16).collect()
}

/// X connection with the requests privacy mode needs.
struct X {
    conn: RustConnection,
    root: u32,
    enabled_atom: u32,
    xtest_atom: u32,
}

impl X {
    fn connect() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("X11 connection")?;
        let root = conn.setup().roots[screen].root;
        conn.xinput_xi_query_version(2, 0)?.reply().context("XInput 2 not available")?;
        conn.randr_query_version(1, 2)?.reply().context("RandR not available")?;
        let atom = |name: &str| -> Result<u32> { Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
        let enabled_atom = atom("Device Enabled")?;
        let xtest_atom = atom("XTEST Device")?;
        Ok(Self { conn, root, enabled_atom, xtest_atom })
    }

    /// CRTCs with a gamma ramp, with its size.
    fn gamma_crtcs(&self) -> Result<Vec<(u32, u16)>> {
        let resources = self.conn.randr_get_screen_resources_current(self.root)?.reply()?;
        let mut crtcs = Vec::new();
        for crtc in resources.crtcs {
            let size = self.conn.randr_get_crtc_gamma_size(crtc)?.reply()?.size;
            if size > 0 {
                crtcs.push((crtc, size));
            }
        }
        Ok(crtcs)
    }

    fn gamma(&self, crtc: u32) -> Result<[Vec<u16>; 3]> {
        let g = self.conn.randr_get_crtc_gamma(crtc)?.reply()?;
        Ok([g.red, g.green, g.blue])
    }

    fn set_gamma(&self, crtc: u32, ramp: &[u16]) -> Result<()> {
        self.conn.randr_set_crtc_gamma(crtc, ramp, ramp, ramp)?;
        Ok(())
    }

    fn set_gamma_rgb(&self, crtc: u32, [r, g, b]: &[Vec<u16>; 3]) -> Result<()> {
        self.conn.randr_set_crtc_gamma(crtc, r, g, b)?;
        Ok(())
    }

    /// Waits until the server has handled every request sent so far.
    fn sync(&self) -> Result<()> {
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    fn property_u8(&self, device: u16, property: u32) -> Option<u8> {
        let reply = self.conn.xinput_xi_get_property(device, false, property, 0u32, 0, 1).ok()?.reply().ok()?;
        match reply.items {
            XIGetPropertyItems::Data8(v) => v.first().copied(),
            _ => None,
        }
    }

    fn device_enabled(&self, device: u16) -> Option<bool> {
        self.property_u8(device, self.enabled_atom).map(|v| v != 0)
    }

    fn set_enabled(&self, device: u16, on: bool) -> Result<()> {
        self.conn.xinput_xi_change_property(
            device,
            PropMode::REPLACE,
            self.enabled_atom,
            AtomEnum::INTEGER.into(),
            1,
            &XIChangePropertyAux::Data8(vec![u8::from(on)]),
        )?
        .check()?;
        Ok(())
    }

    /// Physical mouse and keyboard devices that are on (not XTest, not masters).
    fn local_devices_on(&self) -> Result<Vec<(u16, String)>> {
        let reply = self.conn.xinput_xi_query_device(ALL_DEVICES)?.reply()?;
        Ok(reply
            .infos
            .into_iter()
            .filter(|d| matches!(d.type_, DeviceType::SLAVE_POINTER | DeviceType::SLAVE_KEYBOARD))
            .filter(|d| d.enabled)
            .filter(|d| self.property_u8(d.deviceid, self.xtest_atom) != Some(1))
            .map(|d| (d.deviceid, String::from_utf8_lossy(&d.name).into_owned()))
            .filter(|(_, name)| !name.contains("XTEST"))
            .collect())
    }
}

/// Blocks SIGTERM, SIGINT and SIGHUP and returns a descriptor that becomes
/// readable when one arrives.
pub(super) fn signal_fd() -> std::io::Result<i32> {
    // SAFETY: plain libc calls on a local signal set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC);
        if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(fd) }
    }
}

/// Runs the privacy child process when the program was started for it (never
/// returns then); otherwise does nothing. Call it first thing in `main`.
pub fn overlay_if_asked() {
    if std::env::args_os().nth(1).is_some_and(|a| a == OVERLAY_ARG) {
        std::process::exit(if !crate::wayland_session() {
            child::run()
        } else if super::kde::supported() {
            super::kde::run_child()
        } else {
            super::wayland::run_child()
        });
    }
}

mod child {
    use super::*;

    /// What the child changed, to put back.
    struct Applied {
        devices: Vec<u16>,
        gamma: HashMap<u32, [Vec<u16>; 3]>,
    }

    fn say(line: &str) {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{line}").and_then(|()| out.flush());
    }

    pub fn run() -> i32 {
        // Termination signals are read from a descriptor instead of ending the process.
        let signals = match signal_fd() {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("privacy: signals: {e}");
                say("error @agent.privacy_failed");
                return 2;
            }
        };
        let x = match X::connect() {
            Ok(x) => x,
            Err(e) => {
                eprintln!("privacy: {e:#}");
                say("error @agent.privacy_unsupported_linux");
                return 2;
            }
        };
        let mut applied = Applied { devices: Vec::new(), gamma: HashMap::new() };
        if let Err(e) = enforce(&x, &mut applied) {
            eprintln!("privacy: {e:#}");
            restore(&x, &applied);
            say("error @agent.privacy_failed");
            return 2;
        }
        if applied.gamma.is_empty() && !black_optional() {
            restore(&x, &applied);
            say("error @agent.privacy_unsupported_linux");
            return 2;
        }
        say(&format!("ready devices={} screens={}", applied.devices.len(), applied.gamma.len()));

        let stdin = std::io::stdin();
        let mut fds = [
            libc::pollfd { fd: stdin.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: signals, events: libc::POLLIN, revents: 0 },
        ];
        loop {
            // SAFETY: two valid pollfd entries.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ENFORCE_EVERY.as_millis() as i32) };
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
            if let Err(e) = enforce(&x, &mut applied) {
                // The X server went away (sign-out): nothing left to protect.
                eprintln!("privacy: {e:#}");
                break;
            }
        }
        restore(&x, &applied);
        eprintln!("privacy: devices and screens restored");
        0
    }

    fn enforce(x: &X, applied: &mut Applied) -> Result<()> {
        let mut changed = false;
        for (id, name) in x.local_devices_on()? {
            x.set_enabled(id, false)?;
            if !applied.devices.contains(&id) {
                applied.devices.push(id);
                changed = true;
                eprintln!("privacy: device {id} \"{name}\" off");
            }
        }
        for (crtc, size) in x.gamma_crtcs()? {
            let current = x.gamma(crtc)?;
            let black = current.iter().all(|ramp| ramp.iter().all(|&v| v == 0));
            // Kept the first time: later changes are someone else's (night light).
            // Already black at the start: left so by a killed process, linear afterwards.
            applied.gamma.entry(crtc).or_insert_with(|| {
                if black {
                    let linear = linear_ramp(size);
                    [linear.clone(), linear.clone(), linear]
                } else {
                    current
                }
            });
            if !black {
                x.set_gamma(crtc, &vec![0; usize::from(size)])?;
            }
        }
        x.sync()?;
        if changed && let Some(path) = state_path() {
            let ids: Vec<String> = applied.devices.iter().map(u16::to_string).collect();
            let _ = std::fs::write(path, ids.join(" "));
        }
        Ok(())
    }

    fn restore(x: &X, applied: &Applied) {
        for &id in &applied.devices {
            if let Err(e) = x.set_enabled(id, true) {
                eprintln!("privacy: device {id} not switched on: {e:#}");
            }
        }
        for (&crtc, ramps) in &applied.gamma {
            let _ = x.set_gamma_rgb(crtc, ramps);
        }
        // A round trip: everything above is done before the process ends.
        if x.sync().is_ok()
            && let Some(path) = state_path()
        {
            let _ = std::fs::remove_file(path);
        }
    }
}
