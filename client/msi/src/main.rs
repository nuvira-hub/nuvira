//! nuvira-msi — builds `nuvira.msi`, the Windows Installer package of NuviraHub, for
//! deployment tools (Intune, Group Policy, RMM tools):
//!
//! ```text
//! nuvira-msi --program target/release/nuvira.exe --version 0.17.0 --out dist/nuvira.msi
//! msiexec /i nuvira.msi /qn SERVER=nuvira.example.com KEY=<fingerprint> PASSWORD=<permanent>
//! ```
//!
//! Like the NSIS setup it is a wrapper around the installation the program already does
//! by itself: the package carries nuvira.exe in its Binary table and runs
//! `nuvira.exe service install --msi-product <ProductCode> ...` as SYSTEM. The package
//! owns no file: the service replaces the program at every automatic update, and
//! Windows Installer would otherwise "repair" it back to the packaged version. Removing
//! the package runs `service uninstall --remove-program --from-msi`; the installed apps
//! entry is the program's own (pointing at `MsiExec.exe /X`), the package's is hidden.
//!
//! Every version gets its own ProductCode (a major upgrade replaces the earlier one);
//! the UpgradeCode never changes. Both are derived, not random, so that a build is
//! reproducible from the same program and version.
//!
//! The database is written through Windows Installer itself (msi.dll), as WiX does:
//! only on Windows, where the releases are built and signed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

/// Never changes: Windows Installer finds the earlier versions of NuviraHub by it.
const UPGRADE_CODE: &str = "{4A53094B-9DEE-444F-9292-377CAAEC74C0}";
const MANUFACTURER: &str = "Alessandro Blason";

#[derive(Parser)]
#[command(about = "Builds the NuviraHub MSI package around nuvira.exe")]
struct Args {
    /// The program to carry (signed, as released).
    #[arg(long)]
    program: PathBuf,
    /// Package version, X.Y.Z (the program's).
    #[arg(long)]
    version: String,
    /// The package to write.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let parts: Vec<u32> = args
        .version
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .with_context(|| format!("version {:?} is not X.Y.Z", args.version))?;
    if parts.len() != 3 || parts[0] > 255 || parts[1] > 255 {
        bail!("version {:?} is not X.Y.Z (X and Y at most 255)", args.version);
    }
    let program =
        std::fs::read(&args.program).with_context(|| format!("reading {}", args.program.display()))?;
    if program.get(..2) != Some(b"MZ") {
        bail!("{} is not a Windows program", args.program.display());
    }
    let product_code = guid(&[b"nuvira-msi product ", args.version.as_bytes()]);
    let package_code = guid(&[b"nuvira-msi package ", args.version.as_bytes(), &digest(&program)]);
    let _ = std::fs::remove_file(&args.out);
    build(&args.out, &args.program, &args.version, &product_code, &package_code)?;
    println!("{} {} {product_code}", args.out.display(), args.version);
    Ok(())
}

/// GUID in registry form from the SHA-256 of `parts` (version 4 and variant bits set,
/// as a random one).
fn guid(parts: &[&[u8]]) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for part in parts {
        ctx.update(part);
    }
    let mut b = [0u8; 16];
    b.copy_from_slice(&ctx.finish().as_ref()[..16]);
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let hex = |r: std::ops::Range<usize>| b[r].iter().map(|x| format!("{x:02X}")).collect::<String>();
    format!("{{{}-{}-{}-{}-{}}}", hex(0..4), hex(4..6), hex(6..8), hex(8..10), hex(10..16))
}

fn digest(data: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().to_vec()
}

