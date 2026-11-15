//! macOS: the other side's files on the pasteboard, fetched when Finder pastes them.
//!
//! Finder does not paste file promises (`NSFilePromiseProvider` is for drag and
//! drop only), and a pasteboard data provider must hand its data over on the
//! main thread, synchronously, while Finder waits. So, as RustDesk does, the
//! pasteboard gets real files: empty placeholders with the copied names, in a
//! folder of ours. Finder pastes them as it would any file (with its own "Keep
//! both / Replace" questions), and an FSEvents stream on the home folder and
//! `/Volumes` spots the copies: each placeholder carries an extended attribute
//! (`com.nuvirahub.paste`, a token of this offer and the entry's index) that
//! Finder copies with it. The pasted placeholder is then filled with the
//! file's contents, or, for a folder, with what it holds; the attribute is
//! removed at the end. Pasting again elsewhere fills that copy too.
//!
//! Nothing here needs the main thread or a run loop: the pasteboard is written
//! with the file URLs at once, and the events come on a dispatch queue.

use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use tracing::{debug, info, warn};

use crate::share::{RemoteFiles, fetch_to_file};

/// The extended attribute that marks a placeholder (and Finder's copies of it).
const MARK: &str = "com.nuvirahub.paste";

// FSEvents (CoreServices), from FSEvents.h.
type StreamRef = *mut c_void;
type Callback = extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_IGNORE_SELF: u32 = 0x08;
const CREATE_FILE_EVENTS: u32 = 0x10;
const ITEM_CREATED: u32 = 0x100;
const ITEM_RENAMED: u32 = 0x800;
const ITEM_XATTR_MOD: u32 = 0x8000;
const ITEM_IS_FILE: u32 = 0x1_0000;
const ITEM_IS_DIR: u32 = 0x2_0000;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: Callback,
        context: *const StreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> StreamRef;
    fn FSEventStreamSetDispatchQueue(stream: StreamRef, queue: *mut c_void);
    fn FSEventStreamStart(stream: StreamRef) -> u8;
    fn FSEventStreamStop(stream: StreamRef);
    fn FSEventStreamInvalidate(stream: StreamRef);
    fn FSEventStreamRelease(stream: StreamRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
}

/// The files offered now.
struct Offer {
    token: String,
    files: RemoteFiles,
    /// Placeholders of this offer (not watched for: they are ours).
    dir: PathBuf,
}

#[derive(Default)]
struct Shared {
    offer: Option<Offer>,
    /// Pasted copies already being filled, by path and inode (the same copy
    /// gives several events; a copy pasted again over it is a new inode).
    filling: HashSet<(PathBuf, u64)>,
}

/// The pasteboard side of the other side's files, for the clipboard thread.
pub(crate) struct MacOffer {
    shared: Arc<Mutex<Shared>>,
    stream: StreamRef,
    queue: *mut c_void,
    /// Raw `Arc` given to the stream as its info.
    info: *const Mutex<Shared>,
    root: PathBuf,
    generation: u32,
}

// SAFETY: the stream and queue are only started and stopped from the owner;
// FSEvents calls back on its queue with `info`, an `Arc` kept alive until the
// stream is invalidated.
unsafe impl Send for MacOffer {}

impl MacOffer {
    pub(crate) fn spawn() -> Result<Self> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut watched = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            watched.push(CFString::new(&Path::new(&home).to_string_lossy()));
        }
        watched.push(CFString::new("/Volumes"));
        let paths = CFArray::from_CFTypes(&watched);
        let info = Arc::into_raw(shared.clone());
        let context = StreamContext {
            version: 0,
            info: info as *mut c_void,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        // SAFETY: FSEvents API as documented; the context is copied by the
        // call, the paths array retained by it. The queue is serial.
        let (stream, queue) = unsafe {
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                on_events,
                &context,
                paths.as_concrete_TypeRef() as *const c_void,
                SINCE_NOW,
                0.2,
                CREATE_NO_DEFER | CREATE_IGNORE_SELF | CREATE_FILE_EVENTS,
            );
            if stream.is_null() {
                drop(Arc::from_raw(info));
                anyhow::bail!("file events not available");
            }
            let queue = dispatch_queue_create(c"com.nuvirahub.paste".as_ptr(), std::ptr::null());
            FSEventStreamSetDispatchQueue(stream, queue);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
                drop(Arc::from_raw(info));
                anyhow::bail!("file events not started");
            }
            (stream, queue)
        };
        Ok(Self { shared, stream, queue, info, root: paste_root(), generation: 0 })
    }

    /// Placeholders for `files`, ready to go on the pasteboard: the top-level
    /// entries' paths.
    pub(crate) fn set(&mut self, files: RemoteFiles) -> Result<Vec<PathBuf>> {
        self.generation += 1;
        let dir = self.root.join(self.generation.to_string());
        let _ = std::fs::remove_dir_all(&self.root);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let token = new_token();
        let mut top = Vec::new();
        for (index, file) in files.files.iter().enumerate() {
            let Some(rel) = crate::list::safe_relative(&file.path) else { continue };
            if rel.components().count() != 1 {
                continue;
            }
            let path = dir.join(&rel);
            if file.dir {
                std::fs::create_dir(&path)?;
            } else {
                std::fs::File::create(&path)?;
            }
            set_mark(&path, &format!("{token}/{index}"))?;
            top.push(path);
        }
        debug!(entries = files.files.len(), top = top.len(), "files of the other side on the pasteboard");
        self.shared.lock().expect("paste state").offer = Some(Offer { token, files, dir });
        Ok(top)
    }
}

