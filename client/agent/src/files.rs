//! Receiving the files sent by the viewer.
//!
//! Files end up in the Downloads folder of the user signed in on the PC, also
//! when the agent is the service's (SYSTEM): the folder is derived from the token
//! of the session's user. `name.part` is written first and renamed once the file
//! is complete; if the name exists " (1)", " (2)"… is added.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::FileChunk;
use nuvira_transport::FramedRecv;
use tokio::io::AsyncWriteExt;
use tracing::info;

/// Receives a file from the stream; returns its name and final path.
/// Saves the file of a stream whose first chunk (`first`, with name and size) was already read.
pub async fn receive(
    first: FileChunk,
    mut rx: FramedRecv,
) -> Result<(String, PathBuf), (String, anyhow::Error)> {
    let name = safe_name(&first.name);
    let result = async {
        let dir = downloads_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let part = dir.join(format!("{name}.part"));
        // Also when the transfer is stopped half way (files switched off at the PC).
        let mut leftover = RemoveOnDrop(Some(part.clone()));
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
        leftover.0 = None;
        std::fs::rename(&part, &target).with_context(|| format!("saving {}", target.display()))?;
        info!("file received: {} ({size} bytes)", target.display());
        crate::stats::file_in(size);
        Ok(target)
    }
    .await;
    result.map(|path| (name.clone(), path)).map_err(|e| (name, e))
}

async fn write(path: &Path, first: Vec<u8>, rx: &mut FramedRecv) -> Result<u64> {
    let mut file =
        tokio::fs::File::create(path).await.with_context(|| format!("creating {}", path.display()))?;
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
pub(crate) fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String =
        base.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let clean = clean.trim().trim_end_matches(['.', ' ']).to_string();
    if clean.is_empty() || clean == "." || clean == ".." { "file".into() } else { clean }
}

/// `name.ext`, or `name (1).ext`, `name (2).ext`… if it already exists.
pub(crate) fn unique_path(dir: &Path, name: &str) -> PathBuf {
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

/// Token of the user signed in to this Windows session, if the agent is SYSTEM
/// (the service's); `None` if it runs as a normal user (WTSQueryUserToken is not
/// allowed then, and its own identity is already the user's). To be closed by the caller.
#[cfg(windows)]
pub(crate) fn session_user_token() -> Result<Option<windows::Win32::Foundation::HANDLE>> {
    use windows::Win32::Foundation::{ERROR_PRIVILEGE_NOT_HELD, HANDLE};
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;

    let mut token = HANDLE::default();
    match unsafe { WTSQueryUserToken(crate::ipc::current_session(), &mut token) } {
        Ok(()) => Ok(Some(token)),
        Err(e) if e.code() == ERROR_PRIVILEGE_NOT_HELD.to_hresult() => Ok(None),
        Err(e) => {
            tracing::warn!("no user token for the session: {e}");
            bail!("@agent.no_user")
        }
    }
}

/// Downloads folder of the user signed in to this Windows session.
#[cfg(windows)]
fn downloads_dir() -> Result<PathBuf> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    let user_token = session_user_token()?;
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, user_token) };
    if let Some(token) = user_token {
        let _ = unsafe { CloseHandle(token) };
    }
    let path = path.context("Downloads folder")?;
    let text = unsafe { path.to_string() };
    unsafe { CoTaskMemFree(Some(path.0 as *const _)) };
    Ok(PathBuf::from(text?))
}

/// Downloads folder of the user: on Linux the one of `user-dirs.dirs`, which
/// follows the language of the desktop ("Scaricati", "Téléchargements"...).
#[cfg(not(windows))]
fn downloads_dir() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    let config = std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    let dirs = std::fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();
    Ok(xdg_download_dir(&dirs, &home).unwrap_or_else(|| home.join("Downloads")))
}

