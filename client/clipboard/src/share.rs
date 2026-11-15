//! Files copied on one side, pasted on the other. The list goes at once
//! (`ClipboardFiles`); a file's contents only when the other side pastes it
//! (`ClipboardFileRequest`, answered on a stream of `FileChunk`s), so what is
//! copied and never pasted elsewhere never leaves the computer.
//!
//! Requests name a file of the current list by its number: the other side can
//! read only what was copied here, nothing else.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::{ClipboardFile, ClipboardFileRequest, ClipboardFiles, FileChunk};
use nuvira_transport::{Connection, FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::list::Listed;

/// Bytes of a pasted file as they arrive; an `Err` says why it stopped.
pub type Chunk = Result<Vec<u8>, String>;

const CHUNK: usize = 256 * 1024;

type Fetch = dyn Fn(u32, u64) -> mpsc::Receiver<Chunk> + Send + Sync;

/// Files copied on the other side, as this side's clipboard offers them.
#[derive(Clone)]
pub struct RemoteFiles {
    pub files: Vec<ClipboardFile>,
    fetch: Arc<Fetch>,
}

impl RemoteFiles {
    pub fn new(files: Vec<ClipboardFile>, fetch: impl Fn(u32, u64) -> mpsc::Receiver<Chunk> + Send + Sync + 'static) -> Self {
        Self { files, fetch: Arc::new(fetch) }
    }

    /// No files: written to the clipboard, it takes the other side's files off
    /// it (when they are still what it has), e.g. once files are switched off.
    pub fn none() -> Self {
        Self::new(Vec::new(), |_, _| mpsc::channel(1).1)
    }

    /// The contents of file `index` from `offset` on. The channel ends after the
    /// last byte; it may also end early (session closed): the caller compares
    /// with the size.
    pub fn fetch(&self, index: u32, offset: u64) -> mpsc::Receiver<Chunk> {
        (self.fetch)(index, offset)
    }
}

impl std::fmt::Debug for RemoteFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteFiles({} entries)", self.files.len())
    }
}

impl PartialEq for RemoteFiles {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.fetch, &other.fetch)
    }
}

/// One session's file sharing through the clipboard (one per side).
pub struct Sharing {
    /// What was copied here last, with its number.
    own: Mutex<Option<(u32, Vec<Listed>)>>,
    next_list: AtomicU32,
    /// Pastes here waiting for the other side's data, by request id.
    pending: Mutex<HashMap<u32, mpsc::Sender<Chunk>>>,
    next_id: AtomicU32,
    requests: mpsc::UnboundedSender<ClipboardFileRequest>,
}

