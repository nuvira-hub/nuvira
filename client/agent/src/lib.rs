//! NexoDesk agent — il dispositivo controllato.
//!
//! Libreria usata dall'app `nexodesk` e dal programma a riga di comando `nexodesk-agent`:
//! si registra su nexodesk-server (o attende connessioni dirette in LAN), trasmette
//! lo schermo e applica mouse e tastiera ricevuti. Con il server comunica con
//! chi lo usa attraverso [`AgentEvent`] e [`AgentCommand`].

mod abr;
#[cfg(windows)]
pub mod ipc;
mod pipeline;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nexodesk_capture::{Capturer, SyntheticCapturer};
use nexodesk_codec::{EncoderPlan, EncoderSettings};
use nexodesk_proto::v1::{
    AuthResult, Codec, ControlMessage, DisplayInfo, Displays, Hello, InputEvent, Pong, Quality, VideoFormat,
    VideoFrame, control_message::Msg,
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
    /// Chi entra con la password temporanea deve essere confermato da chi usa il PC
    /// (evento [`AgentEvent::Incoming`]). Senza, basta la password.
    pub confirm: bool,
    /// Password permanente per l'accesso non presidiato: mai confermata.
    pub permanent_password: Option<String>,
    /// Test automatici: dove scrivere "ID password" appena registrati.
    pub info_file: Option<PathBuf>,
}

/// Cosa succede, per chi mostra lo stato all'utente (anche attraverso il canale
/// locale tra servizio e app, vedi [`ipc`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
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
    /// Password temporanea attuale (all'avvio e dopo [`AgentCommand::NewPassword`]).
    Password {
        password: String,
    },
    /// Qualcuno ha inserito la password temporanea: rispondere con
    /// [`AgentCommand::Respond`] entro [`CONFIRM_TIMEOUT`].
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
    /// Messaggio di chat da chi controlla.
    Chat {
        text: String,
    },
}

/// Cosa chiede chi usa il PC.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum AgentCommand {
    Respond { session_id: String, accept: bool },
    EndSession,
    /// Genera una nuova password temporanea (risposta: [`AgentEvent::Password`]).
    NewPassword,
    /// Messaggio di chat per chi controlla (sessione in corso).
    Chat { text: String },
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
        permanent: opts.permanent_password.clone(),
        confirm: opts.confirm,
        pending: Mutex::new(HashMap::new()),
        events,
        chat: Mutex::new(None),
        busy: AtomicBool::new(false),
        end_session: Notify::new(),
    });
    let current = ctx.password.lock().expect("password").clone();
    let _ = ctx.events.send(AgentEvent::Password { password: current });

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
    /// Password temporanea, mostrata a chi usa il PC.
    password: Arc<Mutex<String>>,
    permanent: Option<String>,
    confirm: bool,
    /// Richieste in attesa della risposta di chi usa il PC, per sessione.
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
    events: mpsc::UnboundedSender<AgentEvent>,
    /// Chat verso il viewer della sessione in corso.
    chat: Mutex<Option<mpsc::UnboundedSender<String>>>,
    busy: AtomicBool,
    end_session: Notify,
}