/// `XDG_DOWNLOAD_DIR="$HOME/..."` of a `user-dirs.dirs` file.
#[cfg(not(windows))]
fn xdg_download_dir(dirs: &str, home: &std::path::Path) -> Option<PathBuf> {
    let value = dirs.lines().find_map(|line| line.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?;
    let value = value.trim().trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // A value equal to $HOME means "no such folder" for the specification.
    (path.is_absolute() && path != home).then_some(path)
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

    #[cfg(not(windows))]
    #[test]
    fn xdg_downloads() {
        let home = std::path::Path::new("/home/a");
        let dirs = "# comment
XDG_DESKTOP_DIR=\"$HOME/Scrivania\"
XDG_DOWNLOAD_DIR=\"$HOME/Scaricati\"
";
        assert_eq!(xdg_download_dir(dirs, home), Some(home.join("Scaricati")));
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", home), Some("/data/dl".into()));
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"$HOME/\"", home), None);
        assert_eq!(xdg_download_dir("", home), None);
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("nuvira-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        assert_eq!(unique_path(&dir, "b.txt"), dir.join("b.txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Removes the partial file of a transfer that did not end (dropped half way).
struct RemoveOnDrop(Option<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Characterisation tests (Phase 2 of the full review) of the names given to files
/// received from the viewer.
#[cfg(test)]
mod characterisation {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn characterises_names_windows_reserves_are_kept() {
        // KNOWN LIMIT (Phase 2): device names Windows reserves (CON, NUL, COM1...)
        // pass through unchanged; on Windows "CON.part" / "CON" then fail or reach a
        // device. Expected after fix (if decided): such names prefixed or replaced.
        for name in ["CON", "nul", "COM1", "lpt1.txt", "aux.tar.gz"] {
            assert_eq!(safe_name(name), name);
        }
    }

    #[test]
    fn name_edge_cases() {
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("   "), "file");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name("a/"), "file", "the part after the last separator is empty");
        assert_eq!(safe_name("dir\\"), "file");
        assert_eq!(safe_name(".bashrc"), ".bashrc", "leading dots kept");
        assert_eq!(safe_name("  a.txt  "), "a.txt");
        assert_eq!(safe_name("a\u{0}b\u{1f}c\u{7f}"), "a_b_c_");
        assert_eq!(safe_name("x:y*z?\"<>|"), "x_y_z_____");
        // Format characters (not "control" for Rust) are kept: a right-to-left
        // override can make "evil\u{202e}txt.exe" look like "evilexe.txt".
        assert_eq!(safe_name("evil\u{202e}txt.exe"), "evil\u{202e}txt.exe");
        // No length limit (file systems allow 255 bytes): a longer name fails later.
        assert_eq!(safe_name(&"a".repeat(1000)).len(), 1000);
        // Unicode stays.
        assert_eq!(safe_name("città ✓.pdf"), "città ✓.pdf");
    }

    #[test]
    fn unique_path_edge_cases() {
        let dir = std::env::temp_dir().join(format!("nuvira-p2-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for existing in [".hidden", "noext", "a.tar.gz"] {
            std::fs::write(dir.join(existing), b"x").unwrap();
        }
        // A leading dot is not an extension; only the last extension is kept apart.
        assert_eq!(unique_path(&dir, ".hidden"), dir.join(".hidden (1)"));
        assert_eq!(unique_path(&dir, "noext"), dir.join("noext (1)"));
        assert_eq!(unique_path(&dir, "a.tar.gz"), dir.join("a.tar (1).gz"));
        // A folder with the name counts as taken.
        std::fs::create_dir_all(dir.join("taken")).unwrap();
        assert_eq!(unique_path(&dir, "taken"), dir.join("taken (1)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 3000, ..ProptestConfig::default() })]

        /// Whatever the viewer sends, the name is one plain component: no separator,
        /// no character Windows refuses, not empty, not "." or "..", no trailing dot
        /// or space, and joining it to a folder stays inside the folder.
        #[test]
        fn safe_names_are_one_component(name in r"\PC{0,40}|[./\\: ]{0,10}|.{0,5}(\.\.[/\\]){1,4}.{0,5}") {
            let s = safe_name(&name);
            prop_assert!(!s.is_empty());
            prop_assert!(s != "." && s != "..");
            prop_assert!(!s.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)));
            prop_assert!(!s.ends_with('.') && !s.ends_with(' '));
            prop_assert!(!s.starts_with(char::is_whitespace));
            let dir = Path::new("base");
            let joined = dir.join(&s);
            prop_assert_eq!(joined.parent(), Some(dir));
            prop_assert_eq!(safe_name(&s), s.clone(), "idempotent");
        }
    }
}
