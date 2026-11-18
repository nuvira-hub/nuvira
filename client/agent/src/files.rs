//! Receiving the files sent by the viewer.
//!
//! Files end up in the Downloads folder of the user signed in on the PC, also
//! when the agent is the service's (SYSTEM): the folder is derived from the token
//! of the session's user, and every step on that folder (create, write, rename,
//! delete) runs as that user, never as SYSTEM (see `fsops::as_user`). `name.part` is written first and renamed once the file
//! is complete; if the name exists " (1)", " (2)"… is added.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::FileChunk;
use nuvira_transport::FramedRecv;
use tokio::io::AsyncWriteExt;
use tracing::info;

use crate::fsops::as_user;

/// Receives a file from the stream; returns its name and final path.
/// Saves the file of a stream whose first chunk (`first`, with name and size) was already read.
pub async fn receive(
    first: FileChunk,
    mut rx: FramedRecv,
) -> Result<(String, PathBuf), (String, anyhow::Error)> {
    let name = safe_name(&first.name);
    let result = async {
        let dir = downloads_dir()?;
        let part = dir.join(format!("{name}.part"));
        // Every step on the folder runs as the signed-in user (`as_user`), as in the
        // file manager: the folder comes from the user's profile, which that user
        // controls (it can be a junction to a folder only SYSTEM may write), so the
        // service's agent (SYSTEM) must never create, rename or delete there itself.
        // The handle opened as the user is then written as is.
        let file = {
            let (dir, part) = (dir.clone(), part.clone());
            as_user(move || {
                std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
                std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))
            })
            .await?
        };
        // Removed on every failure, also when the transfer is stopped half way (files
        // switched off at the PC).
        let mut leftover = RemoveOnDrop(Some(part.clone()));
        let size = write(tokio::fs::File::from_std(file), first.data, first.size, &mut rx).await?;
        if size != first.size {
            bail!("incomplete file: received {size} bytes out of {}", first.size);
        }
        leftover.0 = None;
        let target = {
            let name = name.clone();
            as_user(move || {
                let target = unique_path(&dir, &name);
                std::fs::rename(&part, &target).with_context(|| format!("saving {}", target.display()))?;
                Ok(target)
            })
            .await?
        };
        info!("file received: {} ({size} bytes)", target.display());
        crate::stats::file_in(size);
        Ok(target)
    }
    .await;
    result.map(|path| (name.clone(), path)).map_err(|e| (name, e))
}

/// Writes the stream into `file`; stops (error) as soon as more than `announced`
/// bytes arrive, so nothing beyond the size of the file is ever written.
async fn write(
    mut file: tokio::fs::File,
    first: Vec<u8>,
    announced: u64,
    rx: &mut FramedRecv,
) -> Result<u64> {
    let too_much = || anyhow::anyhow!("more data than announced ({announced} bytes)");
    if first.len() as u64 > announced {
        return Err(too_much());
    }
    file.write_all(&first).await?;
    let mut size = first.len() as u64;
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        if size + chunk.data.len() as u64 > announced {
            return Err(too_much());
        }
        file.write_all(&chunk.data).await?;
        size += chunk.data.len() as u64;
    }
    file.flush().await?;
    Ok(size)
}

/// Longest name kept, in bytes (UTF-8; so also at most this many UTF-16 units):
/// with ".part" it stays within the 255 of every usual file system.
const MAX_NAME_BYTES: usize = 200;

/// Only the file name, without folders or characters Windows does not accept;
/// without the invisible characters that change the direction of the text (a
/// right-to-left override can make "evil\u{202e}txt.exe" look like "evilexe.txt");
/// a name Windows reserves for a device (`CON`, `NUL.txt`, `com1.tar.gz`…) gets
/// a "_" in front; at most [`MAX_NAME_BYTES`], keeping a short extension.
pub(crate) fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !is_bidi_format(*c))
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let clean = unreserved(trim_name(&clean).to_string());
    // Cut after the prefix (which can make it one byte longer); a cut and trimmed
    // name can be reserved again only when it became short, hence the last check.
    let clean = unreserved(trim_name(&shorten(&clean)).to_string());
    if clean.is_empty() || clean == "." || clean == ".." { "file".into() } else { clean }
}

