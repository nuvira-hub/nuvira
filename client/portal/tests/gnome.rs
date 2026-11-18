//! Phase 2 characterisation of `nuvira-portal` on a real GNOME Wayland session:
//! Mutter's DisplayConfig (read only) and the remote desktop session through
//! GNOME's own API (`set_direct(true)`, as unattended access opens it, no
//! dialog), plus the portal path refusing to ask when told not to.
//!
//! Needs the person's GNOME session: only with `NUVIRA_TEST_SCREEN=1` and
//! `WAYLAND_DISPLAY` set, on a test machine (MSI-UBUNTU, VM Ubuntu/Fedora).
//! Without them: `NOT RUN (needs ...)`, not a pass. Nothing is changed (the
//! resolution test that changes modes is `capture/tests/resolution.rs`).

#![cfg(target_os = "linux")]

use std::io::Write;
use std::sync::Arc;

use nuvira_portal::display_config;

fn report(text: &str) {
    let _ = writeln!(std::io::stderr(), "RESULT {text}");
}

fn gnome_gate() -> bool {
    let screen = std::env::var("NUVIRA_TEST_SCREEN").as_deref() == Ok("1");
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let gnome = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_ascii_uppercase().contains("GNOME"));
    if screen && wayland && gnome {
        return true;
    }
    let _ = writeln!(
        std::io::stderr(),
        "NOT RUN (needs a GNOME Wayland session: NUVIRA_TEST_SCREEN=1, WAYLAND_DISPLAY and XDG_CURRENT_DESKTOP=GNOME)"
    );
    false
}

#[test]
fn display_config_lists_the_monitors() {
    if !gnome_gate() {
        return;
    }
    assert!(display_config::available(), "Mutter lets programs change the monitors");
    let monitors = display_config::monitors().expect("monitors");
    report(&format!("{} monitors", monitors.len()));
    assert!(!monitors.is_empty());
    assert!(monitors[0].primary || monitors.iter().all(|m| !m.primary), "the primary one first");
    for m in &monitors {
        let current = m.mode(&m.current).expect("current mode among the modes");
        report(&format!(
            "{} at {},{} scale {} transform {}: current {}x{}@{:.2}, {} modes",
            m.connector,
            m.x,
            m.y,
            m.scale,
            m.transform,
            current.width,
            current.height,
            current.refresh,
            m.modes.len()
        ));
        let id =
            display_config::mode_for(m, current.width, current.height).expect("a mode of the current size");
        let chosen = m.mode(&id).unwrap();
        assert_eq!((chosen.width, chosen.height), (current.width, current.height));
        // The refresh closest to the current one: the current one itself.
        assert!(
            (chosen.refresh - current.refresh).abs() < 1e-6,
            "same refresh: {} vs {}",
            chosen.refresh,
            current.refresh
        );
        assert!(display_config::mode_for(m, 12_345, 6_789).is_err(), "no such mode");
    }
}

#[test]
fn sessions_direct_reuse_forget_and_refuse() {
    if !gnome_gate() {
        return;
    }
    let token = std::env::temp_dir().join(format!("nuvira-portal-token-{}", std::process::id()));
    let _ = std::fs::remove_file(&token);

    // The portal, not allowed to ask, without a saved permission: refused at once.
    nuvira_portal::set_direct(false);
    let started = std::time::Instant::now();
    let err = nuvira_portal::session(token.clone(), false).err().expect("no permission, no dialog");
    report(&format!("portal without permission: {err:#} in {:?}", started.elapsed()));
    assert!(format!("{err:#}").contains("no saved remote desktop permission"), "{err:#}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(nuvira_portal::current().is_none());

    // GNOME's own API: no dialog, no token needed.
    nuvira_portal::set_direct(true);
    let a = nuvira_portal::session(token.clone(), false).expect("Mutter remote desktop session");
    report(&format!("screens {:?}", a.screens()));
    assert!(a.is_open());
    assert!(!a.screens().is_empty());
    let s = a.screen();
    assert!(s.width > 0 && s.height > 0 && s.node_id > 0);
    assert_eq!(
        a.pixel_rect(0),
        Some((s.left, s.top, s.width, s.height)),
        "logical size until the stream tells"
    );
    a.set_pixels(0, s.width * 2, s.height * 2);
    assert_eq!(
        a.pixel_rect(0),
        Some((s.left * 2, s.top * 2, s.width * 2, s.height * 2)),
        "scaled by pixels/logical"
    );
    a.set_pixels(0, s.width, s.height);
    assert!(a.pipewire().is_ok(), "a PipeWire connection for the stream");
    assert!(!token.exists(), "the direct API saves no token");

    // The open session is reused while it lives.
    let b = nuvira_portal::session(token.clone(), false).expect("again");
    assert!(Arc::ptr_eq(&a, &b), "reused");
    assert!(nuvira_portal::current().is_some_and(|c| Arc::ptr_eq(&c, &a)));

    // forget: the next one is new, the old one lives on with its users.
    nuvira_portal::forget();
    assert!(nuvira_portal::current().is_none());
    let c = nuvira_portal::session(token.clone(), false).expect("a new one after forget");
    assert!(!Arc::ptr_eq(&a, &c), "a new session after forget");
    assert!(a.is_open(), "the old one still open while held");
    drop((a, b));
    assert!(c.is_open());
    drop(c);
    assert!(nuvira_portal::current().is_none(), "closed with its last Arc");
    nuvira_portal::set_direct(false);
    let _ = std::fs::remove_file(&token);
}
