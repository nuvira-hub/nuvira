//! The start of a connection, before the viewer is let in: its control stream, its
//! Hello and its password, each within [`HANDSHAKE_READ`], and the Hello of the agent.

use anyhow::{Context, Result, anyhow, bail};
use nuvira_proto::v1::{Auth, ControlMessage, Hello, control_message::Msg};
use nuvira_transport::{Connection, FramedRecv};
use tracing::info;

use crate::HANDSHAKE_READ;
#[cfg(doc)]
use crate::{connected::run_session, shared};

/// Characters of the viewer's device name, system and version written to the log.
const LOGGED_NAME_CHARS: usize = 64;

/// The viewer's Hello, the first message on the control stream (logged as `what`),
/// of a protocol version this agent still serves. Shared by [`run_session`] and
/// [`shared::guest_session`].
pub(crate) async fn read_hello(ctrl_rx: &mut FramedRecv, what: &str) -> Result<Hello> {
    let first = tokio::time::timeout(HANDSHAKE_READ, ctrl_rx.recv::<ControlMessage>())
        .await
        .map_err(|_| anyhow!("the viewer did not introduce itself within {} s", HANDSHAKE_READ.as_secs()))?;
    let Some(ControlMessage { msg: Some(Msg::Hello(hello)) }) = first? else {
        bail!("the viewer did not introduce itself");
    };
    let (device, os, version) = (logged(&hello.device_name), logged(&hello.os), logged(&hello.app_version));
    info!(device, os, version, "{what}");
    if hello.protocol_version < nuvira_proto::MIN_PROTOCOL_VERSION {
        bail!("viewer protocol version not supported: {}", hello.protocol_version);
    }
    Ok(hello)
}

/// At most [`LOGGED_NAME_CHARS`] characters of a text the viewer chose, for the log
/// (tracing already escapes control characters in such fields).
fn logged(text: &str) -> &str {
    text.char_indices().nth(LOGGED_NAME_CHARS).map_or(text, |(at, _)| &text[..at])
}

/// The viewer's first `Auth`, right after the agent's Hello (within
/// [`HANDSHAKE_READ`]: a viewer that never sends it is let go).
pub(crate) async fn read_auth(ctrl_rx: &mut FramedRecv) -> Result<Auth> {
    let auth = tokio::time::timeout(HANDSHAKE_READ, ctrl_rx.recv::<ControlMessage>())
        .await
        .map_err(|_| anyhow!("the viewer did not send the password within {} s", HANDSHAKE_READ.as_secs()))?;
    match auth? {
        Some(ControlMessage { msg: Some(Msg::Auth(auth)) }) => Ok(auth),
        _ => bail!("the viewer did not send the password"),
    }
}

/// The control stream the viewer opens first, within [`HANDSHAKE_READ`]: LAN mode
/// and switch sides serve one connection at a time, and one that never opens it
/// would keep the next viewer out.
pub(crate) async fn control_channel(conn: &Connection) -> Result<(quinn::SendStream, quinn::RecvStream)> {
    let secs = HANDSHAKE_READ.as_secs();
    tokio::time::timeout(HANDSHAKE_READ, conn.accept_bi())
        .await
        .map_err(|_| anyhow!("the viewer did not open the control channel within {secs} s"))?
        .context("control channel")
}

/// `work` on the viewer's connection `conn`, closed and ended as "timed out" once
/// nothing has arrived from the viewer for 10 s (a viewer killed or cut off). The
/// connection's own idle timeout is longer, 30 s (review NET-1): this keeps the
/// computer from staying busy that long, from a task of its own.
pub(crate) async fn watched<T>(conn: &Connection, work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    let watch = nuvira_transport::liveness::watch(conn, nuvira_transport::liveness::PEER_SILENCE_LIMIT);
    let result = work.await;
    watch.result(result)
}

pub(crate) fn local_hello() -> Hello {
    Hello {
        protocol_version: nuvira_proto::PROTOCOL_VERSION,
        device_name: nuvira_transport::device_name(),
        os: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        password_required: false,
        cursor: false,
        cursor_cache: false,
        clipboard_files: false,
        handover: false,
        switch_sides: false,
        lock_on_end: false,
        two_factor: false,
        idle_limit: false,
        wallpaper: false,
        restart_safe_mode: false,
        microphone: false,
        reopen: false,
        end_reason: false,
        file_manager: false,
        video_datagrams: false,
        privacy_mode: false,
        terminal: false,
        terminal_only: false,
        tunnel: false,
        restart: false,
        restart_users: false,
        text: false,
        paste: false,
        audio: false,
        print: false,
        recording: false,
        recording_required: false,
        resolution: false,
    }
}

/// The handshake helpers (review lot 2).
#[cfg(test)]
mod handshake_tests {
    #[test]
    fn logged_names_are_cut_at_64_characters() {
        assert_eq!(super::logged("PC-OFFICE"), "PC-OFFICE");
        assert_eq!(super::logged(""), "");
        let long = "é".repeat(100);
        assert_eq!(super::logged(&long).chars().count(), 64);
        assert_eq!(super::logged(&"x".repeat(64)), "x".repeat(64));
        assert_eq!(super::logged(&"x".repeat(16 << 20)).len(), 64);
    }
}
