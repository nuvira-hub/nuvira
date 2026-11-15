//! Linux: the other side's files as a read-only FUSE file system, so a paste
//! gets their paths at once and their contents only while the file manager
//! reads them (as Windows' virtual files do; RustDesk does the same on Linux).
//!
//! Answering a paste only once everything was fetched does not work: GNOME's
//! Mutter cancels any clipboard transfer between XWayland and Wayland after
//! 15 seconds (`meta-selection.c`), and Qt waits 5 seconds for the X11
//! clipboard; a large folder takes longer.
//!
//! Mounted without root through `fusermount3` (the `fuser` crate, no libfuse)
//! when files are first offered, at `$XDG_RUNTIME_DIR/nuvira/paste/<pid>`: it
//! takes no space, and the runtime folder is emptied at sign-out, so a mount
//! left by a process that was killed does not stay in the home folder (it is
//! also undone at the next start). Each copy is a folder `<n>` there, holding
//! the copied items. The last few copies stay readable, so a paste still
//! running when something else is copied finishes.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyXattr, Request,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::share::{Chunk, IDLE, RemoteFiles, recv_timeout};

/// Copies kept readable (a paste of an earlier one may still be running).
const KEPT: usize = 4;
const TTL: Duration = Duration::from_secs(1);

/// Inode numbers: the root is 1; copy `n` has its folder at `n << 32` and its
/// entry `i` at `(n << 32) + 1 + i`.
fn copy_ino(copy: u32) -> u64 {
    (copy as u64) << 32
}

fn entry_ino(copy: u32, index: usize) -> u64 {
    copy_ino(copy) + 1 + index as u64
}

/// Which copy and which entry (`None`: the copy's folder) an inode is.
fn split(ino: u64) -> (u32, Option<usize>) {
    let copy = (ino >> 32) as u32;
    let low = ino & 0xFFFF_FFFF;
    (copy, low.checked_sub(1).map(|i| i as usize))
}

/// One copy: its files and, for each folder (`None`: the copy's own), what it holds.
struct Tree {
    files: RemoteFiles,
    names: Vec<String>,
    children: HashMap<Option<usize>, Vec<usize>>,
}

impl Tree {
    fn new(files: RemoteFiles) -> Self {
        let mut names = Vec::with_capacity(files.files.len());
        let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        let mut dirs: HashMap<PathBuf, usize> = HashMap::new();
        for (index, file) in files.files.iter().enumerate() {
            let rel = crate::list::safe_relative(&file.path);
            let name = rel
                .as_ref()
                .and_then(|r| r.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            names.push(name);
            let Some(rel) = rel else { continue };
            // Folders come before what they hold; an entry whose folder is
            // missing from the list is not shown.
            let parent = match rel.parent().filter(|p| !p.as_os_str().is_empty()) {
                None => None,
                Some(p) => match dirs.get(p) {
                    Some(&d) => Some(d),
                    None => continue,
                },
            };
            if file.dir {
                dirs.insert(rel.clone(), index);
            }
            children.entry(parent).or_default().push(index);
        }
        Self { files, names, children }
    }

    fn top(&self) -> impl Iterator<Item = &str> {
        self.children.get(&None).into_iter().flatten().map(|&i| self.names[i].as_str())
    }
}

/// What a read of an open file has: the stream from the other side and how far it got.
struct Reader {
    copy: u32,
    index: usize,
    pos: u64,
    rx: Option<mpsc::Receiver<Chunk>>,
    buf: Vec<u8>,
    done: bool,
}

struct State {
    trees: BTreeMap<u32, Tree>,
    open: HashMap<u64, Reader>,
    next_fh: u64,
}

struct Fs {
    state: Mutex<State>,
    uid: u32,
    gid: u32,
}

/// The mounted file system; unmounted when dropped.
pub(crate) struct Mounted {
    session: Option<fuser::BackgroundSession>,
    fs: std::sync::Arc<Fs>,
    point: PathBuf,
}

impl Mounted {
    /// Mounts an empty file system at `point` (created if needed).
    pub(crate) fn mount(point: &Path) -> Result<Self> {
        unmount_stale(point);
        std::fs::create_dir_all(point).with_context(|| format!("creating {}", point.display()))?;
        // SAFETY: plain libc calls without arguments.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let fs = std::sync::Arc::new(Fs {
            state: Mutex::new(State { trees: BTreeMap::new(), open: HashMap::new(), next_fh: 1 }),
            uid,
            gid,
        });
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            fuser::MountOption::FSName("nuvira".into()),
            fuser::MountOption::Subtype("nuvira".into()),
            fuser::MountOption::RO,
            fuser::MountOption::NoExec,
            fuser::MountOption::DefaultPermissions,
        ];
        let session = fuser::spawn_mount(Shared(fs.clone()), point, &config)
            .with_context(|| format!("mounting the pasted files at {}", point.display()))?;
        debug!(point = %point.display(), "pasted files mounted");
        Ok(Self { session: Some(session), fs, point: point.to_path_buf() })
    }

    /// Shows copy `copy`; returns the paths of its top-level items.
    pub(crate) fn add(&self, copy: u32, files: RemoteFiles) -> Vec<PathBuf> {
        let tree = Tree::new(files);
        let dir = self.point.join(copy.to_string());
        let top = tree.top().map(|n| dir.join(n)).collect();
        let mut state = self.fs.state.lock().expect("fuse state");
        state.trees.insert(copy, tree);
        while state.trees.len() > KEPT {
            let oldest = *state.trees.keys().next().expect("not empty");
            state.trees.remove(&oldest);
            state.open.retain(|_, r| r.copy != oldest);
        }
        top
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(session) = self.session.take()
            && let Err(e) = session.umount_and_join()
        {
            warn!("pasted files not unmounted: {e}");
        }
        let _ = std::fs::remove_dir(&self.point);
    }
}