/// Richiesta accettata dal server, in attesa del percorso.
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

    let mut accepted: HashMap<String, Request> = HashMap::new();

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
                    } else {
                        // La conferma di chi usa il PC, se serve, arriva dopo la password.
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
                    let session_id = route.session_id.clone();
                    tokio::spawn(async move {
                        let result = relayed_session(&ctx, &identity, &p2p_route, &request, &session_id).await;
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
                    if let Some(answer) = ctx.pending.lock().expect("richieste").remove(&session_id) {
                        let _ = answer.send(accept);
                    }
                    None
                }
                Some(AgentCommand::Chat { text }) => {
                    if let Some(chat) = ctx.chat.lock().expect("chat").as_ref() {
                        let _ = chat.send(text);
                    }
                    None
                }
                Some(AgentCommand::NewPassword) => {
                    let password = random_password();
                    *ctx.password.lock().expect("password") = password.clone();
                    let _ = ctx.events.send(AgentEvent::Password { password });
                    None
                }
                Some(AgentCommand::EndSession) => {
                    ctx.end_session.notify_waiters();
                    None
                }
                None => return Ok(()),
            },
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
    session_id: &str,
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
    let gate = Gate { ctx, session_id, request, direct };
    let result = tokio::select! {
        r = session(conn.clone(), streams, &ctx.config, Some(&gate)) => r,
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

/// Controllo d'accesso di una sessione arrivata dal server: password permanente
/// (accesso non presidiato) oppure temporanea, con la conferma di chi usa il PC
/// se richiesta.
struct Gate<'a> {
    ctx: &'a SessionContext,
    session_id: &'a str,
    request: &'a Request,
    direct: bool,
}

impl Gate<'_> {
    /// `Err` con il motivo da mostrare al viewer.
    async fn authorize(&self, given: &str) -> Result<(), String> {
        let ctx = self.ctx;
        if ctx.permanent.as_deref().is_some_and(|p| same_secret(given, p)) {
            info!("accesso con la password permanente");
            return self.started();
        }
        let temporary = ctx.password.lock().expect("password").clone();
        if !same_secret(given, &temporary) {
            return Err("password errata".into());
        }
        if !ctx.confirm {
            return self.started();
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.pending.lock().expect("richieste").insert(self.session_id.to_string(), tx);
        let _ = ctx.events.send(AgentEvent::Incoming {
            session_id: self.session_id.to_string(),
            viewer_id: self.request.viewer_id.clone(),
            viewer_name: self.request.viewer_name.clone(),
        });
        let answer = tokio::time::timeout(CONFIRM_TIMEOUT, rx).await;
        ctx.pending.lock().expect("richieste").remove(self.session_id);
        let _ = ctx.events.send(AgentEvent::IncomingClosed { session_id: self.session_id.to_string() });
        match answer {
            Ok(Ok(true)) => self.started(),
            // Rifiutato da chi usa il PC, oppure nessuna app aperta per chiederglielo.
            Ok(Ok(false)) => {
                info!("collegamento non accettato sul dispositivo");
                Err("collegamento non accettato sul dispositivo".into())
            }
            _ => {
                info!("nessuna risposta alla richiesta di collegamento: rifiutata");
                Err("nessuna risposta dal dispositivo".into())
            }
        }
    }

    fn started(&self) -> Result<(), String> {
        let _ = self.ctx.events.send(AgentEvent::SessionStarted {
            viewer_id: self.request.viewer_id.clone(),
            viewer_name: self.request.viewer_name.clone(),
            direct: self.direct,
        });
        Ok(())
    }
}

/// Una sessione con un viewer già collegato. Con `gate` il viewer deve
/// autenticarsi (ed essere eventualmente confermato) prima di ricevere qualunque immagine.
async fn session(
    conn: Connection,
    (send, recv): (quinn::SendStream, quinn::RecvStream),
    config: &AgentConfig,
    gate: Option<&Gate<'_>>,
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
    let hello = Hello { password_required: gate.is_some(), ..local_hello() };
    ctrl_tx.send(&control(Msg::Hello(hello))).await?;
    if let Some(gate) = gate {
        let given = match ctrl_rx.recv::<ControlMessage>().await? {
            Some(ControlMessage { msg: Some(Msg::Auth(auth)) }) => auth.password,
            _ => bail!("il viewer non ha inviato la password"),
        };
        let verdict = gate.authorize(&given).await;
        let (ok, message) = match &verdict {
            Ok(()) => (true, String::new()),
            Err(reason) => (false, reason.clone()),
        };
        ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok, message }))).await?;
        if let Err(reason) = verdict {
            // Il motivo deve arrivare al viewer prima che la connessione si chiuda; con la
            // password errata la pausa è più lunga, contro i tentativi a raffica.
            let pause = if reason == "password errata" { 2000 } else { 500 };
            tokio::time::sleep(Duration::from_millis(pause)).await;
            bail!("{reason}");
        }
    }

    let keyframe = Arc::new(AtomicBool::new(true));
    let max_bps = config.bitrate_kbps * 1000;
    // Si parte da metà del massimo: la rete ignota potrebbe non reggere di più.
    let mut abr = (!config.fixed_bitrate).then(|| abr::Abr::new(max_bps / 2, MIN_BITRATE, max_bps));
    let bitrate = Arc::new(AtomicU32::new(abr.as_ref().map_or(max_bps, |a| a.bitrate())));
    let fps = Arc::new(AtomicU32::new(config.fps));
    let session_start = Instant::now();
    // Momento dell'ultimo frame inviato (ms dall'inizio): serve a capire se manca il feedback.
    let last_sent_ms = Arc::new(AtomicU64::new(0));
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    let source = VideoSource {
        config: config.clone(),
        frames: frame_tx,
        keyframe: keyframe.clone(),
        bitrate: bitrate.clone(),
        fps: fps.clone(),
    };
    let (mut display, first) = source.start(config.display).await?;
    ctrl_tx.send(&video_format(&display)).await?;
    if let Some(msg) = displays_message(config, &display) {
        ctrl_tx.send(&msg).await?;
    }
    // Cattura in corso: il canale di controllo la sostituisce quando cambia lo schermo.
    let pipeline = std::sync::Mutex::new(Some(first));
    let (screen_tx, mut screen_rx) = tokio::sync::watch::channel(screen_rect(&display));

    // Chat: i messaggi di chi usa il PC arrivano dall'app attraverso il contesto.
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<String>();
    let events = gate.map(|g| {
        *g.ctx.chat.lock().expect("chat") = Some(chat_tx.clone());
        g.ctx.events.clone()
    });
    // Appunti condivisi: il testo copiato qui va al viewer, quello copiato là arriva qui.
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<String>();
    let clipboard = if config.view_only || config.synthetic.is_some() {
        None
    } else {
        match nexodesk_clipboard::spawn_sync(move |text| clip_tx.send(text).is_ok()) {
            Ok(tx) => Some(tx),
            Err(e) => {
                warn!("appunti non condivisi: {e:#}");
                None
            }
        }
    };

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
                Some(text) = clip_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Clipboard(nexodesk_proto::v1::Clipboard { text }))).await?;
                    continue;
                }
                Some(text) = chat_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Chat(nexodesk_proto::v1::Chat { text }))).await?;
                    continue;
                }
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
                Some(Msg::SelectDisplay(select)) if config.synthetic.is_none() => {
                    let index = select.index as usize;
                    if index == display.index {
                        continue;
                    }
                    // Prima si ferma la cattura attuale, poi si apre la nuova.
                    let old = pipeline.lock().expect("pipeline").take();
                    let _ = tokio::task::spawn_blocking(move || drop(old)).await;
                    let (new_display, new_pipeline) = match source.start(Some(index)).await {
                        Ok(started) => started,
                        Err(e) => {
                            warn!("schermo {index} non disponibile: {e:#}; resto sul precedente");
                            source.start(Some(display.index)).await?
                        }
                    };
                    *pipeline.lock().expect("pipeline") = Some(new_pipeline);
                    display = new_display;
                    ctrl_tx.send(&video_format(&display)).await?;
                    if let Some(msg) = displays_message(config, &display) {
                        ctrl_tx.send(&msg).await?;
                    }
                    let _ = screen_tx.send(screen_rect(&display));
                }
                Some(Msg::Clipboard(clip)) => {
                    if let Some(clipboard) = &clipboard {
                        let _ = clipboard.send(clip.text);
                    }
                }
                Some(Msg::Chat(chat)) => match &events {
                    Some(events) => {
                        let _ = events.send(AgentEvent::Chat { text: chat.text });
                    }
                    None => info!("chat dal viewer: {}", chat.text),
                },
                Some(Msg::SetQuality(q)) => {
                    let (limit_bps, limit_fps) = quality_limits(config, q.quality());
                    fps.store(limit_fps, Ordering::Relaxed);
                    let bps = match abr.as_mut() {
                        Some(abr) => abr.set_max(limit_bps),
                        None => Some(limit_bps),
                    };
                    if let Some(bps) = bps {
                        bitrate.store(bps, Ordering::Relaxed);
                    }
                    info!("qualità {:?}: fino a {} kbit/s e {limit_fps} fps", q.quality(), limit_bps / 1000);
                }
                _ => {}
            }
        }
    };

    let input = async {
        let mut rx = FramedRecv::new(conn.accept_uni().await?);
        let injector = if config.view_only || config.synthetic.is_some() {
            None
        } else {
            let screen = *screen_rx.borrow_and_update();
            Some(input_thread(nexodesk_input::open(screen)?)?)
        };
        let closed = || anyhow!("thread dell'input terminato");
        loop {
            tokio::select! {
                event = rx.recv::<InputEvent>() => match event? {
                    Some(event) => if let Some(injector) = &injector {
                        injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                    },
                    None => break,
                },
                Ok(()) = screen_rx.changed() => if let Some(injector) = &injector {
                    let screen = *screen_rx.borrow_and_update();
                    injector.send(InputCmd::Screen(screen)).map_err(|_| closed())?;
                },
            }
        }
        anyhow::Ok(())
    };

    let result = tokio::select! {
        r = video => r.context("invio video"),
        r = control_loop => r.context("canale di controllo"),
        r = input => r.context("canale input"),
    };
    let last = pipeline.lock().expect("pipeline").take();
    let _ = tokio::task::spawn_blocking(move || drop(last)).await;
    if let Some(gate) = gate {
        *gate.ctx.chat.lock().expect("chat") = None;
    }
    drop(chat_tx);
    result
}

