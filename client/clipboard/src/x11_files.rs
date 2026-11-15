//! Linux: the other side's files on the X11 clipboard (CLIPBOARD; on GNOME and
//! KDE with Wayland through XWayland, like the text). File managers ask for
//! them as `x-special/gnome-copied-files` (Nautilus, Nemo, Caja) or
//! `text/uri-list` (Dolphin, Thunar) when pasting. The answer names them in
//! a FUSE file system of ours (`fuse_files`), at once; their contents come
//! while the file manager copies them. Without FUSE the files are fetched into
//! a folder first and the answer names them there (fine on X11 with GTK, too
//! slow for Mutter's 15 s and Qt's 5 s limits on a large paste).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::share::{RemoteFiles, fetch_to_file};

enum Command {
    /// The files, and their paths in the FUSE file system when mounted.
    Set(RemoteFiles, Option<Vec<PathBuf>>),
}

/// The thread that owns the other side's files on the clipboard. Dropping it
/// lets the clipboard go and removes the files fetched for pastes.
pub struct X11Offer {
    commands: Sender<Command>,
    conn: std::sync::Arc<RustConnection>,
    window: u32,
    wake: u32,
    root: PathBuf,
    /// The clipboard is still this offer's (nothing else copied since).
    owned: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Mounted with the first files offered (`None` inside: FUSE not available).
    fuse: std::sync::OnceLock<Option<crate::fuse_files::Mounted>>,
    copies: std::sync::atomic::AtomicU32,
}

impl X11Offer {
    pub fn spawn() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("no X11 display for the clipboard's files")?;
        let conn = std::sync::Arc::new(conn);
        let screen = &conn.setup().roots[screen];
        let window = conn.generate_id()?;
        conn.create_window(
            0,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let wake = conn.intern_atom(false, b"NUVIRA_WAKE")?.reply()?.atom;
        conn.flush()?;
        let root = paste_root();
        let (commands, rx) = channel();
        let owned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (thread_conn, thread_root, thread_owned) = (conn.clone(), root.clone(), owned.clone());
        std::thread::Builder::new().name("clipboard-files".into()).spawn(move || {
            if let Err(e) = run(&thread_conn, window, wake, &thread_root, rx, &thread_owned) {
                warn!("files on the clipboard: {e:#}");
            }
            thread_owned.store(false, std::sync::atomic::Ordering::Relaxed);
        })?;
        Ok(Self { commands, conn, window, wake, root, owned, fuse: Default::default(), copies: Default::default() })
    }

    /// Offers `files` on the clipboard, in place of what was there.
    pub fn set(&self, files: RemoteFiles) {
        let copy = self.copies.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let fuse = self.fuse.get_or_init(|| {
            crate::fuse_files::Mounted::mount(&crate::fuse_files::mount_point())
                .inspect_err(|e| info!("pasted files fetched before the paste is answered: {e:#}"))
                .ok()
        });
        let paths = fuse.as_ref().map(|fuse| fuse.add(copy, files.clone()));
        if self.commands.send(Command::Set(files, paths)).is_ok() {
            self.poke();
        }
    }

