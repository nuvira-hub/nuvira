//! Receiving the files sent by the viewer.
//!
//! Files end up in the Downloads folder of the user signed in on the PC, also
//! when the agent is the service's (SYSTEM): the folder is derived from the token
//! of the session's user. `name.part` is written first and renamed once the file
//! is complete; if the name exists " (1)", " (2)"… is added.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nexodesk_proto::v1::FileChunk;
use nexodesk_transport::FramedRecv;
use tokio::io::AsyncWriteExt;
use tracing::info;

/// Receives a file from the stream; returns its name and final path.
pub async fn receive(stream: quinn::RecvStream) -> Result<(String, PathBuf), (String, anyhow::Error)> {
    let mut rx = FramedRecv::new(stream);
    let first = match rx.recv::<FileChunk>().await {
        Ok(Some(first)) => first,
        Ok(None) => return Err((String::new(), anyhow::anyhow!("empty file stream"))),
        Err(e) => return Err((String::new(), e)),
    };
    let name = safe_name(&first.name);
    let result = async {
        let dir = downloads_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let part = dir.join(format!("{name}.part"));
        let written = write(&part, first.data, &mut rx).await;
        let size = match written {
            Ok(size) => size,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
        };
        if size != first.size {
            let _ = std::fs::remove_file(&part);
            bail!("incomplete file: received {size} bytes out of {}", first.size);
        }
        let target = unique_path(&dir, &name);
        std::fs::rename(&part, &target).with_context(|| format!("saving {}", target.display()))?;
        info!("file received: {} ({size} bytes)", target.display());
        Ok(target)
    }
    .await;
    result.map(|path| (name.clone(), path)).map_err(|e| (name, e))
}

async fn write(path: &Path, first: Vec<u8>, rx: &mut FramedRecv) -> Result<u64> {
    let mut file = tokio::fs::File::create(path).await.with_context(|| format!("creating {}", path.display()))?;
    file.write_all(&first).await?;
    let mut size = first.len() as u64;
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        file.write_all(&chunk.data).await?;
        size += chunk.data.len() as u64;
    }
    file.flush().await?;
    Ok(size)
}

/// Only the file name, without folders or characters Windows does not accept.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let clean = clean.trim().trim_end_matches(['.', ' ']).to_string();
    if clean.is_empty() || clean == "." || clean == ".." { "file".into() } else { clean }
}

/// `name.ext`, or `name (1).ext`, `name (2).ext`… if it already exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("a free name always exists")
}

/// Downloads folder of the user signed in to this Windows session.
#[cfg(windows)]
fn downloads_dir() -> Result<PathBuf> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_PRIVILEGE_NOT_HELD, HANDLE};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // As SYSTEM (service) the token of the session's user is used; as a normal user
    // WTSQueryUserToken is not allowed and the own profile applies.
    let mut token = HANDLE::default();
    let user_token = match unsafe { WTSQueryUserToken(crate::ipc::current_session(), &mut token) } {
        Ok(()) => Some(token),
        Err(e) if e.code() == ERROR_PRIVILEGE_NOT_HELD.to_hresult() => None,
        Err(e) => {
            tracing::warn!("no user token for the session: {e}");
            bail!("@agent.no_user")
        }
    };
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, user_token) };
    if let Some(token) = user_token {
        let _ = unsafe { CloseHandle(token) };
    }
    let path = path.context("Downloads folder")?;
    let text = unsafe { path.to_string() };
    unsafe { CoTaskMemFree(Some(path.0 as *const _)) };
    Ok(PathBuf::from(text?))
}

#[cfg(not(windows))]
fn downloads_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Downloads"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(safe_name("..\\..\\Windows\\evil.exe"), "evil.exe");
        assert_eq!(safe_name("/etc/passwd"), "passwd");
        assert_eq!(safe_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("nome. "), "nome");
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("nexodesk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        assert_eq!(unique_path(&dir, "b.txt"), dir.join("b.txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
