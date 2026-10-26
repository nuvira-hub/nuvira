//! Nexo agent — il dispositivo controllato.
//!
//! Prototipo della fase 0: attende in LAN una connessione diretta dal viewer,
//! trasmette lo schermo e applica mouse e tastiera ricevuti.
//! Nessuna autenticazione oltre all'impronta del certificato: da usare solo in reti fidate.

mod pipeline;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use nexo_capture::{Capturer, SyntheticCapturer};
use nexo_codec::{EncoderChoice, EncoderSettings};
use nexo_proto::v1::{
    Codec, ControlMessage, Hello, InputEvent, Pong, VideoFormat, VideoFrame, control_message::Msg,
};
use nexo_transport::{Connection, FramedRecv, FramedSend};

use crate::pipeline::Pipeline;
use tokio::sync::mpsc;
use tracing::{info, warn};

const COPYRIGHT: &str = "Nexo © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/alexbn71/Nexo";

#[derive(Parser, Debug, Clone)]
#[command(version, about = "Nexo agent: rende controllabile questo PC (prototipo in LAN)", after_help = COPYRIGHT)]
struct Args {
    /// Porta UDP su cui attendere il viewer.
    #[arg(long, default_value_t = nexo_transport::DEFAULT_PORT)]
    port: u16,

    /// Schermo da trasmettere (vedi --list-displays). Predefinito: il principale.
    #[arg(long)]
    display: Option<usize>,

    /// Elenca gli schermi ed esce.
    #[arg(long)]
    list_displays: bool,

    /// Scrive l'impronta del certificato in questo file (test automatici).
    #[arg(long, value_name = "FILE")]
    fingerprint_file: Option<std::path::PathBuf>,

    /// Esce al termine della prima sessione (test automatici).
    #[arg(long)]
    once: bool,

    /// Trasmette un'immagine sintetica invece dello schermo, es. 1920x1080@60.
    #[arg(long, value_name = "LxA@FPS")]
    synthetic: Option<String>,

    /// Bitrate video in kbit/s.
    #[arg(long, default_value_t = 8000)]
    bitrate: u32,

    /// Frame al secondo massimi.
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Encoder video: hardware se disponibile (auto), solo hardware o solo software.
    #[arg(long, value_enum, default_value_t = EncoderArg::Auto)]
    encoder: EncoderArg,

