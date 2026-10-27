//! Account e rubrica su nexodesk-server.
//!
//! Le richieste viaggiano sulla connessione di segnalazione (registrata come
//! dispositivo non raggiungibile), ognuna con un numero che ritorna nella
//! risposta. Se la connessione cade, le richieste in corso e le successive
//! falliscono: chi usa il client ne apre uno nuovo.

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::{Result, anyhow, bail};
use nexodesk_proto::v1::{ApiRequest, ApiResponse, api_request, api_response, client_message, server_message};
use tokio::sync::{mpsc, oneshot};

use crate::identity::Identity;
use crate::signal::{self, DeviceInfo, TransportMode};

type Reply = oneshot::Sender<Result<ApiResponse>>;

#[derive(Clone)]
pub struct ApiClient {
    requests: mpsc::UnboundedSender<(api_request::Call, Reply)>,
}

impl ApiClient {
    /// Si collega al server (impronta memorizzata come per agent e viewer).
    pub async fn connect(
        server_name: &str,
        server: SocketAddr,
        ws_url: &str,
        mode: TransportMode,
        identity: &Identity,
        device_name: &str,
    ) -> Result<Self> {
        let expected = signal::known_server::get(server_name);
        let device = DeviceInfo { name: device_name, reachable: false };
        let mut sig = signal::connect_auto(server, ws_url, mode, expected, identity, &device).await?;
        if expected.is_none() {
            signal::known_server::remember(server_name, &sig.server_fingerprint)?;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<(api_request::Call, Reply)>();
        tokio::spawn(async move {
            let mut pending: HashMap<u32, Reply> = HashMap::new();
            let mut next_id = 0u32;
            let reason = loop {
                tokio::select! {
                    request = rx.recv() => {
                        let Some((call, reply)) = request else { break "client chiuso".to_string() };
                        next_id = next_id.wrapping_add(1);
                        let msg = client_message::Msg::Api(ApiRequest { id: next_id, call: Some(call) });
                        if let Err(e) = sig.tx.send(msg).await {
                            let _ = reply.send(Err(anyhow!("richiesta non inviata: {e:#}")));
                            break format!("{e:#}");
                        }
                        pending.insert(next_id, reply);
                    }
                    msg = sig.rx.recv() => match msg {
                        Ok(Some(server_message::Msg::Api(resp))) => {
                            if let Some(reply) = pending.remove(&resp.id) {
                                let _ = reply.send(Ok(resp));
                            }
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => break "il server ha chiuso la connessione".to_string(),
                        Err(e) => break format!("{e:#}"),
                    },
                }
            };
            tracing::debug!("connessione alla rubrica chiusa: {reason}");
            for (_, reply) in pending {
                let _ = reply.send(Err(anyhow!("connessione con il server persa: {reason}")));
            }
        });
        Ok(Self { requests: tx })
    }

    /// Esegue una richiesta; `Err` con il messaggio del server se non riesce.
    pub async fn call(&self, call: api_request::Call) -> Result<Option<api_response::Result>> {
        let (reply, answer) = oneshot::channel();
        self.requests.send((call, reply)).map_err(|_| anyhow!("connessione con il server persa"))?;
        let resp = tokio::time::timeout(std::time::Duration::from_secs(20), answer)
            .await
            .map_err(|_| anyhow!("il server non risponde"))?
            .map_err(|_| anyhow!("connessione con il server persa"))??;
        if !resp.error.is_empty() {
            bail!("{}", resp.error);
        }
        Ok(resp.result)
    }

    /// Il client non è più utilizzabile (connessione caduta).
    pub fn is_closed(&self) -> bool {
        self.requests.is_closed()
    }
}
