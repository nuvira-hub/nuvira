//! Linux: the local mouse and keyboard blocked by root, for privacy mode on
//! Wayland.
//!
//! On Wayland the compositor reads the input devices itself and no client can
//! hold them back (a GNOME Shell extension's event filter runs after Mutter has
//! given the events to the windows and run its shortcuts). Root can: an
//! `EVIOCGRAB` on a `/dev/input/event*` device makes the kernel deliver its
//! events only to the grabbing descriptor, so the compositor, and logind,
//! receive nothing from it. The technician's input is not affected: the
//! compositor's remote desktop devices (Mutter's, through the portal) are
//! virtual, with no `/dev/input` node.
//!
//! The root side ([`serve`]) runs in the sign-in screen's system service
//! (`nuvira service signin-run`), so it is there whenever unattended access
//! was installed with the administrator password. It listens on [`SOCKET`]:
//! a client writes `block`, gets `ok <devices>` or `error <text>`, and the
//! devices stay grabbed as long as its connection is open. Closing it, or the
//! client dying, releases them: the grabbing descriptors are closed and the
//! kernel drops the grab with them. Only the user of the active session of
//! `seat0` may ask (`SO_PEERCRED`): the person at the computer, whose input
//! it is.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

pub const SOCKET: &str = "/run/nuvira-input.sock";
/// How often the devices are looked at again (a mouse plugged in during privacy mode).
const RESCAN_EVERY: Duration = Duration::from_millis(500);
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Devices whose name starts so are NuviraHub's own (uinput) and stay free.
const OWN_DEVICE_PREFIX: &[u8] = b"NuviraHub";

// ioctl numbers of <linux/input.h>.
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
const fn eviocgbit_ev(len: libc::c_ulong) -> libc::c_ulong {
    eviocgbit(0, len)
}
/// EVIOCGBIT(ev, len): the codes a device has for one event type.
const fn eviocgbit(ev: u32, len: libc::c_ulong) -> libc::c_ulong {
    // _IOC(_IOC_READ, 'E', 0x20 + ev, len)
    (2 << 30) | (len << 16) | ((b'E' as libc::c_ulong) << 8) | (0x20 + ev as libc::c_ulong)
}
const fn eviocgname(len: libc::c_ulong) -> libc::c_ulong {
    (2 << 30) | (len << 16) | ((b'E' as libc::c_ulong) << 8) | 0x06
}
const EV_KEY: u32 = 1;
const EV_REL: u32 = 2;
const EV_ABS: u32 = 3;
const KEY_A: usize = 30;
const KEY_Z: usize = 44;
const KEY_SPACE: usize = 57;
const BTN_LEFT: usize = 0x110;
const BTN_TOUCH: usize = 0x14a;
const REL_X: usize = 0;
const ABS_X: usize = 0;
const ABS_MT_POSITION_X: usize = 0x35;

/// The root side is there (the system part of unattended access is installed).
pub fn available() -> bool {
    Path::new(SOCKET).exists()
}

/// The local input is blocked as long as this value lives.
pub struct Block {
    stream: UnixStream,
    pub devices: usize,
}

impl Block {
    /// Holds the connection; `false` once root has closed it (its service stopped:
    /// the devices are free again).
    pub fn alive(&self) -> bool {
        // Root never writes after "ok": anything readable is the end of the connection.
        let mut b = [0u8; 16];
        match (&self.stream).read(&mut b) {
            Ok(0) => false,
            Ok(_) => true,
            Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
        }
    }
}

/// Asks root to block the local input.
pub fn block() -> Result<Block> {
    let mut stream = UnixStream::connect(SOCKET).context("@agent.privacy_needs_system_linux")?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.write_all(b"block\n")?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).context("no answer from the system service")?;
    let line = line.trim();
    if let Some(n) = line.strip_prefix("ok ") {
        stream.set_read_timeout(None)?;
        stream.set_nonblocking(true)?;
        return Ok(Block { stream, devices: n.parse().unwrap_or(0) });
    }
    bail!("{}", line.strip_prefix("error ").unwrap_or("@agent.privacy_failed"));
}

