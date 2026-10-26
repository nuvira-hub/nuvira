//! Nexo agent — il dispositivo controllato.
//!
//! Prototipo della fase 0: attende in LAN una connessione diretta dal viewer,
//! trasmette lo schermo e applica mouse e tastiera ricevuti.
//! Nessuna autenticazione oltre all'impronta del certificato: da usare solo in reti fidate.

mod abr;
mod pipeline;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use nexo_capture::{Capturer, SyntheticCapturer};
use nexo_codec::{EncoderChoice, EncoderPlan, EncoderSettings};
use nexo_proto::v1::{
    AuthResult, Codec, ControlMessage, Hello, InputEvent, Pong, VideoFormat, VideoFrame, control_message::Msg,
};
use nexo_transport::{Connection, FramedRecv, FramedSend};

use crate::pipeline::Pipeline;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Bitrate minimo del controllo adattivo.
const MIN_BITRATE: u32 = 500_000;

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

    /// Encoder video: il più veloce tra hardware e software (auto), solo hardware o solo software.
    #[arg(long, value_enum, default_value_t = EncoderArg::Auto)]
    encoder: EncoderArg,

    /// Bitrate fisso (--bitrate) invece di adattarlo alla rete.
    #[arg(long)]
    fixed_bitrate: bool,

    /// Solo visualizzazione: ignora mouse e tastiera del viewer.
    #[arg(long)]
    view_only: bool,

    /// nexo-server a cui registrarsi (host[:porta]): il viewer si collega per ID.
    /// Senza, l'agent attende connessioni dirette in LAN sulla porta --port.
    #[arg(long, value_name = "HOST[:PORTA]")]
    server: Option<String>,

    /// Impronta del certificato del server. Senza, si memorizza quella vista al primo collegamento.
    #[arg(long, value_name = "IMPRONTA")]
    server_key: Option<String>,

    /// Trasporto verso il server: auto (UDP, poi WebSocket se UDP è bloccato), udp, websocket.
    #[arg(long, value_enum, default_value_t = TransportArg::Auto)]
    transport: TransportArg,

    /// URL WebSocket del server per il ripiego (predefinito: ws://<server>:<porta>/nexo),
    /// es. wss://nexo.example.it/nexo dietro un reverse proxy.
    #[arg(long, value_name = "URL")]
    server_ws: Option<String>,

    /// Password di sessione (con --server). Senza, ne viene generata una a ogni avvio.
    #[arg(long)]
    password: Option<String>,

    /// File con la chiave del dispositivo (predefinito: device.key nella cartella dati).
    #[arg(long, value_name = "FILE")]
    identity: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum TransportArg {
    Auto,
    Udp,
    Websocket,
}

impl From<TransportArg> for nexo_transport::signal::TransportMode {
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
    // Con `auto` gli encoder vengono provati qui, così non ritardano la prima sessione.
    let plan = nexo_codec::plan_encoder(args.encoder.into(), encoder_settings(&args));
    let rt = tokio::runtime::Runtime::new()?;
    if args.server.is_some() {
        rt.block_on(serve_via_server(args, plan))
    } else {
        rt.block_on(serve(args, plan))
    }
}

fn encoder_settings(args: &Args) -> EncoderSettings {
    EncoderSettings { bitrate_bps: args.bitrate * 1000, max_fps: args.fps }
}

async fn serve(args: Args, plan: EncoderPlan) -> Result<()> {
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
        let result = match conn.accept_bi().await.context("canale di controllo") {
            Ok(streams) => session(conn.clone(), streams, &args, plan, None).await,
            Err(e) => Err(e),
        };
        match result {
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

/// Password casuale di 8 caratteri facili da dettare (niente 0/O, 1/l/I).
fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 8];
    rand_fill(&mut bytes);
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

fn rand_fill(buf: &mut [u8]) {
    // Generatore casuale del sistema operativo (tramite ring).
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), buf).expect("generatore casuale");
}

