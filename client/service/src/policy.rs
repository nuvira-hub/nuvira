//! Settings an administrator sets for the PCs of an organisation through Group Policy
//! or Intune (the NuviraHub ADMX template, deploy/windows/policy): values under
//! `HKLM\SOFTWARE\Policies\nuvira`, readable by every user, writable only by
//! administrators and by the system applying the policies. They win over the
//! settings of the service and of the app.

use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{HSTRING, w};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// NuviraHub server (host[:port]).
    pub server: Option<String>,
    /// Its certificate fingerprint.
    pub server_key: Option<String>,
    /// Permanent password for unattended access, as a hash (`nuvira service password-hash`):
    /// the key is readable by every user, so never the password itself.
    pub password_hash: Option<String>,
    /// Profile of a customised installer of the Pro server (logo, colours, contacts).
    pub profile: Option<String>,
}

impl Policy {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The policies in place (empty values count as not set).
pub fn read() -> Policy {
    Policy {
        server: value("Server"),
        server_key: value("ServerKey"),
        password_hash: value("PasswordHash").filter(|h| nuvira_secret::is_hash(h)),
        profile: value("Profile"),
    }
}

fn value(name: &str) -> Option<String> {
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
    Some(text.trim_end_matches('\0').trim().to_string()).filter(|v| !v.is_empty())
}
