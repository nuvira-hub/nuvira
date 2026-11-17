//! Microsoft Edge WebView2 Runtime: the app's window and the viewer's toolbar,
//! file manager, chat and terminal are WebViews, and on Windows they need it.
//!
//! It ships with Windows 11 and recent Windows 10, but not with older Windows 10
//! builds, LTSC or Windows Server, and it can be removed. Without it the app stopped
//! with WebView2's own error and the person had to find the right download.
//! [`ensure`] installs it with Microsoft's Evergreen Bootstrapper (a small signed
//! program that downloads and installs the runtime), as Tauri's own installers do:
//! from the setup and the MSI (`service install`, as administrator: for every user),
//! or from the app (as the person: for that person only, no administrator needed).

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::core::HSTRING;

/// The runtime's client key (Microsoft's "Detect if a WebView2 Runtime is already installed").
const CLIENT: &str = r"Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

/// Microsoft's permanent link to the Evergreen Bootstrapper.
const BOOTSTRAPPER_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

/// The page for the people who install it by hand.
pub const DOWNLOAD_PAGE: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The installed runtime's version, if any: per machine (64-bit or 32-bit view) or for
/// this user. "0.0.0.0" or empty means removed.
pub fn installed() -> Option<String> {
    [
        (HKEY_LOCAL_MACHINE, format!(r"SOFTWARE\WOW6432Node\{CLIENT}")),
        (HKEY_LOCAL_MACHINE, format!(r"SOFTWARE\{CLIENT}")),
        (HKEY_CURRENT_USER, format!(r"Software\{CLIENT}")),
    ]
    .into_iter()
    .find_map(|(root, key)| version_at(root, &key))
}

fn version_at(root: HKEY, key: &str) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: plain registry read into a local buffer of `size` bytes.
    let status = unsafe {
        RegGetValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from("pv"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1).min(buf.len());
    let version = String::from_utf16_lossy(&buf[..len]).trim_end_matches('\0').trim().to_string();
    (!version.is_empty() && version != "0.0.0.0").then_some(version)
}

/// Installs the runtime if it is missing; `Ok(version)` once it is there. `log` gets
/// one line per step. Needs the Internet (the bootstrapper downloads the runtime);
/// as administrator it installs for every user, otherwise for this user.
pub fn ensure(log: &dyn Fn(&str)) -> Result<String> {
    if let Some(version) = installed() {
        return Ok(version);
    }
    log("Microsoft Edge WebView2 Runtime missing: installing it");
    let dir = std::env::temp_dir().join(format!("nuvira-webview2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let result = download_and_run(&dir, log);
    let _ = std::fs::remove_dir_all(&dir);
    result?;
    match installed() {
        Some(version) => {
            log(&format!("WebView2 Runtime {version} installed"));
            Ok(version)
        }
        None => bail!("the WebView2 Runtime is still missing after its installer"),
    }
}

fn download_and_run(dir: &Path, log: &dyn Fn(&str)) -> Result<()> {
    let setup = dir.join("MicrosoftEdgeWebview2Setup.exe");
    // curl.exe ships with Windows 10 1803 and later; it follows Microsoft's redirect.
    let status = Command::new(system32("curl.exe"))
        .args(["-fsSL", "--retry", "2", "--max-time", "300", "-o"])
        .arg(&setup)
        .arg(BOOTSTRAPPER_URL)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .context("starting curl.exe to download the WebView2 installer")?;
    if !status.success() {
        bail!("downloading the WebView2 installer failed ({})", exit_code(status));
    }
    verify_microsoft_signature(&setup)?;
    log("WebView2 installer downloaded and signed by Microsoft: running it");
    let mut child = Command::new(&setup)
        .args(["/silent", "/install"])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("starting the WebView2 installer")?;
    // The bootstrapper downloads about 150 MB: usually a minute, longer on slow links.
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("the WebView2 installer failed ({})", exit_code(status));
            }
            return Ok(());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("the WebView2 installer did not finish within 10 minutes");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// "exit code 5", or how the process ended without one.
fn exit_code(status: std::process::ExitStatus) -> String {
    status.code().map_or_else(|| status.to_string(), |code| format!("exit code {code}"))
}

/// A valid Authenticode signature by Microsoft Corporation, else an error (a download
/// that is not Microsoft's bootstrapper is never run, above all as administrator).
fn verify_microsoft_signature(file: &Path) -> Result<()> {
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{}'; \
         if ($s.Status -eq 'Valid' -and $s.SignerCertificate.Subject -match 'O=Microsoft Corporation') {{ exit 0 }} \
         Write-Output (\"$($s.Status) $($s.SignerCertificate.Subject)\"); exit 1",
        file.display().to_string().replace('\'', "''")
    );
    let out = Command::new(system32(r"WindowsPowerShell\v1.0\powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("checking the signature of the WebView2 installer")?;
    if !out.status.success() {
        bail!(
            "the downloaded WebView2 installer is not signed by Microsoft ({})",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

/// A program of the system folder by full path (never one found first in PATH or in
/// the current folder).
fn system32(name: &str) -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Path::new(&root).join("System32").join(name)
}
