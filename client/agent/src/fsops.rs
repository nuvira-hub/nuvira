//! File manager on the controlled PC: listing folders, sending files to the viewer
//! and receiving them into a chosen folder, with resume (`FsRequest`, `FileChunk`).
//!
//! As SYSTEM (the service's agent) every operation runs as the user signed in on the
//! PC: the thread impersonates the session user's token, so the person in control
//! sees and writes only what that user could, not the whole system. Without a user
//! signed in the file manager is not available. As a normal user (the app's agent)
//! the operations simply run as that user.

use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::{FileChunk, FsEntry, FsResponse, FsUploadOffset, fs_request};
use nuvira_transport::{Connection, FramedRecv, FramedSend};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::files::{safe_name, unique_path};

/// At most this many entries per folder (the rest is left out).
const MAX_ENTRIES: usize = 20_000;
const CHUNK: usize = 256 * 1024;

/// Answers a request. A download is checked here (file readable by the user) and
/// then sent on its own stream.
pub async fn handle(id: u32, op: Option<fs_request::Op>, conn: Connection) -> FsResponse {
    let result = match op {
        Some(fs_request::Op::List(path)) => as_user(move || list(&path))
            .await
            .map(|(path, entries)| FsResponse { path, entries, ..Default::default() }),
        Some(fs_request::Op::UploadOffset(FsUploadOffset { dir, name })) => {
            let part = Path::new(&dir).join(format!("{}.part", safe_name(&name)));
            as_user(move || Ok(std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0)))
                .await
                .map(|offset| FsResponse { offset, ..Default::default() })
        }
        Some(fs_request::Op::Download(download)) => {
            let path = PathBuf::from(&download.path);
            let opened = {
                let path = path.clone();
                as_user(move || {
                    let file =
                        std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
                    let size = file.metadata()?.len();
                    Ok((file, size))
                })
                .await
            };
            opened.map(|(file, size)| {
                tokio::spawn(async move {
                    if let Err(e) = send(conn, id, &path, file, size, download.offset).await {
                        warn!("sending {}: {e:#}", path.display());
                    }
                });
                FsResponse::default()
            })
        }
        None => Err(anyhow::anyhow!("empty request")),
    };
    match result {
        Ok(response) => FsResponse { id, ..response },
        Err(e) => FsResponse { id, error: format!("{e:#}"), ..Default::default() },
    }
}

/// Sends `file` from `offset` on: header (name, total size, offset, id) then data.
async fn send(
    conn: Connection,
    id: u32,
    path: &Path,
    mut file: std::fs::File,
    size: u64,
    offset: u64,
) -> Result<()> {
    if offset > size {
        bail!("resume point beyond the end of the file");
    }
    file.seek(SeekFrom::Start(offset))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut tx = FramedSend::new(conn.open_uni().await?);
    tx.send(&FileChunk { name: name.clone(), size, offset, transfer_id: id, ..Default::default() }).await?;
    // Reading a local file is quick: done in blocks between the sends.
    let mut file = tokio::fs::File::from_std(file);
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await?;
        if n == 0 {
            break;
        }
        tx.send(&FileChunk { data: buffer[..n].to_vec(), ..Default::default() }).await?;
    }
    tx.finish()?;
    info!("sent {name} ({} bytes from {offset})", size - offset);
    crate::stats::file_out(size - offset);
    Ok(())
}

/// Receives an upload into the folder in its header: `name.part` (continued from
/// `offset` if an earlier attempt left it), renamed once complete. Returns the final path.
pub async fn receive(first: FileChunk, mut rx: FramedRecv) -> Result<PathBuf> {
    let name = safe_name(&first.name);
    let dir = PathBuf::from(&first.dir);
    let part = dir.join(format!("{name}.part"));
    let (offset, size) = (first.offset, first.size);
    if offset > size {
        bail!("resume point beyond the end of the file");
    }
    let file = {
        let part = part.clone();
        as_user(move || {
            if offset == 0 {
                return std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()));
            }
            let file = std::fs::OpenOptions::new().append(true).open(&part).context("nothing to resume")?;
            let have = file.metadata()?.len();
            if have != offset {
                bail!("cannot resume: {have} bytes there, not {offset}");
            }
            Ok(file)
        })
        .await?
    };
    let mut file = tokio::fs::File::from_std(file);
    let mut written = offset;
    file.write_all(&first.data).await?;
    written += first.data.len() as u64;
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        file.write_all(&chunk.data).await?;
        written += chunk.data.len() as u64;
        if written > size {
            bail!("more data than announced");
        }
    }
    file.flush().await?;
    drop(file);
    if written != size {
        // The part stays: the transfer can be resumed from here.
        bail!("interrupted at {written} bytes out of {size}");
    }
    let target = as_user(move || {
        let target = unique_path(&dir, &name);
        std::fs::rename(&part, &target).with_context(|| format!("saving {}", target.display()))?;
        Ok(target)
    })
    .await?;
    info!("file received: {} ({size} bytes, from {offset})", target.display());
    crate::stats::file_in(size - offset);
    Ok(target)
}

