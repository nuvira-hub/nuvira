//! NexoDesk agent — il dispositivo controllato.
//!
//! Libreria usata dall'app `nexodesk` e dal programma a riga di comando `nexodesk-agent`:
//! si registra su nexodesk-server (o attende connessioni dirette in LAN), trasmette
//! lo schermo e applica mouse e tastiera ricevuti. Con il server comunica con
//! chi lo usa attraverso [`AgentEvent`] e [`AgentCommand`].

mod abr;
mod pipeline;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nexodesk_capture::{Capturer, SyntheticCapturer};
use nexodesk_codec::{EncoderPlan, EncoderSettings};
use nexodesk_proto::v1::{
    AuthResult, Codec, ControlMessage, Hello, InputEvent, Pong, VideoFormat, VideoFrame, control_message::Msg,
};
use nexodesk_transport::signal::TransportMode;
use nexodesk_transport::{Connection, Fingerprint, FramedRecv, FramedSend};
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use crate::pipeline::Pipeline;

/// Bitrate minimo del controllo adattivo.
const MIN_BITRATE: u32 = 500_000;

/// Tempo concesso a chi usa il PC per accettare una richiesta di collegamento.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// Come catturare e comprimere lo schermo.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Schermo da trasmettere; `None` = il principale.
    pub display: Option<usize>,
    /// Immagine sintetica (larghezza, altezza, fps) invece dello schermo (test).
    pub synthetic: Option<(u32, u32, u32)>,
    /// Bitrate massimo in kbit/s.
    pub bitrate_kbps: u32,
    pub fps: u32,
    /// Encoder deciso all'avvio (vedi `nexodesk_codec::plan_encoder`).
    pub plan: EncoderPlan,
    /// Bitrate fisso invece di adattarlo alla rete.
    pub fixed_bitrate: bool,
    /// Ignora mouse e tastiera del viewer.
    pub view_only: bool,
}

impl AgentConfig {
    pub fn encoder_settings(&self) -> EncoderSettings {
        EncoderSettings { bitrate_bps: self.bitrate_kbps * 1000, max_fps: self.fps }
    }

    fn open_capturer(&self) -> Result<Box<dyn Capturer>> {
        match self.synthetic {
            Some((w, h, fps)) => Ok(Box::new(SyntheticCapturer::new(w, h, fps))),
            None => nexodesk_capture::open(self.display),
        }
    }
}

/// Immagine sintetica nel formato "LxA@FPS", es. "1920x1080@60".
pub fn parse_synthetic(spec: &str) -> Result<(u32, u32, u32)> {
    let (size, fps) = spec.split_once('@').unwrap_or((spec, "60"));
    let (w, h) = size.split_once('x').context("formato atteso LxA@FPS, es. 1920x1080@60")?;
    Ok((w.parse()?, h.parse()?, fps.parse()?))
}

/// Come raggiungere nexodesk-server e come comportarsi con le richieste.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// host[:porta] del server.
    pub server_name: String,
    /// Impronta attesa del certificato; senza, si usa quella memorizzata (o si memorizza).
    pub server_key: Option<Fingerprint>,
    pub transport: TransportMode,
    /// URL del ripiego WebSocket; senza, `ws://<server>:<porta>/nexodesk`.
    pub server_ws: Option<String>,
    pub identity_path: PathBuf,
    /// Chiedere conferma (evento [`AgentEvent::Incoming`]) prima di accettare.
    pub confirm: bool,
    /// Test automatici: dove scrivere "ID password" appena registrati.
    pub info_file: Option<PathBuf>,
}

/// Cosa succede, per chi mostra lo stato all'utente.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Connecting,
    /// Registrato sul server: si è raggiungibili con questo ID.
    Online {
        device_id: String,
        websocket: bool,
    },
    Offline {
        reason: String,
    },
    /// Qualcuno vuole collegarsi: rispondere con [`AgentCommand::Respond`] entro [`CONFIRM_TIMEOUT`].
    Incoming {
        session_id: String,
        viewer_id: String,
        viewer_name: String,
    },
    /// La richiesta non è più in attesa (scaduta o già risposta).
    IncomingClosed {
        session_id: String,
    },
    SessionStarted {
        viewer_id: String,
        viewer_name: String,
        direct: bool,
    },
    SessionEnded {
        error: Option<String>,
    },
}

/// Cosa chiede chi usa il PC.
#[derive(Debug, Clone)]
pub enum AgentCommand {
    Respond { session_id: String, accept: bool },
    EndSession,
}

