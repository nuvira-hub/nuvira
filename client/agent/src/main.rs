//! nexodesk-agent — il dispositivo controllato, da riga di comando (test e uso senza interfaccia).
//!
//! Con `--server` si registra su nexodesk-server e accetta le sessioni per ID e
//! password; senza, attende connessioni dirette in LAN (fase 0, solo reti fidate).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use nexodesk_agent::{AgentConfig, AgentEvent, ServerOptions};
use nexodesk_codec::EncoderChoice;
use tokio::sync::mpsc;
use tracing::info;

const COPYRIGHT: &str = "NexoDesk © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/alexbn71/NexoDesk";

#[derive(Parser, Debug, Clone)]
#[command(version, about = "NexoDesk agent: rende controllabile questo PC", after_help = COPYRIGHT)]
struct Args {
    /// Porta UDP su cui attendere il viewer (senza --server).
    #[arg(long, default_value_t = nexodesk_transport::DEFAULT_PORT)]
    port: u16,

    /// Schermo da trasmettere (vedi --list-displays). Predefinito: il principale.
    #[arg(long)]
    display: Option<usize>,

    /// Elenca gli schermi ed esce.
    #[arg(long)]
    list_displays: bool,

    /// Test automatici: scrive l'impronta (senza --server) o "ID password" (con --server) in questo file.
    #[arg(long, value_name = "FILE")]
    fingerprint_file: Option<std::path::PathBuf>,

    /// Esce al termine della prima sessione (test automatici, senza --server).
    #[arg(long)]
    once: bool,

    /// Trasmette un'immagine sintetica invece dello schermo, es. 1920x1080@60.
    #[arg(long, value_name = "LxA@FPS")]
    synthetic: Option<String>,

    /// Bitrate video massimo in kbit/s.
    #[arg(long, default_value_t = 8000)]
    bitrate: u32,

    /// Frame al secondo massimi.
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Encoder video: il più veloce tra hardware e software (auto), solo hardware o solo software.
    #[arg(long, value_enum, default_value_t = EncoderArg::Auto)]
    encoder: EncoderArg,

    /// Bitrate fisso (--bitrate) invece di adattarlo alla rete.
    #[arg(long)]
    fixed_bitrate: bool,

    /// Solo visualizzazione: ignora mouse e tastiera del viewer.
    #[arg(long)]
    view_only: bool,

    /// nexodesk-server a cui registrarsi (host[:porta]): il viewer si collega per ID.
    #[arg(long, value_name = "HOST[:PORTA]")]
    server: Option<String>,

    /// Impronta del certificato del server. Senza, si memorizza quella vista al primo collegamento.
    #[arg(long, value_name = "IMPRONTA")]
    server_key: Option<String>,

    /// Trasporto verso il server: auto (UDP, poi WebSocket se UDP è bloccato), udp, websocket.
    #[arg(long, value_enum, default_value_t = TransportArg::Auto)]
    transport: TransportArg,

    /// URL WebSocket del server per il ripiego (predefinito: ws://<server>:<porta>/nexodesk),
    /// es. wss://nexodesk.example.it/nexodesk dietro un reverse proxy.
    #[arg(long, value_name = "URL")]
    server_ws: Option<String>,

    /// Password permanente (con --server): chi la usa entra senza conferma. In ogni caso
    /// a ogni avvio viene generata anche una password temporanea.
    #[arg(long, env = "NEXODESK_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    /// Canale locale con l'app NexoDesk (usato da nexodesk-service): l'app mostra ID e
    /// password temporanea, e chi entra con quella va confermato da chi usa il PC.
    #[arg(long)]
    ipc: bool,