impl Sharing {
    /// The receiver gets the requests to send to the other side (control channel).
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<ClipboardFileRequest>) {
        let (requests, rx) = mpsc::unbounded_channel();
        let sharing = Self {
            own: Mutex::new(None),
            next_list: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            requests,
        };
        (Arc::new(sharing), rx)
    }

    /// Files copied here: kept to answer the other side's requests; returns the
    /// message that tells it.
    pub fn announce(&self, listed: Vec<Listed>) -> ClipboardFiles {
        let list = self.next_list.fetch_add(1, Ordering::Relaxed);
        let files = listed
            .iter()
            .map(|l| ClipboardFile { path: l.path.clone(), size: l.size, dir: l.dir, modified: l.modified })
            .collect();
        info!(list, entries = listed.len(), "files copied: offered to the other side");
        *self.own.lock().expect("own") = Some((list, listed));
        ClipboardFiles { list, files }
    }

    /// Something else was copied here, or sharing was switched off: the last
    /// files can no longer be read from the other side.
    pub fn forget(&self) {
        if self.own.lock().expect("own").take().is_some() {
            debug!("copied files no longer offered");
        }
    }

    /// The other side's files, for this side's clipboard.
    pub fn remote(self: &Arc<Self>, msg: ClipboardFiles) -> RemoteFiles {
        let weak: Weak<Self> = Arc::downgrade(self);
        let list = msg.list;
        RemoteFiles::new(msg.files, move |index, offset| {
            // A few chunks in flight: the network waits while the paste is slower.
            let (tx, rx) = mpsc::channel(16);
            if let Some(sharing) = weak.upgrade() {
                let id = sharing.next_id.fetch_add(1, Ordering::Relaxed);
                sharing.pending.lock().expect("pending").insert(id, tx);
                debug!(id, list, index, offset, "asking for a pasted file");
                let _ = sharing.requests.send(ClipboardFileRequest { id, list, index, offset });
            }
            rx
        })
    }

    /// A paste here that may not ask the other side (switched off meanwhile).
    pub fn refuse(&self, id: u32, reason: &str) {
        if let Some(tx) = self.pending.lock().expect("pending").remove(&id) {
            let _ = tx.try_send(Err(reason.to_string()));
        }
    }

    /// The file a request of the other side names, if it is one of this side's
    /// current list.
    pub fn requested(&self, req: &ClipboardFileRequest) -> Result<PathBuf> {
        let own = self.own.lock().expect("own");
        let Some((list, files)) = own.as_ref() else { bail!("nothing copied any more") };
        if *list != req.list {
            bail!("something else was copied in the meantime");
        }
        let Some(file) = files.get(req.index as usize) else { bail!("no such file") };
        if file.dir {
            bail!("a folder has no contents");
        }
        Ok(file.source.clone())
    }

    /// Answers a request with `opened` (the file, opened by the caller as the
    /// right user) or with the reason it could not be.
    pub async fn answer(conn: &Connection, req: &ClipboardFileRequest, opened: Result<std::fs::File>) -> Result<()> {
        let mut tx = FramedSend::new(conn.open_uni().await?);
        let header = FileChunk { clipboard_file: true, transfer_id: req.id, offset: req.offset, ..Default::default() };
        let file = match opened {
            Ok(file) => file,
            Err(e) => {
                tx.send(&FileChunk { error: format!("{e:#}"), ..header }).await?;
                tx.finish()?;
                return Ok(());
            }
        };
        let size = file.metadata()?.len();
        if req.offset > size {
            tx.send(&FileChunk { error: "beyond the end of the file".into(), ..header }).await?;
            tx.finish()?;
            return Ok(());
        }
        tx.send(&FileChunk { size, ..header }).await?;
        let mut file = tokio::fs::File::from_std(file);
        tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(req.offset)).await?;
        let mut buffer = vec![0u8; CHUNK];
        let mut sent = 0u64;
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await?;
            if n == 0 {
                break;
            }
            tx.send(&FileChunk { data: buffer[..n].to_vec(), ..Default::default() }).await?;
            sent += n as u64;
        }
        tx.finish()?;
        debug!(id = req.id, sent, "pasted file sent");
        Ok(())
    }

    /// A stream answering one of this side's requests: its data to the paste
    /// waiting for it. Returns when the stream ends or the paste no longer waits
    /// (dropping the stream stops the other side).
    pub async fn receive(&self, first: FileChunk, mut rx: FramedRecv) {
        let Some(tx) = self.pending.lock().expect("pending").remove(&first.transfer_id) else {
            debug!(id = first.transfer_id, "pasted file nobody waits for");
            return;
        };
        if !first.error.is_empty() {
            let _ = tx.send(Err(first.error)).await;
            return;
        }
        let expected = first.size.saturating_sub(first.offset);
        let mut got = 0u64;
        loop {
            match rx.recv::<FileChunk>().await {
                Ok(Some(chunk)) => {
                    got += chunk.data.len() as u64;
                    if got > expected {
                        let _ = tx.send(Err("more data than the file has".into())).await;
                        return;
                    }
                    if tx.send(Ok(chunk.data)).await.is_err() {
                        debug!(id = first.transfer_id, "paste cancelled");
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(format!("{e:#}"))).await;
                    return;
                }
            }
        }
        if got < expected {
            let _ = tx.send(Err("the transfer was interrupted".into())).await;
        }
    }
}

/// Reads a whole pasted file into `out` (the platforms that need it on disk
/// before they can offer it), checking it is all there.
pub fn fetch_to_file(files: &RemoteFiles, index: u32, out: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let size = files.files.get(index as usize).context("no such file")?.size;
    let mut rx = files.fetch(index, 0);
    let mut file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut got = 0u64;
    while let Some(chunk) = recv_timeout(&mut rx, IDLE)? {
        let data = chunk.map_err(anyhow::Error::msg)?;
        got += data.len() as u64;
        file.write_all(&data)?;
    }
    if got != size {
        bail!("{} of {size} bytes received", got);
    }
    Ok(())
}

/// How long a paste waits for the next data before it gives up.
pub const IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// The next chunk, waiting on a thread outside the runtime (the paste's), at
/// most `idle`. `Ok(None)` at the end of the channel.
pub fn recv_timeout(rx: &mut mpsc::Receiver<Chunk>, idle: std::time::Duration) -> Result<Option<Chunk>> {
    let start = std::time::Instant::now();
    loop {
        match rx.try_recv() {
            Ok(chunk) => return Ok(Some(chunk)),
            Err(mpsc::error::TryRecvError::Disconnected) => return Ok(None),
            Err(mpsc::error::TryRecvError::Empty) => {
                if start.elapsed() > idle {
                    bail!("no data from the other computer for {} s", idle.as_secs());
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}
