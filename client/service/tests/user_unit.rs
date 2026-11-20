//! Phase 2 characterisation: Linux unattended access as a systemd user service
//! (`nuvira_service::user`), installed and removed for real in the user's own
//! systemd, then checked file by file.
//!
//! Installs a unit for the user running it (and the GNOME Shell extension on
//! GNOME), then removes them: only with `NUVIRA_TEST_SERVICE=1`, on a test
//! machine where NuviraHub's user service is NOT installed (the test refuses to
//! touch an existing one and says `NOT RUN`). The data folder is a temporary one
//! (`NUVIRA_DATA_DIR`). The unit's program is this test binary, which exits at
//! once with those arguments: systemd keeps restarting it every 5 s until the
//! test removes it.
//!
//! Pinned (v0.28.0): the unit and autostart texts, `service.env` mode 0600 with
//! the password hash, a generated 8-character password the first time only, the
//! password kept or replaced, the state reported, and an uninstall that removes
//! the unit, the autostart entry, `service.env` and the GNOME extension, and
//! takes the extension out of `enabled-extensions`. Since the review (P8, APP-16)
//! the session variables `start` put into the person's systemd are taken away
//! again at uninstall, and the ones the desktop had set already are kept.
//! On a computer installed for
//! everyone (`/etc/nuvira/managed.json`) both are refused with `@install.managed`.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use nuvira_service::user::{self, InstallOptions, ServiceState};

fn report(text: &str) {
    let _ = writeln!(std::io::stderr(), "RESULT {text}");
}

fn not_run(why: &str) {
    let _ = writeln!(std::io::stderr(), "NOT RUN (needs {why})");
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).expect("HOME")
}

