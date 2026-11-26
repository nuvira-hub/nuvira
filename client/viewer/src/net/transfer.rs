//! Files sent to the agent and downloaded from it, each on a stream of its own:
//! "Send files" and the file manager's uploads and downloads (with resume).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::FileChunk;
use nuvira_transport::{FramedRecv, FramedSend};
use tracing::info;
use winit::event_loop::EventLoopProxy;

use super::NetEvent;

/// File manager upload: `local` from `offset` on, into `dir` on the remote PC.
pub(super) async fn send_upload(
    conn: &nuvira_transport::Connection,
    id: u32,
    local: &std::path::Path,
    dir: String,
    offset: u64,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file =
        tokio::fs::File::open(local).await.with_context(|| format!("opening {}", local.display()))?;
    let total = file.metadata().await?.len();
    if offset > total {
        bail!("resume point beyond the end of the file");
    }
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut tx = FramedSend::new(conn.open_uni().await?);
    tx.send(&FileChunk {
        name: file_name(local),
        size: total,
        dir,
        offset,
        transfer_id: id,
        ..Default::default()
    })
    .await?;
    let mut buf = vec![0u8; 256 * 1024];
    let (mut done, mut last) = (offset, Instant::now());
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        tx.send(&FileChunk { data: buf[..n].to_vec(), ..Default::default() }).await?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            let _ = proxy.send_event(NetEvent::Transfer { id, done, total });
        }
    }
    tx.finish()?;
    let _ = proxy.send_event(NetEvent::Transfer { id, done, total });
    Ok(())
}

/// File manager download into `target`: written to `target.part` (continued from
/// the header's offset, if an earlier attempt left it), renamed once complete. If
/// `target` exists a free name is used. Returns the final path.
pub(super) async fn receive_download(
    first: FileChunk,
    mut rx: FramedRecv,
    target: &std::path::Path,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<std::path::PathBuf> {
    use tokio::io::AsyncWriteExt;
    let (id, size, offset) = (first.transfer_id, first.size, first.offset);
    let part = part_path(target);
    let mut file = if offset == 0 {
        tokio::fs::File::create(&part).await.with_context(|| format!("creating {}", part.display()))?
    } else {
        let file =
            tokio::fs::OpenOptions::new().append(true).open(&part).await.context("nothing to resume")?;
        let have = file.metadata().await?.len();
        if have != offset {
            bail!("cannot resume: {have} bytes there, not {offset}");
        }
        file
    };
    let mut done = offset + first.data.len() as u64;
    file.write_all(&first.data).await?;
    let mut last = Instant::now();
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        file.write_all(&chunk.data).await?;
        done += chunk.data.len() as u64;
        if done > size {
            bail!("more data than announced");
        }
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            let _ = proxy.send_event(NetEvent::Transfer { id, done, total: size });
        }
    }
    file.flush().await?;
    drop(file);
    let _ = proxy.send_event(NetEvent::Transfer { id, done, total: size });
    if done != size {
        // The part stays: the download can be resumed from here.
        bail!("interrupted at {done} bytes out of {size}");
    }
    let final_path = free_path(target);
    tokio::fs::rename(&part, &final_path)
        .await
        .with_context(|| format!("saving {}", final_path.display()))?;
    info!("downloaded {} ({size} bytes, from {offset})", final_path.display());
    Ok(final_path)
}

/// `file.ext.part`, where a download is written until complete.
pub fn part_path(target: &std::path::Path) -> std::path::PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

/// `target`, or `name (1).ext`, `name (2).ext`… if it exists.
fn free_path(target: &std::path::Path) -> std::path::PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let stem = target.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = target.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|i| target.with_file_name(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("a free name")
}

pub(super) fn file_name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Sends a file on a new stream; the outcome then comes from the agent (`FileReceived`).
pub(super) async fn send_file(
    conn: &nuvira_transport::Connection,
    path: &std::path::Path,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let name = file_name(path);
    if tokio::fs::metadata(path).await.is_ok_and(|m| m.is_dir()) {
        bail!("{}", nuvira_i18n::t("viewer.folder_not_supported"));
    }
    let mut file =
        tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
    let total = file.metadata().await?.len();
    info!("sending {name} ({total} bytes)");
    let mut tx = FramedSend::new(conn.open_uni().await?);
    tx.send(&FileChunk { name: name.clone(), size: total, ..Default::default() }).await?;
    let mut buf = vec![0u8; 256 * 1024];
    let (mut sent, mut last) = (0u64, Instant::now());
    let _ = proxy.send_event(NetEvent::FileProgress { name: name.clone(), sent, total });
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        tx.send(&FileChunk { data: buf[..n].to_vec(), ..Default::default() }).await?;
        sent += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            let _ = proxy.send_event(NetEvent::FileProgress { name: name.clone(), sent, total });
        }
    }
    tx.finish()?;
    let _ = proxy.send_event(NetEvent::FileProgress { name, sent, total });
    Ok(())
}