/// Cattura e compressione di uno schermo, riavviabili su un altro durante la sessione.
struct VideoSource {
    config: AgentConfig,
    /// Copia del mittente: tiene aperto il canale video mentre si cambia schermo.
    frames: mpsc::Sender<VideoFrame>,
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    fps: Arc<AtomicU32>,
}

impl VideoSource {
    async fn start(&self, display: Option<usize>) -> Result<(nexodesk_capture::Display, Pipeline)> {
        let mut cap_config = self.config.clone();
        cap_config.display = display;
        // L'encoder parte dal bitrate attuale del controllo adattivo.
        let settings = EncoderSettings {
            bitrate_bps: self.bitrate.load(Ordering::Relaxed),
            max_fps: self.fps.load(Ordering::Relaxed),
        };
        let plan = self.config.plan;
        // Un flusso nuovo deve iniziare con un keyframe.
        self.keyframe.store(true, Ordering::Relaxed);
        let (frames, keyframe, bitrate, fps) =
            (self.frames.clone(), self.keyframe.clone(), self.bitrate.clone(), self.fps.clone());
        tokio::task::spawn_blocking(move || {
            Pipeline::start(
                move || cap_config.open_capturer(),
                move |gpu| nexodesk_codec::create_encoder(plan, settings, gpu),
                plan == EncoderPlan::HardwareGpu,
                fps,
                frames,
                keyframe,
                bitrate,
            )
        })
        .await?
        .context("avvio di cattura e compressione")
    }
}

