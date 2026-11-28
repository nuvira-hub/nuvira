//! The NuviraHub computers of the local network ("On this network").
//!
//! An agent connected to its server announces itself with DNS-SD over mDNS
//! (RFC 6762/6763) as `<ID>._nuvira._udp.local`, with a TXT record that carries
//! its ID, computer name, operating system and a tag of its server; the app looks
//! for these announcements to list the computers it can connect to without
//! typing their ID. The connection itself is the usual one, through the server.
//!
//! Each system's own DNS-SD service does the work, so no program opens port 5353
//! by itself and no firewall rule is needed: the DNS Client service on Windows
//! (`DnsServiceRegister` / `DnsServiceBrowse`, Windows 10 1903 or later), Avahi
//! over D-Bus on Linux, mDNSResponder (Bonjour) on macOS. Where the service is
//! missing (Avahi not running) announcing and searching fail and the callers
//! carry on without the list.

use std::time::Duration;

use anyhow::Result;

#[cfg(target_os = "linux")]
mod avahi;
#[cfg(target_os = "macos")]
mod bonjour;
#[cfg(windows)]
mod windows_dns;

/// The DNS-SD service type, without the domain.
pub const SERVICE_TYPE: &str = "_nuvira._udp";
/// Version of the TXT record's content.
const TXT_VERSION: &str = "1";
/// Longest computer name announced (a TXT string holds at most 255 bytes).
const MAX_NAME: usize = 63;

/// What an agent announces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    /// The 9-digit ID on its server.
    pub id: String,
    /// The computer's name.
    pub name: String,
    /// `windows`, `macos` or `linux`.
    pub os: String,
    /// [`server_tag`] of the server the ID belongs to.
    pub server: String,
}

/// A computer found on the local network.
pub type Found = Announcement;

impl Announcement {
    /// The TXT record's strings (`key=value`).
    pub fn txt(&self) -> Vec<String> {
        vec![
            format!("v={TXT_VERSION}"),
            format!("id={}", self.id),
            format!("n={}", short_name(&self.name)),
            format!("os={}", self.os),
            format!("s={}", self.server),
        ]
    }

    /// Back from a TXT record; `None` when it is not a valid announcement.
    pub fn from_txt<S: AsRef<str>>(strings: &[S]) -> Option<Self> {
        let mut a =
            Announcement { id: String::new(), name: String::new(), os: String::new(), server: String::new() };
        for s in strings {
            let Some((key, value)) = s.as_ref().split_once('=') else { continue };
            match key {
                "id" => a.id = value.to_string(),
                "n" => a.name = short_name(value),
                "os" => a.os = value.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect(),
                "s" => a.server = value.chars().filter(|c| c.is_ascii_hexdigit()).take(16).collect(),
                _ => {}
            }
        }
        valid_id(&a.id).then_some(a)
    }

    /// The DNS-SD instance name: the ID, unique on the network.
    pub fn instance(&self) -> &str {
        &self.id
    }
}

/// An ID as the server gives them: 9 digits.
pub fn valid_id(id: &str) -> bool {
    id.len() == 9 && id.bytes().all(|b| b.is_ascii_digit())
}

fn short_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    let mut end = clean.len().min(MAX_NAME);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    clean[..end].to_string()
}

/// Tag of a server in the announcements: the start of its certificate's
/// fingerprint in hex, so that the app lists only computers it can reach with the
/// same ID (the same server), without naming the server on the network.
pub fn server_tag(fingerprint: &[u8]) -> String {
    fingerprint.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// The operating system's name in the announcements.
pub fn this_os() -> &'static str {
    std::env::consts::OS
}

/// An announcement in progress: it ends when this is dropped.
pub struct Announcer {
    #[cfg(windows)]
    _inner: windows_dns::Registration,
    #[cfg(target_os = "linux")]
    _inner: avahi::Registration,
    #[cfg(target_os = "macos")]
    _inner: bonjour::Registration,
}

/// Announces this computer on the local network until the [`Announcer`] is dropped.
pub async fn announce(a: &Announcement) -> Result<Announcer> {
    anyhow::ensure!(valid_id(&a.id), "not an ID: {}", a.id);
    #[cfg(windows)]
    {
        Ok(Announcer { _inner: windows_dns::register(a)? })
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Announcer { _inner: avahi::register(a).await? })
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Announcer { _inner: bonjour::register(a)? })
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("no DNS-SD on this system")
    }
}

/// The computers that announce themselves on the local network, gathered for
/// `wait`, sorted by name; each ID once.
pub async fn scan(wait: Duration) -> Result<Vec<Found>> {
    #[cfg(windows)]
    let found = windows_dns::browse(wait).await?;
    #[cfg(target_os = "linux")]
    let found = avahi::browse(wait).await?;
    #[cfg(target_os = "macos")]
    let found = bonjour::browse(wait).await?;
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let found: Vec<Found> = {
        let _ = wait;
        anyhow::bail!("no DNS-SD on this system")
    };
    Ok(tidy(found))
}

fn tidy(mut found: Vec<Found>) -> Vec<Found> {
    found.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
    let mut seen = std::collections::HashSet::new();
    found.retain(|f| seen.insert(f.id.clone()));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Announcement {
        Announcement {
            id: "123456789".into(),
            name: "Reception PC".into(),
            os: "windows".into(),
            server: "a1b2c3d4".into(),
        }
    }

    #[test]
    fn txt_round_trip() {
        let a = sample();
        assert_eq!(Announcement::from_txt(&a.txt()), Some(a));
    }

    #[test]
    fn txt_without_a_valid_id_is_ignored() {
        assert_eq!(Announcement::from_txt(&["v=1", "n=x"]), None);
        assert_eq!(Announcement::from_txt(&["id=12345678x"]), None);
        assert_eq!(Announcement::from_txt(&["id=1234567890"]), None);
    }

    #[test]
    fn txt_values_are_cleaned() {
        let long = "Ä".repeat(60);
        let a = Announcement::from_txt(&[
            "id=987654321".to_string(),
            format!("n=  {long}\u{7}  "),
            "os=win<dows>".to_string(),
            "s=zz12ab".to_string(),
            "unknown=1".to_string(),
            "noequals".to_string(),
        ])
        .unwrap();
        assert!(a.name.len() <= MAX_NAME);
        assert!(a.name.chars().all(|c| c == 'Ä'));
        assert_eq!(a.os, "windows");
        assert_eq!(a.server, "12ab");
    }

    #[test]
    fn server_tag_is_short_hex() {
        assert_eq!(server_tag(&[0xab, 0x01, 0xff, 0x10, 0x99]), "ab01ff10");
    }

    #[test]
    fn tidy_sorts_and_keeps_each_id_once() {
        let mut b = sample();
        b.id = "111111111".into();
        b.name = "archive".into();
        let mut c = sample();
        c.name = "Zeta".into();
        let out = tidy(vec![sample(), c, b.clone(), sample()]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], b);
        assert_eq!(out[1].id, "123456789");
    }
}
