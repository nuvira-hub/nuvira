//! Linux, installed from the .deb or .rpm: `nuvira service update-package`, run
//! as root by the `nuvira-update.timer` of the package (docs/19-updates.md). The
//! users' agents cannot replace a program of root's, so root does it:
//! - the server is the organisation's (`/etc/nuvira/policy.json`, then the
//!   installation for the whole computer, `/etc/nuvira/managed.json`), otherwise
//!   the one of a user's unattended access (the `--server` of
//!   `~/.config/systemd/user/nuvira-agent.service`), otherwise NuviraHub's public
//!   one;
//! - a newer package correctly signed with the release key is downloaded into
//!   `/var/cache/nuvira` and installed with the package manager, unless a session
//!   is in progress (a user's agent keeps `~/.config/nuvira/busy` meanwhile): the
//!   timer tries again an hour later;
//! - the agents restart by themselves with the new program once idle.
//!
//! Without a policy or an installation for the whole computer the server is the
//! first one found in the users' units, and any user's `busy` file postpones the
//! update: on a computer shared by people of different organisations, a policy
//! (`/etc/nuvira/policy.json`, `"server"`) fixes the server. The users' files are
//! read as root, so only regular files, never through a link, and only small ones.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nuvira_update::unix::PackageKind;

const CACHE: &str = "/var/cache/nuvira";

/// Home folders of the people using this computer (uid 1000 and up), from /etc/passwd.
fn homes(passwd: &str) -> Vec<PathBuf> {
    passwd
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let uid: u32 = fields.get(2)?.parse().ok()?;
            let home = fields.get(5)?;
            ((1000..65534).contains(&uid) && home.starts_with('/')).then(|| PathBuf::from(home))
        })
        .collect()
}

/// The largest user file read here (a unit is a few hundred bytes).
const USER_FILE_MAX: u64 = 64 * 1024;

/// A file in a user's home folder, read as root: only if it is a regular file
/// (not a link, a FIFO that would hang the timer, a device such as /dev/zero)
/// of at most [`USER_FILE_MAX`] bytes.
fn read_user_file(path: &Path) -> Option<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    // O_NONBLOCK: opening a FIFO does not wait for a writer.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > USER_FILE_MAX {
        return None;
    }
    let mut text = String::new();
    file.take(USER_FILE_MAX).read_to_string(&mut text).ok()?;
    Some(text)
}

/// A user's flag file is there: a regular file, not a link to something else.
fn user_flag(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// The `--server` of a user service definition.
fn server_of_unit(unit: &str) -> Option<String> {
    let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    let rest = &exec[exec.find("--server ")? + "--server ".len()..];
    Some(match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?.to_string(),
        None => rest.split_whitespace().next()?.to_string(),
    })
}

pub fn run() -> Result<()> {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!("run by root's nuvira-update.timer");
    }
    let Some(kind) = PackageKind::installed() else {
        bail!("NuviraHub is not installed from a .deb or .rpm package");
    };
    let homes = homes(&std::fs::read_to_string("/etc/passwd").unwrap_or_default());
    // Installed for the whole computer there is no user unit: without this the
    // timer asked the public server instead of the organisation's (Kubuntu,
    // 2026-11-09).
    let server = crate::policy::read()
        .server
        .filter(|s| !s.is_empty())
        .or_else(|| crate::managed::state().map(|s| s.server))
        .or_else(|| {
            homes.iter().find_map(|h| {
                let unit = read_user_file(&h.join(".config/systemd/user/nuvira-agent.service"))?;
                server_of_unit(&unit)
            })
        })
        .unwrap_or_default();
    let server = nuvira_transport::signal::server_or_public(&server).to_string();
    let base = nuvira_update::base_url(&server, None, nuvira_update::DEFAULT_PORT);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let Some(manifest) =
        rt.block_on(nuvira_update::check_channel(&base, &nuvira_update::PUBLIC_KEYS, kind.channel()))?
    else {
        println!("up to date ({}, server {server})", nuvira_update::CURRENT);
        return Ok(());
    };
    println!("version {} available on {server} (this is {})", manifest.version, nuvira_update::CURRENT);

    let cache = Path::new(CACHE);
    std::fs::create_dir_all(cache)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o700))?;
    }
    let file = cache.join(format!("nuvira-{}{}", manifest.version, nuvira_update::extension(kind.channel())));
    // Older downloads.
    for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
        if entry.path() != file {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    // A file already there is installed only if it is this version's package (APP-8).
    if rt.block_on(nuvira_update::download_unless_cached(&base, &manifest, &file))? {
        println!("downloaded and checked");
    }
    if let Some(home) = homes.iter().find(|h| user_flag(&h.join(".config/nuvira/busy"))) {
        println!("session in progress ({}): next time", home.display());
        return Ok(());
    }
    nuvira_update::unix::install_package(kind, &file).context("installing the update")?;
    let _ = std::fs::remove_file(&file);
    println!("version {} installed", manifest.version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_and_servers() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      lp:x:7:7:lp:/var/spool/lpd:/usr/sbin/nologin\n\
                      alessandro:x:1000:1000:Alessandro:/home/alessandro:/bin/bash\n\
                      nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";
        assert_eq!(homes(passwd), vec![PathBuf::from("/home/alessandro")]);
        let unit = "[Service]\nExecStart=\"/home/a/.local/share/nuvira/nuvira\" agent --ipc --server \"nuvira.example.com:21116\" --server-key ab\n";
        assert_eq!(server_of_unit(unit).as_deref(), Some("nuvira.example.com:21116"));
        assert_eq!(server_of_unit("ExecStart=/x agent --ipc --server \"\"\n").as_deref(), Some(""));
        assert_eq!(server_of_unit("ExecStart=/x agent --ipc\n"), None);
    }

    /// APP-19: users' files are read only when they are small regular files.
    #[test]
    fn user_files_only_regular_and_small() {
        let dir = std::env::temp_dir().join(format!("nuvira-user-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let unit = dir.join("unit");
        std::fs::write(&unit, "ExecStart=/x agent --ipc --server \"a.example\"\n").unwrap();
        assert_eq!(read_user_file(&unit).as_deref().and_then(server_of_unit).as_deref(), Some("a.example"));
        assert!(user_flag(&unit));

        // A link (to a readable file, or to /dev/zero that never ends).
        let link = dir.join("link");
        std::os::unix::fs::symlink(&unit, &link).unwrap();
        assert_eq!(read_user_file(&link), None);
        assert!(!user_flag(&link));
        let zero = dir.join("zero");
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        assert_eq!(read_user_file(&zero), None);
        assert_eq!(read_user_file(Path::new("/dev/zero")), None, "not a regular file");

        // A FIFO with no writer: refused at once, not waited on.
        let fifo = dir.join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid, NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(read_user_file(&fifo), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!user_flag(&fifo));

        // Too large.
        let big = dir.join("big");
        std::fs::write(&big, vec![b'x'; USER_FILE_MAX as usize + 1]).unwrap();
        assert_eq!(read_user_file(&big), None);
        // A folder, and nothing.
        assert_eq!(read_user_file(&dir), None);
        assert!(!user_flag(&dir));
        assert!(!user_flag(&dir.join("missing")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
