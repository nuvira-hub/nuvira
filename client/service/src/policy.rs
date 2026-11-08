//! Settings an administrator sets for the computers of an organisation. They win
//! over the settings of the service and of the app.
//!
//! - Windows: Group Policy or Intune (the NuviraHub ADMX template,
//!   deploy/windows/policy), values under `HKLM\SOFTWARE\Policies\nuvira`,
//!   readable by every user, writable only by administrators and by the system
//!   applying the policies.
//! - Linux: `/etc/nuvira/policy.json`, put there by the organisation's tools
//!   (Ansible, Puppet, Salt, Landscape…): `{"server": …, "server_key": …,
//!   "password_hash": …, "profile": …}`. Only a file of root's that nobody else can
//!   change counts.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// NuviraHub server (host[:port]).
    pub server: Option<String>,
    /// Its certificate fingerprint.
    pub server_key: Option<String>,
    /// Permanent password for unattended access, as a hash (`nuvira service password-hash`):
    /// the policies are readable by every user, so never the password itself.
    pub password_hash: Option<String>,
    /// Profile of a customised installer of the Pro server (logo, colours, contacts).
    pub profile: Option<String>,
}

impl Policy {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Empty values count as not set; a password that is not a hash is ignored.
    fn cleaned(self) -> Self {
        let set = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Self {
            server: set(self.server),
            server_key: set(self.server_key),
            password_hash: set(self.password_hash).filter(|h| nuvira_secret::is_hash(h)),
            profile: set(self.profile),
        }
    }
}

/// The policies in place.
#[cfg(windows)]
pub fn read() -> Policy {
    Policy {
        server: windows_value("Server"),
        server_key: windows_value("ServerKey"),
        password_hash: windows_value("PasswordHash"),
        profile: windows_value("Profile"),
    }
    .cleaned()
}

#[cfg(windows)]
fn windows_value(name: &str) -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HSTRING, w};

    let name = HSTRING::from(name);
    let mut len = 0u32;
    let key = w!("SOFTWARE\\Policies\\nuvira");
    unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key, &name, RRF_RT_REG_SZ, None, None, Some(&mut len)) }
        .ok()
        .ok()?;
    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            &name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    }
    .ok()
    .ok()?;
    let text = String::from_utf16_lossy(&buf[..(len as usize / 2).min(buf.len())]);
    Some(text.trim_end_matches('\0').to_string())
}

/// The file of the policies on Linux.
#[cfg(target_os = "linux")]
pub const POLICY_FILE: &str = "/etc/nuvira/policy.json";

/// The policies in place.
#[cfg(target_os = "linux")]
pub fn read() -> Policy {
    read_file(std::path::Path::new(POLICY_FILE))
}

#[cfg(target_os = "linux")]
fn read_file(path: &std::path::Path) -> Policy {
    #[derive(serde::Deserialize)]
    struct File {
        server: Option<String>,
        server_key: Option<String>,
        password_hash: Option<String>,
        profile: Option<String>,
    }
    if !root_only(path) {
        return Policy::default();
    }
    let Ok(text) = std::fs::read_to_string(path) else { return Policy::default() };
    match serde_json::from_str::<File>(&text) {
        Ok(f) => Policy { server: f.server, server_key: f.server_key, password_hash: f.password_hash, profile: f.profile }
            .cleaned(),
        Err(e) => {
            tracing::warn!("{} ignored: {e}", path.display());
            Policy::default()
        }
    }
}

/// The file and its folders belong to root and nobody else can write them
/// (otherwise any user could choose the server and the password).
#[cfg(target_os = "linux")]
fn root_only(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if !path.exists() {
        return false;
    }
    let mut current = Some(path);
    while let Some(p) = current {
        match std::fs::metadata(p) {
            Ok(m) if m.uid() == 0 && m.mode() & 0o022 == 0 => {}
            Ok(_) => {
                if cfg!(not(test)) {
                    tracing::warn!("{} ignored: it must belong to root and be writable by root only", path.display());
                }
                return false;
            }
            Err(_) => return false,
        }
        current = p.parent().filter(|p| !p.as_os_str().is_empty());
    }
    true
}

/// macOS: managed preferences (a `.mobileconfig` profile of an MDM) are not read yet.
#[cfg(target_os = "macos")]
pub fn read() -> Policy {
    Policy::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_values_are_not_set() {
        let p = Policy { server: Some("  ".into()), password_hash: Some("clear text".into()), ..Default::default() };
        assert!(p.cleaned().is_empty());
        let p = Policy { server: Some(" nuvira.example.com ".into()), ..Default::default() };
        assert_eq!(p.cleaned().server.as_deref(), Some("nuvira.example.com"));
    }
}
