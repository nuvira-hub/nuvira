//! Nexo agent — il dispositivo controllato.
//!
//! Prototipo della fase 0: attende in LAN una connessione diretta dal viewer,
//! trasmette lo schermo e applica mouse e tastiera ricevuti.
//! Nessuna autenticazione oltre all'impronta del certificato: da usare solo in reti fidate.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use nexo_capture::{Capturer, SyntheticCapturer};
use nexo_codec::{BgraImage, EncoderChoice, EncoderSettings};
use nexo_proto::v1::{
    Codec, ControlMessage, Hello, InputEvent, Pong, VideoFormat, VideoFrame, control_message::Msg,
};
use nexo_transport::{Connection, FramedRecv, FramedSend};
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
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
    info!("impronta dell'agent: {}", nexo_transport::fingerprint_hex(&fingerprint));

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
    }
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

    // La cattura si apre nel thread che la userà; le dimensioni tornano indietro subito.
    let stop = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::new(AtomicBool::new(true));
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let pipeline = {
        let (args, stop, keyframe) = (args.clone(), stop.clone(), keyframe.clone());
        std::thread::Builder::new()
            .name("cattura".into())
            .spawn(move || capture_loop(&args, ready_tx, frame_tx, &stop, &keyframe))?
    };
    let display = ready_rx.await.context("avvio della cattura")??;
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
    stop.store(true, Ordering::Relaxed);
    let _ = tokio::task::spawn_blocking(move || pipeline.join()).await;
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

/// Thread di cattura e compressione. Si ferma quando `stop` diventa vero o la sessione chiude il canale.
fn capture_loop(
    args: &Args,
    ready: tokio::sync::oneshot::Sender<Result<nexo_capture::Display>>,
    frames: mpsc::Sender<VideoFrame>,
    stop: &AtomicBool,
    keyframe: &AtomicBool,
) {
    let setup = (|| {
        let capturer = open_capturer(args)?;
        let settings = EncoderSettings { bitrate_bps: args.bitrate * 1000, max_fps: args.fps };
        let encoder = nexo_codec::create_encoder(args.encoder.into(), settings)?;
        Ok((capturer, encoder))
    })();
    let (mut capturer, mut encoder) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let screen = capturer.display().clone();
    info!("trasmetto {} {}x{} con {}", screen.name, screen.width, screen.height, encoder.name());
    let _ = ready.send(Ok(screen));

    let min_interval = Duration::from_secs(1) / args.fps.max(1);
    let mut last_sent = Instant::now() - min_interval;
    let mut seq = 0u64;
    let mut stats = Stats::default();

    while !stop.load(Ordering::Relaxed) {
        let force_key = keyframe.load(Ordering::Relaxed);
        let frame = match capturer.next_frame(Duration::from_millis(100)) {
            Ok(Some(frame)) => frame,
            // Schermo fermo: se serve un keyframe si ricomprime l'ultima immagine.
            Ok(None) if force_key => match capturer.last_frame() {
                Some(frame) => frame,
                None => continue,
            },
            Ok(None) => continue,
            Err(e) => {
                // Es. desktop sicuro o schermo bloccato: si riprova senza chiudere la sessione.
                warn!("cattura non riuscita: {e:#}");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        // Limite di frame al secondo: un'immagine più recente arriverà comunque.
        if !force_key && last_sent.elapsed() < min_interval {
            continue;
        }

        let image =
            BgraImage { width: frame.width, height: frame.height, stride: frame.stride, data: &frame.data };
        let captured_us = frame.captured_us;
        let t = Instant::now();
        let encoded = match encoder.encode(&image, force_key) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => {
                warn!("compressione non riuscita: {e:#}");
                continue;
            }
        };
        if encoded.keyframe {
            keyframe.store(false, Ordering::Relaxed);
        }
        let encode_us = t.elapsed().as_micros() as u32;
        stats.add(encode_us, encoded.data.len());
        seq += 1;
        last_sent = Instant::now();
        let msg = VideoFrame {
            seq,
            capture_us: captured_us,
            encode_us,
            keyframe: encoded.keyframe,
            data: encoded.data,
        };
        // Coda da un solo frame: se la rete è lenta la cattura rallenta invece di accumulare ritardo.
        if frames.blocking_send(msg).is_err() {
            break;
        }
        stats.report();
    }
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    bytes: usize,
    encode_us: u64,
}

impl Stats {
    fn add(&mut self, encode_us: u32, bytes: usize) {
        self.since.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.bytes += bytes;
        self.encode_us += encode_us as u64;
    }

    fn report(&mut self) {
        let Some(since) = self.since else { return };
        let secs = since.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        info!(
            "{:.1} fps, compressione {:.1} ms/frame, {:.0} kbit/s",
            self.frames as f64 / secs,
            self.encode_us as f64 / self.frames.max(1) as f64 / 1000.0,
            self.bytes as f64 * 8.0 / secs / 1000.0
        );
        *self = Stats::default();
    }
}
