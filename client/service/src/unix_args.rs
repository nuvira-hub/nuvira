//! The options of the Linux and macOS subcommands (`nuvira service install`,
//! `deploy`, `printer`, `system`): `--name value` and `--name=value`. A typo,
//! an option of another subcommand, a missing value or an option taken as the
//! value of the one before it are refused: these commands are run by scripts
//! (Ansible, MDM), and ignoring an option silently could install a computer on
//! the wrong server.

use anyhow::{Result, bail};

/// What a subcommand was given.
#[derive(Debug, Default)]
pub(crate) struct Options {
    values: Vec<(String, String)>,
    flags: Vec<String>,
}

impl Options {
    /// The options after the subcommand: `values` take a value, `flags` none
    /// (names without the leading `--`).
    pub(crate) fn parse(args: &[String], values: &[&str], flags: &[&str]) -> Result<Self> {
        let mut options = Self::default();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let Some(option) = arg.strip_prefix("--").filter(|o| !o.is_empty()) else {
                bail!("unexpected argument {arg:?}");
            };
            let (name, inline) = match option.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (option, None),
            };
            if options.given(name) {
                bail!("--{name} is given twice");
            }
            if values.contains(&name) {
                let value = match inline {
                    Some(value) => value,
                    None => rest.next().map(String::as_str).unwrap_or("--"),
                };
                if value.starts_with("--") {
                    bail!("--{name} needs a value");
                }
                options.values.push((name.to_string(), value.to_string()));
            } else if flags.contains(&name) {
                if inline.is_some() {
                    bail!("--{name} takes no value");
                }
                options.flags.push(name.to_string());
            } else {
                bail!("unknown option --{name}");
            }
        }
        Ok(options)
    }

    fn given(&self, name: &str) -> bool {
        self.values.iter().any(|(n, _)| n == name) || self.flags.iter().any(|f| f == name)
    }

    pub(crate) fn value(&self, name: &str) -> Option<String> {
        self.values.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }

    pub(crate) fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// Nothing but the flag `name`.
    pub(crate) fn only(&self, name: &str) -> bool {
        self.values.is_empty() && self.flags.iter().all(|f| f == name)
    }
}

/// A permanent password read from a file (removed afterwards), with the same
/// rule as the app and `deploy`: at least 8 characters.
#[cfg(unix)]
pub(crate) fn password_file(file: &str) -> Result<String> {
    let text = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
    let _ = std::fs::remove_file(file);
    let password = text.trim().to_string();
    if password.chars().count() < 8 {
        bail!("the password in {file} must have at least 8 characters");
    }
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Options> {
        let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        Options::parse(&args, &["server", "server-key", "password-file"], &["remove"])
    }

    fn error(line: &str) -> String {
        format!("{:#}", parse(line).expect_err(line))
    }

    #[test]
    fn both_forms_of_a_value() {
        let o = parse("--server a.example --server-key=ab:cd").unwrap();
        assert_eq!(o.value("server").as_deref(), Some("a.example"));
        assert_eq!(o.value("server-key").as_deref(), Some("ab:cd"));
        assert_eq!(o.value("password-file"), None);
        assert!(!o.flag("remove"));
        // An empty value (the public server) in both forms.
        let args = vec!["--server".to_string(), String::new()];
        let o = Options::parse(&args, &["server"], &[]).unwrap();
        assert_eq!(o.value("server").as_deref(), Some(""));
        assert_eq!(parse("--server=").unwrap().value("server").as_deref(), Some(""));
        // '=' inside the value is kept.
        assert_eq!(parse("--server=a=b").unwrap().value("server").as_deref(), Some("a=b"));
        assert!(parse("").unwrap().only("remove"));
    }

    #[test]
    fn flags() {
        let o = parse("--remove").unwrap();
        assert!(o.flag("remove") && o.only("remove"));
        assert!(!parse("--remove --server x").unwrap().only("remove"));
        assert_eq!(error("--remove=yes"), "--remove takes no value");
    }

    #[test]
    fn mistakes_are_refused() {
        assert_eq!(error("--sever a.example"), "unknown option --sever");
        assert_eq!(error("--password-hash x"), "unknown option --password-hash");
        assert_eq!(error("--server"), "--server needs a value");
        assert_eq!(error("--server --server-key ab"), "--server needs a value");
        assert_eq!(error("--server=--server-key"), "--server needs a value");
        assert_eq!(error("--server a --server b"), "--server is given twice");
        assert_eq!(error("--server=a --server b"), "--server is given twice");
        assert_eq!(error("a.example"), "unexpected argument \"a.example\"");
        assert_eq!(error("--server a b"), "unexpected argument \"b\"");
        assert_eq!(error("--"), "unexpected argument \"--\"");
        assert_eq!(error("-s a"), "unexpected argument \"-s\"");
    }

    #[cfg(unix)]
    #[test]
    fn passwords_from_a_file() {
        let dir = std::env::temp_dir().join(format!("nuvira-password-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p");
        let path = file.to_str().unwrap();
        for (text, ok) in
            [("", false), ("\n", false), ("   \n", false), ("short1\n", false), ("eight ch\n", true)]
        {
            std::fs::write(&file, text).unwrap();
            assert_eq!(password_file(path).is_ok(), ok, "{text:?}");
            assert!(!file.exists(), "removed after reading");
        }
        std::fs::write(&file, "  long enough  \n").unwrap();
        assert_eq!(password_file(path).unwrap(), "long enough");
        assert!(password_file(path).is_err(), "missing file");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
