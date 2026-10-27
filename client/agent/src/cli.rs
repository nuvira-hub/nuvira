//! The controlled device from the command line (tests and use without an interface):
//! `nexodesk-agent`, and `nexodesk agent` in the app, which also starts it this way for the service.
//!
//! With `--server` it registers with nexodesk-server and accepts sessions by ID
//! and password; without it, it waits for direct LAN connections (trusted networks only).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use crate::{AgentConfig, AgentEvent, ServerOptions};
use nexodesk_codec::EncoderChoice;
use tokio::sync::mpsc;
use tracing::info;

const COPYRIGHT: &str = "NexoDesk © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/alexbn71/NexoDesk";

#[derive(Parser, Debug, Clone)]
#[command(version, about = "NexoDesk agent: makes this PC controllable", after_help = COPYRIGHT)]
struct Args {
    /// UDP port to wait for the viewer on (without --server).
    #[arg(long, default_value_t = nexodesk_transport::DEFAULT_PORT)]
    port: u16,

    /// Screen to stream (see --list-displays). Default: the primary one.
    #[arg(long)]
    display: Option<usize>,

    /// Lists the screens and exits.
    #[arg(long)]
    list_displays: bool,

    /// Automated tests: writes the fingerprint (without --server) or "ID password" (with --server) to this file.
    #[arg(long, value_name = "FILE")]
    fingerprint_file: Option<std::path::PathBuf>,

    /// File present while a session is in progress (the service does not update meanwhile).
    #[arg(long, value_name = "FILE", hide = true)]
    busy_file: Option<std::path::PathBuf>,

    /// Exits at the end of the first session (automated tests, without --server).
    #[arg(long)]
    once: bool,

    /// Streams a synthetic image instead of the screen, e.g. 1920x1080@60.
    #[arg(long, value_name = "LxA@FPS")]
    synthetic: Option<String>,

    /// Maximum video bitrate in kbit/s.
    #[arg(long, default_value_t = 8000)]
    bitrate: u32,

    /// Maximum frames per second.
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Video encoder: the fastest of hardware and software (auto), hardware only or software only.
    #[arg(long, value_enum, default_value_t = EncoderArg::Auto)]
    encoder: EncoderArg,

    /// Fixed bitrate (--bitrate) instead of adapting it to the network.
    #[arg(long)]
    fixed_bitrate: bool,

    /// View only: ignores the viewer's mouse and keyboard.
    #[arg(long)]
    view_only: bool,

    /// nexodesk-server to register with (host[:port]): the viewer connects by ID.
    #[arg(long, value_name = "HOST[:PORTA]")]
    server: Option<String>,

    /// Server certificate fingerprint. Without it, the one seen at the first connection is remembered.
    #[arg(long, value_name = "IMPRONTA")]
    server_key: Option<String>,

    /// Transport towards the server: auto (UDP, then WebSocket if UDP is blocked), udp, websocket.
    #[arg(long, value_enum, default_value_t = TransportArg::Auto)]
    transport: TransportArg,

    /// WebSocket URL of the server for the fallback (default: ws://<server>:<port>/nexodesk),
    /// e.g. wss://nexodesk.example.com/nexodesk behind a reverse proxy.
    #[arg(long, value_name = "URL")]
    server_ws: Option<String>,

    /// Permanent password (with --server): whoever uses it connects without confirmation.
    /// A temporary password is also generated at every start.
    #[arg(long, env = "NEXODESK_PASSWORD", hide_env_values = true, conflicts_with = "password_hash")]
    password: Option<String>,

    /// The permanent password as stored by the service (hash): the agent never sees it
    /// in clear text.
    #[arg(long, env = "NEXODESK_PASSWORD_HASH", hide = true, hide_env_values = true)]
    password_hash: Option<String>,

    /// Local channel with the NexoDesk app (used by nexodesk-service): the app shows the ID
    /// and the temporary password, and whoever uses that one must be confirmed at the PC.
    #[arg(long)]
    ipc: bool,

    /// File with the device key (default: device.key in the data folder).
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

/// Colours in the logs only if the terminal understands them (the old Windows
/// prompt would show them as `←[2m`).
fn ansi_colors() -> bool {
    #[cfg(windows)]
    return std::io::IsTerminal::is_terminal(&std::io::stderr())
        && nu_ansi_term::enable_ansi_support().is_ok();
    #[cfg(not(windows))]
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// The agent's command line: `args` as from `std::env::args_os()` (program name first).
pub fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_ansi(ansi_colors())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
    let args = Args::parse_from(args);
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
                if d.primary { " [primary]" } else { "" }
            );
        }
        return Ok(());
    }
    let synthetic = args.synthetic.as_deref().map(crate::parse_synthetic).transpose()?;

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
    // With `auto` the encoders are tried here, so they do not delay the first session.
    config.plan = nexodesk_codec::plan_encoder(args.encoder.into(), config.encoder_settings());

    let rt = tokio::runtime::Runtime::new()?;
    let Some(server_name) = args.server.clone() else {
        return rt.block_on(crate::serve_direct(config, args.port, args.once, args.fingerprint_file));
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
        // Without the app there is nobody to ask: the password is enough.
        confirm: ipc,
        permanent_password_hash: args
            .password_hash
            .clone()
            .or_else(|| args.password.as_deref().map(nexodesk_secret::hash)),
        info_file: args.fingerprint_file.clone(),
    };
    let password = Arc::new(Mutex::new(crate::random_password()));
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    // The command channel must stay open: closing it would stop the agent.
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();

    rt.block_on(async move {
        let (app_tx, app_rx) = mpsc::unbounded_channel::<AgentEvent>();
        #[cfg(windows)]
        if ipc {
            let commands = commands_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::ipc::serve(server_name, app_rx, commands).await {
                    tracing::warn!("channel with the app closed: {e:#}");
                }
            });
        }
        #[cfg(not(windows))]
        drop((app_rx, server_name));
        let busy_file = args.busy_file.clone();
        if let Some(file) = &busy_file {
            // Left over by an agent that did not end cleanly.
            let _ = std::fs::remove_file(file);
        }
        tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                if let Some(file) = &busy_file {
                    match &event {
                        AgentEvent::SessionStarted { .. } => {
                            let _ = std::fs::write(file, b"");
                        }
                        AgentEvent::SessionEnded { .. } => {
                            let _ = std::fs::remove_file(file);
                        }
                        _ => {}
                    }
                }
                match &event {
                    AgentEvent::Online { device_id: id, .. } => info!("ID {} {} {}", &id[..3], &id[3..6], &id[6..]),
                    // With the app the temporary password is seen by the person at the PC, not the log.
                    AgentEvent::Password { password } if !ipc => info!("temporary password {password}"),
                    _ => {}
                }
                if ipc {
                    let _ = app_tx.send(event);
                }
            }
        });
        let _commands = commands_tx;
        crate::serve_via_server(config, opts, password, events_tx, commands_rx).await
    })
}