/// A cell of a row to insert.
pub enum V<'a> {
    S(&'a str),
    I(i32),
    N,
    /// Binary column: the contents of this file.
    File(&'a Path),
}
use V::{I, N, S};

// Custom action types (Windows Installer SDK, "Summary List of All Custom Action Types").
const CA_EXE_BINARY: i32 = 2; // program in the Binary table
const CA_SET_ERROR: i32 = 19; // ends the installation with the message in Target
const CA_EXE_PROPERTY: i32 = 50; // program whose path is in a property
const CA_SET_PROPERTY: i32 = 51;
const CA_IGNORE_RETURN: i32 = 0x40;
const CA_DEFERRED: i32 = 0x400;
const CA_NO_IMPERSONATE: i32 = 0x800;
const CA_HIDE_TARGET: i32 = 0x2000; // the command line holds the password: never in the log

/// The tables, as (name, columns in Windows Installer SQL, primary key).
const SCHEMA: &[(&str, &str, &str)] = &[
    ("Property", "`Property` CHAR(72) NOT NULL, `Value` LONGCHAR NOT NULL", "`Property`"),
    (
        "Directory",
        "`Directory` CHAR(72) NOT NULL, `Directory_Parent` CHAR(72), `DefaultDir` CHAR(255) NOT NULL",
        "`Directory`",
    ),
    (
        "Feature",
        "`Feature` CHAR(38) NOT NULL, `Feature_Parent` CHAR(38), `Title` CHAR(64), `Description` CHAR(255), \
         `Display` SHORT, `Level` SHORT NOT NULL, `Directory_` CHAR(72), `Attributes` SHORT NOT NULL",
        "`Feature`",
    ),
    (
        "Component",
        "`Component` CHAR(72) NOT NULL, `ComponentId` CHAR(38), `Directory_` CHAR(72) NOT NULL, \
         `Attributes` SHORT NOT NULL, `Condition` CHAR(255), `KeyPath` CHAR(72)",
        "`Component`",
    ),
    (
        "FeatureComponents",
        "`Feature_` CHAR(38) NOT NULL, `Component_` CHAR(72) NOT NULL",
        "`Feature_`, `Component_`",
    ),
    (
        "Registry",
        "`Registry` CHAR(72) NOT NULL, `Root` SHORT NOT NULL, `Key` CHAR(255) NOT NULL, `Name` CHAR(255), \
         `Value` LONGCHAR, `Component_` CHAR(72) NOT NULL",
        "`Registry`",
    ),
    ("Binary", "`Name` CHAR(72) NOT NULL, `Data` OBJECT NOT NULL", "`Name`"),
    // No files, but RegisterProduct reads it.
    (
        "Media",
        "`DiskId` SHORT NOT NULL, `LastSequence` SHORT NOT NULL, `DiskPrompt` CHAR(64), `Cabinet` CHAR(255),          `VolumeLabel` CHAR(32), `Source` CHAR(72)",
        "`DiskId`",
    ),
    (
        "CustomAction",
        "`Action` CHAR(72) NOT NULL, `Type` SHORT NOT NULL, `Source` CHAR(72), `Target` CHAR(255)",
        "`Action`",
    ),
    (
        "Upgrade",
        "`UpgradeCode` CHAR(38) NOT NULL, `VersionMin` CHAR(20), `VersionMax` CHAR(20), `Language` CHAR(255), \
         `Attributes` LONG NOT NULL, `Remove` CHAR(255), `ActionProperty` CHAR(72) NOT NULL",
        "`UpgradeCode`, `VersionMin`, `VersionMax`, `Language`, `Attributes`",
    ),
    (
        "InstallExecuteSequence",
        "`Action` CHAR(72) NOT NULL, `Condition` CHAR(255), `Sequence` SHORT",
        "`Action`",
    ),
    ("InstallUISequence", "`Action` CHAR(72) NOT NULL, `Condition` CHAR(255), `Sequence` SHORT", "`Action`"),
];

/// Names of the columns of `table`, from [`SCHEMA`].
fn columns(table: &str) -> Result<Vec<&'static str>> {
    let (_, columns, _) = SCHEMA.iter().find(|(name, _, _)| *name == table).context("unknown table")?;
    Ok(columns.split(',').filter_map(|c| c.trim().strip_prefix('`')?.split('`').next()).collect())
}