    /// Solo visualizzazione: ignora mouse e tastiera del viewer.
    #[arg(long)]
    view_only: bool,
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

fn parse_synthetic(spec: &str) -> Result<(u32, u32, u32)> {
    let (size, fps) = spec.split_once('@').unwrap_or((spec, "60"));
    let (w, h) = size.split_once('x').context("formato atteso LxA@FPS, es. 1920x1080@60")?;
    Ok((w.parse()?, h.parse()?, fps.parse()?))
}

fn open_capturer(args: &Args) -> Result<Box<dyn Capturer>> {
    match &args.synthetic {
        Some(spec) => {
            let (w, h, fps) = parse_synthetic(spec)?;
            Ok(Box::new(SyntheticCapturer::new(w, h, fps)))
        }
        None => nexo_capture::open(args.display),
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
    nexo_input::set_dpi_awareness();

    if args.list_displays {
        for d in nexo_capture::displays()? {
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
    if let Some(spec) = &args.synthetic {
        parse_synthetic(spec)?;
    }

    println!("{COPYRIGHT}");
    tokio::runtime::Runtime::new()?.block_on(serve(args))
}

async fn serve(args: Args) -> Result<()> {
    let bind: SocketAddr = ([0, 0, 0, 0], args.port).into();
    let (endpoint, fingerprint) = nexo_transport::server_endpoint(bind)?;
    info!("in attesa del viewer su UDP {bind}");
    let fingerprint = nexo_transport::fingerprint_hex(&fingerprint);
    info!("impronta dell'agent: {fingerprint}");
    if let Some(path) = &args.fingerprint_file {
        std::fs::write(path, &fingerprint).with_context(|| format!("scrittura di {}", path.display()))?;
    }

    // Una sessione alla volta: le connessioni successive attendono in coda.
    while let Some(incoming) = endpoint.accept().await {
        let conn = match incoming.await {
            Ok(conn) => conn,
            Err(e) => {
                warn!("connessione non riuscita: {e}");
                continue;
            }
        };
        let peer = conn.remote_address();
        info!("viewer collegato da {peer}");
        match session(conn.clone(), &args).await {
            Ok(()) => info!("sessione con {peer} terminata"),
            Err(e) => warn!("sessione con {peer} terminata: {e:#}"),
        }
        conn.close(0u32.into(), b"fine sessione");
        if args.once {
            break;
        }
    }
    endpoint.wait_idle().await;
    Ok(())
}

async fn session(conn: Connection, args: &Args) -> Result<()> {
    let (send, recv) = conn.accept_bi().await.context("canale di controllo")?;
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    let Some(ControlMessage { msg: Some(Msg::Hello(hello)) }) = ctrl_rx.recv().await? else {
        bail!("il viewer non si è presentato");
    };
    info!(device = hello.device_name, os = hello.os, version = hello.app_version, "viewer");
    if hello.protocol_version < nexo_proto::PROTOCOL_VERSION {
        bail!("versione del protocollo del viewer non supportata: {}", hello.protocol_version);
    }
    ctrl_tx.send(&control(Msg::Hello(local_hello()))).await?;

    let keyframe = Arc::new(AtomicBool::new(true));
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    let (display, pipeline) = {
        let (cap_args, enc_args, keyframe) = (args.clone(), args.clone(), keyframe.clone());
        let settings = EncoderSettings { bitrate_bps: args.bitrate * 1000, max_fps: args.fps };
        let fps = args.fps;
        tokio::task::spawn_blocking(move || {
            Pipeline::start(
                move || open_capturer(&cap_args),
                move || nexo_codec::create_encoder(enc_args.encoder.into(), settings),
                fps,
                frame_tx,
                keyframe,
            )
        })
        .await?
        .context("avvio di cattura e compressione")?
    };
    ctrl_tx
        .send(&control(Msg::VideoFormat(VideoFormat {
            codec: Codec::H264 as i32,
            width: display.width,
            height: display.height,
        })))
        .await?;

    let mut video_tx = FramedSend::new(conn.open_uni().await?);
    tracing::debug!("canale video aperto");
    let video = async move {
        while let Some(frame) = frame_rx.recv().await {
            tracing::trace!(seq = frame.seq, bytes = frame.data.len(), "invio frame");
            video_tx.send(&frame).await?;
        }
        anyhow::Ok(())
    };

    let control_loop = async {
        while let Some(msg) = ctrl_rx.recv::<ControlMessage>().await? {
            match msg.msg {
                Some(Msg::Ping(ping)) => {
                    let pong = Pong { ping_sent_us: ping.sent_us, agent_us: nexo_proto::now_us() };
                    ctrl_tx.send(&control(Msg::Pong(pong))).await?;
                }
                Some(Msg::RequestKeyframe(_)) => keyframe.store(true, Ordering::Relaxed),
                _ => {}
            }
        }
        anyhow::Ok(())
    };

    let input = async {
        let mut rx = FramedRecv::new(conn.accept_uni().await?);
        let mut injector = if args.view_only || args.synthetic.is_some() {
            None
        } else {
            let screen = nexo_input::ScreenRect {
                left: display.left,
                top: display.top,
                width: display.width,
                height: display.height,
            };
            Some(nexo_input::open(screen)?)
        };
        while let Some(event) = rx.recv::<InputEvent>().await? {
            if let Some(injector) = injector.as_mut() {
                injector.inject(&event)?;
            }
        }
        anyhow::Ok(())
    };

    let result = tokio::select! {
        r = video => r.context("invio video"),
        r = control_loop => r.context("canale di controllo"),
        r = input => r.context("canale input"),
    };
    let _ = tokio::task::spawn_blocking(move || drop(pipeline)).await;
    result
}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

fn local_hello() -> Hello {
    Hello {
        protocol_version: nexo_proto::PROTOCOL_VERSION,
        device_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
        os: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
    }
}
