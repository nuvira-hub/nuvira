//! The session's other streams towards the agent: remote terminals, TCP tunnels and
//! switch sides (this computer's screen served inside the session).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use nuvira_transport::{FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::info;
use winit::event_loop::EventLoopProxy;

use super::{NetEvent, TermIn};

/// One remote terminal: opens its stream, then keys and size go out and the output
/// comes back, until the agent says the shell ended or the window closes (`rx`
/// closed: the stream is finished and the agent ends the shell). Returns the exit
/// code and the reason ("" when the shell ended by itself or the window closed).
pub(super) async fn run_terminal(
    conn: &nuvira_transport::Connection,
    id: u32,
    cols: u16,
    rows: u16,
    mut rx: mpsc::UnboundedReceiver<TermIn>,
    proxy: &EventLoopProxy<NetEvent>,
) -> Result<(i32, String)> {
    use nuvira_proto::v1::{StreamOpen, TerminalMessage, TerminalOpen, TerminalSize, stream_open::Kind, terminal_message::Msg as T};

    let (send, recv) = conn.open_bi().await?;
    let (mut tx, mut stream_rx) = (FramedSend::new(send), FramedRecv::new(recv));
    let size = |cols: u16, rows: u16| TerminalSize { cols: cols as u32, rows: rows as u32 };
    tx.send(&StreamOpen { kind: Some(Kind::Terminal(TerminalOpen { size: Some(size(cols, rows)) })) }).await?;
    info!(id, cols, rows, "terminal opened");
    loop {
        tokio::select! {
            input = rx.recv() => match input {
                Some(TermIn::Data(data)) => tx.send(&TerminalMessage { msg: Some(T::Data(data)) }).await?,
                Some(TermIn::Resize(cols, rows)) => {
                    tx.send(&TerminalMessage { msg: Some(T::Resize(size(cols, rows))) }).await?;
                }
                None => {
                    let _ = tx.finish();
                    return Ok((0, String::new()));
                }
            },
            msg = stream_rx.recv::<TerminalMessage>() => match msg? {
                Some(TerminalMessage { msg: Some(T::Data(data)) }) => {
                    let _ = proxy.send_event(NetEvent::TerminalOutput { id, data });
                }
                Some(TerminalMessage { msg: Some(T::Exit(exit)) }) => {
                    return Ok((exit.code, nuvira_i18n::message(&exit.error)));
                }
                Some(_) => {}
                None => return Ok((0, String::new())),
            },
        }
    }
}

/// Switch sides, at the technician's: a session of this computer's screen served
/// inside a stream of the session (`nuvira_transport::nested`), by the agent code
/// the app hands over ([`crate::REVERSE_HOST`]).
#[derive(Default)]
pub(super) struct ReverseHosting {
    active: Option<(nuvira_transport::Endpoint, tokio::task::JoinHandle<()>)>,
}

impl ReverseHosting {
    pub(super) async fn start(&mut self, conn: &nuvira_transport::Connection, proxy: &EventLoopProxy<NetEvent>) -> Result<()> {
        use nuvira_proto::v1::{ReverseOpen, StreamOpen, stream_open::Kind};
        if self.active.is_some() {
            return Ok(());
        }
        let host = *crate::REVERSE_HOST.get().context("not available in this program")?;
        let password = random_secret();
        let (config, fingerprint) = nuvira_transport::server_config()?;
        let (mut send, recv) = conn.open_bi().await?;
        let open = ReverseOpen {
            password: password.clone(),
            fingerprint: nuvira_transport::fingerprint_hex(&fingerprint),
            name: nuvira_transport::device_name(),
        };
        send.write_all(&nuvira_proto::framing::encode(&StreamOpen { kind: Some(Kind::Reverse(open)) })).await?;
        let endpoint = nuvira_transport::nested::server_over_stream(config, send, recv)?;
        let proxy = proxy.clone();
        let served = endpoint.clone();
        let task = tokio::spawn(async move {
            let result = host(served, password).await;
            if let Err(e) = &result {
                info!("switch sides ended: {e:#}");
            }
            let error = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
            let _ = proxy.send_event(NetEvent::SwitchSides { on: false, error });
        });
        info!("switch sides: serving this screen to the other computer");
        self.active = Some((endpoint, task));
        Ok(())
    }

    /// The technician takes control back (or the window closed): the stream ends,
    /// and the window at the other computer with it.
    pub(super) fn stop(&mut self) {
        if let Some((endpoint, task)) = self.active.take() {
            info!("switch sides: stopped");
            endpoint.close(0u32.into(), b"control taken back");
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                task.abort();
            });
        }
    }
}

impl Drop for ReverseHosting {
    fn drop(&mut self) {
        if let Some((endpoint, task)) = self.active.take() {
            endpoint.close(0u32.into(), b"session ended");
            task.abort();
        }
    }
}

/// A password for the switched session (it travels inside the session).
fn random_secret() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..4)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u32(i);
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
            format!("{:016x}", h.finish())
        })
        .collect()
}

/// One connection through a tunnel: a stream to the agent, which connects to
/// `host`:`port` of its network; then the bytes are copied both ways. `Err` with the
/// reason when the agent refused it (translated) or the connection broke.
pub(super) async fn forward(conn: &nuvira_transport::Connection, socket: tokio::net::TcpStream, host: &str, port: u16) -> Result<()> {
    use nuvira_proto::v1::{StreamOpen, TunnelOpen, TunnelResult, stream_open::Kind};
    use tokio::io::AsyncWriteExt;

    let _ = socket.set_nodelay(true);
    let (mut send, mut recv) = conn.open_bi().await?;
    let open = StreamOpen { kind: Some(Kind::Tunnel(TunnelOpen { host: host.into(), port: port as u32 })) };
    send.write_all(&nuvira_proto::framing::encode(&open)).await?;
    let limit = nuvira_transport::STREAM_HEADER_LIMIT;
    match nuvira_transport::recv_one_with_limit::<TunnelResult>(&mut recv, limit).await? {
        Some(TunnelResult { ok: true, .. }) => {}
        Some(TunnelResult { error, .. }) => bail!("{}", nuvira_i18n::message(&error)),
        None => bail!("the agent closed the tunnel"),
    }
    let (mut socket_rx, mut socket_tx) = socket.into_split();
    let up = async {
        let _ = tokio::io::copy(&mut socket_rx, &mut send).await;
        let _ = send.finish();
    };
    let down = async {
        let _ = tokio::io::copy(&mut recv, &mut socket_tx).await;
        let _ = socket_tx.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}