fn build(out: &Path, program: &Path, version: &str, product_code: &str, package_code: &str) -> Result<()> {
    let db = db::Database::create(out)?;
    for (name, columns, key) in SCHEMA {
        db.exec(&format!("CREATE TABLE `{name}` ({columns} PRIMARY KEY {key})"))
            .with_context(|| format!("table {name}"))?;
    }

    let secure = "NEWERFOUND;OLDERFOUND;SERVER;KEY;PASSWORD;PROFILE";
    for (k, v) in [
        ("ProductName", "NuviraHub"),
        ("ProductVersion", version),
        ("Manufacturer", MANUFACTURER),
        ("ProductCode", product_code),
        ("UpgradeCode", UPGRADE_CODE),
        ("ProductLanguage", "1033"),
        ("ALLUSERS", "1"),
        // The installed apps entry is the program's own (see the module notes).
        ("ARPSYSTEMCOMPONENT", "1"),
        ("ARPURLINFOABOUT", "https://nuvirahub.dev"),
        ("MsiHiddenProperties", "PASSWORD"),
        ("SecureCustomProperties", secure),
    ] {
        db.insert("Property", &[S(k), S(v)])?;
    }

    // Program Files, only to give the uninstall action the installed program's path.
    db.insert("Directory", &[S("TARGETDIR"), N, S("SourceDir")])?;
    db.insert("Directory", &[S("ProgramFiles64Folder"), S("TARGETDIR"), S(".")])?;

    // One component without files: a registry value with the ProductCode, its key path.
    db.insert("Feature", &[S("NuviraHub"), N, S("NuviraHub"), N, I(0), I(1), S("TARGETDIR"), I(0)])?;
    const COMPONENT_REGISTRY_64: i32 = 0x4 | 0x100; // key path in the registry, 64-bit
    let component_id = guid(&[b"nuvira-msi component registration"]);
    db.insert(
        "Component",
        &[
            S("Registration"),
            S(&component_id),
            S("TARGETDIR"),
            I(COMPONENT_REGISTRY_64),
            N,
            S("RegistrationValue"),
        ],
    )?;
    db.insert("FeatureComponents", &[S("NuviraHub"), S("Registration")])?;
    const HKLM: i32 = 2;
    db.insert(
        "Registry",
        &[
            S("RegistrationValue"),
            I(HKLM),
            S(r"SOFTWARE\nuvira\Msi"),
            S("ProductCode"),
            S("[ProductCode]"),
            S("Registration"),
        ],
    )?;

    db.insert("Binary", &[S("NuviraExe"), V::File(program)])?;
    db.insert("Media", &[I(1), I(0), N, N, N, N])?;

    let install = "service install --msi-product [ProductCode] --server \"[SERVER]\" --server-key \"[KEY]\" \
                   --password \"[PASSWORD]\" --profile \"[PROFILE]\"";
    let deferred = CA_DEFERRED | CA_NO_IMPERSONATE;
    db.insert(
        "CustomAction",
        &[
            S("NewerFound"),
            I(CA_SET_ERROR),
            N,
            S("A newer version of NuviraHub is already installed by Windows Installer."),
        ],
    )?;
    db.insert(
        "CustomAction",
        &[
            S("SetNuviraExe"),
            I(CA_SET_PROPERTY),
            S("NUVIRAEXE"),
            S(r"[ProgramFiles64Folder]nuvira\nuvira.exe"),
        ],
    )?;
    db.insert(
        "CustomAction",
        &[S("NuviraInstall"), I(CA_EXE_BINARY | deferred | CA_HIDE_TARGET), S("NuviraExe"), S(install)],
    )?;
    // Its result is ignored: a package that cannot be removed would block every later
    // installation; whatever is left the program removes when it is installed again.
    db.insert(
        "CustomAction",
        &[
            S("NuviraUninstall"),
            I(CA_EXE_PROPERTY | deferred | CA_IGNORE_RETURN),
            S("NUVIRAEXE"),
            S("service uninstall --remove-program --from-msi"),
        ],
    )?;

    const UPGRADE_ONLY_DETECT: i32 = 0x2;
    const UPGRADE_MIN_INCLUSIVE: i32 = 0x100;
    // Earlier versions, removed by this one; a later one, over which this does not install.
    db.insert(
        "Upgrade",
        &[S(UPGRADE_CODE), S("0.0.0"), S(version), N, I(UPGRADE_MIN_INCLUSIVE), N, S("OLDERFOUND")],
    )?;
    db.insert("Upgrade", &[S(UPGRADE_CODE), S(version), N, N, I(UPGRADE_ONLY_DETECT), N, S("NEWERFOUND")])?;

    let removing = r#"REMOVE~="ALL""#;
    let uninstall_condition = format!("{removing} AND NOT UPGRADINGPRODUCTCODE");
    let install_condition = format!("NOT {removing}");
    let execute: &[(&str, Option<&str>, i32)] = &[
        ("FindRelatedProducts", None, 200),
        ("NewerFound", Some("NEWERFOUND"), 210),
        ("CostInitialize", None, 800),
        ("FileCost", None, 900),
        ("CostFinalize", None, 1000),
        ("SetNuviraExe", None, 1010),
        ("InstallValidate", None, 1400),
        ("InstallInitialize", None, 1500),
        ("RemoveExistingProducts", None, 1510),
        ("ProcessComponents", None, 1600),
        ("UnpublishFeatures", None, 1800),
        ("RemoveRegistryValues", None, 2600),
        ("WriteRegistryValues", None, 5000),
        ("RegisterProduct", None, 6100),
        ("PublishFeatures", None, 6300),
        ("PublishProduct", None, 6400),
        ("NuviraUninstall", Some(&uninstall_condition), 6450),
        ("NuviraInstall", Some(&install_condition), 6500),
        ("InstallFinalize", None, 6600),
    ];
    for (action, condition, n) in execute {
        db.insert("InstallExecuteSequence", &[S(action), condition.map(S).unwrap_or(N), I(*n)])?;
    }
    for (action, condition, n) in [
        ("FindRelatedProducts", None, 200),
        ("NewerFound", Some("NEWERFOUND"), 210),
        ("CostInitialize", None, 800),
        ("FileCost", None, 900),
        ("CostFinalize", None, 1000),
        ("ExecuteAction", None, 1300),
    ] {
        db.insert("InstallUISequence", &[S(action), condition.map(S).unwrap_or(N), I(n)])?;
    }

    let comments = format!("NuviraHub {version}: unattended access, for deployment tools.");
    db.summary(&[
        (1, db::P::I2(1252)), // codepage
        (2, db::P::Str("Installation Database")),
        (3, db::P::Str("NuviraHub")),
        (4, db::P::Str(MANUFACTURER)),
        (5, db::P::Str("Installer, NuviraHub, remote desktop")),
        (6, db::P::Str(&comments)),
        (7, db::P::Str("x64;1033")),   // template: platform and language
        (9, db::P::Str(package_code)), // revision number: the package code
        (12, db::P::Now),
        (13, db::P::Now),
        (14, db::P::I4(500)), // page count: Windows Installer 5.0
        (15, db::P::I4(2)),   // word count: long file names, compressed
        (18, db::P::Str("nuvira-msi")),
        (19, db::P::I4(2)), // security: read-only recommended
    ])?;
    db.commit()
}

