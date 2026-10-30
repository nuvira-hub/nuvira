//! Accounts and address book on nuvira-server.
//!
//! Requests travel on the signalling connection (registered as an unreachable
//! device), each with a number that comes back in the answer. If the connection
//! drops, the pending and later requests fail: the user of the client opens a new one.

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::{Result, anyhow, bail};
use nuvira_proto::v1::{ApiRequest, ApiResponse, api_request, api_response, client_message, server_message};
use tokio::sync::{mpsc, oneshot};

use crate::identity::Identity;
use crate::signal::{self, DeviceInfo, TransportMode};

type Reply = oneshot::Sender<Result<ApiResponse>>;

#[derive(Clone)]
pub struct ApiClient {
    requests: mpsc::UnboundedSender<(api_request::Call, Reply)>,
    features: Vec<String>,
}

impl ApiClient {
    /// Connects to the server (fingerprint remembered as for agent and viewer).
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
        let features = std::mem::take(&mut sig.features);
        let (tx, mut rx) = mpsc::unbounded_channel::<(api_request::Call, Reply)>();
        tokio::spawn(async move {
            let mut pending: HashMap<u32, Reply> = HashMap::new();
            let mut next_id = 0u32;
            let reason = loop {
                tokio::select! {
                    request = rx.recv() => {
                        let Some((call, reply)) = request else { break "client closed".to_string() };
                        next_id = next_id.wrapping_add(1);
                        let msg = client_message::Msg::Api(ApiRequest { id: next_id, call: Some(call) });
                        if let Err(e) = sig.tx.send(msg).await {
                            let _ = reply.send(Err(anyhow!("request not sent: {e:#}")));
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
                        Ok(None) => break "the server closed the connection".to_string(),
                        Err(e) => break format!("{e:#}"),
                    },
                }
            };
            tracing::debug!("address book connection closed: {reason}");
            for (_, reply) in pending {
                let _ = reply.send(Err(anyhow!("connection with the server lost: {reason}")));
            }
        });
        Ok(Self { requests: tx, features })
    }

    /// Runs a request; `Err` with the server's message if it fails.
    pub async fn call(&self, call: api_request::Call) -> Result<Option<api_response::Result>> {
        let (reply, answer) = oneshot::channel();
        self.requests.send((call, reply)).map_err(|_| anyhow!("connection with the server lost"))?;
        let resp = tokio::time::timeout(std::time::Duration::from_secs(20), answer)
            .await
            .map_err(|_| anyhow!("the server does not answer"))?
            .map_err(|_| anyhow!("connection with the server lost"))??;
        if !resp.error.is_empty() {
            bail!("{}", resp.error);
        }
        Ok(resp.result)
    }

    /// Optional modules of the server, from its registration answer.
    pub fn features(&self) -> &[String] {
        &self.features
    }

    /// The client cannot be used any more (connection dropped).
    pub fn is_closed(&self) -> bool {
        self.requests.is_closed()
    }
}