fn video_format(display: &nexodesk_capture::Display) -> ControlMessage {
    control(Msg::VideoFormat(VideoFormat { codec: Codec::H264 as i32, width: display.width, height: display.height }))
}

/// Elenco degli schermi per il viewer (niente con l'immagine sintetica dei test).
fn displays_message(config: &AgentConfig, current: &nexodesk_capture::Display) -> Option<ControlMessage> {
    if config.synthetic.is_some() {
        return None;
    }
    let list = nexodesk_capture::displays().ok()?;
    let displays = list
        .into_iter()
        .map(|d| DisplayInfo {
            index: d.index as u32,
            name: d.name,
            width: d.width,
            height: d.height,
            primary: d.primary,
        })
        .collect();
    Some(control(Msg::Displays(Displays { displays, current: current.index as u32 })))
}

fn screen_rect(display: &nexodesk_capture::Display) -> nexodesk_input::ScreenRect {
    nexodesk_input::ScreenRect { left: display.left, top: display.top, width: display.width, height: display.height }
}

/// Tetto del bitrate (bit/s) e fps per la qualità scelta dal viewer.
fn quality_limits(config: &AgentConfig, quality: Quality) -> (u32, u32) {
    let (bps, fps) = (config.bitrate_kbps * 1000, config.fps);
    match quality {
        Quality::Sharp => (bps * 2, fps.min(30)),
        Quality::Fast => ((bps / 2).max(MIN_BITRATE), fps),
        Quality::Balanced | Quality::Unspecified => (bps, fps),
    }
}

/// Cosa riceve il thread dell'input.
enum InputCmd {
    Event(InputEvent),
    /// Cambio di schermo: le coordinate del mouse si riferiscono a questo.
    Screen(nexodesk_input::ScreenRect),
}

/// Inietta l'input su un thread dedicato, che può seguire il desktop attivo
/// (UAC, schermata di blocco) senza toccare i thread di tokio. Chiudendo il
/// canale il thread termina e rilascia i tasti ancora premuti.
fn input_thread(
    mut injector: Box<dyn nexodesk_input::InputInjector>,
) -> Result<std::sync::mpsc::Sender<InputCmd>> {
    let (tx, rx) = std::sync::mpsc::channel::<InputCmd>();
    std::thread::Builder::new().name("input".into()).spawn(move || {
        for cmd in rx {
            let event = match cmd {
                InputCmd::Event(event) => event,
                InputCmd::Screen(screen) => {
                    injector.set_screen(screen);
                    continue;
                }
            };
            #[cfg(windows)]
            if let Some(name) = nexodesk_capture::desktop::follow_input_desktop() {
                info!("input sul desktop {name}");
            }
            if let Err(e) = injector.inject(&event) {
                warn!("input non iniettato: {e:#}");
            }
        }
    })?;
    Ok(tx)
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
