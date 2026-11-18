//! Phase 2 characterisation: hiding and putting back the desktop wallpaper on a
//! real desktop (Windows, GNOME, Cinnamon, KDE Plasma).
//!
//! Changes the wallpaper of the session it runs in (and puts it back, also when
//! the test fails): run with `NUVIRA_TEST_SCREEN=1` only on the test machines.
//! Without it: `NOT RUN (needs ...)`, not a pass.
//!
//! Pinned: `hide` writes the current setting to the state file and hides the
//! picture; a second `hide` with the state file present changes nothing;
//! `restore` puts it back and deletes the file; `recover` does the same after
//! an agent that was killed; `restore` without a state file does nothing.

mod common;

use std::path::{Path, PathBuf};

use common::{gate, report};
use nuvira_capture::wallpaper;

/// What the desktop shows now, read the way a person would check it.
fn current() -> String {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        };
        let mut buf = [0u16; 1024];
        unsafe {
            SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                buf.len() as u32,
                Some(buf.as_mut_ptr().cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .expect("SPI_GETDESKWALLPAPER");
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }
    #[cfg(target_os = "linux")]
    {
        let running = |name: &str| {
            std::process::Command::new("pgrep").args(["-x", name]).output().is_ok_and(|o| o.status.success())
        };
        let gsettings = |schema: &str| {
            let out = std::process::Command::new("gsettings")
                .args(["get", schema, "picture-options"])
                .output()
                .expect("gsettings");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        if running("cinnamon") {
            gsettings("org.cinnamon.desktop.background")
        } else if running("gnome-shell") {
            gsettings("org.gnome.desktop.background")
        } else {
            let out = std::process::Command::new("busctl")
                .args(["--user", "call", "org.kde.plasmashell", "/PlasmaShell", "org.kde.PlasmaShell"])
                .args(["evaluateScript", "s"])
                .arg("var p=[];var d=desktops();for(var i=0;i<d.length;i++){p.push(d[i].wallpaperPlugin);}print(p.join(','));")
                .output()
                .expect("busctl");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    String::new()
}

/// Whatever happens, the wallpaper goes back.
struct PutBack(PathBuf);

impl Drop for PutBack {
    fn drop(&mut self) {
        if self.0.exists() {
            wallpaper::restore(&self.0);
        }
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap_or(Path::new(".")));
    }
}

#[test]
fn wallpaper_hidden_and_put_back() {
    if !gate("NUVIRA_TEST_SCREEN", "a real desktop session whose wallpaper the test may change") {
        return;
    }
    // macOS covers the wallpaper with black windows held by a helper that is
    // this same program (`helper_if_asked` in `main`): a libtest binary cannot
    // be it.
    if cfg!(target_os = "macos") {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"NOT RUN (needs the agent binary: macOS wallpaper helper is a child process of the program)
",
        );
        return;
    }
    assert!(wallpaper::available(), "wallpaper hiding available on this desktop");
    let dir = std::env::temp_dir().join(format!("nuvira-wallpaper-{}", std::process::id()));
    let state = dir.join("sub").join("wallpaper.state");
    let _guard = PutBack(state.clone());
    let before = current();
    report(&format!("wallpaper before: {before:?}"));

    // Hide: the state file (its folder created) holds what was there.
    wallpaper::hide(&state).expect("hide");
    let saved = std::fs::read_to_string(&state).expect("state file written");
    report(&format!("state file: {saved:?}"));
    let hidden = current();
    report(&format!("wallpaper hidden: {hidden:?}"));
    #[cfg(windows)]
    {
        assert_eq!(saved, format!("windows\n{before}"));
        assert_eq!(hidden, "", "no picture while hidden");
    }
    #[cfg(target_os = "linux")]
    {
        let kind = saved.split('\n').next().unwrap_or_default();
        assert!(["gnome", "cinnamon", "plasma"].contains(&kind), "state kind {kind}");
        if kind == "plasma" {
            assert!(hidden.contains("org.kde.color"), "Plasma: plain colour wallpaper while hidden");
        } else {
            assert_eq!(saved, format!("{kind}\n{before}"));
            assert_eq!(hidden, "'none'", "picture-options none while hidden");
        }
    }

    // A second hide with the state file there: nothing changes (the original
    // stays recorded, not the hidden one).
    wallpaper::hide(&state).expect("hide again");
    assert_eq!(std::fs::read_to_string(&state).unwrap(), saved);

    wallpaper::restore(&state);
    assert!(!state.exists(), "state file deleted once put back");
    assert_eq!(current(), before, "wallpaper put back");

    // Restore without a state file: nothing.
    wallpaper::restore(&state);
    assert_eq!(current(), before);

    // An agent killed while hidden: the next start's recover puts it back.
    wallpaper::hide(&state).expect("hide");
    assert_eq!(current(), hidden, "hidden again");
    wallpaper::recover(&state);
    assert!(!state.exists());
    assert_eq!(current(), before, "recover put the wallpaper back");
    // recover without a state file: nothing.
    wallpaper::recover(&state);
    assert_eq!(current(), before);
}
