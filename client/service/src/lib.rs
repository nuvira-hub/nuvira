//! nuvira-service — unattended access on Windows.
//!
//! System service (LocalSystem) that keeps nuvira-agent running in the session
//! of the person using the PC, with a fixed ID and a permanent password. As SYSTEM
//! the agent also sees and controls administrator prompts (UAC) and the lock
//! screen, which a program started by the user cannot capture.
//!
//! ```text
//! nuvira-service install --server nuvira.example.com   (administrator prompt)
//! nuvira-service status
//! nuvira-service uninstall
//! ```

pub mod branding;
#[cfg(target_os = "linux")]
pub mod user;
#[cfg(target_os = "linux")]
pub use user::{ServiceState, installed_version, service_state};
#[cfg(windows)]
mod config;
#[cfg(windows)]
mod install;
#[cfg(windows)]
pub use install::{ServiceState, installed_version, run_elevated, service_state};
#[cfg(windows)]
mod launch;
#[cfg(windows)]
pub mod policy;
#[cfg(windows)]
mod printer;

/// The program installed with the service (in Program Files).
#[cfg(windows)]
pub fn installed_program() -> std::path::PathBuf {
    config::program_dir().join(config::PROGRAM_EXE)
}
#[cfg(windows)]
mod scm;
#[cfg(windows)]
mod update;

const COPYRIGHT: &str =
    "NuviraHub © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/nuvira-hub/nuvira";

#[cfg(windows)]
#[derive(clap::Parser, Debug)]
#[command(version, about = "NuviraHub: service for unattended access", after_help = COPYRIGHT)]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[cfg(windows)]
#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// Installs (or updates) and starts the service. Needs an administrator prompt.
    Install {
        /// nuvira-server to register with (host[:port]); empty: NuviraHub's public server.
        #[arg(long, value_name = "HOST[:PORT]", default_value = "")]
        server: String,
        /// Permanent password. Without it, the one already set is kept or a new one is generated.
        #[arg(long)]
        password: Option<String>,
        /// Server certificate fingerprint. Without it, the one seen at the first connection is remembered.
        #[arg(long, value_name = "FINGERPRINT")]
        server_key: Option<String>,
        /// Transport towards the server: auto, udp, websocket.
        #[arg(long, default_value = "auto")]
        transport: String,
        /// WebSocket URL of the server for the fallback.
        #[arg(long, value_name = "URL")]
        server_ws: Option<String>,
        /// Reads the permanent password from this file and deletes it (used by the app:
        /// a command line can be read by other programs on the PC).
        #[arg(long, value_name = "FILE", conflicts_with = "password")]
        password_file: Option<std::path::PathBuf>,
        /// Writes the outcome to this file as JSON (used by the app, which cannot see
        /// the output of a process started as administrator).
        #[arg(long, value_name = "FILE", hide = true)]
        report: Option<std::path::PathBuf>,
        /// Started by the NuviraHub MSI package (its ProductCode): empty values keep
        /// the settings in place, and the installed apps entry removes the package.
        #[arg(long, value_name = "PRODUCT_CODE", hide = true)]
        msi_product: Option<String>,
    },
    /// Stops and removes the service (data and ID remain). Asks for administrator
    /// rights by itself when they are missing.
    Uninstall {
        /// Also removes the program from Program Files, the Start menu shortcut and the
        /// entry in the installed apps (used by that entry).
        #[arg(long)]
        remove_program: bool,
        /// Started by the MSI package while Windows Installer removes it.
        #[arg(long, hide = true)]
        from_msi: bool,
    },
    /// Shows the service state and the device ID.
    Status,
    /// Prints the hash of a permanent password, read from the input, for the
    /// "Permanent password" policy (Group Policy, Intune): the policies are readable by
    /// every user of the PC, so they hold only the hash.
    PasswordHash,
    /// Starts the stopped service, first installing this program over the installed
    /// one if that is older (used by the app, as administrator).
    #[command(hide = true)]
    Repair {
        /// Writes the outcome to this file as JSON.
        #[arg(long, value_name = "FILE")]
        report: Option<std::path::PathBuf>,
    },
    /// Installs the NuviraHub printer (remote printing) again, or removes it with
    /// --remove; the service installs and removes it by itself.
    #[command(hide = true)]
    Printer {
        #[arg(long)]
        remove: bool,
    },
    /// Used by the Windows service manager.
    #[command(hide = true)]
    Run,
    /// Installs this program over the installed one, keeping its settings (started
    /// by the service for automatic updates).
    #[command(hide = true)]
    Update,
}

