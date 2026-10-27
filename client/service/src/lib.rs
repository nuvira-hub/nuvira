//! nexodesk-service — unattended access on Windows.
//!
//! System service (LocalSystem) that keeps nexodesk-agent running in the session
//! of the person using the PC, with a fixed ID and a permanent password. As SYSTEM
//! the agent also sees and controls administrator prompts (UAC) and the lock
//! screen, which a program started by the user cannot capture.
//!
//! ```text
//! nexodesk-service install --server nexodesk.example.com   (administrator prompt)
//! nexodesk-service status
//! nexodesk-service uninstall
//! ```

#[cfg(windows)]
mod config;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod launch;
#[cfg(windows)]
mod scm;

const COPYRIGHT: &str = "NexoDesk © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/alexbn71/NexoDesk";

#[cfg(windows)]
#[derive(clap::Parser, Debug)]
#[command(version, about = "NexoDesk: service for unattended access", after_help = COPYRIGHT)]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[cfg(windows)]
#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// Installs (or updates) and starts the service. Needs an administrator prompt.
    Install {
        /// nexodesk-server to register with (host[:port]).
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
    },
    /// Stops and removes the service (data and ID remain).
    Uninstall,
    /// Shows the service state and the device ID.
    Status,
    /// Used by the Windows service manager.
    #[command(hide = true)]
    Run,
}

/// The service's command line: `args` as from `std::env::args_os()` (program name first).
#[cfg(windows)]
pub fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    use clap::Parser;

    let args = Args::parse_from(args);
    match args.command {
        Cmd::Install { server, password, server_key, transport, server_ws } => {
            println!("{COPYRIGHT}");
            install::install(install::InstallOptions { server, server_key, password, transport, server_ws })
        }
        Cmd::Uninstall => install::uninstall(),
        Cmd::Status => install::status(),
        Cmd::Run => run_service(),
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
        std::env::set_var("NEXODESK_PASSWORD", &service_config.password);
        std::env::set_var("NEXODESK_DATA_DIR", &data_dir);
    }
    let result = scm::dispatch();
    if let Err(e) = &result {
        tracing::error!("{e:#}");
    }
    result
}

#[cfg(not(windows))]
fn main() {
    eprintln!("{COPYRIGHT}\nnexodesk-service exists only for Windows for now");
    std::process::exit(1);
}
