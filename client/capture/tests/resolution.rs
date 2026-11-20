//! Phase 2 characterisation: changing a real screen's resolution for a session
//! and putting it back (Windows ChangeDisplaySettingsEx, X11 RandR, GNOME
//! DisplayConfig).
//!
//! Changes the resolution of the machine it runs on and restores it (also when
//! the test fails): only with `NUVIRA_TEST_SCREEN=1` and
//! `NUVIRA_TEST_RESOLUTION=1`, on the test machines. Without them: `NOT RUN`.
//!
//! Pinned: `fit` takes the best mode for the area and writes the original to the
//! state file given to `recover`; a second session is refused while the first
//! one holds the screen (`@agent.resolution_busy`); the capture sees the new
//! size; `restore` puts the original back and deletes the state file; after an
//! agent that was killed, `recover` of the state file puts the original back.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{gates, report};
use nuvira_capture::resolution::{self, Size};

/// Whatever happens, every session's change goes back.
struct PutBack(PathBuf);

impl Drop for PutBack {
    fn drop(&mut self) {
        for owner in 1..=3 {
            resolution::restore(owner, None);
        }
        resolution::recover(&self.0);
        let _ = std::fs::remove_file(&self.0);
    }
}

fn primary() -> nuvira_capture::Display {
    common::refresh_displays();
    let list = common::displays();
    list.iter().find(|d| d.primary).or(list.first()).cloned().expect("a display")
}

/// The frame size the capture gives now (a new capture).
fn captured_size() -> (u32, u32) {
    common::refresh_displays();
    let mut capturer = nuvira_capture::open(None).expect("capture");
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(Some(f)) = capturer.next_frame(Duration::from_millis(200)) {
            return (f.width, f.height);
        }
    }
    capturer.last_frame().map(|f| (f.width, f.height)).expect("a frame")
}