/// Without white space around and dots at the end (Windows drops those).
fn trim_name(name: &str) -> &str {
    name.trim_start().trim_end_matches(|c: char| c.is_whitespace() || c == '.')
}

/// Directional formatting characters (Unicode "bidi controls").
fn is_bidi_format(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `_CON` for `CON`: Windows maps these names, with any extension, to devices.
fn unreserved(name: String) -> String {
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    let numbered = (stem.starts_with("COM") || stem.starts_with("LPT"))
        && matches!(
            &stem[3..],
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}"
        );
    if numbered || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        format!("_{name}")
    } else {
        name
    }
}

/// At most [`MAX_NAME_BYTES`], cut on a character boundary; an extension shorter
/// than 16 bytes is kept.
fn shorten(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() < 16 => (stem, &name[stem.len()..]),
        _ => (name, ""),
    };
    let mut end = MAX_NAME_BYTES - ext.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", trim_name(&stem[..end]))
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
            // As the user, like every other step on the folder (see `receive`).
            let _ = crate::fsops::run_as_user(|| Ok(std::fs::remove_file(&path)?));
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
    fn names_windows_reserves_are_prefixed() {
        // Was K14 (Phase 2): device names Windows reserves passed through unchanged
        // ("CON.part" / "CON" then fail or reach a device). Fixed (AGT-9): "_" in front.
        for name in ["CON", "nul", "COM1", "lpt1.txt", "aux.tar.gz", "PRN .txt", "com\u{b9}", "CONOUT$"] {
            assert_eq!(safe_name(name), format!("_{name}"));
        }
        for name in ["CONSOLE", "COM10", "nul_", "_CON", "xCON.txt", "LPT", "COM"] {
            assert_eq!(safe_name(name), name, "not reserved");
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
        // Was K14: the direction formatting characters were kept, and a right-to-left
        // override made "evil\u{202e}txt.exe" look like "evilexe.txt". Fixed (AGT-9): dropped.
        assert_eq!(safe_name("evil\u{202e}txt.exe"), "eviltxt.exe");
        assert_eq!(safe_name("a\u{200e}\u{200f}\u{202a}\u{2066}\u{2069}\u{61c}b"), "ab");
        // Was: no length limit. Fixed (AGT-9): at most 200 bytes, a short extension kept.
        assert_eq!(safe_name(&"a".repeat(1000)), "a".repeat(200));
        assert_eq!(safe_name(&format!("{}.pdf", "é".repeat(300))), format!("{}.pdf", "é".repeat(98)));
        assert_eq!(safe_name(&format!("NUL{}x", " ".repeat(300))), "_NUL", "cut, trimmed, then reserved");
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
        fn safe_names_are_one_component(name in r"\PC{0,40}|[./\\: ]{0,10}|.{0,5}(\.\.[/\\]){1,4}.{0,5}|(?i:con|nul|com1|lpt9|aux)[. ]{0,3}\PC{0,5}|\PC{190,260}") {
            let s = safe_name(&name);
            prop_assert!(!s.is_empty());
            prop_assert!(s != "." && s != "..");
            prop_assert!(!s.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)));
            prop_assert!(!s.ends_with('.') && !s.ends_with(' '));
            prop_assert!(!s.starts_with(char::is_whitespace));
            prop_assert!(!s.chars().any(is_bidi_format));
            prop_assert!(s.len() <= MAX_NAME_BYTES, "{} bytes", s.len());
            prop_assert_eq!(unreserved(s.clone()), s.clone(), "not a device name");
            let dir = Path::new("base");
            let joined = dir.join(&s);
            prop_assert_eq!(joined.parent(), Some(dir));
            prop_assert_eq!(safe_name(&s), s.clone(), "idempotent");
        }
    }
}
