//! Ricezione dei file inviati dal viewer.
//!
//! I file finiscono nella cartella Download dell'utente collegato al PC, anche
//! quando l'agent è quello del servizio (SYSTEM): la cartella si ricava dal token
//! dell'utente della sessione. Si scrive prima `nome.part`, rinominato a file
//! completo; se il nome esiste già si aggiunge " (1)", " (2)"…

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nexodesk_proto::v1::FileChunk;
use nexodesk_transport::FramedRecv;
use tokio::io::AsyncWriteExt;
use tracing::info;

/// Riceve un file dallo stream; restituisce nome e percorso finale.
pub async fn receive(stream: quinn::RecvStream) -> Result<(String, PathBuf), (String, anyhow::Error)> {
    let mut rx = FramedRecv::new(stream);
    let first = match rx.recv::<FileChunk>().await {
        Ok(Some(first)) => first,
        Ok(None) => return Err((String::new(), anyhow::anyhow!("canale del file vuoto"))),
        Err(e) => return Err((String::new(), e)),
    };
    let name = safe_name(&first.name);
    let result = async {
        let dir = downloads_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creazione di {}", dir.display()))?;
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
            bail!("file incompleto: ricevuti {size} byte su {}", first.size);
        }
        let target = unique_path(&dir, &name);
        std::fs::rename(&part, &target).with_context(|| format!("salvataggio di {}", target.display()))?;
        info!("file ricevuto: {} ({size} byte)", target.display());
        Ok(target)
    }
    .await;
    result.map(|path| (name.clone(), path)).map_err(|e| (name, e))
}

async fn write(path: &Path, first: Vec<u8>, rx: &mut FramedRecv) -> Result<u64> {
    let mut file = tokio::fs::File::create(path).await.with_context(|| format!("creazione di {}", path.display()))?;
    file.write_all(&first).await?;
    let mut size = first.len() as u64;
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        file.write_all(&chunk.data).await?;
        size += chunk.data.len() as u64;
    }
    file.flush().await?;
    Ok(size)
}

/// Solo il nome del file, senza cartelle né caratteri che Windows non accetta.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let clean = clean.trim().trim_end_matches(['.', ' ']).to_string();
    if clean.is_empty() || clean == "." || clean == ".." { "file".into() } else { clean }
}

/// `nome.ext`, oppure `nome (1).ext`, `nome (2).ext`… se esiste già.
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
        .expect("un nome libero esiste sempre")
}

/// Cartella Download dell'utente collegato a questa sessione di Windows.
#[cfg(windows)]
fn downloads_dir() -> Result<PathBuf> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_PRIVILEGE_NOT_HELD, HANDLE};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // Da SYSTEM (servizio) si usa il token dell'utente della sessione; da utente
    // normale WTSQueryUserToken non è permesso e vale il proprio profilo.
    let mut token = HANDLE::default();
    let user_token = match unsafe { WTSQueryUserToken(crate::ipc::current_session(), &mut token) } {
        Ok(()) => Some(token),
        Err(e) if e.code() == ERROR_PRIVILEGE_NOT_HELD.to_hresult() => None,
        Err(e) => bail!("nessun utente collegato al PC a cui consegnare il file ({e})"),
    };
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, user_token) };
    if let Some(token) = user_token {
        let _ = unsafe { CloseHandle(token) };
    }
    let path = path.context("cartella Download")?;
    let text = unsafe { path.to_string() };
    unsafe { CoTaskMemFree(Some(path.0 as *const _)) };
    Ok(PathBuf::from(text?))
}

#[cfg(not(windows))]
fn downloads_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME non impostata")?;
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
