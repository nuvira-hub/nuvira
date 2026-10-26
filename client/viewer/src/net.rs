//! Lato rete del viewer: connessione all'agent, ricezione e decompressione del
//! video, invio dell'input e misura di RTT e differenza tra gli orologi.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nexodesk_codec::YuvFrame;
use nexodesk_proto::v1::{
    ControlMessage, Feedback, Hello, InputEvent, Ping, RequestKeyframe, VideoFrame, control_message::Msg,
};
use nexodesk_transport::{Fingerprint, FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::{info, warn};
use winit::event_loop::EventLoopProxy;

/// Eventi dalla rete verso la finestra.
pub enum NetEvent {
    Connected {
        agent: String,
        width: u32,
        height: u32,
    },
    /// Nello [`FrameSlot`] c'è un frame nuovo da mostrare.
    FrameReady,
    Clock {
        rtt_us: u64,
        offset_us: i64,
    },
    Disconnected(String),
    /// Scaduta la durata impostata con `--duration`.
    TestFinished,
}

pub struct DecodedFrame {
    pub yuv: YuvFrame,
    /// Momento della cattura secondo l'orologio dell'agent.
    pub capture_us: u64,
    pub encode_us: u32,
    pub decode_us: u32,
    pub bytes: usize,
}

/// Ultimo frame decompresso, in attesa di essere mostrato.
///
/// Se la finestra è più lenta della decompressione (scheda video modesta, vsync),
/// il frame non ancora mostrato viene sostituito dal nuovo invece di finire in
/// coda: si salta un frame ma la latenza non cresce.
#[derive(Default)]
pub struct FrameSlot {
    frame: Mutex<Option<Box<DecodedFrame>>>,
    notified: AtomicBool,
    dropped: AtomicU64,
}

impl FrameSlot {
    /// Deposita un frame; `true` se la finestra va avvisata.
    fn put(&self, frame: Box<DecodedFrame>) -> bool {
        if self.frame.lock().expect("slot").replace(frame).is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        !self.notified.swap(true, Ordering::AcqRel)
    }

    pub fn take(&self) -> Option<Box<DecodedFrame>> {
        self.notified.store(false, Ordering::Release);
        self.frame.lock().expect("slot").take()
    }

    /// Frame decompressi ma mai mostrati perché superati da uno più recente.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Come raggiungere l'agent.
#[derive(Clone)]
pub enum Target {
    /// Connessione diretta in LAN (fase 0), con l'impronta dell'agent.
    Direct { addr: SocketAddr, fingerprint: Option<Fingerprint> },
    /// Per ID attraverso nexodesk-server e il suo relay.
    Server {
        server: SocketAddr,
        server_name: String,
        server_key: Option<Fingerprint>,
        transport: nexodesk_transport::signal::TransportMode,
        server_ws: Option<String>,
        device_id: String,
        identity: std::path::PathBuf,
    },
}

/// Avvia la sessione su un thread dedicato. Restituisce il canale per l'input
/// (chiuderlo termina la sessione) e il thread, da attendere prima di uscire.
pub fn start(
    target: Target,
    password: Option<String>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
) -> (mpsc::UnboundedSender<InputEvent>, mpsc::UnboundedSender<Feedback>, std::thread::JoinHandle<()>) {
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (feedback_tx, feedback_rx) = mpsc::unbounded_channel();
    let thread = std::thread::Builder::new()
        .name("rete".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("runtime tokio");
            let reason =
                match rt.block_on(session(target, password, proxy.clone(), slot, input_rx, feedback_rx)) {
                    Ok(()) => "sessione terminata".to_string(),
                    Err(e) => format!("{e:#}"),
                };
            let _ = proxy.send_event(NetEvent::Disconnected(reason));
        })
        .expect("thread di rete");
    (input_tx, feedback_tx, thread)
}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

/// Connessione all'agent: diretta oppure chiedendo il percorso a nexodesk-server.
async fn open_connection(target: Target) -> Result<nexodesk_transport::Connection> {
    use nexodesk_proto::v1::{Connect, Role, client_message, server_message};
    use nexodesk_transport::{identity::Identity, p2p, signal};
    match target {
        Target::Direct { addr, fingerprint } => nexodesk_transport::connect(addr, fingerprint).await,
        Target::Server { server, server_name, server_key, transport, server_ws, device_id, identity } => {
            let identity = Identity::load_or_create(&identity)?;
            let expected = server_key.or_else(|| signal::known_server::get(&server_name));
            let name = std::env::var("COMPUTERNAME").unwrap_or_default();
            let ws_url = server_ws.unwrap_or_else(|| nexodesk_transport::ws::default_url(&server_name, server));
            let mut sig =
                signal::connect_auto(server, &ws_url, transport, expected, &identity, &name).await?;
            if let nexodesk_transport::ServerTransport::WebSocket(url) = &sig.transport {
                info!("collegato a nexodesk-server attraverso il WebSocket {url}");
            }
            if expected.is_none() {
                signal::known_server::remember(&server_name, &sig.server_fingerprint)?;
            }
            info!(id = sig.device_id, "registrato su {server_name}; richiedo {device_id}");
            sig.tx.send(client_message::Msg::Connect(Connect { device_id: device_id.clone() })).await?;
            let route = loop {
                match sig.rx.recv().await? {
                    Some(server_message::Msg::Route(r)) if r.role() == Role::Viewer => break r,
                    Some(server_message::Msg::Error(e)) => bail!("{}", e.message),
                    Some(_) => {}
                    None => bail!("nexodesk-server ha chiuso la connessione"),
                }
            };
            let route = p2p::Route {
                relay: server,
                transport: sig.transport.clone(),
                token: route.relay_token.as_slice().try_into().context("gettone del relay non valido")?,
                peer_public_key: route
                    .peer_public_key
                    .as_slice()
                    .try_into()
                    .context("chiave dell'agent non valida")?,
                role: p2p::Role::Viewer,
            };
            let (conn, path) = p2p::viewer_connect(&identity, &route).await?;
            info!(?path, remote = %conn.remote_address(), "percorso");
            Ok(conn)
        }
    }
}

async fn session(
    target: Target,
    password: Option<String>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    mut feedback_rx: mpsc::UnboundedReceiver<Feedback>,
) -> Result<()> {
    let conn = open_connection(target).await?;
    let (send, recv) = conn.open_bi().await?;
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    ctrl_tx
        .send(&control(Msg::Hello(Hello {
            protocol_version: nexodesk_proto::PROTOCOL_VERSION,
            device_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
            os: std::env::consts::OS.into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            password_required: false,
        })))
        .await?;
    let Some(ControlMessage { msg: Some(Msg::Hello(agent)) }) = ctrl_rx.recv().await? else {
        bail!("l'agent non si è presentato");
    };
    if agent.password_required {
        let password = password.context("il dispositivo richiede la password (--password)")?;
        ctrl_tx.send(&control(Msg::Auth(nexodesk_proto::v1::Auth { password }))).await?;
        match ctrl_rx.recv().await? {
            Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) if r.ok => {}
            Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) => bail!("accesso negato: {}", r.message),
            _ => bail!("nessuna risposta alla password"),
        }
    }
    let Some(ControlMessage { msg: Some(Msg::VideoFormat(format)) }) = ctrl_rx.recv().await? else {
        bail!("formato video mancante");
    };
    info!(agent = agent.device_name, w = format.width, h = format.height, "collegato");
    let _ = proxy.send_event(NetEvent::Connected {
        agent: agent.device_name,
        width: format.width,
        height: format.height,
    });

    let mut input_tx = FramedSend::new(conn.open_uni().await?);
    tracing::debug!("canale input aperto");
    let mut video_rx = FramedRecv::new(conn.accept_uni().await?);
    tracing::debug!("canale video accettato");

    // La decompressione avviene su un thread a parte per non bloccare la rete.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<()>();
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<(VideoFrame, Instant)>(4);
    let decoder_proxy = proxy.clone();
    let decoder = std::thread::Builder::new()
        .name("decodifica".into())
        .spawn(move || decode_loop(frame_rx, decoder_proxy, slot, key_tx))?;

    let video = async {
        while let Some(frame) = video_rx.recv::<VideoFrame>().await? {
            if frame_tx.send((frame, Instant::now())).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    };

    let input = async {
        while let Some(event) = input_rx.recv().await {
            input_tx.send(&event).await?;
        }
        anyhow::Ok(())
    };

    let control_loop = async {
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    ctrl_tx.send(&control(Msg::Ping(Ping { sent_us: nexodesk_proto::now_us() }))).await?;
                }
                Some(feedback) = feedback_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Feedback(feedback))).await?;
                }
                Some(()) = key_rx.recv() => {
                    ctrl_tx.send(&control(Msg::RequestKeyframe(RequestKeyframe {}))).await?;
                }
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(ControlMessage { msg: Some(Msg::Pong(pong)) }) => {
                        let now = nexodesk_proto::now_us();
                        let rtt_us = now.saturating_sub(pong.ping_sent_us);
                        // Orologio dell'agent meno il nostro, assumendo andata e ritorno simmetrici.
                        let midpoint = (pong.ping_sent_us + now) / 2;
                        let offset_us = pong.agent_us as i64 - midpoint as i64;
                        let _ = proxy.send_event(NetEvent::Clock { rtt_us, offset_us });
                    }
                    Some(_) => {}
                    None => return anyhow::Ok(()),
                },
            }
        }
    };

    let result = tokio::select! {
        r = video => r.context("ricezione video"),
        r = input => r.context("invio input"),
        r = control_loop => r.context("canale di controllo"),
    };
    conn.close(0u32.into(), b"fine sessione");
    // Si dà il tempo al messaggio di chiusura di partire: senza, l'agent se ne
    // accorgerebbe solo al timeout e resterebbe "occupato" nel frattempo.
    let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
    // Con il tunnel WebSocket i datagrammi passano da una coda svuotata da un task:
    // un attimo di tempo perché il messaggio di chiusura parta davvero.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Il decoder termina quando si chiude il suo canale: lo si attende, così
    // all'uscita del programma nessun nostro thread è ancora al lavoro.
    drop(frame_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || decoder.join()))
        .await;
    result
}

