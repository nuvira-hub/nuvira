//! Privacy mode on Linux, Wayland sessions (GNOME).
//!
//! - Black screen: the gamma ramp of every active CRTC goes to zero through
//!   Mutter's `org.gnome.Mutter.DisplayConfig.SetCrtcGamma` (the API GNOME's
//!   colour tools used). The ramp is applied by the display hardware after the
//!   frame buffer, which is what the screencast reads: the technician still sees
//!   the real image. No message (nothing can be read on a black screen) and no
//!   window, since Mutter's screencast cannot leave one out. Needs a driver with
//!   a gamma ramp: virtual machines (virtio-gpu) have none, then privacy mode is
//!   not offered (`NUVIRA_PRIVACY_NO_BLACK=1` offers it without the black, for
//!   tests). Other compositors (KDE) have no such API: not offered.
//! - Local mouse and keyboard: grabbed by root (`local_input`), which needs the
//!   system part of unattended access.
//!
//! As on X11 the work is done by the privacy child process (`--privacy-overlay`):
//! it gives the ramps back when its input closes or a termination signal comes,
//! and applies the black again every [`ENFORCE_EVERY`] (night light, screens
//! changed). If it is killed, its connection to root closes and the kernel frees
//! the devices at once; the ramps are put back by [`recover`] when the agent
//! starts or ends privacy mode.

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::warn;
use zbus::zvariant::OwnedValue;

use crate::local_input;

const NO_BLACK_ENV: &str = "NUVIRA_PRIVACY_NO_BLACK";
const ENFORCE_EVERY: Duration = Duration::from_millis(500);

const DEST: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const IFACE: &str = "org.gnome.Mutter.DisplayConfig";

fn black_optional() -> bool {
    std::env::var_os(NO_BLACK_ENV).is_some_and(|v| v == "1")
}

/// Mutter's display configuration on the session bus.
struct Mutter {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
}

/// A CRTC of `GetResources`: id, winsys id, x, y, width, height, current mode,
/// current transform, transforms, properties.
type Crtc = (u32, i64, i32, i32, i32, i32, i32, u32, Vec<u32>, HashMap<String, OwnedValue>);
type Output = (u32, i64, i32, Vec<u32>, String, Vec<u32>, Vec<u32>, HashMap<String, OwnedValue>);
type Mode = (u32, i64, u32, u32, f64, u32);
type Ramps = (Vec<u16>, Vec<u16>, Vec<u16>);

impl Mutter {
    fn connect() -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let conn = rt.block_on(zbus::Connection::session()).context("session bus")?;
        Ok(Self { rt, conn })
    }

    /// The configuration's serial and its active CRTCs (those showing a mode).
    fn crtcs(&self) -> Result<(u32, Vec<u32>)> {
        let reply = self
            .rt
            .block_on(self.conn.call_method(Some(DEST), PATH, Some(IFACE), "GetResources", &()))
            .context("Mutter's display configuration")?;
        let (serial, crtcs, _, _, _, _): (u32, Vec<Crtc>, Vec<Output>, Vec<Mode>, i32, i32) =
            reply.body().deserialize()?;
        Ok((serial, crtcs.into_iter().filter(|c| c.6 >= 0).map(|c| c.0).collect()))
    }

    fn gamma(&self, serial: u32, crtc: u32) -> Result<Ramps> {
        let reply = self.rt.block_on(self.conn.call_method(
            Some(DEST),
            PATH,
            Some(IFACE),
            "GetCrtcGamma",
            &(serial, crtc),
        ))?;
        Ok(reply.body().deserialize()?)
    }

    fn set_gamma(&self, serial: u32, crtc: u32, (r, g, b): &Ramps) -> Result<()> {
        self.rt.block_on(self.conn.call_method(
            Some(DEST),
            PATH,
            Some(IFACE),
            "SetCrtcGamma",
            &(serial, crtc, r, g, b),
        ))?;
        Ok(())
    }
}

fn is_black((r, g, b): &Ramps) -> bool {
    [r, g, b].iter().all(|ramp| ramp.iter().all(|&v| v == 0))
}

fn linear(size: usize) -> Vec<u16> {
    let last = size.max(2) - 1;
    (0..size).map(|i| (i * 65535 / last) as u16).collect()
}

