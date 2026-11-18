//! Phase 2 characterisation: macOS unattended access as a LaunchAgent
//! (`nuvira_service::user` is `launchd.rs` on macOS), installed and removed for
//! real in the user's GUI domain. Written without a Mac (none available in Phase
//! 2): a manual entry until one is rented.
//!
//! Only with `NUVIRA_TEST_SERVICE=1` on a test Mac where NuviraHub's
//! LaunchAgent is NOT installed (an existing one is never touched: `NOT RUN`),
//! in a terminal of the person's session (the GUI domain). The data folder is
//! temporary (`NUVIRA_DATA_DIR`); the job's program is this test binary, which
//! exits at once with those arguments, so launchd keeps restarting it every 5 s
//! until the test removes it.
//!
//! Pinned (v0.28.0): the plist (mode 0600) with the label, the program and its
//! arguments, the password hash in its environment (the same as service.env),
//! the job loaded, a generated 8-character password, and an uninstall that
//! boots the job out and removes the plist and service.env. On a Mac installed
//! for everyone both are refused with `@install.managed`.

#![cfg(target_os = "macos")]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use nuvira_service::user::{self, InstallOptions, ServiceState};

const LABEL: &str = "io.github.nuvira-hub.nuvira.agent";

fn report(text: &str) {
    let _ = writeln!(std::io::stderr(), "RESULT {text}");
}

fn not_run(why: &str) {
    let _ = writeln!(std::io::stderr(), "NOT RUN (needs {why})");
}

fn loaded() -> bool {
    let uid = unsafe { libc::getuid() };
    Command::new("/bin/launchctl")
        .args(["print", &format!("gui/{uid}/{LABEL}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn launch_agent_installed_and_removed() {
    if std::env::var("NUVIRA_TEST_SERVICE").as_deref() != Ok("1") {
        not_run("a test Mac whose GUI domain may get a LaunchAgent: set NUVIRA_TEST_SERVICE=1 to run it");
        return;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    let plist = home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"));
    let data = std::env::temp_dir().join(format!("nuvira-launchd-test-{}", std::process::id()));
    // SAFETY: the only test of this binary.
    unsafe { std::env::set_var("NUVIRA_DATA_DIR", &data) };
    let env = data.join("service.env");
    let opts = |password: Option<&str>| InstallOptions {
        server: "127.0.0.1:9".into(),
        server_key: Some("ab12".into()),
        password: password.map(str::to_string),
    };
    // `managed::installed()`: the state file of the installation for the whole Mac.
    if std::path::Path::new("/Library/Application Support/io.github.nuvira-hub.nuvira/managed.json").exists()
    {
        match user::install(&opts(None)) {
            Err(e) if format!("{e:#}").contains("@install.managed") => {
                report("managed Mac: install refused with @install.managed");
                return;
            }
            other => {
                let _ = user::uninstall();
                panic!("managed Mac: install not refused: {other:?}");
            }
        }
    }
    if plist.exists() {
        not_run("a Mac without NuviraHub's LaunchAgent (one is installed here: not touched)");
        return;
    }
    assert_eq!(user::service_state().unwrap(), ServiceState::NotInstalled);

    let result = std::panic::catch_unwind(|| {
        let generated = user::install(&opts(None)).expect("install").expect("a generated password");
        assert_eq!(generated.len(), 8);
        let text = std::fs::read_to_string(&plist).expect("plist written");
        report(&format!("plist:\n{text}"));
        assert_eq!(std::fs::metadata(&plist).unwrap().permissions().mode() & 0o777, 0o600, "plist private");
        let exe = std::env::current_exe().unwrap();
        assert!(text.contains(&format!("<string>{LABEL}</string>")));
        for arg in [
            exe.display().to_string().as_str(),
            "agent",
            "--ipc",
            "--server",
            "127.0.0.1:9",
            "--server-key",
            "ab12",
        ] {
            assert!(text.contains(&format!("<string>{arg}</string>")), "argument {arg}");
        }
        let hash_line = std::fs::read_to_string(&env).expect("service.env");
        let hash = hash_line.trim().strip_prefix("NUVIRA_PASSWORD_HASH=").expect("hash line");
        assert!(text.contains(&format!("<key>NUVIRA_PASSWORD_HASH</key>\n\t\t<string>{hash}</string>")));
        assert!(!text.contains(&generated), "only the hash");
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(text.contains("<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>"));
        std::thread::sleep(Duration::from_secs(1));
        assert!(loaded(), "job loaded in the GUI domain");
        assert_ne!(user::service_state().unwrap(), ServiceState::NotInstalled);

        assert_eq!(user::install(&opts(None)).expect("reinstall"), None, "password kept");
        user::start().expect("start (kickstart)");

        user::uninstall().expect("uninstall");
        std::thread::sleep(Duration::from_secs(1));
        assert!(!plist.exists() && !env.exists(), "plist and service.env removed");
        assert!(!loaded(), "job booted out");
        assert_eq!(user::service_state().unwrap(), ServiceState::NotInstalled);
        user::uninstall().expect("uninstall again");
    });
    let _ = user::uninstall();
    let _ = std::fs::remove_dir_all(&data);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
