//! nexodesk-service — accesso non presidiato su Windows.
//!
//! Servizio di sistema (LocalSystem) che tiene attivo nexodesk-agent nella
//! sessione di chi usa il PC, con un ID fisso e una password permanente. Da
//! SYSTEM l'agent vede e controlla anche le richieste di amministratore (UAC)
//! e la schermata di blocco, che un programma avviato dall'utente non può catturare.
//!
//! ```text
//! nexodesk-service install --server nexodesk.example.it   (prompt da amministratore)
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
#[command(version, about = "NexoDesk: servizio per l'accesso non presidiato", after_help = COPYRIGHT)]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[cfg(windows)]
#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// Installa (o aggiorna) e avvia il servizio. Serve un prompt da amministratore.
    Install {
        /// nexodesk-server a cui registrarsi (host[:porta]).
        #[arg(long, value_name = "HOST[:PORTA]")]
        server: String,
        /// Password permanente. Senza, si tiene quella già impostata o se ne genera una.
        #[arg(long)]
        password: Option<String>,
        /// Impronta del certificato del server. Senza, si memorizza quella vista al primo collegamento.
        #[arg(long, value_name = "IMPRONTA")]
        server_key: Option<String>,
        /// Trasporto verso il server: auto, udp, websocket.
        #[arg(long, default_value = "auto")]
        transport: String,
        /// URL WebSocket del server per il ripiego.
        #[arg(long, value_name = "URL")]
        server_ws: Option<String>,
    },
    /// Ferma e rimuove il servizio (dati e ID restano).
    Uninstall,
    /// Mostra stato del servizio e ID del dispositivo.
    Status,
    /// Usato dal gestore servizi di Windows.
    #[command(hide = true)]
    Run,
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use clap::Parser;

    let args = Args::parse();
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
        .context("apertura del log del servizio")?;
    tracing_subscriber::fmt().with_ansi(false).with_writer(std::sync::Mutex::new(log)).init();

    let service_config = config::ServiceConfig::load()?;
    // Ereditate dall'agent: la password non compare sulla riga di comando, e i dati
    // (impronta del server memorizzata) restano nella cartella protetta.
    // SAFETY: il processo ha ancora un solo thread.
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
    eprintln!("{COPYRIGHT}\nnexodesk-service per ora esiste solo per Windows");
    std::process::exit(1);
}