#[test]
fn resolution_fitted_refused_to_others_and_put_back() {
    if !gates(&[
        ("NUVIRA_TEST_SCREEN", "a real desktop session"),
        ("NUVIRA_TEST_RESOLUTION", "permission to change the screen resolution of this machine"),
    ]) {
        return;
    }
    let _ = tracing_subscriber::fmt().with_env_filter("nuvira_capture=debug").try_init();
    #[cfg(windows)]
    {
        nuvira_input::set_dpi_awareness();
        common::win::wake_display();
    }
    #[cfg(target_os = "linux")]
    {
        // Wayland without GNOME (KDE Plasma): no remote resolution at all.
        let gnome =
            std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_ascii_uppercase().contains("GNOME"));
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && !gnome {
            assert!(!resolution::supported(), "not supported on a Wayland desktop other than GNOME");
            let d = nuvira_capture::Display {
                index: 0,
                name: "Screen".into(),
                left: 0,
                top: 0,
                width: 1280,
                height: 800,
                primary: true,
            };
            let err = resolution::fit(1, &d, Size { width: 800, height: 600 }).expect_err("refused");
            assert!(format!("{err:#}").contains("@agent.resolution_unavailable"), "{err:#}");
            report(
                "Wayland without GNOME: resolution unsupported, fit refused with @agent.resolution_unavailable",
            );
            return;
        }
        nuvira_portal::set_direct(true);
    }
    assert!(resolution::supported(), "resolution changes supported here");

    let state = std::env::temp_dir().join(format!("nuvira-resolution-{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    // No state file: nothing to recover; from now on changes are written there.
    resolution::recover(&state);
    let _guard = PutBack(state.clone());

    let display = primary();
    let (modes, original) = resolution::modes(&display).expect("modes");
    report(&format!("display {display:?}: current {original}, modes {modes:?}"));
    // The largest mode below the current one (at least 800x600), as a viewer
    // window a little smaller than the screen would ask.
    let target = *modes
        .iter()
        .filter(|m| {
            m.width < original.width && m.height < original.height && m.width >= 800 && m.height >= 600
        })
        .max_by_key(|m| m.width as u64 * m.height as u64)
        .expect("a smaller mode to switch to");
    assert_eq!(resolution::choose(&modes, target), Some(target), "an exact mode is chosen as it is");

    assert!(!resolution::adapted(1, &display));
    let got = resolution::fit(1, &display, target).expect("fit");
    report(&format!("fitted to {got} for a {target} area"));
    assert_eq!(got, target);
    std::thread::sleep(Duration::from_secs(2));
    let display_now = primary();
    let (_, now) = resolution::modes(&display_now).expect("modes");
    assert_eq!(now, target, "the screen is at the new mode");
    assert!(resolution::adapted(1, &display_now));
    let record = std::fs::read_to_string(&state).expect("state file written by the first change");
    report(&format!("state file: {record:?}"));
    assert_eq!(record.lines().count(), 1);

    // The capture sees the new size (at 100% scaling the display size too).
    let size = captured_size();
    report(&format!("captured at {size:?}"));
    assert_eq!(size, (target.width, target.height));

    // Another session may not change it meanwhile.
    let other = resolution::fit(2, &display_now, original).expect_err("busy for another session");
    assert!(format!("{other:#}").contains("@agent.resolution_busy"), "{other:#}");
    // The same session may: back to the original mode is "already at" or a change.
    // A fit to the same mode is a no-op.
    assert_eq!(resolution::fit(1, &display_now, target).expect("same mode"), target);

    assert!(resolution::restore(1, Some(&display_now)), "something was put back");
    std::thread::sleep(Duration::from_secs(2));
    let display_back = primary();
    assert_eq!(resolution::modes(&display_back).expect("modes").1, original, "original mode back");
    assert!(!resolution::adapted(1, &display_back));
    assert!(!state.exists(), "state file deleted when nothing is changed any more");
    assert!(!resolution::restore(1, None), "nothing left to put back");

    // An agent killed while the screen was changed: the next start's recover.
    // Not on macOS: the change is tied to the process (the system puts the mode
    // back when it ends), so `recover` is a no-op there by design and cannot be
    // tried in the process that made the change.
    #[cfg(target_os = "macos")]
    report("macOS: recover step not run (the system restores the mode when the process ends)");
    #[cfg(not(target_os = "macos"))]
    killed_agent_recovered(&state, &display_back, target, original);
}

/// The last step of [`resolution_fitted_refused_to_others_and_put_back`]: a change
/// left behind (as by an agent that was killed) is put back by `recover`.
#[cfg(not(target_os = "macos"))]
fn killed_agent_recovered(
    state: &std::path::Path,
    display: &nuvira_capture::Display,
    target: Size,
    original: Size,
) {
    resolution::fit(3, display, target).expect("fit again");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(resolution::modes(&primary()).expect("modes").1, target);
    assert!(state.exists());
    resolution::recover(state);
    std::thread::sleep(Duration::from_secs(2));
    let after = resolution::modes(&primary()).expect("modes").1;
    report(&format!("after recover: {after}"));
    assert_eq!(after, original, "recover put the original mode back");
    assert!(!state.exists(), "recover deletes the state file");
}

/// Pure choice of the mode with a real screen's list (no change made).
#[test]
fn real_modes_choose_the_current_one_for_its_own_size() {
    if !common::gate("NUVIRA_TEST_SCREEN", "a real desktop session") {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let gnome =
            std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_ascii_uppercase().contains("GNOME"));
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && !gnome {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"NOT RUN (needs X11 or GNOME: no resolution modes on other Wayland desktops)
",
            );
            return;
        }
        nuvira_portal::set_direct(true);
    }
    let display = primary();
    let (modes, current) = resolution::modes(&display).expect("modes");
    assert!(modes.contains(&current), "the current mode is one of the modes");
    assert_eq!(resolution::choose(&modes, current), Some(current));
    let half = Size { width: current.width / 2, height: current.height / 2 };
    let chosen = resolution::choose(&modes, half).expect("a mode for half the size");
    report(&format!("{current}: for {half} -> {chosen}"));
    assert!(chosen.width <= current.width);
}