/// Password casuale di 8 caratteri facili da dettare (niente 0/O, 1/l/I).
pub fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 8];
    // Generatore casuale del sistema operativo (tramite ring).
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("generatore casuale");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

/// Confronto a tempo costante (non rivela quanti caratteri iniziali sono giusti).
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Modalità LAN senza server (fase 0): attende connessioni dirette sulla porta UDP indicata.
pub async fn serve_direct(
    config: AgentConfig,
    port: u16,
    once: bool,
    fingerprint_file: Option<PathBuf>,
) -> Result<()> {
    let bind: SocketAddr = ([0, 0, 0, 0], port).into();
    let (endpoint, fingerprint) = nexodesk_transport::server_endpoint(bind)?;
    info!("in attesa del viewer su UDP {bind}");
    let fingerprint = nexodesk_transport::fingerprint_hex(&fingerprint);
    info!("impronta dell'agent: {fingerprint}");
    if let Some(path) = &fingerprint_file {
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
            Ok(streams) => session(conn.clone(), streams, &config, None).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => info!("sessione con {peer} terminata"),
            Err(e) => warn!("sessione con {peer} terminata: {e:#}"),
        }
        conn.close(0u32.into(), b"fine sessione");
        if once {
            break;
        }
    }
    endpoint.wait_idle().await;
    Ok(())
}