/// Contents of `path`, folders first; "" = drives and known folders.
fn list(path: &str) -> Result<(String, Vec<FsEntry>)> {
    if path.is_empty() {
        return Ok((String::new(), roots()));
    }
    let mut entries = Vec::new();
    for item in
        std::fs::read_dir(path).with_context(|| format!("reading {path}"))?.flatten().take(MAX_ENTRIES)
    {
        let Ok(meta) = item.metadata() else { continue };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        entries.push(FsEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: item.path().to_string_lossy().into_owned(),
            dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: modified as i64,
        });
    }
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok((path.to_string(), entries))
}

/// Desktop, Documents and Downloads of the user, then the drives.
fn roots() -> Vec<FsEntry> {
    let mut entries: Vec<FsEntry> = known_folders()
        .into_iter()
        .map(|p| FsEntry {
            name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            path: p.to_string_lossy().into_owned(),
            dir: true,
            ..Default::default()
        })
        .collect();
    entries.extend(drives().into_iter().map(|d| FsEntry {
        name: d.trim_end_matches('\\').into(),
        path: d,
        dir: true,
        ..Default::default()
    }));
    entries
}

#[cfg(windows)]
fn drives() -> Vec<String> {
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26).filter(|i| mask & (1 << i) != 0).map(|i| format!("{}:\\", (b'A' + i as u8) as char)).collect()
}

#[cfg(not(windows))]
fn drives() -> Vec<String> {
    vec!["/".into()]
}

#[cfg(windows)]
fn known_folders() -> Vec<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };

    // Called while impersonating: the folders of the user signed in.
    [FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads]
        .iter()
        .filter_map(|id| {
            let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
            let text = unsafe { path.to_string() };
            unsafe { CoTaskMemFree(Some(path.0 as *const _)) };
            text.ok().map(PathBuf::from)
        })
        .collect()
}

#[cfg(not(windows))]
fn known_folders() -> Vec<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).into_iter().collect()
}

/// Runs `f` on a blocking thread as the user signed in on the session (agent =
/// SYSTEM), or as is (agent = that user).
pub(crate) async fn as_user<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(move || {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};

            let Some(token) = crate::files::session_user_token()? else { return f() };
            let impersonated = unsafe { ImpersonateLoggedOnUser(token) };
            let result = match impersonated {
                Ok(()) => f(),
                Err(e) => Err(e).context("acting as the signed-in user"),
            };
            unsafe {
                let _ = RevertToSelf();
                let _ = CloseHandle(token);
            }
            result
        }
        #[cfg(not(windows))]
        f()
    })
    .await?
}

/// Reads the whole rest of `file` (tests).
#[cfg(test)]
fn read_all(mut file: std::fs::File) -> Vec<u8> {
    use std::io::Read;
    let mut data = Vec::new();
    file.read_to_end(&mut data).expect("read");
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_folders_first() {
        let dir = std::env::temp_dir().join(format!("nuvira-fs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Zeta")).unwrap();
        std::fs::write(dir.join("alpha.txt"), b"12345").unwrap();
        std::fs::create_dir_all(dir.join("beta")).unwrap();
        let (path, entries) = list(&dir.to_string_lossy()).unwrap();
        assert_eq!(path, dir.to_string_lossy());
        let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.dir, e.size)).collect();
        assert_eq!(names, [("beta", true, 0), ("Zeta", true, 0), ("alpha.txt", false, 5)]);
        assert!(read_all(std::fs::File::open(dir.join("alpha.txt")).unwrap()) == b"12345");
        std::fs::remove_dir_all(&dir).unwrap();
        let roots = list("").unwrap().1;
        assert!(roots.iter().any(|e| e.dir && !e.path.is_empty()));
    }
}

/// Characterisation tests (Phase 2 of the full review) of the file manager's
/// uploads (resume, sizes, paths), downloads and listings, over a QUIC connection
/// on the loopback. They pin the CURRENT behaviour.
#[cfg(test)]
mod characterisation {
    use super::*;
    use nuvira_proto::v1::FsDownload;
    use std::time::Duration;

