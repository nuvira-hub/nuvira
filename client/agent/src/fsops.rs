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
async fn as_user<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
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