impl Drop for MacOffer {
    fn drop(&mut self) {
        // SAFETY: the stream made in spawn, stopped and released once; after
        // invalidation no callback runs, so the info Arc can go.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
            drop(Arc::from_raw(self.info));
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

extern "C" fn on_events(
    _stream: StreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    // SAFETY: info is the Arc<Mutex<Shared>> given in spawn, alive while the
    // stream runs; without kFSEventStreamCreateFlagUseCFTypes the paths are
    // `count` C strings and the flags `count` values.
    let (shared, paths, flags) = unsafe {
        let shared = &*(info as *const Mutex<Shared>);
        let paths = std::slice::from_raw_parts(paths as *const *const c_char, count);
        let flags = std::slice::from_raw_parts(flags, count);
        (shared, paths, flags)
    };
    for (&path, &flag) in paths.iter().zip(flags) {
        if flag & (ITEM_CREATED | ITEM_RENAMED | ITEM_XATTR_MOD) == 0 || flag & (ITEM_IS_FILE | ITEM_IS_DIR) == 0 {
            continue;
        }
        // SAFETY: a C string of the event, valid during the callback.
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(unsafe { CStr::from_ptr(path) }.to_bytes()));
        pasted(shared, path);
    }
}

/// A file created or changed: a copy of one of our placeholders gets filled.
fn pasted(shared: &Mutex<Shared>, path: PathBuf) {
    let Some(mark) = get_mark(&path) else { return };
    let mut state = shared.lock().expect("paste state");
    let Some(offer) = state.offer.as_ref() else { return };
    if path.starts_with(&offer.dir) {
        return;
    }
    let Some(index) = mark.strip_prefix(&format!("{}/", offer.token)).and_then(|i| i.parse::<usize>().ok()) else {
        // A placeholder of an earlier offer, pasted afterwards: nothing to fill it with.
        return;
    };
    let files = offer.files.clone();
    let Ok(meta) = std::fs::symlink_metadata(&path) else { return };
    if !state.filling.insert((path.clone(), std::os::unix::fs::MetadataExt::ino(&meta))) {
        return;
    }
    std::thread::spawn(move || {
        info!(path = %path.display(), "files of the other side pasted");
        match fill(&files, index, &path) {
            Ok(()) => info!(path = %path.display(), "pasted files received"),
            Err(e) => warn!(path = %path.display(), "pasted files not received: {e:#}"),
        }
        let _ = remove_mark(&path);
    });
}

/// Fills the pasted copy of entry `index` (a file, or a folder and all it holds).
fn fill(files: &RemoteFiles, index: usize, pasted: &Path) -> Result<()> {
    let entry = files.files.get(index).context("no such entry")?;
    let top = crate::list::safe_relative(&entry.path).context("bad name")?;
    if !entry.dir {
        return fetch_file(files, index, pasted);
    }
    for (i, file) in files.files.iter().enumerate() {
        let Some(rel) = crate::list::safe_relative(&file.path) else { continue };
        let Ok(inner) = rel.strip_prefix(&top) else { continue };
        if inner.as_os_str().is_empty() {
            continue;
        }
        let path = pasted.join(inner);
        if file.dir {
            std::fs::create_dir_all(&path)?;
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            fetch_file(files, i, &path).with_context(|| file.path.clone())?;
        }
    }
    Ok(())
}

/// One file, with its time; a file not received whole is not left behind.
fn fetch_file(files: &RemoteFiles, index: usize, path: &Path) -> Result<()> {
    if let Err(e) = fetch_to_file(files, index as u32, path) {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    let modified = files.files[index].modified;
    if modified > 0
        && let Ok(f) = std::fs::File::options().write(true).open(path)
    {
        let _ = f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified as u64));
    }
    Ok(())
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path with a NUL")
}

fn set_mark(path: &Path, value: &str) -> Result<()> {
    let p = c_path(path)?;
    let name = CString::new(MARK).expect("no NUL");
    // SAFETY: valid C strings and a buffer of the given length.
    let r = unsafe { libc::setxattr(p.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("marking {}", path.display()));
    }
    Ok(())
}

fn get_mark(path: &Path) -> Option<String> {
    let p = c_path(path).ok()?;
    let name = CString::new(MARK).expect("no NUL");
    let mut buf = [0u8; 128];
    // SAFETY: valid C strings and a buffer of the given length; XATTR_NOFOLLOW
    // so a link to a placeholder is not taken for one.
    let n = unsafe {
        libc::getxattr(p.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW)
    };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn remove_mark(path: &Path) -> Result<()> {
    let p = c_path(path)?;
    let name = CString::new(MARK).expect("no NUL");
    // SAFETY: valid C strings.
    unsafe { libc::removexattr(p.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) };
    Ok(())
}

/// A token telling this offer's placeholders from earlier ones.
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

/// `~/Library/Caches/com.nuvirahub.paste/<pid>`, removing what processes no
/// longer running left there.
fn paste_root() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library/Caches"))
        .unwrap_or_else(std::env::temp_dir)
        .join("com.nuvirahub.paste");
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let alive = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()).is_some_and(|pid| {
                // SAFETY: signal 0 only checks that the process exists.
                unsafe { libc::kill(pid, 0) == 0 || *libc::__error() == libc::EPERM }
            });
            if !alive {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    base.join(std::process::id().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_round_trip() {
        let dir = std::env::temp_dir().join(format!("nuvira-mark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a");
        std::fs::File::create(&file).unwrap();
        set_mark(&file, "t/3").unwrap();
        assert_eq!(get_mark(&file).as_deref(), Some("t/3"));
        remove_mark(&file).unwrap();
        assert_eq!(get_mark(&file), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