fn gsettings(key: &str) -> Option<String> {
    let out = Command::new("gsettings").args(["get", "org.gnome.shell", key]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_active() -> String {
    let out =
        Command::new("systemctl").args(["--user", "is-active", "nuvira-agent.service"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const HASH_VAR: &str = "NUVIRA_PASSWORD_HASH=";

/// The environment of this user's systemd manager.
fn manager_environment() -> Vec<(String, String)> {
    let out = Command::new("systemctl").args(["--user", "show-environment"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

#[test]
fn user_service_installed_and_removed() {
    if std::env::var("NUVIRA_TEST_SERVICE").as_deref() != Ok("1") {
        not_run(
            "a test machine where the user's systemd may get a unit: set NUVIRA_TEST_SERVICE=1 to run it",
        );
        return;
    }
    let unit = home().join(".config/systemd/user/nuvira-agent.service");
    let autostart = home().join(".config/autostart/nuvira-agent.desktop");
    let extension = home().join(".local/share/gnome-shell/extensions/remote-access@nuvirahub.dev");
    let managed = std::path::Path::new("/etc/nuvira/managed.json").exists();
    let data = std::env::temp_dir().join(format!("nuvira-service-test-{}", std::process::id()));
    // SAFETY: the only test of this binary.
    unsafe { std::env::set_var("NUVIRA_DATA_DIR", &data) };
    let env = data.join("service.env");
    let opts = |password: Option<&str>| InstallOptions {
        server: "127.0.0.1:9".into(),
        server_key: Some("ab12".into()),
        password: password.map(str::to_string),
    };

    if managed {
        // Installed for the whole computer: a person's own install is refused.
        let err = user::install(&opts(None)).expect_err("refused when managed");
        assert!(format!("{err:#}").contains("@install.managed"), "{err:#}");
        let err = user::uninstall().expect_err("refused when managed");
        assert!(format!("{err:#}").contains("@install.managed"), "{err:#}");
        report("managed computer: install and uninstall refused with @install.managed");
        return;
    }
    if unit.exists() || autostart.exists() || extension.exists() {
        not_run("a machine without NuviraHub's user service (one is installed here: not touched)");
        return;
    }
    assert_eq!(user::service_state().unwrap(), ServiceState::NotInstalled);
    let enabled_before = gsettings("enabled-extensions");
    // P8: a session variable the person's systemd does not have yet, given to
    // this process: `start` imports it, `uninstall` takes it away again. One it
    // has already (the desktop's own, same value) stays.
    let environment_before = manager_environment();
    let added = ["WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY", "XDG_CURRENT_DESKTOP"]
        .into_iter()
        .find(|name| !environment_before.iter().any(|(n, _)| n == name));
    if let Some(name) = added {
        // SAFETY: the only test of this binary.
        unsafe { std::env::set_var(name, "nuvira-test-p8") };
    }
    let kept = environment_before.iter().find(|(n, _)| n == "XDG_RUNTIME_DIR").cloned();
    if let Some((name, value)) = &kept {
        // SAFETY: the only test of this binary.
        unsafe { std::env::set_var(name, value) };
    }

    let result = std::panic::catch_unwind(|| {
        // First install, no password: one is generated and returned.
        let generated = user::install(&opts(None)).expect("install").expect("a generated password");
        report(&format!("generated password of {} characters", generated.len()));
        assert_eq!(generated.len(), 8);
        assert!(generated.chars().all(|c| "abcdefghijkmnpqrstuvwxyz23456789".contains(c)));

        let exe = std::env::current_exe().unwrap();
        let text = std::fs::read_to_string(&unit).expect("unit written");
        report(&format!("unit:\n{text}"));
        assert_eq!(
            text,
            format!(
                "[Unit]\nDescription=NuviraHub agent (unattended access)\n\n[Service]\n\
                 ExecStart=\"{}\" agent --ipc --server \"127.0.0.1:9\" --server-key ab12\n\
                 EnvironmentFile={}\nRestart=always\nRestartSec=5\n",
                exe.display(),
                env.display()
            )
        );
        let desktop = std::fs::read_to_string(&autostart).expect("autostart written");
        assert_eq!(
            desktop,
            format!(
                "[Desktop Entry]\nType=Application\nName=NuviraHub unattended access\nExec=\"{}\" service start\n\
                 NoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            )
        );
        let mode = std::fs::metadata(&env).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "service.env private");
        let hash1 = std::fs::read_to_string(&env).unwrap();
        assert!(hash1.starts_with(HASH_VAR) && hash1.ends_with('\n'), "{hash1}");
        assert!(!hash1.contains(&generated), "only the hash is stored");

        std::thread::sleep(Duration::from_secs(1));
        let state = user::service_state().unwrap();
        report(&format!("state after install: {state:?} (systemctl: {})", is_active()));
        assert_ne!(state, ServiceState::NotInstalled);

        // GNOME: the extension is written and enabled (takes effect at the next sign-in).
        let gnome = Command::new("gnome-shell").arg("--version").output().is_ok_and(|o| o.status.success());
        if gnome {
            assert!(extension.join("extension.js").exists() && extension.join("metadata.json").exists());
            let enabled = gsettings("enabled-extensions").unwrap_or_default();
            assert!(enabled.contains("remote-access@nuvirahub.dev"), "{enabled}");
        }

        // Again without a password: the stored one is kept, nothing returned.
        assert_eq!(user::install(&opts(None)).expect("reinstall"), None);
        assert_eq!(std::fs::read_to_string(&env).unwrap(), hash1, "password kept");
        // With a password: replaced, nothing returned.
        assert_eq!(user::install(&opts(Some("new password 1"))).expect("reinstall"), None);
        let hash2 = std::fs::read_to_string(&env).unwrap();
        assert_ne!(hash2, hash1);
        assert!(!hash2.contains("new password 1"));
        assert_eq!(std::fs::metadata(&env).unwrap().permissions().mode() & 0o777, 0o600);

        // `start` (the autostart entry's) restarts it.
        user::start().expect("start");
        if let Some(name) = added {
            let value = manager_environment().into_iter().find(|(n, _)| n == name).map(|(_, v)| v);
            assert_eq!(value.as_deref(), Some("nuvira-test-p8"), "{name} imported");
        }

        user::uninstall().expect("uninstall");
        let environment_after = manager_environment();
        match added {
            Some(name) => {
                assert!(!environment_after.iter().any(|(n, _)| n == name), "{name} taken away at uninstall");
                report(&format!("P8: {name} imported by start, taken away by uninstall"));
            }
            None => report("P8: every variable is set in this systemd already: removal not checked"),
        }
        if let Some(kept) = &kept {
            assert!(environment_after.contains(kept), "{} kept", kept.0);
        }
        assert!(!unit.exists() && !autostart.exists() && !env.exists(), "files removed");
        assert!(!extension.exists(), "GNOME extension removed");
        assert_eq!(user::service_state().unwrap(), ServiceState::NotInstalled);
        std::thread::sleep(Duration::from_millis(500));
        let active = is_active();
        report(&format!("systemctl after uninstall: {active}"));
        assert_ne!(active, "active");
        // A second uninstall: nothing to do, no error.
        user::uninstall().expect("uninstall again");
    });
    // Whatever happened: nothing of the test stays.
    let _ = user::uninstall();
    if let Some(before) = &enabled_before
        && gsettings("enabled-extensions").as_ref() != Some(before)
    {
        let _ =
            Command::new("gsettings").args(["set", "org.gnome.shell", "enabled-extensions", before]).status();
    }
    let _ = std::fs::remove_dir_all(&data);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
    assert_eq!(gsettings("enabled-extensions"), enabled_before, "enabled-extensions as before");
}