/// The service's command line: `args` as from `std::env::args_os()` (program name first).
#[cfg(windows)]
pub fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    use clap::Parser;

    let args = Args::parse_from(args);
    match args.command {
        Cmd::Install { server, password, server_key, transport, server_ws, password_file, report, msi_product } => {
            println!("{COPYRIGHT}");
            let password = match password_file {
                Some(file) => {
                    let text = std::fs::read_to_string(&file);
                    let _ = std::fs::remove_file(&file);
                    Some(text?.trim().to_string()).filter(|p| !p.is_empty())
                }
                None => password,
            };
            // The package passes every property, also the empty ones.
            let given = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
            let (password, server_key, server_ws) = (given(password), given(server_key), given(server_ws));
            let server = server.trim().to_string();
            let result = install::install(install::InstallOptions {
                server,
                server_key,
                password,
                transport,
                server_ws,
                msi_product: given(msi_product),
            });
            // The password only when generated: shown once, then only its hash exists.
            write_report(
                report,
                &result,
                |done| serde_json::json!({ "id": done.id, "password": done.generated_password }),
            )?;
            result.map(|_| ())
        }
        Cmd::Repair { report } => {
            let result = install::repair();
            write_report(report, &result, |_| serde_json::json!({}))?;
            result
        }
        Cmd::Uninstall { remove_program, from_msi } => install::uninstall(remove_program, from_msi),
        Cmd::Status => install::status(),
        Cmd::PasswordHash => {
            use std::io::BufRead;
            eprintln!("permanent password (at least 8 characters), then Enter:");
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let password = line.trim_end_matches(['\r', '\n']);
            if password.chars().count() < 8 {
                anyhow::bail!("the password must have at least 8 characters");
            }
            println!("{}", nuvira_secret::hash(password));
            Ok(())
        }
        Cmd::Printer { remove: false } => printer::install().map(|done| println!("{done}")),
        Cmd::Printer { remove: true } => printer::uninstall().map(|()| println!("NuviraHub printer removed")),
        Cmd::Run => run_service(),
        Cmd::Update => install::update(),
    }
}

/// Outcome for the app, which cannot see the output of a process started as
/// administrator: `{"ok": true, ...fields}` or `{"ok": false, "error": ...}`.
#[cfg(windows)]
fn write_report<T>(
    report: Option<std::path::PathBuf>,
    result: &anyhow::Result<T>,
    fields: impl FnOnce(&T) -> serde_json::Value,
) -> anyhow::Result<()> {
    let Some(report) = report else { return Ok(()) };
    let json = match result {
        Ok(done) => {
            let mut json = fields(done);
            json["ok"] = true.into();
            json
        }
        Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
    };
    std::fs::write(report, json.to_string())?;
    Ok(())
}

#[cfg(windows)]
fn run_service() -> anyhow::Result<()> {
    use anyhow::Context;

    let data_dir = config::data_dir();
    std::fs::create_dir_all(&data_dir)?;
    config::rotate_log(&config::service_log(), 5 << 20);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config::service_log())
        .context("opening the service log")?;
    tracing_subscriber::fmt().with_ansi(false).with_writer(std::sync::Mutex::new(log)).init();

    let mut service_config = config::ServiceConfig::load()?;
    if service_config.apply_policy(&policy::read()) {
        tracing::info!("settings from the organisation's policies applied (server {})", service_config.server);
        service_config.save()?;
    }
    // Inherited by the agent: the password does not appear on the command line, and the
    // data (remembered server fingerprint) stays in the protected folder.
    // SAFETY: the process still has a single thread.
    unsafe {
        std::env::set_var("NUVIRA_PASSWORD_HASH", &service_config.password_hash);
        std::env::set_var("NUVIRA_DATA_DIR", &data_dir);
    }
    let result = scm::dispatch();
    if let Err(e) = &result {
        tracing::error!("{e:#}");
    }
    result
}

/// Linux: `nuvira service install`, `start` (run at login by the autostart entry) and
/// `uninstall`; the app's settings install it too.
#[cfg(target_os = "linux")]
pub fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    let args: Vec<String> = args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    match args.get(1).map(String::as_str) {
        // For scripts: the same as "Install" in the app's settings.
        Some("install") => {
            let password = match value("--password-file") {
                Some(file) => {
                    let text = std::fs::read_to_string(&file)?;
                    let _ = std::fs::remove_file(&file);
                    Some(text.trim().to_string())
                }
                None => None,
            };
            let opts = user::InstallOptions {
                server: value("--server").unwrap_or_default(),
                server_key: value("--server-key"),
                password,
            };
            user::ask_screen_permission()?;
            if let Some(generated) = user::install(&opts)? {
                println!("permanent password: {generated}");
            }
            Ok(())
        }
        Some("start") => user::start(),
        Some("portal") => user::portal(),
        Some("uninstall") => user::uninstall(),
        _ => anyhow::bail!(
            "{COPYRIGHT}\nusage: nuvira service install [--server HOST] [--server-key FP] [--password-file FILE] | start | uninstall"
        ),
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn run(_args: impl IntoIterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    anyhow::bail!("{COPYRIGHT}\nnuvira-service exists only for Windows and Linux for now")
}

/// Random 8-character password that is easy to dictate, like the agent's.
pub fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("random generator");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}