    /// (client side, agent side) of a connection on the loopback.
    async fn pair() -> (Connection, Connection) {
        let (server, fp) = nuvira_transport::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move { server.accept().await.unwrap().await.unwrap() });
        let client = nuvira_transport::connect(addr, Some(fp)).await.unwrap();
        (client, accept.await.unwrap())
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nuvira-p2-fsops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Sends `first` and then `chunks` on a uni stream, and has `receive` save them.
    async fn upload(first: FileChunk, chunks: &[&[u8]]) -> Result<PathBuf> {
        let (client, agent) = pair().await;
        let mut tx = FramedSend::new(client.open_uni().await.unwrap());
        tx.send(&first).await.unwrap();
        for c in chunks {
            tx.send(&FileChunk { data: c.to_vec(), ..Default::default() }).await.unwrap();
        }
        tx.finish().unwrap();
        let mut rx = FramedRecv::new(agent.accept_uni().await.unwrap());
        let first: FileChunk = rx.recv().await.unwrap().unwrap();
        receive(first, rx).await
    }

    fn header(dir: &Path, name: &str, size: u64, offset: u64, data: &[u8]) -> FileChunk {
        FileChunk {
            name: name.into(),
            dir: dir.to_string_lossy().into_owned(),
            size,
            offset,
            data: data.to_vec(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn upload_new_file_and_name_clash() {
        let dir = temp("new");
        let path = upload(header(&dir, "../..\\evil.txt", 6, 0, b"ab"), &[b"cd", b"ef"]).await.unwrap();
        assert_eq!(path, dir.join("evil.txt"));
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        assert!(!dir.join("evil.txt.part").exists());
        // Same name again: " (1)".
        let path = upload(header(&dir, "evil.txt", 1, 0, b"x"), &[]).await.unwrap();
        assert_eq!(path, dir.join("evil (1).txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn characterises_upload_folder_is_any_path_the_viewer_gives() {
        // KNOWN LEAD (features.md, UNVERIFIED, fsops.rs:27): the folder of an upload
        // is the viewer's text as is: relative parts ("..") are followed, and a
        // relative folder is relative to the agent's working directory. Only the
        // file name is sanitised. (As SYSTEM the write is done as the signed-in
        // user, see `as_user`: what the user may write.)
        let dir = temp("dotdot");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        let via = dir.join("a").join("..").join("b");
        std::fs::create_dir_all(dir.join("b")).unwrap();
        let path = upload(header(&via, "f.bin", 3, 0, b"xyz"), &[]).await.unwrap();
        assert_eq!(path, via.join("f.bin"));
        assert_eq!(std::fs::read(dir.join("b").join("f.bin")).unwrap(), b"xyz");
        // A folder that does not exist: an error ("creating ...part"), nothing created.
        let err = upload(header(&dir.join("missing"), "f.bin", 1, 0, b"x"), &[]).await.unwrap_err();
        assert!(format!("{err:#}").starts_with("creating "), "{err:#}");
        assert!(!dir.join("missing").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn upload_resume() {
        let dir = temp("resume");
        std::fs::write(dir.join("r.txt.part"), b"abc").unwrap();
        let path = upload(header(&dir, "r.txt", 6, 3, b"d"), &[b"ef"]).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        // The resume point must be the size of the part there.
        std::fs::write(dir.join("s.txt.part"), b"ab").unwrap();
        let err = upload(header(&dir, "s.txt", 6, 3, b""), &[]).await.unwrap_err().to_string();
        assert_eq!(err, "cannot resume: 2 bytes there, not 3");
        let err = upload(header(&dir, "none.txt", 6, 3, b""), &[]).await.unwrap_err().to_string();
        assert_eq!(err, "nothing to resume");
        let err = upload(header(&dir, "s.txt", 2, 3, b""), &[]).await.unwrap_err().to_string();
        assert_eq!(err, "resume point beyond the end of the file");
        // offset 0 restarts: an old part is truncated.
        let path = upload(header(&dir, "s.txt", 1, 0, b"z"), &[]).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"z");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn upload_interrupted_keeps_the_part_for_resume() {
        let dir = temp("interrupted");
        let err = upload(header(&dir, "i.txt", 10, 0, b"abc"), &[b"de"]).await.unwrap_err().to_string();
        assert_eq!(err, "interrupted at 5 bytes out of 10");
        assert_eq!(std::fs::read(dir.join("i.txt.part")).unwrap(), b"abcde");
        // The viewer asks where to resume.
        let (client, _agent) = pair().await;
        let op = fs_request::Op::UploadOffset(FsUploadOffset {
            dir: dir.to_string_lossy().into_owned(),
            name: "i.txt".into(),
        });
        let response = handle(7, Some(op), client.clone()).await;
        assert_eq!((response.id, response.offset, response.error.as_str()), (7, 5, ""));
        // No part: offset 0, no error; also for a folder that does not exist.
        for d in [dir.clone(), dir.join("nope")] {
            let op = fs_request::Op::UploadOffset(FsUploadOffset {
                dir: d.to_string_lossy().into_owned(),
                name: "other.txt".into(),
            });
            let response = handle(8, Some(op), client.clone()).await;
            assert_eq!((response.offset, response.error.as_str()), (0, ""));
        }
        let path = upload(header(&dir, "i.txt", 10, 5, b"fghij"), &[]).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abcdefghij");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn characterises_more_data_than_announced_leaves_an_oversized_part() {
        // KNOWN BUG (Phase 2): an upload that sends more than its size is refused,
        // but the `.part` stays with the extra bytes in it (the check is after the
        // write): a resume then asks for an offset beyond the size and is refused.
        // The data in the header itself is not checked against the size until the
        // next chunk or the end. Expected after fix: the part removed (or cut to
        // `size`) on this error.
        let dir = temp("oversized");
        let err = upload(header(&dir, "o.txt", 3, 0, b"ab"), &[b"cdef"]).await.unwrap_err().to_string();
        assert_eq!(err, "more data than announced");
        // The error returns without flushing the tokio file: its last write may still
        // be under way on the blocking pool (seen on Linux), so wait for it.
        let part = dir.join("o.txt.part");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::fs::read(&part).map(|d| d.len()).unwrap_or(0) < 6 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read(&part).unwrap(), b"abcdef");
        // All in the header: noticed at the end, as "interrupted" with more than the size.
        let err = upload(header(&dir, "p.txt", 1, 0, b"abc"), &[]).await.unwrap_err().to_string();
        assert_eq!(err, "interrupted at 3 bytes out of 1");
        assert_eq!(std::fs::read(dir.join("p.txt.part")).unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_and_missing_requests() {
        let (client, _agent) = pair().await;
        let response = handle(3, None, client.clone()).await;
        assert_eq!((response.id, response.error.as_str()), (3, "empty request"));
        let dir = temp("missing-list");
        let missing = dir.join("nope");
        let response = handle(4, Some(fs_request::Op::List(missing.to_string_lossy().into_owned())), client).await;
        assert!(response.error.starts_with("reading "), "{}", response.error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Downloads `path` from `offset`: the response and, if a stream came within
    /// 2 s, its header and data.
    async fn download(path: &Path, offset: u64) -> (FsResponse, Option<(FileChunk, Vec<u8>)>) {
        let (client, agent) = pair().await;
        let op = fs_request::Op::Download(FsDownload { path: path.to_string_lossy().into_owned(), offset });
        // The agent sends on `agent`; the viewer side reads on `client`.
        let response = handle(9, Some(op), agent.clone()).await;
        let stream = tokio::time::timeout(Duration::from_secs(2), client.accept_uni()).await;
        let Ok(Ok(stream)) = stream else { return (response, None) };
        let mut rx = FramedRecv::new(stream);
        let Ok(Some(first)) = rx.recv::<FileChunk>().await else { return (response, None) };
        let mut data = Vec::new();
        while let Ok(Some(chunk)) = rx.recv::<FileChunk>().await {
            data.extend(chunk.data);
        }
        (response, Some((first, data)))
    }

    #[tokio::test]
    async fn downloads() {
        let dir = temp("download");
        let file = dir.join("d.bin");
        let content: Vec<u8> = (0..600_000u32).map(|i| i as u8).collect();
        std::fs::write(&file, &content).unwrap();
        let (response, got) = download(&file, 0).await;
        assert_eq!((response.id, response.error.as_str()), (9, ""));
        let (first, data) = got.unwrap();
        assert_eq!((first.name.as_str(), first.size, first.offset, first.transfer_id), ("d.bin", 600_000, 0, 9));
        assert_eq!(data, content);
        // From an offset: the total size in the header, the data from there.
        let (_, got) = download(&file, 599_990).await;
        let (first, data) = got.unwrap();
        assert_eq!((first.size, first.offset), (600_000, 599_990));
        assert_eq!(data, content[599_990..]);
        // At the very end: a header and no data.
        let (_, got) = download(&file, 600_000).await;
        assert_eq!(got.unwrap().1.len(), 0);
        // A file that does not exist: an error in the response.
        let (response, got) = download(&dir.join("nope"), 0).await;
        assert!(response.error.starts_with("opening "), "{}", response.error);
        assert!(got.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn characterises_download_offset_beyond_the_end_answers_ok_and_sends_nothing() {
        // KNOWN BUG (Phase 2): the resume point is checked in the spawned `send`,
        // after `handle` has already answered with no error: the viewer gets an OK
        // response and then no stream at all (the error goes only to the agent's
        // log). Expected after fix: the error in the response.
        let dir = temp("download-beyond");
        let file = dir.join("d.bin");
        std::fs::write(&file, b"12345").unwrap();
        let (response, got) = download(&file, 6).await;
        assert_eq!(response.error, "");
        assert!(got.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_of_a_folder() {
        let dir = temp("download-dir");
        let (response, got) = download(&dir, 0).await;
        if cfg!(windows) {
            // Windows cannot open a folder as a file.
            assert!(response.error.starts_with("opening "), "{}", response.error);
            assert!(got.is_none());
        } else {
            // Unix opens it; reading fails after the header was sent: the viewer gets
            // an OK response, a header and no data.
            assert_eq!(response.error, "");
            let (_, data) = got.expect("a header");
            assert!(data.is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_details() {
        let dir = temp("list");
        std::fs::write(dir.join("b.TXT"), b"1").unwrap();
        std::fs::write(dir.join("A.txt"), b"22").unwrap();
        std::fs::create_dir_all(dir.join("z-dir")).unwrap();
        let (path, entries) = list(&dir.to_string_lossy()).unwrap();
        assert_eq!(path, dir.to_string_lossy());
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        // Folders first, then case-insensitive order.
        assert_eq!(names, ["z-dir", "A.txt", "b.TXT"]);
        assert!(entries.iter().all(|e| e.modified > 0));
        assert_eq!(entries[1].path, dir.join("A.txt").to_string_lossy());
        // Roots: the user's folders, then the drives ("C:" for "C:\" on Windows, "/" elsewhere).
        let roots = list("").unwrap();
        assert_eq!(roots.0, "");
        let last = roots.1.last().unwrap();
        if cfg!(windows) {
            assert!(last.path.ends_with(":\\") && last.name.len() == 2, "{last:?}");
        } else {
            assert_eq!((last.name.as_str(), last.path.as_str()), ("/", "/"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths() {
        let dir = temp("winpaths");
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        // Verbatim (\\?\) paths list, and the entries keep the prefix.
        let verbatim = format!(r"\\?\{}", dir.display());
        let (_, entries) = list(&verbatim).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].path.starts_with(r"\\?\"), "{}", entries[0].path);
        // Forward slashes work too.
        let slashes = dir.to_string_lossy().replace('\\', "/");
        assert_eq!(list(&slashes).unwrap().1.len(), 1);
        // A drive root.
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(!list(&format!("{system}\\")).unwrap().1.is_empty());
        // The administrative share of the system drive on this PC (UNC).
        let unc = format!(r"\\localhost\{}$\", &system[..1]);
        match list(&unc) {
            Ok((path, entries)) => {
                assert_eq!(path, unc);
                assert!(entries.iter().all(|e| e.path.starts_with(r"\\localhost\")));
            }
            // Without the administrative shares (or rights to them): an error, no panic.
            Err(e) => assert!(format!("{e:#}").starts_with("reading "), "{e:#}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn characterises_symlinks_are_listed_as_links_not_followed() {
        // `DirEntry::metadata` does not follow symbolic links: a link to a folder
        // is listed as a file (dir false, size of the link itself), a dangling link
        // is listed too. Listing a link to a folder by its path follows it.
        let dir = temp("symlinks");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join("real").join("inside.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("dangling")).unwrap();
        let (_, entries) = list(&dir.to_string_lossy()).unwrap();
        let by_name = |n: &str| entries.iter().find(|e| e.name == n).unwrap().clone();
        assert!(by_name("real").dir);
        let link = by_name("link");
        assert!(!link.dir);
        assert_eq!(link.size, dir.join("real").to_string_lossy().len() as u64);
        assert!(!by_name("dangling").dir);
        let (_, inside) = list(&dir.join("link").to_string_lossy()).unwrap();
        assert_eq!(inside.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uploads_follow_a_symlinked_part() {
        // A `.part` that is a symbolic link is followed when resuming (append) and
        // replaced by the rename at the end: the link's target gets the bytes.
        let dir = temp("symlink-part");
        std::fs::write(dir.join("target.bin"), b"abc").unwrap();
        std::os::unix::fs::symlink(dir.join("target.bin"), dir.join("u.bin.part")).unwrap();
        let path = upload(header(&dir, "u.bin", 6, 3, b"def"), &[]).await.unwrap();
        assert_eq!(std::fs::read(dir.join("target.bin")).unwrap(), b"abcdef");
        // The rename moved the link itself to the final name.
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