/// Some active screen has a gamma ramp. Asked once (it is the hardware), on a
/// thread of its own: the caller may be inside the agent's async runtime.
fn gamma_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        std::thread::spawn(|| -> Result<bool> {
            let m = Mutter::connect()?;
            let (serial, crtcs) = m.crtcs()?;
            Ok(crtcs.into_iter().any(|c| m.gamma(serial, c).is_ok_and(|g| !g.0.is_empty())))
        })
        .join()
        .ok()
        .and_then(Result::ok)
        .unwrap_or(false)
    })
}

/// Why privacy mode cannot start here, if it cannot.
pub fn check() -> Result<()> {
    if !black_optional() && !gamma_supported() {
        bail!("@agent.privacy_unsupported_linux");
    }
    if !local_input::available() {
        bail!("@agent.privacy_needs_system_linux");
    }
    Ok(())
}

/// Gives a linear ramp to the screens left black by a killed privacy process.
pub fn recover() {
    let result = std::thread::spawn(|| -> Result<usize> {
        let m = Mutter::connect()?;
        let (serial, crtcs) = m.crtcs()?;
        let mut fixed = 0;
        for crtc in crtcs {
            let current = m.gamma(serial, crtc)?;
            if !current.0.is_empty() && is_black(&current) {
                let ramp = linear(current.0.len());
                m.set_gamma(serial, crtc, &(ramp.clone(), ramp.clone(), ramp))?;
                fixed += 1;
            }
        }
        Ok(fixed)
    })
    .join();
    if let Ok(Ok(n)) = result
        && n > 0
    {
        warn!(screens = n, "privacy mode of a killed process undone (gamma)");
    }
}

fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// The privacy child process on Wayland; returns its exit code.
pub fn run_child() -> i32 {
    let signals = match super::x11::signal_fd() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("privacy: signals: {e}");
            say("error @agent.privacy_failed");
            return 2;
        }
    };
    let mutter = match Mutter::connect() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("privacy: {e:#}");
            say("error @agent.privacy_unsupported_linux");
            return 2;
        }
    };
    let mut block = match local_input::block() {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("privacy: local input: {e:#}");
            say(&format!("error {e}"));
            return 2;
        }
    };
    // Ramps as they were, per CRTC, to put back.
    let mut saved: HashMap<u32, Ramps> = HashMap::new();
    if let Err(e) = enforce(&mutter, &mut saved) {
        eprintln!("privacy: {e:#}");
        restore(&mutter, &saved);
        say("error @agent.privacy_failed");
        return 2;
    }
    if saved.is_empty() && !black_optional() {
        restore(&mutter, &saved);
        say("error @agent.privacy_unsupported_linux");
        return 2;
    }
    say(&format!("ready devices={} screens={}", block.as_ref().map_or(0, |b| b.devices), saved.len()));

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
        if let Err(e) = enforce(&mutter, &mut saved) {
            // Mutter went away (sign-out): nothing left to protect.
            eprintln!("privacy: {e:#}");
            break;
        }
    }
    drop(block);
    restore(&mutter, &saved);
    eprintln!("privacy: devices and screens restored");
    0
}

fn enforce(m: &Mutter, saved: &mut HashMap<u32, Ramps>) -> Result<()> {
    let (serial, crtcs) = m.crtcs()?;
    for crtc in crtcs {
        let current = m.gamma(serial, crtc)?;
        if current.0.is_empty() {
            continue;
        }
        let black = is_black(&current);
        // Kept the first time: later changes are someone else's (night light).
        // Already black at the start: left so by a killed process, linear afterwards.
        saved.entry(crtc).or_insert_with(|| {
            if black {
                let ramp = linear(current.0.len());
                (ramp.clone(), ramp.clone(), ramp)
            } else {
                current.clone()
            }
        });
        if !black {
            let zero = vec![0; current.0.len()];
            m.set_gamma(serial, crtc, &(zero.clone(), zero.clone(), zero))?;
        }
    }
    Ok(())
}

fn restore(m: &Mutter, saved: &HashMap<u32, Ramps>) {
    if saved.is_empty() {
        return;
    }
    let Ok((serial, crtcs)) = m.crtcs() else { return };
    for (crtc, ramps) in saved {
        if crtcs.contains(crtc)
            && let Err(e) = m.set_gamma(serial, *crtc, ramps)
        {
            eprintln!("privacy: gamma of CRTC {crtc} not restored: {e:#}");
        }
    }
}