/// Confronto a tempo costante (non rivela quanti caratteri iniziali sono giusti).
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Registra il dispositivo su nexo-server e accetta le sessioni che arrivano attraverso il relay.
/// Se il server cade, si ricollega.
async fn serve_via_server(args: Args, plan: EncoderPlan) -> Result<()> {
    let server_name = args.server.clone().expect("--server");
    let server = nexo_transport::resolve_addr(&server_name, nexo_transport::signal::DEFAULT_PORT)?;
    let identity_path =
        args.identity.clone().unwrap_or_else(|| nexo_transport::identity::data_dir().join("device.key"));
    let identity = Arc::new(nexo_transport::identity::Identity::load_or_create(&identity_path)?);
    let password = args.password.clone().unwrap_or_else(random_password);
    let busy = Arc::new(AtomicBool::new(false));

    loop {
        let expected = match &args.server_key {
            Some(fp) => Some(nexo_transport::parse_fingerprint(fp)?),
            None => nexo_transport::signal::known_server::get(&server_name),
        };
        match signal_loop(&args, plan, &identity, server, &server_name, expected, &password, &busy).await {
            Ok(()) => return Ok(()),
            Err(e) => warn!("collegamento con nexo-server perso: {e:#}; riprovo tra 5 s"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn signal_loop(
    args: &Args,
    plan: EncoderPlan,
    identity: &Arc<nexo_transport::identity::Identity>,
    server: SocketAddr,
    server_name: &str,
    expected: Option<nexo_transport::Fingerprint>,
    password: &str,
    busy: &Arc<AtomicBool>,
) -> Result<()> {
    use nexo_proto::v1::{Accept, Reject, client_message, server_message};
    let device_name = std::env::var("COMPUTERNAME").unwrap_or_default();
    let ws_url =
        args.server_ws.clone().unwrap_or_else(|| nexo_transport::ws::default_url(server_name, server));
    let mut signal = nexo_transport::signal::connect_auto(
        server,
        &ws_url,
        args.transport.into(),
        expected,
        identity,
        &device_name,
    )
    .await?;
    if let nexo_transport::ServerTransport::WebSocket(url) = &signal.transport {
        info!("collegato a nexo-server attraverso il WebSocket {url}");
    }
    if expected.is_none() {
        nexo_transport::signal::known_server::remember(server_name, &signal.server_fingerprint)?;
        info!(
            "impronta di nexo-server memorizzata: {}",
            nexo_transport::fingerprint_hex(&signal.server_fingerprint)
        );
    }
    let id = &signal.device_id;
    info!("registrato su {server_name}: ID {} {} {}, password {password}", &id[..3], &id[3..6], &id[6..]);
    if let Some(path) = &args.fingerprint_file {
        // Nei test automatici il viewer legge da qui ID e password.
        std::fs::write(path, format!("{id} {password}"))?;
    }

    loop {
        match signal.rx.recv().await? {
            Some(server_message::Msg::Incoming(inc)) => {
                // TODO (fase 2): conferma dell'utente prima di accettare.
                let reply = if busy.load(Ordering::Relaxed) {
                    client_message::Msg::Reject(Reject {
                        session_id: inc.session_id,
                        reason: "dispositivo occupato in un'altra sessione".into(),
                    })
                } else {
                    info!("richiesta di collegamento da {} ({})", inc.viewer_name, inc.viewer_id);
                    client_message::Msg::Accept(Accept { session_id: inc.session_id })
                };
                signal.tx.send(reply).await?;
            }
            Some(server_message::Msg::Route(route)) => {
                let route = nexo_transport::p2p::Route {
                    relay: server,
                    transport: signal.transport.clone(),
                    token: route.relay_token.as_slice().try_into().context("gettone del relay non valido")?,
                    peer_public_key: route
                        .peer_public_key
                        .as_slice()
                        .try_into()
                        .context("chiave del viewer non valida")?,
                    role: nexo_transport::p2p::Role::Agent,
                };
                busy.store(true, Ordering::Relaxed);
                let (args, identity, busy, password) =
                    (args.clone(), identity.clone(), busy.clone(), password.to_string());
                tokio::spawn(async move {
                    if let Err(e) = relayed_session(&args, plan, &identity, &route, &password).await {
                        warn!("sessione terminata: {e:#}");
                    }
                    busy.store(false, Ordering::Relaxed);
                });
            }
            Some(server_message::Msg::Error(e)) => warn!("nexo-server: {}", e.message),
            Some(_) => {}
            None => bail!("nexo-server ha chiuso la connessione"),
        }
    }
}

async fn relayed_session(
    args: &Args,
    plan: EncoderPlan,
    identity: &nexo_transport::identity::Identity,
    route: &nexo_transport::p2p::Route,
    password: &str,
) -> Result<()> {
    let endpoint = nexo_transport::p2p::agent_endpoint(identity, route).await?;
    // Il viewer può aprire due connessioni (diretta e attraverso il relay): vale quella
    // su cui apre il canale di controllo; l'altra la chiude lui.
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let acceptor = {
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    if let Ok(streams) = conn.accept_bi().await {
                        let _ = tx.send((conn, streams)).await;
                    }
                });
            }
        })
    };
    let (conn, streams) = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
        .await
        .context("il viewer non si è collegato entro 20 s")?
        .context("endpoint chiuso")?;
    acceptor.abort();
    let via = if conn.remote_address() == route.relay { "attraverso il relay" } else { "in diretta" };
    info!("viewer collegato {via} ({})", conn.remote_address());
    let result = session(conn.clone(), streams, args, plan, Some(password)).await;
    // Si chiude l'endpoint intero: anche l'eventuale connessione rimasta via relay,
    // che altrimenti terrebbe l'agent "occupato" fino al suo timeout.
    endpoint.close(0u32.into(), b"fine sessione");
    endpoint.wait_idle().await;
    if result.is_ok() {
        info!("sessione terminata");
    }
    result
}

