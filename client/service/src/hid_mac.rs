//! macOS: typing into the **lock screen** through a virtual HID keyboard.
//!
//! macOS 27's lock screen drops every synthetic event (CGEventPost and the older
//! APIs, as the person or as root) but takes the input of a HID device. The
//! DriverKit driver of Karabiner-DriverKit-VirtualHIDDevice (public domain, signed
//! and notarized by its author, bundled in the app's Resources) provides one; its
//! daemon runs as root and takes requests on a root-only Unix socket.
//!
//! `nuvira service hid-run`, a LaunchDaemon of ours (root), keeps that daemon
//! running, opens the virtual keyboard and listens on [`SOCKET`] for the person's
//! agent: only the program of this same bundle may connect (checked by the peer's
//! executable path). The agent sends HID keyboard reports while the screen is
//! locked; they are passed on as they are.
//!
//! Installed and removed with the rest of the system part (`nuvira service system`);
//! the driver extension must be allowed once in System Settings (the app opens the
//! page). A Karabiner already on the Mac is used; the driver is removed with
//! unattended access unless Karabiner-Elements is there.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

/// Where the agent sends its reports.
pub const SOCKET: &str = "/var/run/nuvira-hid.sock";
pub const LABEL: &str = "io.github.nuvira-hub.nuvira.hid";
const LOG: &str = "/Library/Logs/NuviraHub/hid.log";

const KV_ROOT: &str = "/Library/Application Support/org.pqrs/Karabiner-DriverKit-VirtualHIDDevice";
const KV_DAEMON: &str = "/Library/Application Support/org.pqrs/Karabiner-DriverKit-VirtualHIDDevice/Applications/Karabiner-VirtualHIDDevice-Daemon.app/Contents/MacOS/Karabiner-VirtualHIDDevice-Daemon";
const KV_SOCKET: &str = "/Library/Application Support/org.pqrs/tmp/rootonly/karabiner_virtual_hid_device_service.sock";
const KV_MANAGER: &str = "/Applications/.Karabiner-VirtualHIDDevice-Manager.app/Contents/MacOS/Karabiner-VirtualHIDDevice-Manager";
/// `client_protocol_version` of Karabiner-DriverKit-VirtualHIDDevice 8.x.
const KV_PROTOCOL: u16 = 7;
/// Size of a keyboard report: report id, modifiers, reserved, 32 keys (u16).
pub const REPORT_SIZE: usize = 67;

pub fn plist_path() -> PathBuf {
    PathBuf::from("/Library/LaunchDaemons").join(format!("{LABEL}.plist"))
}

pub fn installed() -> bool {
    plist_path().exists()
}

/// The virtual keyboard can be used (driver installed, our daemon running).
pub fn available() -> bool {
    Path::new(SOCKET).exists()
}

// ---------------------------------------------------------------- Karabiner's daemon

/// A connection to Karabiner's daemon with the virtual keyboard opened. A thread
/// reads what the daemon sends: its status messages are requests that want an
/// answer (unanswered for 15 s, the daemon closes the connection).
struct Karabiner {
    writer: Arc<Mutex<UnixStream>>,
    next_id: u64,
    /// False once the reading thread saw the connection end.
    alive: Arc<std::sync::atomic::AtomicBool>,
}

/// Frame of pqrs' unix_domain_stream: body size (u32 BE), type, request id (u64 BE), data.
fn frame(kind: u8, request_id: Option<u64>, data: &[u8]) -> Vec<u8> {
    let id_len = if request_id.is_some() { 8 } else { 0 };
    let body = 1 + id_len + data.len();
    let mut out = Vec::with_capacity(4 + body);
    out.extend_from_slice(&(body as u32).to_be_bytes());
    out.push(kind);
    if let Some(id) = request_id {
        out.extend_from_slice(&id.to_be_bytes());
    }
    out.extend_from_slice(data);
    out
}