/// Where this process mounts the pasted files, after undoing the mounts that
/// processes no longer running left (and removing their folders).
pub(crate) fn mount_point() -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    // SAFETY: getuid has no arguments and cannot fail.
    let uid = unsafe { libc::getuid() };
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("nuvira-{uid}")))
        .join("nuvira")
        .join("paste");
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&base);
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let alive = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
            if !alive {
                unmount_stale(&entry.path());
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }
    let own = base.join(std::process::id().to_string());
    unmount_stale(&own);
    own
}

/// A mount left by a process that ended without unmounting (its folder cannot
/// even be listed until then).
pub(crate) fn unmount_stale(point: &Path) {
    let mounted = std::fs::read_to_string("/proc/self/mounts")
        .is_ok_and(|m| m.lines().any(|l| l.split(' ').nth(1).is_some_and(|p| Path::new(p) == point)));
    if mounted {
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(point)
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// `fuser` takes the file system by value; the mount and this share it.
struct Shared(std::sync::Arc<Fs>);

impl Fs {
    fn attr(&self, ino: u64, state: &State) -> Option<FileAttr> {
        let (copy, entry) = split(ino);
        let (kind, size, mtime) = if ino == INodeNo::ROOT.0 {
            (FileType::Directory, 0, SystemTime::now())
        } else {
            let tree = state.trees.get(&copy)?;
            match entry {
                None => (FileType::Directory, 0, SystemTime::now()),
                Some(i) => {
                    let f = tree.files.files.get(i)?;
                    let mtime = UNIX_EPOCH + Duration::from_secs(f.modified.max(0) as u64);
                    if f.dir { (FileType::Directory, 0, mtime) } else { (FileType::RegularFile, f.size, mtime) }
                }
            }
        };
        let dir = kind == FileType::Directory;
        Some(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm: if dir { 0o755 } else { 0o644 },
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 128 * 1024,
            flags: 0,
        })
    }

    /// What folder `ino` holds, as (inode, kind, name).
    fn list(&self, ino: u64, state: &State) -> Option<Vec<(u64, FileType, String)>> {
        if ino == INodeNo::ROOT.0 {
            return Some(state.trees.keys().map(|&c| (copy_ino(c), FileType::Directory, c.to_string())).collect());
        }
        let (copy, entry) = split(ino);
        let tree = state.trees.get(&copy)?;
        let kids = tree.children.get(&entry).map(Vec::as_slice).unwrap_or_default();
        Some(
            kids.iter()
                .map(|&i| {
                    let kind = if tree.files.files[i].dir { FileType::Directory } else { FileType::RegularFile };
                    (entry_ino(copy, i), kind, tree.names[i].clone())
                })
                .collect(),
        )
    }

    /// Up to `size` bytes at `offset`, from the other side.
    fn read_at(&self, fh: u64, offset: u64, size: usize) -> Result<Vec<u8>, Errno> {
        // The open file's reader is taken out while it waits for data, so other
        // requests (another file's, a listing) are not held by the lock.
        let (mut reader, files) = {
            let mut state = self.state.lock().expect("fuse state");
            let reader = state.open.remove(&fh).ok_or(Errno::EBADF)?;
            let files = state.trees.get(&reader.copy).map(|t| t.files.clone());
            (reader, files)
        };
        let result = (|| {
            let files = files.ok_or(Errno::ENOENT)?;
            if offset != reader.pos {
                // Not where the stream is: start again from there.
                reader.rx = None;
                reader.buf.clear();
                reader.done = false;
                reader.pos = offset;
            }
            if reader.rx.is_none() && !reader.done {
                reader.rx = Some(files.fetch(reader.index as u32, offset));
            }
            while reader.buf.len() < size && !reader.done {
                let rx = reader.rx.as_mut().expect("stream open");
                match recv_timeout(rx, IDLE) {
                    Ok(Some(Ok(data))) => reader.buf.extend_from_slice(&data),
                    Ok(None) => reader.done = true,
                    Ok(Some(Err(e))) => {
                        warn!("pasted file not readable: {e}");
                        return Err(Errno::EIO);
                    }
                    Err(e) => {
                        warn!("pasted file not received: {e:#}");
                        return Err(Errno::EIO);
                    }
                }
            }
            let n = size.min(reader.buf.len());
            let out: Vec<u8> = reader.buf.drain(..n).collect();
            reader.pos += n as u64;
            Ok(out)
        })();
        if result.is_err() {
            reader.rx = None;
            reader.buf.clear();
            reader.done = false;
        }
        self.state.lock().expect("fuse state").open.insert(fh, reader);
        result
    }
}

impl fuser::Filesystem for Shared {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let fs = &self.0;
        let state = fs.state.lock().expect("fuse state");
        let found = fs
            .list(parent.0, &state)
            .and_then(|kids| kids.into_iter().find(|(_, _, n)| OsStr::new(n) == name))
            .and_then(|(ino, _, _)| fs.attr(ino, &state));
        match found {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let fs = &self.0;
        let state = fs.state.lock().expect("fuse state");
        match fs.attr(ino.0, &state) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let fs = &self.0;
        let state = fs.state.lock().expect("fuse state");
        let Some(kids) = fs.list(ino.0, &state) else { return reply.error(Errno::ENOENT) };
        let mut all = vec![(ino.0, FileType::Directory, ".".to_string()), (INodeNo::ROOT.0, FileType::Directory, "..".to_string())];
        all.extend(kids);
        for (i, (child, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let fs = &self.0;
        let mut state = fs.state.lock().expect("fuse state");
        let (copy, entry) = split(ino.0);
        let file = entry.and_then(|i| state.trees.get(&copy)?.files.files.get(i).map(|f| (i, f.dir)));
        match file {
            Some((index, false)) => {
                let fh = state.next_fh;
                state.next_fh += 1;
                state.open.insert(fh, Reader { copy, index, pos: 0, rx: None, buf: Vec::new(), done: false });
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Some((_, true)) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.0.read_at(fh.0, offset, size as usize) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    /// No extended attributes (`cp -a` and file managers ask).
    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 { reply.size(0) } else { reply.data(&[]) }
    }

    fn getxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::NO_XATTR);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.0.state.lock().expect("fuse state").open.remove(&fh.0);
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuvira_proto::v1::ClipboardFile;

    fn file(path: &str, size: u64, dir: bool) -> ClipboardFile {
        ClipboardFile { path: path.into(), size, dir, modified: 1_700_000_000 }
    }

    #[test]
    fn inodes_round_trip() {
        assert_eq!(split(copy_ino(3)), (3, None));
        assert_eq!(split(entry_ino(3, 0)), (3, Some(0)));
        assert_eq!(split(entry_ino(7, 41)), (7, Some(41)));
    }

    #[test]
    fn tree_follows_folders() {
        let files = vec![
            file("a", 0, true),
            file("a/b", 0, true),
            file("a/b/c.bin", 5, false),
            file("top.txt", 3, false),
            file("missing/x", 1, false),
        ];
        let tree = Tree::new(RemoteFiles::new(files, |_, _| mpsc::channel(1).1));
        assert_eq!(tree.top().collect::<Vec<_>>(), ["a", "top.txt"]);
        assert_eq!(tree.children[&Some(0)], [1]);
        assert_eq!(tree.children[&Some(1)], [2]);
        assert!(!tree.children.values().flatten().any(|&i| i == 4));
    }
}
