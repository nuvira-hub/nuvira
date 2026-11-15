//! Rules of unattended access kept next to the device key (`access.json`), read
//! at every session so that a change counts at once:
//!
//! - **who may connect** (`allow`): device IDs of the technicians' computers;
//!   empty, everyone. It holds for the permanent passwords, and also for the
//!   temporary one with `allow_attended`;
//! - **two-factor code** (`totp`): with a secret, every permanent password also
//!   needs the 6-digit code of an authenticator app;
//! - **profiles** (`profiles`): more permanent passwords, each with what it does
//!   not allow (the session's switches, by name: input, clipboard, files…).
//!
//! The file has the rights of the folder it is in: with the Windows service
//! `%ProgramData%\nuvira` (administrators only); on Linux and macOS the person's
//! data folder, copied into the system's part for the sign-in screen.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const FILE: &str = "access.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccessRules {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<Allowed>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_attended: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
}

/// A technician's computer that may connect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Allowed {
    /// Its NuviraHub ID (digits; spaces are ignored).
    pub id: String,
    /// What the person calls it, only to show.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// A further permanent password.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// PBKDF2 hash, as the main permanent password's (`nuvira_secret::hash`).
    pub password_hash: String,
    /// Switches off for its sessions (`crate::switches` names).
    #[serde(default)]
    pub off: Vec<String>,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

impl AccessRules {
    /// The rules in `dir`; none (everything allowed) without the file. A file that
    /// cannot be read refuses every permanent password rather than ignoring rules
    /// someone wrote: `Err` with the reason.
    pub fn load(dir: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path(dir)) {
            Ok(text) => serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .map_err(|e| format!("{} is not valid: {e}", path(dir).display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path(dir).display())),
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let target = path(dir);
        let partial = target.with_extension("json.new");
        std::fs::write(&partial, text)?;
        std::fs::rename(&partial, &target)
    }

    /// Whether the computer `viewer_id` may connect (`attended`: with the
    /// temporary password, the support ticket or a hand-over).
    pub fn allows(&self, viewer_id: &str, attended: bool) -> bool {
        if self.allow.is_empty() || (attended && !self.allow_attended) {
            return true;
        }
        let id = digits(viewer_id);
        !id.is_empty() && self.allow.iter().any(|a| digits(&a.id) == id)
    }

    /// The profile whose password `given` is.
    pub fn profile(&self, given: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| {
            nuvira_secret::is_hash(&p.password_hash) && nuvira_secret::verify(given, &p.password_hash)
        })
    }

    /// Checked before saving: IDs, names and hashes as expected.
    pub fn validate(&self) -> Result<(), String> {
        for a in &self.allow {
            let id = digits(&a.id);
            if id.len() < 6 || a.id.chars().any(|c| !c.is_ascii_digit() && !c.is_whitespace()) {
                return Err(format!("not a NuviraHub ID: {}", a.id));
            }
        }
        if let Some(secret) = &self.totp
            && nuvira_secret::totp_code(secret, 0).is_none()
        {
            return Err("two-factor secret not valid".into());
        }
        for p in &self.profiles {
            if p.name.trim().is_empty() {
                return Err("a profile without a name".into());
            }
            if !nuvira_secret::is_hash(&p.password_hash) {
                return Err(format!("profile {}: password not stored as a hash", p.name));
            }
            if let Some(unknown) = p.off.iter().find(|n| crate::switches::bit(n).is_none()) {
                return Err(format!("profile {}: unknown permission {unknown}", p.name));
            }
        }
        Ok(())
    }
}

fn digits(id: &str) -> String {
    id.chars().filter(char::is_ascii_digit).collect()
}

/// What the app shows of the rules: no hashes, no secret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub allow: Vec<Allowed>,
    pub allow_attended: bool,
    pub two_factor: bool,
    pub profiles: Vec<ProfileView>,
    /// The file could not be read: the rules shown are empty, permanent passwords
    /// are refused until it is saved again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileView {
    pub name: String,
    pub off: Vec<String>,
}

impl View {
    /// The rules in `dir`, as the app shows them.
    pub fn of(dir: &Path) -> Self {
        match AccessRules::load(dir) {
            Ok(rules) => Self {
                allow: rules.allow,
                allow_attended: rules.allow_attended,
                two_factor: rules.totp.is_some(),
                profiles: rules
                    .profiles
                    .into_iter()
                    .map(|p| ProfileView { name: p.name, off: p.off })
                    .collect(),
                error: None,
            },
            Err(e) => Self { error: Some(e), ..Self::default() },
        }
    }
}