const FRAME_HEARTBEAT: u8 = 0;
const FRAME_REQUEST: u8 = 4;
const FRAME_RESPONSE: u8 = 5;
const REQ_KEYBOARD_INITIALIZE: u8 = 0;
const REQ_POST_KEYBOARD: u8 = 6;
const RESP_KEYBOARD_READY: u8 = 4;

/// Reads the daemon's frames: answers its requests, reports the keyboard ready.
fn read_frames(
    mut reader: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
    ready: std::sync::mpsc::Sender<()>,
    alive: Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        let mut header = [0u8; 4];
        if reader.read_exact(&mut header).is_err() {
            break;
        }
        let size = u32::from_be_bytes(header) as usize;
        if size == 0 || size > 4096 {
            warn!(size, "bad frame from Karabiner's daemon");
            break;
        }
        let mut body = vec![0u8; size];
        if reader.read_exact(&mut body).is_err() {
            break;
        }
        if matches!(body[0], FRAME_REQUEST | FRAME_RESPONSE) && body.len() >= 9 {
            if body[9..].chunks_exact(2).any(|pair| pair[0] == RESP_KEYBOARD_READY && pair[1] != 0) {
                let _ = ready.send(());
            }
            if body[0] == FRAME_REQUEST {
                let id = u64::from_be_bytes(body[1..9].try_into().expect("8 bytes"));
                let answer = frame(FRAME_RESPONSE, Some(id), &[]);
                if writer.lock().unwrap_or_else(|e| e.into_inner()).write_all(&answer).is_err() {
                    break;
                }
            }
        }
    }
    alive.store(false, std::sync::atomic::Ordering::Relaxed);
}

impl Karabiner {
    fn connect() -> Result<Self> {
        let stream = UnixStream::connect(KV_SOCKET).context("Karabiner's virtual HID daemon")?;
        let reader = stream.try_clone()?;
        let writer = Arc::new(Mutex::new(stream));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (ready_tx, ready) = std::sync::mpsc::channel();
        {
            let (writer, alive) = (writer.clone(), alive.clone());
            std::thread::spawn(move || read_frames(reader, writer, ready_tx, alive));
        }
        let mut kv = Self { writer, next_id: 1, alive };
        // vendor id, product id, country code (u64 each): its defaults, no country.
        let mut params = Vec::with_capacity(24);
        for v in [0x16c0u64, 0x27db, 0] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        kv.request(REQ_KEYBOARD_INITIALIZE, &params)?;
        if ready.recv_timeout(Duration::from_secs(10)).is_err() {
            bail!("the virtual HID keyboard did not get ready (driver extension not allowed?)");
        }
        info!("virtual HID keyboard ready");
        Ok(kv)
    }

    fn send(&self, bytes: &[u8]) -> Result<()> {
        if !self.alive.load(std::sync::atomic::Ordering::Relaxed) {
            bail!("connection with Karabiner's daemon closed");
        }
        self.writer.lock().unwrap_or_else(|e| e.into_inner()).write_all(bytes)?;
        Ok(())
    }

    fn request(&mut self, request: u8, payload: &[u8]) -> Result<()> {
        let mut data = Vec::with_capacity(3 + payload.len());
        data.extend_from_slice(&KV_PROTOCOL.to_le_bytes());
        data.push(request);
        data.extend_from_slice(payload);
        let id = self.next_id;
        self.next_id += 1;
        self.send(&frame(FRAME_REQUEST, Some(id), &data))
    }

    fn post(&mut self, report: &[u8; REPORT_SIZE]) -> Result<()> {
        self.request(REQ_POST_KEYBOARD, report)
    }

    fn heartbeat(&mut self) -> Result<()> {
        self.send(&frame(FRAME_HEARTBEAT, None, &[]))
    }
}

/// A Karabiner daemon is running (ours, Karabiner-Elements', an earlier one).
fn daemon_running() -> bool {
    Command::new("/usr/bin/pgrep").args(["-x", "Karabiner-VirtualHIDDevice-Daemon"]).output().is_ok_and(|o| o.status.success())
}