/// Registra il dispositivo su nexodesk-server e accetta le sessioni che arrivano.
/// Se il server cade, si ricollega. Termina solo se `commands` si chiude.
pub async fn serve_via_server(
    config: AgentConfig,
    opts: ServerOptions,
    password: Arc<Mutex<String>>,
    events: mpsc::UnboundedSender<AgentEvent>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
) -> Result<()> {
    let server = nexodesk_transport::resolve_addr(&opts.server_name, nexodesk_transport::signal::DEFAULT_PORT)?;
    let identity = Arc::new(nexodesk_transport::identity::Identity::load_or_create(&opts.identity_path)?);
    let ctx = Arc::new(SessionContext {
        config,
        password,
        events,
        busy: AtomicBool::new(false),
        end_session: Notify::new(),
    });

    loop {
        let _ = ctx.events.send(AgentEvent::Connecting);
        let expected =
            opts.server_key.or_else(|| nexodesk_transport::signal::known_server::get(&opts.server_name));
        match signal_loop(&ctx, &opts, &identity, server, expected, &mut commands).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!("collegamento con nexodesk-server perso: {e:#}; riprovo tra 5 s");
                let _ = ctx.events.send(AgentEvent::Offline { reason: format!("{e:#}") });
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Stato condiviso tra il ciclo di segnalazione e la sessione in corso.
struct SessionContext {
    config: AgentConfig,
    password: Arc<Mutex<String>>,
    events: mpsc::UnboundedSender<AgentEvent>,
    busy: AtomicBool,
    end_session: Notify,
}

/// Richiesta in attesa di conferma o già accettata (in attesa del percorso).
struct Request {
    viewer_id: String,
    viewer_name: String,
}

async fn signal_loop(
    ctx: &Arc<SessionContext>,
    opts: &ServerOptions,
    identity: &Arc<nexodesk_transport::identity::Identity>,
    server: SocketAddr,
    expected: Option<Fingerprint>,
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
) -> Result<()> {
    use nexodesk_proto::v1::{Accept, Reject, client_message, server_message};
    let device_name = std::env::var("COMPUTERNAME").unwrap_or_default();
    let ws_url =
        opts.server_ws.clone().unwrap_or_else(|| nexodesk_transport::ws::default_url(&opts.server_name, server));
    let mut signal = nexodesk_transport::signal::connect_auto(
        server,
        &ws_url,
        opts.transport,
        expected,
        identity,
        &device_name,
    )
    .await?;
    let websocket = matches!(signal.transport, nexodesk_transport::ServerTransport::WebSocket(_));
    if let nexodesk_transport::ServerTransport::WebSocket(url) = &signal.transport {
        info!("collegato a nexodesk-server attraverso il WebSocket {url}");
    }
    if expected.is_none() {
        nexodesk_transport::signal::known_server::remember(&opts.server_name, &signal.server_fingerprint)?;
        info!(
            "impronta di nexodesk-server memorizzata: {}",
            nexodesk_transport::fingerprint_hex(&signal.server_fingerprint)
        );
    }
    let id = signal.device_id.clone();
    info!("registrato su {}: ID {} {} {}", opts.server_name, &id[..3], &id[3..6], &id[6..]);
    if let Some(path) = &opts.info_file {
        // Nei test automatici il viewer legge da qui ID e password.
        let password = ctx.password.lock().expect("password").clone();
        std::fs::write(path, format!("{id} {password}"))?;
    }
    let _ = ctx.events.send(AgentEvent::Online { device_id: id, websocket });

    let mut waiting: HashMap<String, Request> = HashMap::new();
    let mut accepted: HashMap<String, Request> = HashMap::new();
    let (expired_tx, mut expired_rx) = mpsc::unbounded_channel::<String>();

    loop {
        let reply = tokio::select! {
            msg = signal.rx.recv() => match msg? {
                Some(server_message::Msg::Incoming(inc)) => {
                    let request = Request { viewer_id: inc.viewer_id, viewer_name: inc.viewer_name };
                    info!("richiesta di collegamento da {} ({})", request.viewer_name, request.viewer_id);
                    if ctx.busy.load(Ordering::Relaxed) {
                        Some(client_message::Msg::Reject(Reject {
                            session_id: inc.session_id,
                            reason: "dispositivo occupato in un'altra sessione".into(),
                        }))
                    } else if opts.confirm {
                        let _ = ctx.events.send(AgentEvent::Incoming {
                            session_id: inc.session_id.clone(),
                            viewer_id: request.viewer_id.clone(),
                            viewer_name: request.viewer_name.clone(),
                        });
                        let (session_id, expired_tx) = (inc.session_id.clone(), expired_tx.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(CONFIRM_TIMEOUT).await;
                            let _ = expired_tx.send(session_id);
                        });
                        waiting.insert(inc.session_id, request);
                        None
                    } else {
                        accepted.insert(inc.session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id: inc.session_id }))
                    }
                }
                Some(server_message::Msg::Route(route)) => {
                    let request = accepted.remove(&route.session_id);
                    let p2p_route = nexodesk_transport::p2p::Route {
                        relay: server,
                        transport: signal.transport.clone(),
                        token: route.relay_token.as_slice().try_into().context("gettone del relay non valido")?,
                        peer_public_key: route
                            .peer_public_key
                            .as_slice()
                            .try_into()
                            .context("chiave del viewer non valida")?,
                        role: nexodesk_transport::p2p::Role::Agent,
                    };
                    let request = request.unwrap_or(Request { viewer_id: route.peer_id, viewer_name: String::new() });
                    ctx.busy.store(true, Ordering::Relaxed);
                    let (ctx, identity) = (ctx.clone(), identity.clone());
                    tokio::spawn(async move {
                        let result = relayed_session(&ctx, &identity, &p2p_route, &request).await;
                        if let Err(e) = &result {
                            warn!("sessione terminata: {e:#}");
                        }
                        ctx.busy.store(false, Ordering::Relaxed);
                        let _ = ctx.events.send(AgentEvent::SessionEnded { error: result.err().map(|e| format!("{e:#}")) });
                    });
                    None
                }
                Some(server_message::Msg::Error(e)) => {
                    warn!("nexodesk-server: {}", e.message);
                    None
                }
                Some(_) => None,
                None => bail!("nexodesk-server ha chiuso la connessione"),
            },
            cmd = commands.recv() => match cmd {
                Some(AgentCommand::Respond { session_id, accept }) => {
                    let Some(request) = waiting.remove(&session_id) else { continue };
                    let _ = ctx.events.send(AgentEvent::IncomingClosed { session_id: session_id.clone() });
                    if accept && !ctx.busy.load(Ordering::Relaxed) {
                        accepted.insert(session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id }))
                    } else {
                        info!("collegamento rifiutato da chi usa il PC");
                        Some(client_message::Msg::Reject(Reject { session_id, reason: "collegamento rifiutato".into() }))
                    }
                }
                Some(AgentCommand::EndSession) => {
                    ctx.end_session.notify_waiters();
                    None
                }
                None => return Ok(()),
            },
            Some(session_id) = expired_rx.recv() => {
                if waiting.remove(&session_id).is_none() {
                    continue;
                }
                let _ = ctx.events.send(AgentEvent::IncomingClosed { session_id: session_id.clone() });
                info!("nessuna risposta alla richiesta di collegamento: rifiutata");
                Some(client_message::Msg::Reject(Reject { session_id, reason: "nessuna risposta dal dispositivo".into() }))
            }
        };
        if let Some(reply) = reply {
            signal.tx.send(reply).await?;
        }
    }
}