#[cfg(windows)]
mod db {
    use std::path::Path;

    use anyhow::{Result, bail};
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ApplicationInstallationAndServicing::{
        MSIDBOPEN_CREATE, MSIHANDLE, MsiCloseHandle, MsiCreateRecord, MsiDatabaseCommit,
        MsiDatabaseOpenViewW, MsiGetSummaryInformationW, MsiOpenDatabaseW, MsiRecordSetInteger,
        MsiRecordSetStreamW, MsiRecordSetStringW, MsiSummaryInfoPersist, MsiSummaryInfoSetPropertyW,
        MsiViewExecute,
    };
    use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    use windows::core::{HSTRING, PCWSTR};

    use super::V;

    /// An MSI handle, closed when dropped.
    struct H(MSIHANDLE);

    impl Drop for H {
        fn drop(&mut self) {
            unsafe { MsiCloseHandle(self.0) };
        }
    }

    fn check(code: u32, what: &str) -> Result<()> {
        if code != 0 {
            bail!("{what}: Windows Installer error {code}");
        }
        Ok(())
    }

    pub enum P<'a> {
        I2(i32),
        I4(i32),
        Str(&'a str),
        Now,
    }

    pub struct Database(H);

    impl Database {
        pub fn create(path: &Path) -> Result<Self> {
            let mut h = MSIHANDLE::default();
            check(
                unsafe { MsiOpenDatabaseW(&HSTRING::from(path.as_os_str()), MSIDBOPEN_CREATE, &mut h) },
                "creating the database",
            )?;
            Ok(Self(H(h)))
        }