/// Karabiner's daemon: the one already running, or one started by us. Never
/// probed by connecting: a connection closed at once makes its server close and
/// bind again a second later (seen with 8.7.0), so only the socket file is looked at.
fn ensure_daemon(child: &mut Option<Child>) -> Result<()> {
    let ours_alive = child.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_none());
    if !ours_alive && !daemon_running() {
        if !Path::new(KV_DAEMON).exists() {
            bail!("Karabiner-DriverKit-VirtualHIDDevice is not installed");
        }
        *child = Some(Command::new(KV_DAEMON).spawn().context("starting Karabiner's daemon")?);
        info!("Karabiner's virtual HID daemon started");
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if Path::new(KV_SOCKET).exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("Karabiner's daemon did not open its socket")
}

// ---------------------------------------------------------------- the agent's side

/// The program of the process at the other end of a Unix socket.
fn peer_program(stream: &UnixStream) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 2;
    unsafe extern "C" {
        fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, size: u32) -> libc::c_int;
    }
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: a valid socket and an out value of the right size.
    let ok = unsafe {
        libc::getsockopt(stream.as_raw_fd(), SOL_LOCAL, LOCAL_PEERPID, (&raw mut pid).cast(), &mut len)
    } == 0;
    if !ok || pid <= 0 {
        return None;
    }
    let mut buf = vec![0u8; 4096];
    // SAFETY: buffer of the given size.
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).into_owned()))
}

/// Opens the virtual keyboard if it is not open.
fn open_keyboard(keyboard: &mut Option<Karabiner>, daemon: &Mutex<Option<Child>>) -> Result<()> {
    if keyboard.is_none() {
        ensure_daemon(&mut daemon.lock().unwrap_or_else(|e| e.into_inner()))?;
        *keyboard = Some(Karabiner::connect()?);
    }
    Ok(())
}

/// Least time between two reports: queued while the keyboard was being opened,
/// they came out in one burst and macOS lost keys.
const REPORT_SPACING: Duration = Duration::from_millis(12);

fn serve_agent(mut stream: UnixStream, keyboard: &Mutex<Option<Karabiner>>, daemon: &Mutex<Option<Child>>) {
    // The agent connects when the screen locks: the keyboard is opened now, so
    // that the first keys find it ready.
    if let Err(e) = open_keyboard(&mut keyboard.lock().unwrap_or_else(|e| e.into_inner()), daemon) {
        warn!("virtual HID keyboard not available: {e:#}");
    }
    let mut report = [0u8; REPORT_SIZE];
    // After a failure the next attempt waits a little (a burst of connections
    // disturbs Karabiner's daemon).
    let mut retry_at = Instant::now();
    let mut last_post = Instant::now();
    loop {
        if stream.read_exact(&mut report).is_err() {
            break;
        }
        if report[0] != 1 {
            warn!("invalid keyboard report from the agent: connection closed");
            break;
        }
        let mut kv = keyboard.lock().unwrap_or_else(|e| e.into_inner());
        if kv.is_none() {
            if Instant::now() < retry_at {
                continue;
            }
            match open_keyboard(&mut kv, daemon) {
                Ok(()) => {}
                Err(e) => {
                    warn!("virtual HID keyboard not available: {e:#}");
                    retry_at = Instant::now() + Duration::from_secs(2);
                    continue;
                }
            }
        }
        let since = last_post.elapsed();
        if since < REPORT_SPACING {
            std::thread::sleep(REPORT_SPACING - since);
        }
        last_post = Instant::now();
        if let Some(k) = kv.as_mut()
            && let Err(e) = k.post(&report)
        {
            warn!("virtual HID keyboard: {e:#}");
            *kv = None;
        }
    }
    // Nothing left held down.
    let mut empty = [0u8; REPORT_SIZE];
    empty[0] = 1;
    if let Some(k) = keyboard.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = k.post(&empty);
    }
}