    /// File con la chiave del dispositivo (predefinito: device.key nella cartella dati).
    #[arg(long, value_name = "FILE")]
    identity: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TransportArg {
    Auto,
    Udp,
    Websocket,
}

impl From<TransportArg> for nexodesk_transport::signal::TransportMode {
    fn from(arg: TransportArg) -> Self {
        match arg {
            TransportArg::Auto => Self::Auto,
            TransportArg::Udp => Self::Udp,
            TransportArg::Websocket => Self::WebSocket,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncoderArg {
    Auto,
    Hardware,
    Software,
}

impl From<EncoderArg> for EncoderChoice {
    fn from(arg: EncoderArg) -> Self {
        match arg {
            EncoderArg::Auto => EncoderChoice::Auto,
            EncoderArg::Hardware => EncoderChoice::Hardware,
            EncoderArg::Software => EncoderChoice::Software,
        }
    }
}

/// Colori nei log solo se il terminale li interpreta (il vecchio prompt di Windows
/// li mostrerebbe come `←[2m`).
fn ansi_colors() -> bool {
    #[cfg(windows)]
    return std::io::IsTerminal::is_terminal(&std::io::stderr())
        && nu_ansi_term::enable_ansi_support().is_ok();
    #[cfg(not(windows))]
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(ansi_colors())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    nexodesk_input::set_dpi_awareness();

    if args.list_displays {
        for d in nexodesk_capture::displays()? {
            println!(
                "{}: {} {}x{} in ({}, {}){}",
                d.index,
                d.name,
                d.width,
                d.height,
                d.left,
                d.top,
                if d.primary { " [principale]" } else { "" }
            );
        }
        return Ok(());
    }
    let synthetic = args.synthetic.as_deref().map(nexodesk_agent::parse_synthetic).transpose()?;

    println!("{COPYRIGHT}");
    let mut config = AgentConfig {
        display: args.display,
        synthetic,
        bitrate_kbps: args.bitrate,
        fps: args.fps,
        plan: nexodesk_codec::EncoderPlan::Software,
        fixed_bitrate: args.fixed_bitrate,
        view_only: args.view_only,
    };
    // Con `auto` gli encoder vengono provati qui, così non ritardano la prima sessione.
    config.plan = nexodesk_codec::plan_encoder(args.encoder.into(), config.encoder_settings());

    let rt = tokio::runtime::Runtime::new()?;
    let Some(server_name) = args.server.clone() else {
        return rt.block_on(nexodesk_agent::serve_direct(config, args.port, args.once, args.fingerprint_file));
    };

    let ipc = args.ipc;
    let opts = ServerOptions {
        server_name: server_name.clone(),
        server_key: args.server_key.as_deref().map(nexodesk_transport::parse_fingerprint).transpose()?,
        transport: args.transport.into(),
        server_ws: args.server_ws.clone(),
        identity_path: args
            .identity
            .clone()
            .unwrap_or_else(|| nexodesk_transport::identity::data_dir().join("device.key")),
        // Senza app non c'è nessuno a cui chiedere: basta la password.
        confirm: ipc,
        permanent_password: args.password.clone(),
        info_file: args.fingerprint_file.clone(),
    };
    let password = Arc::new(Mutex::new(nexodesk_agent::random_password()));
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    // Il canale dei comandi deve restare aperto: chiuderlo fermerebbe l'agent.
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();

    rt.block_on(async move {
        let (app_tx, app_rx) = mpsc::unbounded_channel::<AgentEvent>();
        #[cfg(windows)]
        if ipc {
            let commands = commands_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = nexodesk_agent::ipc::serve(server_name, app_rx, commands).await {
                    tracing::warn!("canale con l'app chiuso: {e:#}");
                }
            });
        }
        #[cfg(not(windows))]
        drop((app_rx, server_name));
        tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                match &event {
                    AgentEvent::Online { device_id: id, .. } => info!("ID {} {} {}", &id[..3], &id[3..6], &id[6..]),
                    // Con l'app la password temporanea la vede chi usa il PC, non il log.
                    AgentEvent::Password { password } if !ipc => info!("password temporanea {password}"),
                    _ => {}
                }
                if ipc {
                    let _ = app_tx.send(event);
                }
            }
        });
        let _commands = commands_tx;
        nexodesk_agent::serve_via_server(config, opts, password, events_tx, commands_rx).await
    })
}
