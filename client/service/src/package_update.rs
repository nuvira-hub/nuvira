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
                let unit = std::fs::read_to_string(h.join(".config/systemd/user/nuvira-agent.service")).ok()?;
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
    if !file.exists() {
        rt.block_on(nuvira_update::download(&base, &manifest, &file))?;
        println!("downloaded and checked");
    }
    if let Some(home) = homes.iter().find(|h| h.join(".config/nuvira/busy").exists()) {
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
}