/// `nuvira service hid-run` (LaunchDaemon, root).
pub fn run() -> Result<()> {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!("hid-run is started by launchd as root");
    }
    let _ = tracing_subscriber::fmt().with_ansi(false).try_init();
    let exe = std::env::current_exe().context("program path")?.canonicalize()?;
    let _ = std::fs::remove_file(SOCKET);
    let listener = UnixListener::bind(SOCKET).context(SOCKET)?;
    // Any user's agent may connect; the program is checked on every connection.
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))?;
    info!("waiting for the agent on {SOCKET}");
    let keyboard = Arc::new(Mutex::new(None::<Karabiner>));
    let daemon = Arc::new(Mutex::new(None::<Child>));
    // The daemon closes a connection silent for 30 s.
    let beat = keyboard.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let mut kv = beat.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(k) = kv.as_mut()
                && let Err(e) = k.heartbeat()
            {
                info!("virtual HID keyboard connection closed: {e:#}");
                *kv = None;
            }
        }
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let program = peer_program(&stream);
        if program.as_deref().and_then(|p| p.canonicalize().ok()).as_deref() != Some(exe.as_path()) {
            warn!(?program, "connection refused: not NuviraHub's agent");
            continue;
        }
        info!("agent connected");
        let (keyboard, daemon) = (keyboard.clone(), daemon.clone());
        std::thread::spawn(move || serve_agent(stream, &keyboard, &daemon));
    }
    Ok(())
}