/// A change made in the app, applied where the file can be read and written (the
/// app cannot read the service's copy on Windows): what it does not know (hashes,
/// the secret) is kept from the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Edit {
    pub allow: Vec<Allowed>,
    pub allow_attended: bool,
    pub totp: TotpEdit,
    pub profiles: Vec<ProfileEdit>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TotpEdit {
    /// As it is.
    #[default]
    Keep,
    Off,
    /// A new secret, already confirmed with a code in the app.
    Set(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileEdit {
    pub name: String,
    pub off: Vec<String>,
    /// A new password's hash; without it the profile with this name keeps its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
}

impl Edit {
    pub fn apply(self, old: Option<&AccessRules>) -> Result<AccessRules, String> {
        let totp = match self.totp {
            TotpEdit::Keep => old.and_then(|o| o.totp.clone()),
            TotpEdit::Off => None,
            TotpEdit::Set(secret) => Some(secret),
        };
        let mut profiles = Vec::new();
        for p in self.profiles {
            let password_hash = match p.password_hash {
                Some(hash) => hash,
                None => old
                    .and_then(|o| o.profiles.iter().find(|q| q.name == p.name))
                    .map(|q| q.password_hash.clone())
                    .ok_or_else(|| format!("profile {}: a password is needed", p.name))?,
            };
            profiles.push(Profile { name: p.name.trim().to_string(), password_hash, off: p.off });
        }
        if let Some(name) = profiles.iter().enumerate().find_map(|(i, p)| {
            profiles[..i].iter().any(|q| q.name.eq_ignore_ascii_case(&p.name)).then(|| p.name.clone())
        }) {
            return Err(format!("two profiles called {name}"));
        }
        let allow = self
            .allow
            .into_iter()
            .map(|a| Allowed { id: digits(&a.id), name: a.name.trim().to_string() })
            .collect();
        let rules = AccessRules { allow, allow_attended: self.allow_attended, totp, profiles };
        rules.validate()?;
        Ok(rules)
    }

    /// Applies the change to the rules in `dir` and saves them.
    pub fn save_in(self, dir: &Path) -> Result<AccessRules, String> {
        // A file that cannot be read is replaced, but then nothing can be kept from it.
        let old = AccessRules::load(dir).ok();
        let rules = self.apply(old.as_ref())?;
        rules.save(dir).map_err(|e| format!("{}: {e}", path(dir).display()))?;
        Ok(rules)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        let dir = std::env::temp_dir().join(format!("nuvira-access-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(AccessRules::load(&dir), Ok(AccessRules::default()));
        let none = AccessRules::default();
        assert!(none.allows("123", false) && none.allows("", true));

        let rules = AccessRules {
            allow: vec![Allowed { id: "123 456 789".into(), name: "Office".into() }],
            totp: Some(nuvira_secret::totp_secret()),
            profiles: vec![Profile {
                name: "View only".into(),
                password_hash: nuvira_secret::hash("look-only-1"),
                off: vec!["input".into(), "files".into()],
            }],
            ..Default::default()
        };
        assert_eq!(rules.validate(), Ok(()));
        assert!(rules.allows("123456789", false));
        assert!(!rules.allows("987654321", false));
        assert!(!rules.allows("", false));
        // The temporary password: only with allow_attended.
        assert!(rules.allows("987654321", true));
        assert!(!AccessRules { allow_attended: true, ..rules.clone() }.allows("987654321", true));
        assert_eq!(rules.profile("look-only-1").map(|p| p.name.as_str()), Some("View only"));
        assert!(rules.profile("look-only-2").is_none());

        rules.save(&dir).unwrap();
        assert_eq!(AccessRules::load(&dir), Ok(rules.clone()));
        std::fs::write(path(&dir), "{ broken").unwrap();
        assert!(AccessRules::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);

        // A change from the app keeps what it does not know.
        rules.save(&dir).unwrap();
        let edit = Edit {
            allow: vec![Allowed { id: "123 456 789".into(), name: " Office ".into() }],
            profiles: vec![ProfileEdit {
                name: "View only".into(),
                off: vec!["input".into()],
                password_hash: None,
            }],
            ..Default::default()
        };
        let saved = edit.save_in(&dir).unwrap();
        assert_eq!(saved.totp, rules.totp);
        assert_eq!(saved.allow[0], Allowed { id: "123456789".into(), name: "Office".into() });
        assert_eq!(saved.profiles[0].password_hash, rules.profiles[0].password_hash);
        assert_eq!(saved.profiles[0].off, ["input"]);
        let view = View::of(&dir);
        assert!(view.two_factor && view.error.is_none());
        assert_eq!(view.profiles, [ProfileView { name: "View only".into(), off: vec!["input".into()] }]);
        let off = Edit { totp: TotpEdit::Off, ..Default::default() }.save_in(&dir).unwrap();
        assert!(off.totp.is_none() && off.profiles.is_empty());
        // A new profile needs a password; two with one name are refused.
        let new = ProfileEdit { name: "New".into(), ..Default::default() };
        assert!(Edit { profiles: vec![new], ..Default::default() }.apply(None).is_err());
        let hash = nuvira_secret::hash("p");
        let twin = |n: &str| ProfileEdit { name: n.into(), off: vec![], password_hash: Some(hash.clone()) };
        assert!(Edit { profiles: vec![twin("A"), twin("a")], ..Default::default() }.apply(None).is_err());
        let _ = std::fs::remove_dir_all(&dir);

        for bad in [
            AccessRules {
                allow: vec![Allowed { id: "12a".into(), ..Default::default() }],
                ..Default::default()
            },
            AccessRules { totp: Some("!!".into()), ..Default::default() },
            AccessRules {
                profiles: vec![Profile { name: "x".into(), password_hash: "clear".into(), off: vec![] }],
                ..Default::default()
            },
            AccessRules {
                profiles: vec![Profile {
                    name: "x".into(),
                    password_hash: nuvira_secret::hash("p"),
                    off: vec!["teleport".into()],
                }],
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