    /// The other side's files are still what the clipboard has: reading it here
    /// would ask for them (as a paste) and send them back.
    pub fn is_owner(&self) -> bool {
        self.owned.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Wakes the thread's event loop (it then reads the commands).
    fn poke(&self) {
        let event = ClientMessageEvent::new(32, self.window, self.wake, [0u32; 5]);
        let _ = self.conn.send_event(false, self.window, EventMask::NO_EVENT, event);
        let _ = self.conn.flush();
    }
}

impl Drop for X11Offer {
    fn drop(&mut self) {
        let (commands, _) = channel();
        drop(std::mem::replace(&mut self.commands, commands));
        self.poke();
        // Pastes already copied the files; what is left here was only for them.
        drop(self.fuse.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Where pasted files are fetched: on disk (the runtime folder is a small
/// tmpfs, and so is /tmp on some systems), only for this user, one folder per
/// process; folders left by processes no longer running are removed.
fn paste_root() -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let base = cache.join("nuvira").join("paste");
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&base);
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let alive = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
            if !alive {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    base.join(std::process::id().to_string())
}

struct Atoms {
    clipboard: u32,
    targets: u32,
    timestamp: u32,
    utf8: u32,
    uri_list: u32,
    gnome: u32,
    kde_cut: u32,
}

fn atoms(conn: &RustConnection) -> Result<Atoms> {
    let intern = |name: &[u8]| -> Result<u32> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
    Ok(Atoms {
        clipboard: intern(b"CLIPBOARD")?,
        targets: intern(b"TARGETS")?,
        timestamp: intern(b"TIMESTAMP")?,
        utf8: intern(b"UTF8_STRING")?,
        uri_list: intern(b"text/uri-list")?,
        gnome: intern(b"x-special/gnome-copied-files")?,
        kde_cut: intern(b"application/x-kde-cutselection")?,
    })
}

/// The files of one offer: in the FUSE file system, or fetched on the first paste.
struct Offer {
    files: RemoteFiles,
    mounted: Option<Vec<PathBuf>>,
    dir: PathBuf,
    /// The top-level items once fetched (what a paste names).
    fetched: Option<Vec<PathBuf>>,
    since: std::time::Instant,
}

/// File managers (Nautilus, Nemo) read the copied files as soon as the
/// clipboard changes, to know whether they were cut; a paste comes later, after
/// someone does it. Requests this soon after the offer are answered with where
/// the files would go, without fetching them.
const PROBE: std::time::Duration = std::time::Duration::from_millis(700);

fn run(
    conn: &RustConnection,
    window: u32,
    wake: u32,
    root: &Path,
    commands: Receiver<Command>,
    owned: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let atoms = atoms(conn)?;
    let mut offer: Option<Offer> = None;
    let mut count = 0u32;
    loop {
        let event = conn.wait_for_event()?;
        match event {
            Event::ClientMessage(m) if m.window == window && m.type_ == wake => loop {
                match commands.try_recv() {
                    Ok(Command::Set(files, mounted)) => {
                        count += 1;
                        let dir = root.join(count.to_string());
                        conn.set_selection_owner(window, atoms.clipboard, x11rb::CURRENT_TIME)?;
                        conn.flush()?;
                        let owner = conn.get_selection_owner(atoms.clipboard)?.reply()?.owner;
                        if owner == window {
                            debug!(entries = files.files.len(), "other side's files on the clipboard");
                            owned.store(true, Relaxed);
                            offer = Some(Offer { files, mounted, dir, fetched: None, since: std::time::Instant::now() });
                        } else {
                            warn!("files not put on the clipboard: another program keeps it");
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if conn.get_selection_owner(atoms.clipboard)?.reply()?.owner == window {
                            conn.set_selection_owner(x11rb::NONE, atoms.clipboard, x11rb::CURRENT_TIME)?;
                            conn.flush()?;
                        }
                        return Ok(());
                    }
                }
            },
            Event::SelectionClear(e) if e.selection == atoms.clipboard => {
                debug!("something else copied: the other side's files are off the clipboard");
                owned.store(false, Relaxed);
                offer = None;
            }
            Event::SelectionRequest(req) => {
                let answered = match offer.as_mut() {
                    Some(offer) if req.selection == atoms.clipboard => answer(conn, &atoms, &req, offer),
                    _ => Ok(false),
                };
                let property = match answered {
                    Ok(true) => req.property,
                    Ok(false) => x11rb::NONE,
                    Err(e) => {
                        warn!("paste of the other side's files: {e:#}");
                        x11rb::NONE
                    }
                };
                let notify = SelectionNotifyEvent {
                    response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property,
                };
                conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)?;
                conn.flush()?;
            }
            _ => {}
        }
    }
}

/// Writes the answer to one request on the requestor's property; `false` for
/// targets not offered.
fn answer(conn: &RustConnection, atoms: &Atoms, req: &SelectionRequestEvent, offer: &mut Offer) -> Result<bool> {
    let t = req.target;
    if t == atoms.targets {
        let list = [atoms.targets, atoms.timestamp, atoms.gnome, atoms.uri_list, atoms.utf8];
        conn.change_property32(PropMode::REPLACE, req.requestor, req.property, AtomEnum::ATOM, &list)?;
        return Ok(true);
    }
    if t == atoms.timestamp {
        conn.change_property32(PropMode::REPLACE, req.requestor, req.property, AtomEnum::INTEGER, &[0])?;
        return Ok(true);
    }
    if t == atoms.kde_cut {
        // A copy, not a cut.
        conn.change_property8(PropMode::REPLACE, req.requestor, req.property, t, b"0")?;
        return Ok(true);
    }
    if t != atoms.gnome && t != atoms.uri_list && t != atoms.utf8 {
        return Ok(false);
    }
    let probe = offer.fetched.is_none() && offer.since.elapsed() < PROBE;
    let planned;
    let paths = if let Some(mounted) = &offer.mounted {
        mounted
    } else if probe {
        debug!("clipboard read right after the offer: files not fetched");
        planned = top_level(&offer.files, &offer.dir);
        &planned
    } else {
        // Pasted: the files come now.
        if offer.fetched.is_none() {
            let start = std::time::Instant::now();
            offer.fetched = Some(fetch_all(&offer.files, &offer.dir)?);
            info!(entries = offer.files.files.len(), ms = start.elapsed().as_millis() as u64, "pasted files fetched");
        }
        offer.fetched.as_ref().expect("fetched")
    };
    let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
    let text = if t == atoms.gnome {
        format!("copy\n{}", uris.join("\n"))
    } else if t == atoms.uri_list {
        uris.iter().map(|u| format!("{u}\r\n")).collect()
    } else {
        paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")
    };
    conn.change_property8(PropMode::REPLACE, req.requestor, req.property, t, text.as_bytes())?;
    Ok(true)
}

/// Fetches every file of `files` into `dir` (folders as folders, times kept);
/// returns the top-level items.
pub(crate) fn fetch_all(files: &RemoteFiles, dir: &Path) -> Result<Vec<PathBuf>> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut top = Vec::new();
    for (index, file) in files.files.iter().enumerate() {
        let Some(rel) = crate::list::safe_relative(&file.path) else { continue };
        let path = dir.join(&rel);
        if file.dir {
            std::fs::create_dir_all(&path)?;
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            fetch_to_file(files, index as u32, &path).with_context(|| file.path.clone())?;
            if file.modified > 0
                && let Ok(f) = std::fs::File::options().write(true).open(&path)
            {
                let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(file.modified as u64);
                let _ = f.set_modified(time);
            }
        }
        if rel.components().count() == 1 {
            top.push(path);
        }
    }
    Ok(top)
}

/// Where the top-level items of `files` go in `dir`.
fn top_level(files: &RemoteFiles, dir: &Path) -> Vec<PathBuf> {
    files
        .files
        .iter()
        .filter_map(|f| crate::list::safe_relative(&f.path))
        .filter(|rel| rel.components().count() == 1)
        .map(|rel| dir.join(rel))
        .collect()
}

/// `file://` URI of a path, with the characters URIs reserve escaped.
pub(crate) fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_escape_spaces_and_accents() {
        assert_eq!(file_uri(Path::new("/tmp/a b/Unicodé.bin")), "file:///tmp/a%20b/Unicod%C3%A9.bin");
    }
}