/// Una sessione con un viewer già collegato. Con `password` il viewer deve
/// autenticarsi prima di ricevere qualunque immagine.
async fn session(
    conn: Connection,
    (send, recv): (quinn::SendStream, quinn::RecvStream),
    args: &Args,
    plan: EncoderPlan,
    password: Option<&str>,
) -> Result<()> {
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    let Some(ControlMessage { msg: Some(Msg::Hello(hello)) }) = ctrl_rx.recv().await? else {
        bail!("il viewer non si è presentato");
    };
    info!(device = hello.device_name, os = hello.os, version = hello.app_version, "viewer");
    if hello.protocol_version < nexo_proto::PROTOCOL_VERSION {
        bail!("versione del protocollo del viewer non supportata: {}", hello.protocol_version);
    }
    let hello = Hello { password_required: password.is_some(), ..local_hello() };
    ctrl_tx.send(&control(Msg::Hello(hello))).await?;
    if let Some(password) = password {
        let given = match ctrl_rx.recv::<ControlMessage>().await? {
            Some(ControlMessage { msg: Some(Msg::Auth(auth)) }) => auth.password,
            _ => bail!("il viewer non ha inviato la password"),
        };
        let ok = same_secret(&given, password);
        let message = if ok { String::new() } else { "password errata".into() };
        ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok, message }))).await?;
        if !ok {
            // Pausa contro i tentativi a raffica.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            bail!("password errata");
        }
    }

    let keyframe = Arc::new(AtomicBool::new(true));
    let max_bps = args.bitrate * 1000;
    // Si parte da metà del massimo: la rete ignota potrebbe non reggere di più.
    let mut abr = (!args.fixed_bitrate).then(|| abr::Abr::new(max_bps / 2, MIN_BITRATE, max_bps));
    let bitrate = Arc::new(AtomicU32::new(abr.as_ref().map_or(max_bps, |a| a.bitrate())));
    let session_start = Instant::now();
    // Momento dell'ultimo frame inviato (ms dall'inizio): serve a capire se manca il feedback.
    let last_sent_ms = Arc::new(AtomicU64::new(0));
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    let (display, pipeline) = {
        let (cap_args, keyframe, bitrate) = (args.clone(), keyframe.clone(), bitrate.clone());
        // L'encoder parte dal bitrate iniziale del controllo adattivo.
        let settings =
            EncoderSettings { bitrate_bps: bitrate.load(Ordering::Relaxed), ..encoder_settings(args) };
        let fps = args.fps;
        tokio::task::spawn_blocking(move || {
            Pipeline::start(
                move || open_capturer(&cap_args),
                move |gpu| nexo_codec::create_encoder(plan, settings, gpu),
                plan == EncoderPlan::HardwareGpu,
                fps,
                frame_tx,
                keyframe,
                bitrate,
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
    let video = {
        let last_sent_ms = last_sent_ms.clone();
        async move {
            while let Some(frame) = frame_rx.recv().await {
                tracing::trace!(seq = frame.seq, bytes = frame.data.len(), "invio frame");
                video_tx.send(&frame).await?;
                last_sent_ms.store(session_start.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            anyhow::Ok(())
        }
    };

    let control_loop = async {
        let mut last_feedback = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            let msg = tokio::select! {
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(msg) => msg,
                    None => return anyhow::Ok(()),
                },
                _ = tick.tick() => {
                    // Frame inviati di recente ma nessun feedback: la rete è probabilmente bloccata.
                    let now_ms = session_start.elapsed().as_millis() as u64;
                    let sending = now_ms.saturating_sub(last_sent_ms.load(Ordering::Relaxed)) < 1000;
                    if let Some(abr) = abr.as_mut()
                        && sending
                        && last_feedback.elapsed() > abr::FEEDBACK_TIMEOUT
                        && let Some(bps) = abr.on_silence(Instant::now())
                    {
                        bitrate.store(bps, Ordering::Relaxed);
                        info!("bitrate {} kbit/s (nessun feedback dal viewer)", bps / 1000);
                    }
                    continue;
                }
            };
            match msg.msg {
                Some(Msg::Ping(ping)) => {
                    let pong = Pong { ping_sent_us: ping.sent_us, agent_us: nexo_proto::now_us() };
                    ctrl_tx.send(&control(Msg::Pong(pong))).await?;
                }
                Some(Msg::RequestKeyframe(_)) => keyframe.store(true, Ordering::Relaxed),
                Some(Msg::Feedback(fb)) => {
                    last_feedback = Instant::now();
                    if let Some(abr) = abr.as_mut()
                        && fb.frames > 0
                        && let Some(bps) = abr.on_feedback(
                            Instant::now(),
                            fb.latency_ms_p50 as f64,
                            fb.latency_ms_min as f64,
                        )
                    {
                        bitrate.store(bps, Ordering::Relaxed);
                        info!(
                            "bitrate {} kbit/s (latenza {} ms, minima {} ms)",
                            bps / 1000,
                            fb.latency_ms_p50,
                            fb.latency_ms_min
                        );
                    }
                }
                _ => {}
            }
        }
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
        password_required: false,
    }
}