async fn relayed_session(
    ctx: &SessionContext,
    identity: &nexodesk_transport::identity::Identity,
    route: &nexodesk_transport::p2p::Route,
    request: &Request,
) -> Result<()> {
    let endpoint = nexodesk_transport::p2p::agent_endpoint(identity, route).await?;
    // Il viewer può aprire più connessioni (diretta e attraverso il relay): vale quella
    // su cui apre il canale di controllo; le altre le chiude lui.
    let (tx, mut rx) = mpsc::channel(2);
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
    let (conn, streams) = tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .context("il viewer non si è collegato entro 20 s")?
        .context("endpoint chiuso")?;
    acceptor.abort();
    let direct = conn.remote_address() != route.relay;
    info!(
        "viewer collegato {} ({})",
        if direct { "in diretta" } else { "attraverso il relay" },
        conn.remote_address()
    );
    let _ = ctx.events.send(AgentEvent::SessionStarted {
        viewer_id: request.viewer_id.clone(),
        viewer_name: request.viewer_name.clone(),
        direct,
    });
    let password = ctx.password.lock().expect("password").clone();
    let result = tokio::select! {
        r = session(conn.clone(), streams, &ctx.config, Some(&password)) => r,
        _ = ctx.end_session.notified() => {
            info!("sessione terminata da chi usa il PC");
            Ok(())
        }
    };
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
    config: &AgentConfig,
    password: Option<&str>,
) -> Result<()> {
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    let Some(ControlMessage { msg: Some(Msg::Hello(hello)) }) = ctrl_rx.recv().await? else {
        bail!("il viewer non si è presentato");
    };
    info!(device = hello.device_name, os = hello.os, version = hello.app_version, "viewer");
    if hello.protocol_version < nexodesk_proto::PROTOCOL_VERSION {
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
            tokio::time::sleep(Duration::from_secs(2)).await;
            bail!("password errata");
        }
    }

    let keyframe = Arc::new(AtomicBool::new(true));
    let max_bps = config.bitrate_kbps * 1000;
    // Si parte da metà del massimo: la rete ignota potrebbe non reggere di più.
    let mut abr = (!config.fixed_bitrate).then(|| abr::Abr::new(max_bps / 2, MIN_BITRATE, max_bps));
    let bitrate = Arc::new(AtomicU32::new(abr.as_ref().map_or(max_bps, |a| a.bitrate())));
    let session_start = Instant::now();
    // Momento dell'ultimo frame inviato (ms dall'inizio): serve a capire se manca il feedback.
    let last_sent_ms = Arc::new(AtomicU64::new(0));
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    let (display, pipeline) = {
        let (cap_config, keyframe, bitrate) = (config.clone(), keyframe.clone(), bitrate.clone());
        // L'encoder parte dal bitrate iniziale del controllo adattivo.
        let settings =
            EncoderSettings { bitrate_bps: bitrate.load(Ordering::Relaxed), ..config.encoder_settings() };
        let (plan, fps) = (config.plan, config.fps);
        tokio::task::spawn_blocking(move || {
            Pipeline::start(
                move || cap_config.open_capturer(),
                move |gpu| nexodesk_codec::create_encoder(plan, settings, gpu),
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
                    let pong = Pong { ping_sent_us: ping.sent_us, agent_us: nexodesk_proto::now_us() };
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
        let mut injector = if config.view_only || config.synthetic.is_some() {
            None
        } else {
            let screen = nexodesk_input::ScreenRect {
                left: display.left,
                top: display.top,
                width: display.width,
                height: display.height,
            };
            Some(nexodesk_input::open(screen)?)
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
        protocol_version: nexodesk_proto::PROTOCOL_VERSION,
        device_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
        os: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        password_required: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_dictatable() {
        let p = random_password();
        assert_eq!(p.len(), 8);
        assert!(p.chars().all(|c| !"01lIoO".contains(c)));
        assert!(same_secret(&p, &p.clone()));
        assert!(!same_secret(&p, "abcdefgh") || p == "abcdefgh");
        assert!(!same_secret("abc", "abcd"));
    }

    #[test]
    fn synthetic_spec() {
        assert_eq!(parse_synthetic("1280x720@30").unwrap(), (1280, 720, 30));
        assert_eq!(parse_synthetic("640x480").unwrap(), (640, 480, 60));
        assert!(parse_synthetic("abc").is_err());
    }
}