/// Root: serves [`SOCKET`] for ever. `allowed` says whether a user id may block
/// the local input now.
pub fn serve(allowed: impl Fn(u32) -> bool + Send + Sync + 'static) -> Result<()> {
    let _ = std::fs::remove_file(SOCKET);
    let listener = UnixListener::bind(SOCKET).with_context(|| SOCKET.to_string())?;
    // Everyone may connect; who may block is checked per connection.
    std::fs::set_permissions(SOCKET, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
    info!(socket = SOCKET, "local input blocking for privacy mode");
    let allowed = Arc::new(allowed);
    // One block at a time: a device can have a single grab.
    let busy = Arc::new(Mutex::new(()));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let allowed = allowed.clone();
        let busy = busy.clone();
        let _ = std::thread::Builder::new().name("input-block".into()).spawn(move || {
            if let Err(e) = serve_one(stream, &*allowed, &busy) {
                warn!("input block: {e:#}");
            }
        });
    }
    Ok(())
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills a ucred of the given size.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

fn serve_one(mut stream: UnixStream, allowed: &dyn Fn(u32) -> bool, busy: &Mutex<()>) -> Result<()> {
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    if line.trim() != "block" {
        bail!("unknown request {:?}", line.trim());
    }
    let Some(uid) = peer_uid(&stream).filter(|&uid| allowed(uid)) else {
        let _ = stream.write_all(b"error @agent.privacy_not_active_user\n");
        bail!("refused: not the user of the active session");
    };
    let Ok(_guard) = busy.try_lock() else {
        let _ = stream.write_all(b"error @agent.privacy_failed\n");
        bail!("refused: already blocked for another connection");
    };
    let mut grabbed = Grabbed::default();
    grabbed.scan();
    stream.write_all(format!("ok {}\n", grabbed.devices.len()).as_bytes())?;
    info!(uid, devices = ?grabbed.names(), "local input blocked");
    stream.set_read_timeout(None)?;
    loop {
        let mut fd = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut fd, 1, RESCAN_EVERY.as_millis() as i32) };
        if n > 0 {
            let mut b = [0u8; 64];
            match stream.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        if !allowed(uid) {
            // Another person at the computer now (user switching): theirs is free.
            warn!(uid, "the active session changed: local input released");
            break;
        }
        grabbed.scan();
    }
    let names = grabbed.names();
    drop(grabbed);
    info!(uid, devices = ?names, "local input released");
    Ok(())
}

/// Grabbed devices by path; dropping it closes them, which ends the grabs.
#[derive(Default)]
struct Grabbed {
    devices: Vec<(String, String, OwnedFd)>,
}

impl Grabbed {
    fn names(&self) -> Vec<String> {
        self.devices.iter().map(|(p, n, _)| format!("{p} {n}")).collect()
    }

    /// Grabs the input devices not grabbed yet; forgets those unplugged.
    fn scan(&mut self) {
        self.devices.retain(|(path, _, _)| Path::new(path).exists());
        let Ok(dir) = std::fs::read_dir("/dev/input") else { return };
        for entry in dir.flatten() {
            let path = entry.path();
            let Some(text) = path.to_str().map(str::to_owned) else { continue };
            if !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event")) {
                continue;
            }
            if self.devices.iter().any(|(p, _, _)| *p == text) {
                continue;
            }
            match grab(&path) {
                Ok(Some((name, fd))) => self.devices.push((text, name, fd)),
                Ok(None) => {}
                Err(e) => warn!(device = text, "not blocked: {e:#}"),
            }
        }
    }
}

/// Opens and grabs a keyboard, mouse, touchpad or touch screen; `None` for other
/// devices and for NuviraHub's own. Power and sleep buttons, brightness keys
/// ("Video Bus"), a laptop's hotkeys and the lid stay free: the person at the
/// computer can still turn it off (on a laptop of 2026-11-12 the power button
/// was blocked with the rest).
fn grab(path: &Path) -> Result<Option<(String, OwnedFd)>> {
    let file =
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
    let fd = OwnedFd::from(file);
    let mut bits = [0u8; 4];
    // SAFETY: the kernel writes at most `len` bytes into `bits`.
    if unsafe { libc::ioctl(fd.as_raw_fd(), eviocgbit_ev(4), bits.as_mut_ptr()) } < 0 {
        return Ok(None);
    }
    let types = u32::from_le_bytes(bits);
    if types & ((1 << EV_KEY) | (1 << EV_REL) | (1 << EV_ABS)) == 0 {
        return Ok(None);
    }
    let has = |ev: u32, codes: &[usize]| -> bool {
        if types & (1 << ev) == 0 {
            return false;
        }
        let mut map = [0u8; 96]; // KEY_MAX 0x2ff: 768 bits
        // SAFETY: the kernel writes at most `len` bytes into `map`.
        if unsafe { libc::ioctl(fd.as_raw_fd(), eviocgbit(ev, map.len() as libc::c_ulong), map.as_mut_ptr()) } < 0 {
            return false;
        }
        codes.iter().all(|&c| map[c / 8] & (1 << (c % 8)) != 0)
    };
    let keyboard = has(EV_KEY, &[KEY_A, KEY_Z, KEY_SPACE]);
    let pointer = has(EV_REL, &[REL_X]) || has(EV_KEY, &[BTN_LEFT]);
    let touch = has(EV_ABS, &[ABS_X]) || has(EV_ABS, &[ABS_MT_POSITION_X]) || has(EV_KEY, &[BTN_TOUCH]);
    if !(keyboard || pointer || touch) {
        return Ok(None);
    }
    let mut name = [0u8; 128];
    // SAFETY: as above.
    let len =
        unsafe { libc::ioctl(fd.as_raw_fd(), eviocgname(name.len() as libc::c_ulong), name.as_mut_ptr()) };
    let name = &name[..usize::try_from(len).unwrap_or(0).min(name.len())];
    let name = name.split(|&b| b == 0).next().unwrap_or_default();
    if name.starts_with(OWN_DEVICE_PREFIX) {
        return Ok(None);
    }
    // SAFETY: EVIOCGRAB takes an int.
    if unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGRAB, 1 as libc::c_int) } < 0 {
        bail!("EVIOCGRAB: {}", std::io::Error::last_os_error());
    }
    Ok(Some((String::from_utf8_lossy(name).into_owned(), fd)))
}