        fn view(&self, sql: &str, record: Option<&H>) -> Result<()> {
            let mut view = MSIHANDLE::default();
            check(unsafe { MsiDatabaseOpenViewW(self.0.0, &HSTRING::from(sql), &mut view) }, sql)?;
            let view = H(view);
            check(unsafe { MsiViewExecute(view.0, record.map(|r| r.0).unwrap_or_default()) }, sql)
        }

        pub fn exec(&self, sql: &str) -> Result<()> {
            self.view(sql, None)
        }

        pub fn insert(&self, table: &str, values: &[V]) -> Result<()> {
            let record = H(unsafe { MsiCreateRecord(values.len() as u32) });
            for (i, value) in values.iter().enumerate() {
                let field = i as u32 + 1;
                let code = match value {
                    V::S(text) => unsafe { MsiRecordSetStringW(record.0, field, &HSTRING::from(*text)) },
                    V::I(n) => unsafe { MsiRecordSetInteger(record.0, field, *n) },
                    V::N => 0,
                    V::File(path) => unsafe {
                        MsiRecordSetStreamW(record.0, field, &HSTRING::from(path.as_os_str()))
                    },
                };
                check(code, &format!("{table}, field {field}"))?;
            }
            let marks = vec!["?"; values.len()].join(", ");
            let columns = super::columns(table)?;
            if columns.len() != values.len() {
                bail!("{table}: {} values for {} columns", values.len(), columns.len());
            }
            let columns = columns.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ");
            self.view(&format!("INSERT INTO `{table}` ({columns}) VALUES ({marks})"), Some(&record))
        }

        pub fn summary(&self, props: &[(u32, P)]) -> Result<()> {
            const VT_I2: u32 = 2;
            const VT_I4: u32 = 3;
            const VT_LPSTR: u32 = 30;
            const VT_FILETIME: u32 = 64;
            let mut h = MSIHANDLE::default();
            check(
                unsafe { MsiGetSummaryInformationW(self.0.0, PCWSTR::null(), props.len() as u32, &mut h) },
                "summary information",
            )?;
            let h = H(h);
            for (id, prop) in props {
                let mut time = FILETIME::default();
                let code = match prop {
                    P::I2(v) => unsafe {
                        MsiSummaryInfoSetPropertyW(h.0, *id, VT_I2, *v, &mut time, PCWSTR::null())
                    },
                    P::I4(v) => unsafe {
                        MsiSummaryInfoSetPropertyW(h.0, *id, VT_I4, *v, &mut time, PCWSTR::null())
                    },
                    P::Str(text) => unsafe {
                        MsiSummaryInfoSetPropertyW(h.0, *id, VT_LPSTR, 0, &mut time, &HSTRING::from(*text))
                    },
                    P::Now => {
                        time = unsafe { GetSystemTimeAsFileTime() };
                        unsafe {
                            MsiSummaryInfoSetPropertyW(h.0, *id, VT_FILETIME, 0, &mut time, PCWSTR::null())
                        }
                    }
                };
                check(code, &format!("summary property {id}"))?;
            }
            check(unsafe { MsiSummaryInfoPersist(h.0) }, "saving the summary information")
        }

        pub fn commit(&self) -> Result<()> {
            check(unsafe { MsiDatabaseCommit(self.0.0) }, "saving the database")
        }
    }
}

#[cfg(not(windows))]
mod db {
    pub enum P<'a> {
        I2(i32),
        I4(i32),
        Str(&'a str),
        Now,
    }
    pub struct Database;
    impl Database {
        pub fn create(_: &std::path::Path) -> anyhow::Result<Self> {
            anyhow::bail!("nuvira-msi needs Windows (msi.dll)")
        }
        pub fn exec(&self, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        pub fn insert(&self, _: &str, _: &[super::V]) -> anyhow::Result<()> {
            unreachable!()
        }
        pub fn summary(&self, _: &[(u32, P)]) -> anyhow::Result<()> {
            unreachable!()
        }
        pub fn commit(&self) -> anyhow::Result<()> {
            unreachable!()
        }
    }
}