fn decode_loop(
    frames: std::sync::mpsc::Receiver<(VideoFrame, Instant)>,
    proxy: EventLoopProxy<NetEvent>,
    slot: Arc<FrameSlot>,
    request_keyframe: mpsc::UnboundedSender<()>,
) {
    let mut decoder = match nexodesk_codec::best_decoder() {
        Ok(d) => d,
        Err(e) => {
            let _ = proxy.send_event(NetEvent::Disconnected(format!("decoder non disponibile: {e:#}")));
            return;
        }
    };
    info!("decoder: {}", decoder.name());
    let mut waiting_key = false;
    while let Ok((frame, _received)) = frames.recv() {
        // Dopo un errore si scartano i frame fino al prossimo keyframe.
        if waiting_key && !frame.keyframe {
            continue;
        }
        let t = Instant::now();
        tracing::trace!(seq = frame.seq, bytes = frame.data.len(), "frame ricevuto");
        match decoder.decode(&frame.data) {
            Ok(Some(yuv)) => {
                waiting_key = false;
                let decoded = DecodedFrame {
                    yuv: YuvFrame {
                        width: yuv.width,
                        height: yuv.height,
                        y: yuv.y.clone(),
                        u: yuv.u.clone(),
                        v: yuv.v.clone(),
                    },
                    capture_us: frame.capture_us,
                    encode_us: frame.encode_us,
                    decode_us: t.elapsed().as_micros() as u32,
                    bytes: frame.data.len(),
                };
                if slot.put(Box::new(decoded)) && proxy.send_event(NetEvent::FrameReady).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!("frame {} non decodificabile: {e:#}; chiedo un keyframe", frame.seq);
                waiting_key = true;
                let _ = request_keyframe.send(());
            }
        }
    }
}
