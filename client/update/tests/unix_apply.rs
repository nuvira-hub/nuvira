//! Phase 2 characterisation of the Unix update swap (`nuvira_update::unix`)
//! through its public API, on real files (no desktop needed; Linux and macOS).
//!
//! Pinned (v0.28.0), beyond the unit tests in `src/unix.rs`:
//! - a program in a folder this user can write is `Install::Program`, one in a
//!   read-only folder is `ReadOnly` and `apply` refuses it with its path;
//!   `/usr/bin/nuvira` is `Package` only when a package says so;
//! - the new program is tried with exactly `agent --version`, gets mode 0755,
//!   the download stays where it was, no `.nuvira.new` is left either way, the
//!   old program goes to `nuvira.old` (replacing an older one), and a process
//!   holding the old file keeps reading the old contents;
//! - a missing download fails without touching the program;
//! - macOS: an unsigned bundle in the zip is refused ("signature of the new app")
//!   and the installed bundle stays (macOS only, a manual entry until a Mac).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nuvira_update::unix::{Install, PackageKind, Replaced, apply, clean, this_install};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nuvira-update-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn script(path: &Path, body: &str, mode: u32) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn install_kind_follows_the_folder() {
    let d = dir("kind");
    let exe = d.join("nuvira");
    script(&exe, "echo old", 0o755);
    assert_eq!(this_install(&exe), Install::Program(exe.clone()));
    assert_eq!(this_install(&exe).channel(), Some(nuvira_update::platform()));

    // root can write anywhere: the read-only case is a normal user's.
    if unsafe { libc::geteuid() } != 0 {
        let ro = d.join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        let ro_exe = ro.join("nuvira");
        script(&ro_exe, "echo old", 0o755);
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let install = this_install(&ro_exe);
        assert_eq!(install, Install::ReadOnly(ro_exe.clone()));
        assert_eq!(install.channel(), None);
        let new = d.join("new");
        script(&new, "echo new", 0o755);
        let err = apply(&install, &new).unwrap_err();
        assert!(format!("{err:#}").contains("cannot be replaced by this user"), "{err:#}");
        assert!(std::fs::read_to_string(&ro_exe).unwrap().contains("old"));
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // /usr/bin/nuvira: the package's when one is installed, else read-only.
    let usr = this_install(Path::new("/usr/bin/nuvira"));
    match PackageKind::installed() {
        Some(kind) => assert_eq!(usr, Install::Package(kind)),
        None if unsafe { libc::geteuid() } != 0 => {
            assert_eq!(usr, Install::ReadOnly("/usr/bin/nuvira".into()))
        }
        None => {}
    }
    for kind in [PackageKind::Deb, PackageKind::Rpm] {
        let err = apply(&Install::Package(kind), &exe).unwrap_err();
        assert!(format!("{err:#}").contains("root's timer"), "{err:#}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn program_swap_details() {
    let d = dir("swap");
    let exe = d.join("nuvira");
    let args_log = d.join("args.txt");
    script(&exe, "echo v1", 0o755);
    let install = Install::Program(exe.clone());

    // The old program held open (a running process keeps its file).
    let held = std::fs::File::open(&exe).unwrap();

    // A download without execute permission, which logs its arguments.
    let v2 = d.join("download-v2");
    script(&v2, &format!("echo \"$@\" >> {}\necho v2", args_log.display()), 0o644);
    let replaced = Replaced::watch(&exe);
    apply(&install, &v2).expect("apply v2");
    assert_eq!(
        std::fs::read_to_string(&args_log).unwrap(),
        "agent --version\n",
        "tried with agent --version once"
    );
    let mode = std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "new program executable");
    assert!(std::fs::read_to_string(&exe).unwrap().contains("v2"));
    assert!(v2.exists(), "the download is copied, not moved");
    assert!(!d.join(".nuvira.new").exists(), "no temporary file left");
    let old = d.join("nuvira.old");
    assert!(std::fs::read_to_string(&old).unwrap().contains("v1"), "old program put aside");
    assert!(replaced.happened());
    let mut text = String::new();
    std::io::Read::read_to_string(&mut &held, &mut text).unwrap();
    assert!(text.contains("v1"), "an open handle still reads the old program");

    // A second update: nuvira.old now holds v2.
    let v3 = d.join("download-v3");
    script(&v3, "echo v3", 0o755);
    apply(&install, &v3).expect("apply v3");
    assert!(std::fs::read_to_string(&old).unwrap().contains("v2"));
    assert!(std::fs::read_to_string(&exe).unwrap().contains("v3"));

    // A program that does not start: refused, nothing changes, no temporary file.
    let broken = d.join("download-broken");
    script(&broken, "echo nope >&2\nexit 7", 0o755);
    let err = apply(&install, &broken).unwrap_err();
    assert!(format!("{err:#}").contains("does not start: nope"), "{err:#}");
    assert!(std::fs::read_to_string(&exe).unwrap().contains("v3"));
    assert!(!d.join(".nuvira.new").exists());

    // A missing download: refused, program intact.
    let err = apply(&install, &d.join("missing")).unwrap_err();
    assert!(format!("{err:#}").contains("copying into"), "{err:#}");
    assert!(std::fs::read_to_string(&exe).unwrap().contains("v3"));

    clean(&install);
    assert!(!old.exists(), "clean removes nuvira.old");
    let _ = std::fs::remove_dir_all(&d);
}

/// macOS: an unsigned bundle is not swapped in. Needs a Mac (`ditto`, `codesign`).
#[cfg(target_os = "macos")]
#[test]
fn unsigned_bundle_refused_on_macos() {
    let d = dir("bundle");
    let make = |root: &Path, text: &str| {
        let macos = root.join("NuviraHub.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        script(&macos.join("nuvira"), &format!("echo {text}"), 0o755);
    };
    let installed = d.join("Applications");
    make(&installed, "old");
    let bundle = installed.join("NuviraHub.app");
    let exe = bundle.join("Contents/MacOS/nuvira");
    assert_eq!(this_install(&exe), Install::Bundle(bundle.clone()));
    let src = d.join("src");
    make(&src, "new");
    let zip = d.join("update.zip");
    let ok = std::process::Command::new("/usr/bin/ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(src.join("NuviraHub.app"))
        .arg(&zip)
        .status()
        .unwrap()
        .success();
    assert!(ok, "ditto");
    let err = apply(&Install::Bundle(bundle.clone()), &zip).unwrap_err();
    assert!(format!("{err:#}").contains("signature of the new app"), "{err:#}");
    assert!(std::fs::read_to_string(&exe).unwrap().contains("old"));
    assert!(!installed.join(".NuviraHub-update").exists(), "work folder removed after a failure");
    let _ = std::fs::remove_dir_all(&d);
}
