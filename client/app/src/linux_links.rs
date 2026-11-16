//! Linux: this program as the handler of `nuvirahub://` links.
//!
//! The packages (.deb, .rpm) register the scheme with their desktop file
//! (`MimeType=x-scheme-handler/nuvirahub`), so an installed program does nothing
//! here, and takes back a handler an earlier version wrote. Anything else (the
//! tar archive, a copy run from elsewhere) writes a hidden desktop file in the
//! person's applications and makes it the default, as the deep-link plugin does,
//! but with `Exec` unquoted when the path needs no quotes: `xdg-open` without a
//! desktop environment (a window manager on its own) does not understand them and
//! gave the link to the browser.

use std::path::{Path, PathBuf};
use std::process::Command;

const SCHEME: &str = "x-scheme-handler/nuvirahub";
/// The hidden handler (the deep-link plugin's name for it, kept for its earlier files).
const HANDLER: &str = "nuvira-handler.desktop";
/// The packages' program and desktop file.
const PACKAGED_EXE: &str = "/usr/bin/nuvira";
const PACKAGED_DESKTOP: &str = "/usr/share/applications/io.github.nuvira-hub.nuvira.desktop";

pub fn register() -> std::io::Result<()> {
    let exe = std::env::var_os("APPIMAGE").map(PathBuf::from).map_or_else(std::env::current_exe, Ok)?;
    let dir = applications_dir().ok_or_else(|| std::io::Error::other("no data folder"))?;
    let handler = dir.join(HANDLER);
    if exe == Path::new(PACKAGED_EXE) && Path::new(PACKAGED_DESKTOP).exists() {
        // An earlier version's handler would stay the default over the package's.
        if handler.exists() {
            std::fs::remove_file(&handler)?;
            refresh(&dir);
            xdg_mime_default(
                Path::new(PACKAGED_DESKTOP).file_name().and_then(|n| n.to_str()).unwrap_or_default(),
            );
            tracing::info!("nuvirahub:// links: the package's handler again");
        }
        return Ok(());
    }
    let content = format!(
        "[Desktop Entry]\nType=Application\nName=NuviraHub\nExec={} %u\nTerminal=false\nMimeType={SCHEME}\nNoDisplay=true\n",
        exec_path(&exe.to_string_lossy())
    );
    if std::fs::read_to_string(&handler).ok().as_deref() != Some(content.as_str()) {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&handler, content)?;
        refresh(&dir);
    }
    xdg_mime_default(HANDLER);
    Ok(())
}

/// `$XDG_DATA_HOME/applications`, or `~/.local/share/applications`.
fn applications_dir() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(data.join("applications"))
}

/// The program's path for `Exec`: as it is when it needs no quoting, otherwise in
/// double quotes with `"`, `` ` ``, `$` and `\` escaped (Desktop Entry spec).
fn exec_path(path: &str) -> String {
    if !path.is_empty() && path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,".contains(c)) {
        return path.to_string();
    }
    let mut quoted = String::from("\"");
    for c in path.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            // Escaped for the quoting, then the backslash for the key's value.
            quoted.push_str("\\\\");
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

fn refresh(dir: &Path) {
    let _ = Command::new("update-desktop-database").arg(dir).output();
}

fn xdg_mime_default(desktop: &str) {
    match Command::new("xdg-mime").args(["default", desktop, SCHEME]).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            tracing::warn!("xdg-mime default {desktop}: {}", String::from_utf8_lossy(&out.stderr).trim())
        }
        Err(e) => tracing::warn!("xdg-mime: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::exec_path;

    #[test]
    fn exec_paths() {
        assert_eq!(exec_path("/usr/bin/nuvira"), "/usr/bin/nuvira");
        assert_eq!(exec_path("/home/a/nuvira-0.27.0/nuvira"), "/home/a/nuvira-0.27.0/nuvira");
        assert_eq!(exec_path("/home/a/My Apps/nuvira"), "\"/home/a/My Apps/nuvira\"");
        assert_eq!(exec_path("/opt/a$b/nuvira"), "\"/opt/a\\\\$b/nuvira\"");
    }
}
