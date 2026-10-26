//! Connessione di segnalazione con nexodesk-server: registrazione del dispositivo
//! (prova della chiave), poi scambio di richieste e percorsi.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use nexodesk_proto::v1::{ClientMessage, Proof, Register, ServerMessage, client_message, server_message};
use quinn::ClientConfig;
use quinn::crypto::rustls::QuicClientConfig;

use crate::identity::Identity;
use crate::{Fingerprint, FramedRecv, FramedSend, PinnedCert, ServerTransport, provider, transport_config};

/// ALPN della segnalazione (deve coincidere con quello del server).
const ALPN: &[u8] = b"nexodesk-signal/1";
/// Versione del protocollo di segnalazione.
pub const PROTOCOL_VERSION: u32 = 1;
/// Prefisso firmato insieme al nonce (come nel server).
const REGISTER_CONTEXT: &[u8] = b"nexodesk-register-v1";
/// Porta UDP predefinita di nexodesk-server.
pub const DEFAULT_PORT: u16 = 21116;

pub struct Signal {
    pub device_id: String,
    /// Indirizzo del server: è anche l'indirizzo del relay.
    pub server: SocketAddr,
    /// Impronta del certificato del server vista in questa connessione.
    pub server_fingerprint: Fingerprint,
    /// Trasporto con cui il server è stato raggiunto: le sessioni useranno lo stesso.
    pub transport: ServerTransport,
    pub tx: SignalSender,
    pub rx: SignalReceiver,
}

pub struct SignalSender(FramedSend);
pub struct SignalReceiver(FramedRecv);

impl SignalSender {
    pub async fn send(&mut self, msg: client_message::Msg) -> Result<()> {
        self.0.send(&ClientMessage { msg: Some(msg) }).await
    }
}

impl SignalReceiver {
    /// Prossimo messaggio dal server; `None` se ha chiuso la connessione.
    pub async fn recv(&mut self) -> Result<Option<server_message::Msg>> {
        Ok(self.0.recv::<ServerMessage>().await?.and_then(|m| m.msg))
    }
}

/// Si collega a nexodesk-server e registra il dispositivo. Con `expected` il certificato
/// del server deve avere quell'impronta; senza, qualunque impronta viene accettata
/// e restituita in `server_fingerprint` (da memorizzare: vedi [`known_server`]).
/// Quale trasporto usare verso il server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportMode {
    /// UDP; se non risponde entro pochi secondi, WebSocket.
    #[default]
    Auto,
    Udp,
    WebSocket,
}

/// Tempo concesso a UDP prima di provare il WebSocket.
const UDP_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(4);

/// Come [`connect`], scegliendo il trasporto: con `Auto` prova UDP e, se il
/// server non risponde (rete che blocca UDP), passa al WebSocket `ws_url`.
pub async fn connect_auto(
    server: SocketAddr,
    ws_url: &str,
    mode: TransportMode,
    expected: Option<Fingerprint>,
    identity: &Identity,
    device_name: &str,
) -> Result<Signal> {
    let ws = ServerTransport::WebSocket(ws_url.to_string());
    match mode {
        TransportMode::Udp => connect(server, &ServerTransport::Udp, expected, identity, device_name).await,
        TransportMode::WebSocket => connect(server, &ws, expected, identity, device_name).await,
        TransportMode::Auto => {
            let udp = tokio::time::timeout(
                UDP_ATTEMPT,
                connect(server, &ServerTransport::Udp, expected, identity, device_name),
            )
            .await;
            match udp {
                Ok(Ok(signal)) => Ok(signal),
                // Il server ha risposto ma rifiuta (impronta, registrazione): il WebSocket non cambierebbe nulla.
                Ok(Err(e)) if !is_unreachable(&e) => Err(e),
                other => {
                    let why = match other {
                        Ok(Err(e)) => format!("{e:#}"),
                        _ => "nessuna risposta".into(),
                    };
                    tracing::info!(
                        "nexodesk-server non raggiungibile via UDP ({why}): provo il WebSocket {ws_url}"
                    );
                    connect(server, &ws, expected, identity, device_name).await
                }
            }
        }
    }
}

/// L'errore indica che il server non è stato raggiunto (e non un rifiuto)?
fn is_unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<quinn::ConnectionError>(),
            Some(quinn::ConnectionError::TimedOut | quinn::ConnectionError::Reset)
        ) || c.downcast_ref::<std::io::Error>().is_some()
    })
}

pub async fn connect(
    server: SocketAddr,
    transport: &ServerTransport,
    expected: Option<Fingerprint>,
    identity: &Identity,
    device_name: &str,
) -> Result<Signal> {
    let seen = Arc::new(Mutex::new(None));
    let verifier = PinnedCert { expected, provider: provider(), seen: Some(seen.clone()) };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport_config());

    let mut endpoint = crate::endpoint_for(transport, server, None, None).await?;
    endpoint.set_default_client_config(config);
    let conn = endpoint
        .connect(server, "nexodesk-server")?
        .await
        .with_context(|| format!("collegamento a nexodesk-server {server}"))?;
    let server_fingerprint = seen.lock().expect("impronta").expect("impronta registrata durante l'handshake");

    let (send, recv) = conn.open_bi().await?;
    let (mut tx, mut rx) = (SignalSender(FramedSend::new(send)), SignalReceiver(FramedRecv::new(recv)));
    tx.send(client_message::Msg::Register(Register {
        public_key: identity.public_key().to_vec(),
        device_name: device_name.to_string(),
        os: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION,
    }))
    .await?;

    let nonce = match rx.recv().await? {
        Some(server_message::Msg::Challenge(c)) => c.nonce,
        Some(server_message::Msg::Error(e)) => bail!("il server rifiuta la registrazione: {}", e.message),
        other => bail!("risposta inattesa alla registrazione: {other:?}"),
    };
    let signature = identity.sign(&[REGISTER_CONTEXT, &nonce].concat())?;
    tx.send(client_message::Msg::Proof(Proof { signature })).await?;
    let device_id = match rx.recv().await? {
        Some(server_message::Msg::Registered(r)) => r.device_id,
        Some(server_message::Msg::Error(e)) => bail!("il server rifiuta la registrazione: {}", e.message),
        other => bail!("risposta inattesa alla registrazione: {other:?}"),
    };
    Ok(Signal { device_id, server, server_fingerprint, transport: transport.clone(), tx, rx })
}

/// Impronte dei server già visti (`servers.txt` nella cartella dati): al primo
/// collegamento si memorizza l'impronta, ai successivi deve restare la stessa.
pub mod known_server {
    use super::*;

    fn path() -> std::path::PathBuf {
        crate::identity::data_dir().join("servers.txt")
    }

    pub fn get(server: &str) -> Option<Fingerprint> {
        let text = std::fs::read_to_string(path()).ok()?;
        text.lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(s, _)| *s == server)
            .and_then(|(_, fp)| crate::parse_fingerprint(fp).ok())
    }

    pub fn remember(server: &str, fp: &Fingerprint) -> Result<()> {
        let path = path();
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&format!("{server} {}\n", crate::fingerprint_hex(fp)));
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text)?;
        Ok(())
    }
}