// ---------------------------------------------------------------- install, remove

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("/bin/launchctl").args(args).output().context("launchctl")
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// As root: Karabiner's package (unless a Karabiner is already there) and our daemon.
pub fn install(exe: &Path) -> Result<()> {
    if !Path::new(KV_DAEMON).exists() {
        let pkg = exe
            .parent()
            .and_then(Path::parent)
            .map(|contents| contents.join("Resources/Karabiner-DriverKit-VirtualHIDDevice.pkg"))
            .context("app bundle")?;
        if !pkg.exists() {
            bail!("{} missing from the app", pkg.display());
        }
        let out = Command::new("/usr/sbin/installer").arg("-pkg").arg(&pkg).args(["-target", "/"]).output()?;
        if !out.status.success() {
            bail!("installing the virtual HID keyboard: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        info!("Karabiner-DriverKit-VirtualHIDDevice installed");
    }
    if let Some(logs) = Path::new(LOG).parent() {
        std::fs::create_dir_all(logs)?;
    }
    // The keyboard's type, as Karabiner-Elements writes it: without it macOS opens
    // the Keyboard Setup Assistant at the virtual keyboard's first key. Key:
    // "productID-vendorID-countryCode" (0x27db, 0x16c0, 0); 40 = ANSI.
    let _ = Command::new("/usr/bin/defaults")
        .args(["write", "/Library/Preferences/com.apple.keyboardtype", "keyboardtype", "-dict-add", "10203-5824-0", "-int", "40"])
        .output();
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t\t<string>service</string>\n\t\t<string>hid-run</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{LOG}</string>\n\
         \t<key>StandardOutPath</key>\n\t<string>{LOG}</string>\n\
         </dict>\n</plist>\n",
        exe = xml(&exe.display().to_string()),
    );
    std::fs::write(plist_path(), plist)?;
    std::fs::set_permissions(plist_path(), std::fs::Permissions::from_mode(0o644))?;
    let _ = launchctl(&["bootout", &format!("system/{LABEL}")]);
    let plist = plist_path().display().to_string();
    let mut last = String::new();
    for _ in 0..10 {
        let out = launchctl(&["bootstrap", "system", &plist])?;
        if out.status.success() {
            info!("virtual HID keyboard service installed");
            return Ok(());
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("launchctl bootstrap {plist}: {last}")
}

/// Karabiner-Elements is on this Mac: it uses the same driver, which then stays.
fn karabiner_elements() -> bool {
    Path::new("/Applications/Karabiner-Elements.app").exists()
        || Path::new("/Library/Application Support/org.pqrs/Karabiner-Elements").exists()
}

/// As the person (the app, before the administrator prompt of the removal):
/// unloads the driver extension that the removal will delete; root cannot.
/// macOS asks for the administrator's password for it (a window of its own,
/// answered within ~2 minutes on the Mac mini: stopping the manager earlier
/// cancelled the request) and finishes it at the next restart.
pub fn deactivate() {
    if !Path::new(KV_MANAGER).exists() || karabiner_elements() {
        return;
    }
    let Ok(mut child) = Command::new(KV_MANAGER).arg("deactivate").spawn() else { return };
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            info!(%status, "virtual HID keyboard driver deactivated (gone at the next restart)");
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    warn!("the driver extension was not deactivated in time: it stays until removed in System Settings");
    let _ = child.kill();
}

/// As root: our daemon, and Karabiner's driver unless Karabiner-Elements uses it.
pub fn uninstall() -> Result<()> {
    let _ = launchctl(&["bootout", &format!("system/{LABEL}")]);
    let _ = std::fs::remove_file(plist_path());
    let _ = std::fs::remove_file(SOCKET);
    if Path::new(KV_DAEMON).exists() && !karabiner_elements() {
        let _ = Command::new("/usr/bin/killall").arg("Karabiner-VirtualHIDDevice-Daemon").output();
        let script = format!("{KV_ROOT}/scripts/uninstall/remove_files.sh");
        let out = Command::new("/bin/bash").arg(&script).output()?;
        if !out.status.success() {
            warn!("{script}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        info!("Karabiner-DriverKit-VirtualHIDDevice removed");
    }
    Ok(())
}

/// As the person (the app, after the administrator prompt): asks macOS to load
/// the driver extension. The first time macOS wants it allowed in System Settings
/// → General → Login Items & Extensions → Driver Extensions: `Ok(false)`, and the
/// page is opened.
pub fn activate() -> Result<bool> {
    if !Path::new(KV_MANAGER).exists() {
        return Ok(true);
    }
    // The manager waits for the approval: started and left alone.
    let mut child = Command::new(KV_MANAGER).arg("activate").spawn().context("activating the driver")?;
    std::thread::sleep(Duration::from_secs(3));
    if driver_enabled() {
        let _ = child.kill();
        return Ok(true);
    }
    let _ = Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.LoginItems-Settings.extension")
        .status();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(false)
}

/// Installed for the whole Mac: asks macOS to load the driver extension, without
/// opening System Settings. Allowed beforehand by the organisation's profile
/// (MDM, system extensions) it loads by itself; otherwise it waits for someone
/// to allow it there, as with the person's own installation.
pub fn activate_quietly() {
    if !Path::new(KV_MANAGER).exists() || driver_enabled() {
        return;
    }
    match Command::new(KV_MANAGER).arg("activate").spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!("virtual keyboard driver not activated: {e}"),
    }
}

/// Karabiner's driver extension is allowed and running.
pub fn driver_enabled() -> bool {
    Command::new("/usr/bin/systemextensionsctl").arg("list").output().is_ok_and(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.contains("org.pqrs.Karabiner-DriverKit-VirtualHIDDevice") && l.contains("[activated enabled]"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_as_pqrs_writes_them() {
        assert_eq!(frame(FRAME_HEARTBEAT, None, &[]), [0, 0, 0, 1, 0]);
        let f = frame(FRAME_REQUEST, Some(2), &[7, 0, 6]);
        assert_eq!(&f[..5], &[0, 0, 0, 12, 4]);
        assert_eq!(&f[5..13], &2u64.to_be_bytes());
        assert_eq!(&f[13..], &[7, 0, 6]);
    }
}
