//! Phase 2 characterisation of `nuvira-msi`: the package it writes, read back
//! through Windows Installer (msi.dll) without installing anything.
//!
//! Runs on any Windows PC (it only creates files in the temp folder). Pinned:
//! argument checks, the ProductCode derived from the version only, the package
//! code from the version and the program, the constant UpgradeCode, the program
//! carried byte for byte in the Binary table, the custom actions (install as
//! SYSTEM with the password hidden from the log, uninstall ignoring its result),
//! their conditions and order, the upgrade rows, properties and summary.
//!
//! What it does not check: installing or removing the package (needs an
//! administrator and a machine whose NuviraHub service may be replaced; see the
//! manual entry proposed in the Phase 2 report).

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

use windows::Win32::System::ApplicationInstallationAndServicing::{
    MSIDBOPEN_READONLY, MSIHANDLE, MsiCloseHandle, MsiDatabaseOpenViewW, MsiGetSummaryInformationW,
    MsiOpenDatabaseW, MsiRecordGetInteger, MsiRecordGetStringW, MsiRecordIsNull, MsiRecordReadStream,
    MsiSummaryInfoGetPropertyW, MsiViewExecute, MsiViewFetch,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

const UPGRADE_CODE: &str = "{4A53094B-9DEE-444F-9292-377CAAEC74C0}";

struct H(MSIHANDLE);
impl Drop for H {
    fn drop(&mut self) {
        unsafe { MsiCloseHandle(self.0) };
    }
}

struct Db(H);

impl Db {
    fn open(path: &Path) -> Self {
        let mut h = MSIHANDLE::default();
        let code = unsafe { MsiOpenDatabaseW(&HSTRING::from(path.as_os_str()), MSIDBOPEN_READONLY, &mut h) };
        assert_eq!(code, 0, "open {}", path.display());
        Self(H(h))
    }

    /// Rows of a query; each cell as text (integers formatted, NULL as None,
    /// streams as their length prefixed with "stream:").
    fn query(&self, sql: &str, columns: usize) -> Vec<Vec<Option<String>>> {
        let mut view = MSIHANDLE::default();
        assert_eq!(unsafe { MsiDatabaseOpenViewW(self.0.0, &HSTRING::from(sql), &mut view) }, 0, "{sql}");
        let view = H(view);
        assert_eq!(unsafe { MsiViewExecute(view.0, MSIHANDLE::default()) }, 0, "{sql}");
        let mut rows = Vec::new();
        loop {
            let mut rec = MSIHANDLE::default();
            if unsafe { MsiViewFetch(view.0, &mut rec) } != 0 {
                break;
            }
            let rec = H(rec);
            let mut row = Vec::new();
            for f in 1..=columns as u32 {
                if unsafe { MsiRecordIsNull(rec.0, f) }.as_bool() {
                    row.push(None);
                    continue;
                }
                let mut buf = vec![0u16; 4096];
                let mut len = buf.len() as u32;
                let code =
                    unsafe { MsiRecordGetStringW(rec.0, f, Some(PWSTR(buf.as_mut_ptr())), Some(&mut len)) };
                if code == 0 {
                    row.push(Some(String::from_utf16_lossy(&buf[..len as usize])));
                } else {
                    row.push(Some(format!("int:{}", unsafe { MsiRecordGetInteger(rec.0, f) })));
                }
            }
            rows.push(row);
        }
        rows
    }

    fn one(&self, sql: &str) -> Option<String> {
        self.query(sql, 1).into_iter().next().and_then(|r| r.into_iter().next().flatten())
    }

    fn property(&self, name: &str) -> Option<String> {
        self.one(&format!("SELECT `Value` FROM `Property` WHERE `Property`='{name}'"))
    }

    fn binary(&self, name: &str) -> Vec<u8> {
        let sql = format!("SELECT `Data` FROM `Binary` WHERE `Name`='{name}'");
        let mut view = MSIHANDLE::default();
        assert_eq!(unsafe { MsiDatabaseOpenViewW(self.0.0, &HSTRING::from(sql.as_str()), &mut view) }, 0);
        let view = H(view);
        assert_eq!(unsafe { MsiViewExecute(view.0, MSIHANDLE::default()) }, 0);
        let mut rec = MSIHANDLE::default();
        assert_eq!(unsafe { MsiViewFetch(view.0, &mut rec) }, 0, "binary {name}");
        let rec = H(rec);
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 65536];
        loop {
            let mut len = chunk.len() as u32;
            let code = unsafe {
                MsiRecordReadStream(rec.0, 1, Some(windows::core::PSTR(chunk.as_mut_ptr())), &mut len)
            };
            assert_eq!(code, 0);
            if len == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..len as usize]);
        }
        out
    }

    /// A string or integer property of the summary information.
    fn summary(&self, id: u32) -> String {
        let mut h = MSIHANDLE::default();
        assert_eq!(unsafe { MsiGetSummaryInformationW(self.0.0, PCWSTR::null(), 0, &mut h) }, 0);
        let h = H(h);
        let (mut kind, mut int) = (0u32, 0i32);
        let mut time = windows::Win32::Foundation::FILETIME::default();
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let code = unsafe {
            MsiSummaryInfoGetPropertyW(
                h.0,
                id,
                &mut kind,
                &mut int,
                Some(&mut time),
                Some(PWSTR(buf.as_mut_ptr())),
                Some(&mut len),
            )
        };
        assert_eq!(code, 0, "summary {id}");
        if kind == 30 { String::from_utf16_lossy(&buf[..len as usize]) } else { format!("{kind}:{int}") }
    }
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("nuvira-msi-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A fake program: "MZ" and some bytes (the builder only checks the first two).
fn program(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    let p = dir.join(name);
    let mut bytes = b"MZ".to_vec();
    bytes.extend_from_slice(body);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn run(program: &Path, version: &str, out: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nuvira-msi"))
        .arg("--program")
        .arg(program)
        .args(["--version", version, "--out"])
        .arg(out)
        .output()
        .expect("nuvira-msi")
}

fn build(program: &Path, version: &str, out: &Path) -> String {
    let o = run(program, version, out);
    assert!(o.status.success(), "nuvira-msi failed: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[test]
fn refuses_bad_versions_and_programs() {
    let d = dir().join("bad");
    std::fs::create_dir_all(&d).unwrap();
    let exe = program(&d, "nuvira.exe", b"x");
    let out = d.join("out.msi");
    for v in ["1.2", "1.2.3.4", "1.x.3", "256.0.0", "0.256.0", "", "-1.0.0"] {
        let o = run(&exe, v, &out);
        assert!(!o.status.success(), "version {v:?} accepted");
        assert!(!out.exists());
    }
    // The build number may be above 255 (only X and Y are limited).
    assert!(run(&exe, "1.2.300", &out).status.success(), "X.Y.Z with Z > 255");
    let not_exe = d.join("notes.txt");
    std::fs::write(&not_exe, b"hello").unwrap();
    let o = run(&not_exe, "1.0.0", &d.join("n.msi"));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("is not a Windows program"));
    let o = run(&d.join("missing.exe"), "1.0.0", &d.join("m.msi"));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("reading"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn package_contents() {
    let d = dir().join("contents");
    std::fs::create_dir_all(&d).unwrap();
    let body: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let exe = program(&d, "nuvira.exe", &body);
    let out = d.join("nuvira.msi");
    // An earlier file at the same path is replaced.
    std::fs::write(&out, b"old").unwrap();
    let printed = build(&exe, "0.28.0", &out);
    let parts: Vec<&str> = printed.split(' ').collect();
    assert_eq!(parts.len(), 3, "stdout: OUT VERSION PRODUCTCODE: {printed}");
    assert_eq!(parts[1], "0.28.0");
    let product_code = parts[2].to_string();
    assert!(product_code.starts_with('{') && product_code.ends_with('}') && product_code.len() == 38);
    // GUID version 4 and RFC 4122 variant bits.
    assert_eq!(&product_code[15..16], "4");
    assert!("89AB".contains(&product_code[20..21]));

    let db = Db::open(&out);
    assert_eq!(db.property("ProductCode").as_deref(), Some(product_code.as_str()));
    assert_eq!(db.property("UpgradeCode").as_deref(), Some(UPGRADE_CODE));
    assert_eq!(db.property("ProductVersion").as_deref(), Some("0.28.0"));
    assert_eq!(db.property("ProductName").as_deref(), Some("NuviraHub"));
    assert_eq!(db.property("Manufacturer").as_deref(), Some("Alessandro Blason"));
    assert_eq!(db.property("ALLUSERS").as_deref(), Some("1"));
    assert_eq!(db.property("ARPSYSTEMCOMPONENT").as_deref(), Some("1"));
    assert_eq!(db.property("MsiHiddenProperties").as_deref(), Some("PASSWORD"));
    assert_eq!(
        db.property("SecureCustomProperties").as_deref(),
        Some("NEWERFOUND;OLDERFOUND;SERVER;KEY;PASSWORD;PROFILE")
    );

    // The program, byte for byte.
    let carried = db.binary("NuviraExe");
    assert_eq!(carried, std::fs::read(&exe).unwrap(), "program carried as it is");

    // Custom actions: install from the Binary table, deferred, as SYSTEM, target hidden.
    let ca = db.query("SELECT `Action`, `Type`, `Source`, `Target` FROM `CustomAction` ORDER BY `Action`", 4);
    let find = |name: &str| ca.iter().find(|r| r[0].as_deref() == Some(name)).cloned().expect(name);
    let install = find("NuviraInstall");
    assert_eq!(install[1].as_deref(), Some(&*format!("{}", 2 | 0x400 | 0x800 | 0x2000)));
    assert_eq!(install[2].as_deref(), Some("NuviraExe"));
    assert_eq!(
        install[3].as_deref(),
        Some(
            "service install --msi-product [ProductCode] --server \"[SERVER]\" --server-key \"[KEY]\" --password \"[PASSWORD]\" --profile \"[PROFILE]\""
        )
    );
    let uninstall = find("NuviraUninstall");
    assert_eq!(uninstall[1].as_deref(), Some(&*format!("{}", 50 | 0x400 | 0x800 | 0x40)));
    assert_eq!(uninstall[2].as_deref(), Some("NUVIRAEXE"));
    assert_eq!(uninstall[3].as_deref(), Some("service uninstall --remove-program --from-msi"));
    let set_exe = find("SetNuviraExe");
    assert_eq!(set_exe[3].as_deref(), Some(r"[ProgramFiles64Folder]nuvira\nuvira.exe"));
    assert_eq!(find("NewerFound")[1].as_deref(), Some("19"));

    // Order and conditions of the execute sequence.
    let seq = db.query("SELECT `Action`, `Condition`, `Sequence` FROM `InstallExecuteSequence`", 3);
    let step = |name: &str| seq.iter().find(|r| r[0].as_deref() == Some(name)).cloned().expect(name);
    assert_eq!(step("NuviraInstall")[1].as_deref(), Some("NOT REMOVE~=\"ALL\""));
    assert_eq!(step("NuviraInstall")[2].as_deref(), Some("6500"));
    assert_eq!(step("NuviraUninstall")[1].as_deref(), Some("REMOVE~=\"ALL\" AND NOT UPGRADINGPRODUCTCODE"));
    assert_eq!(step("NuviraUninstall")[2].as_deref(), Some("6450"));
    assert_eq!(step("RemoveExistingProducts")[2].as_deref(), Some("1510"), "major upgrade removes early");
    assert_eq!(step("NewerFound")[1].as_deref(), Some("NEWERFOUND"));
    assert_eq!(seq.len(), 19);
    let ui = db.query("SELECT `Action` FROM `InstallUISequence`", 1);
    assert_eq!(ui.len(), 6);

    // Upgrades: earlier versions removed (min inclusive), a later one detected only.
    let up = db.query(
        "SELECT `UpgradeCode`, `VersionMin`, `VersionMax`, `Attributes`, `ActionProperty` FROM `Upgrade`",
        5,
    );
    assert_eq!(up.len(), 2);
    let older = up.iter().find(|r| r[4].as_deref() == Some("OLDERFOUND")).expect("OLDERFOUND");
    assert_eq!(
        (older[1].as_deref(), older[2].as_deref(), older[3].as_deref()),
        (Some("0.0.0"), Some("0.28.0"), Some("256"))
    );
    let newer = up.iter().find(|r| r[4].as_deref() == Some("NEWERFOUND")).expect("NEWERFOUND");
    assert_eq!(
        (newer[1].as_deref(), newer[2].clone(), newer[3].as_deref()),
        (Some("0.28.0"), None, Some("2"))
    );

    // No files: one registry component whose key path holds the ProductCode.
    let reg = db.query("SELECT `Root`, `Key`, `Name`, `Value` FROM `Registry`", 4);
    assert_eq!(
        reg,
        vec![vec![
            Some("2".into()),
            Some(r"SOFTWARE\nuvira\Msi".into()),
            Some("ProductCode".into()),
            Some("[ProductCode]".into())
        ]]
    );
    assert_eq!(db.query("SELECT `Component` FROM `Component`", 1).len(), 1);

    // Summary: template, revision = package code, schema 500, compressed.
    assert_eq!(db.summary(7), "x64;1033");
    let package_code = db.summary(9);
    assert!(package_code.starts_with('{') && package_code.len() == 38);
    assert_ne!(package_code, product_code);
    assert_eq!(db.summary(14), "3:500");
    assert_eq!(db.summary(15), "3:2");
    assert_eq!(db.summary(18), "nuvira-msi");
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn codes_are_derived_not_random() {
    let d = dir().join("codes");
    std::fs::create_dir_all(&d).unwrap();
    let a = program(&d, "a.exe", b"program A");
    let b = program(&d, "b.exe", b"program B");
    let codes = |exe: &Path, v: &str, name: &str| {
        let out = d.join(name);
        let printed = build(exe, v, &out);
        let db = Db::open(&out);
        (printed.rsplit(' ').next().unwrap().to_string(), db.summary(9))
    };
    let (p1, k1) = codes(&a, "1.2.3", "1.msi");
    let (p2, k2) = codes(&a, "1.2.3", "2.msi");
    let (p3, k3) = codes(&b, "1.2.3", "3.msi");
    let (p4, k4) = codes(&a, "1.2.4", "4.msi");
    // Same program and version: same codes (reproducible builds).
    assert_eq!((&p1, &k1), (&p2, &k2));
    // Another program, same version: same ProductCode, another package code.
    assert_eq!(p3, p1);
    assert_ne!(k3, k1);
    // Another version: both change.
    assert_ne!(p4, p1);
    assert_ne!(k4, k1);
    let _ = std::fs::remove_dir_all(&d);
}
