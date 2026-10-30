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

#[cfg(windows)]
mod config;
#[cfg(windows)]
mod install;
#[cfg(windows)]
pub use install::run_elevated;
#[cfg(windows)]
mod launch;
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
        /// nuvira-server to register with (host[:port]).
        #[arg(long, value_name = "HOST[:PORT]")]
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
    },
    /// Stops and removes the service (data and ID remain). Asks for administrator
    /// rights by itself when they are missing.
    Uninstall {
        /// Also removes the program from Program Files, the Start menu shortcut and the
        /// entry in the installed apps (used by that entry).
        #[arg(long)]
        remove_program: bool,
    },
    /// Shows the service state and the device ID.
    Status,
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
        Cmd::Install { server, password, server_key, transport, server_ws, password_file, report } => {
            println!("{COPYRIGHT}");
            let password = match password_file {
                Some(file) => {
                    let text = std::fs::read_to_string(&file);
                    let _ = std::fs::remove_file(&file);
                    Some(text?.trim().to_string()).filter(|p| !p.is_empty())
                }
                None => password,
            };
            let result = install::install(install::InstallOptions {
                server,
                server_key,
                password,
                transport,
                server_ws,
            });
            if let Some(report) = report {
                let json = match &result {
                    // The password only when generated: shown once, then only its hash exists.
                    Ok(done) => {
                        serde_json::json!({ "ok": true, "id": done.id, "password": done.generated_password })
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
                };
                std::fs::write(report, json.to_string())?;
            }
            result.map(|_| ())
        }
        Cmd::Uninstall { remove_program } => install::uninstall(remove_program),
        Cmd::Status => install::status(),
        Cmd::Run => run_service(),
        Cmd::Update => install::update(),
    }
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

    let service_config = config::ServiceConfig::load()?;
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

#[cfg(not(windows))]
fn main() {
    eprintln!("{COPYRIGHT}\nnuvira-service exists only for Windows for now");
    std::process::exit(1);
}
