//! Lato rete del viewer: connessione all'agent, ricezione e decompressione del
//! video, invio dell'input e misura di RTT e differenza tra gli orologi.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nexo_codec::YuvFrame;
use nexo_proto::v1::{
    ControlMessage, Hello, InputEvent, Ping, RequestKeyframe, VideoFrame, control_message::Msg,
};
use nexo_transport::{Fingerprint, FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::{info, warn};
use winit::event_loop::EventLoopProxy;

/// Eventi dalla rete verso la finestra.
pub enum NetEvent {
    Connected { agent: String, width: u32, height: u32 },
    Frame(Box<DecodedFrame>),
    Clock { rtt_us: u64, offset_us: i64 },
    Disconnected(String),
}

pub struct DecodedFrame {
    pub yuv: YuvFrame,
    /// Momento della cattura secondo l'orologio dell'agent.
    pub capture_us: u64,
    pub encode_us: u32,
    pub decode_us: u32,
    pub bytes: usize,
}

/// Avvia la sessione su un thread dedicato. Restituisce il canale per l'input.
pub fn start(
    addr: SocketAddr,
    fingerprint: Option<Fingerprint>,
    proxy: EventLoopProxy<NetEvent>,
) -> mpsc::UnboundedSender<InputEvent> {
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("rete".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("runtime tokio");
            let reason = match rt.block_on(session(addr, fingerprint, proxy.clone(), input_rx)) {
                Ok(()) => "sessione terminata".to_string(),
                Err(e) => format!("{e:#}"),
            };
            let _ = proxy.send_event(NetEvent::Disconnected(reason));
        })
        .expect("thread di rete");
    input_tx
}

fn control(msg: Msg) -> ControlMessage {
    ControlMessage { msg: Some(msg) }
}

async fn session(
    addr: SocketAddr,
    fingerprint: Option<Fingerprint>,
    proxy: EventLoopProxy<NetEvent>,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
) -> Result<()> {
    let conn = nexo_transport::connect(addr, fingerprint).await?;
    let (send, recv) = conn.open_bi().await?;
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    ctrl_tx
        .send(&control(Msg::Hello(Hello {
            protocol_version: nexo_proto::PROTOCOL_VERSION,
            device_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
            os: std::env::consts::OS.into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
        })))
        .await?;
    let Some(ControlMessage { msg: Some(Msg::Hello(agent)) }) = ctrl_rx.recv().await? else {
        bail!("l'agent non si è presentato");
    };
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
    std::thread::Builder::new()
        .name("decodifica".into())
        .spawn(move || decode_loop(frame_rx, decoder_proxy, key_tx))?;

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
                    ctrl_tx.send(&control(Msg::Ping(Ping { sent_us: nexo_proto::now_us() }))).await?;
                }
                Some(()) = key_rx.recv() => {
                    ctrl_tx.send(&control(Msg::RequestKeyframe(RequestKeyframe {}))).await?;
                }
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(ControlMessage { msg: Some(Msg::Pong(pong)) }) => {
                        let now = nexo_proto::now_us();
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
    result
}

fn decode_loop(
    frames: std::sync::mpsc::Receiver<(VideoFrame, Instant)>,
    proxy: EventLoopProxy<NetEvent>,
    request_keyframe: mpsc::UnboundedSender<()>,
) {
    let mut decoder = match nexo_codec::best_decoder() {
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
                if proxy.send_event(NetEvent::Frame(Box::new(decoded))).is_err() {
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
