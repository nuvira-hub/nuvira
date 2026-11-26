//! From the start of a session to its channels: the connection to the agent (direct or
//! through nuvira-server), the handshake (password, confirmation, two-factor code) and
//! how the connection is closed.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::{ControlMessage, Hello, InputEvent, control_message::Msg};
use nuvira_transport::{FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::info;
use winit::event_loop::EventLoopProxy;

use super::{Denied, NetEvent, Target, control, remote_is_windows};

/// How the connection goes and the server's session id, for the statistics log.
pub(super) struct PathInfo {
    /// `direct`, `relay` or `websocket`.
    pub(super) path: &'static str,
    pub(super) session_id: Option<String>,
}

/// Connection to the agent: direct, or asking nuvira-server for the route.
/// Also returns whether the path is direct (`false`: through the server's relay).
pub(super) async fn open_connection(
    target: Target,
) -> Result<(nuvira_transport::Connection, bool, Option<crate::upload::Target>, PathInfo)> {
    use nuvira_proto::v1::{Connect, Role, client_message, server_message};
    use nuvira_transport::{identity::Identity, p2p, signal};
    match target {
        Target::Direct { addr, fingerprint } => {
            let info = PathInfo { path: "direct", session_id: None };
            Ok((nuvira_transport::connect(addr, fingerprint).await?, true, None, info))
        }
        Target::Server { server, server_name, server_key, transport, server_ws, device_id, identity } => {
            let identity_path = identity.clone();
            // On a blocking thread: it may wait up to 20 s for a locked keyring
            // (review NET-14).
            let identity = tokio::task::spawn_blocking(move || {
                Identity::load_or_create_within(&identity, Some(std::time::Duration::from_secs(20)))
            })
            .await?
            .map_err(|e| match e.downcast_ref::<nuvira_transport::identity::KeyringLocked>() {
                Some(locked) => {
                    tracing::warn!("{locked}");
                    // The Keychain on macOS; KWallet or GNOME Keyring on Linux.
                    let key = if cfg!(target_os = "macos") { "viewer.keyring_locked_mac" } else { "viewer.keyring_locked" };
                    anyhow::anyhow!(nuvira_i18n::t(key))
                }
                None => e,
            })?;
            let expected = server_key.or_else(|| signal::known_server::get(&server_name));
            let name = nuvira_transport::device_name();
            // The viewer receives no connections: several session windows can coexist.
            let device = signal::DeviceInfo { name: &name, reachable: false, alias: None };
            let ws_url = server_ws.unwrap_or_else(|| nuvira_transport::ws::default_url(&server_name, server));
            let mut sig =
                signal::connect_auto(server, &ws_url, transport, expected, &identity, &device).await?;
            if let nuvira_transport::ServerTransport::WebSocket(url) = &sig.transport {
                info!("connected to nuvira-server through the WebSocket {url}");
            }
            if expected.is_none() {
                signal::known_server::remember(&server_name, &sig.server_fingerprint)?;
            }
            info!(id = sig.device_id, "registered on {server_name}; requesting {device_id}");
            // The app's address book sign-in, for the session log (who worked on it).
            let account_token = std::env::var("NUVIRA_ACCOUNT_TOKEN").unwrap_or_default();
            sig.tx.send(client_message::Msg::Connect(Connect { device_id: device_id.clone(), account_token })).await?;
            let route = loop {
                match sig.rx.recv().await? {
                    Some(server_message::Msg::Route(r)) if r.role() == Role::Viewer => break r,
                    Some(server_message::Msg::Error(e)) => bail!("{}", nuvira_i18n::message(&e.message)),
                    Some(_) => {}
                    None => bail!("nuvira-server closed the connection"),
                }
            };
            let session_id = route.session_id.clone();
            let route = p2p::Route {
                relay: server,
                transport: sig.transport.clone(),
                token: route.relay_token.as_slice().try_into().context("invalid relay token")?,
                peer_public_key: route.peer_public_key.as_slice().try_into().context("invalid agent key")?,
                role: p2p::Role::Viewer,
                relay_alt: match sig.transport {
                    nuvira_transport::ServerTransport::Udp => nuvira_transport::other_family(&server_name, server).await,
                    nuvira_transport::ServerTransport::WebSocket(_) => None,
                },
            };
            // NuviraHub Pro: the recordings of this session go to the server too.
            let upload = sig.features.iter().any(|f| f == "pro.recording").then(|| crate::upload::Target {
                server: server_name.clone(),
                server_key: nuvira_transport::fingerprint_hex(&sig.server_fingerprint),
                transport,
                server_ws: Some(ws_url.clone()),
                identity: identity_path.clone(),
                session: session_id.clone(),
            });
            let (conn, path) = p2p::viewer_connect(&identity, &route).await?;
            let remote = p2p::canonical(conn.remote_address());
            let family = if remote.is_ipv6() { "IPv6" } else { "IPv4" };
            info!(?path, %remote, family, "path");
            let info = PathInfo {
                path: match (path == p2p::Path::Direct, &route.transport) {
                    (true, _) => "direct",
                    (false, nuvira_transport::ServerTransport::WebSocket(_)) => "websocket",
                    (false, nuvira_transport::ServerTransport::Udp) => "relay",
                },
                session_id: Some(session_id),
            };
            Ok((conn, path == p2p::Path::Direct, upload, info))
        }
    }
}

/// Waits for the viewer to drop the input channel (the events sent meanwhile, before
/// the session starts, are discarded).
pub(super) async fn viewer_gone(input_rx: &mut mpsc::UnboundedReceiver<InputEvent>) {
    while input_rx.recv().await.is_some() {}
}

/// Closes the connection and gives the close message time to leave: without it the
/// agent would notice only at the timeout and stay "busy" in the meantime.
/// Why the agent closed the connection, when it says it ("@key", translated):
/// e.g. the screen of a Linux computer is locked.
pub(super) async fn agent_reason(conn: &nuvira_transport::Connection) -> Option<String> {
    let closed = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await.ok()?;
    let quinn::ConnectionError::ApplicationClosed(close) = closed else { return None };
    let reason = std::str::from_utf8(&close.reason).ok()?;
    reason.starts_with('@').then(|| nuvira_i18n::message(reason))
}

pub(super) async fn close(conn: &nuvira_transport::Connection) {
    conn.close(0u32.into(), b"end of session");
    let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
    // With the WebSocket tunnel datagrams go through a queue emptied by a task:
    // a moment for the close message to really leave.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// From the connection to the start of the session: introductions, password (and the
/// confirmation at the remote PC), video format, input and video channels.
pub(super) async fn handshake(
    conn: &nuvira_transport::Connection,
    direct: bool,
    password: Option<String>,
    proxy: &EventLoopProxy<NetEvent>,
    terminal_only: bool,
    audio: bool,
) -> Result<(FramedSend, FramedRecv, Option<FramedSend>, Option<FramedRecv>, bool, bool)> {
    let (send, recv) = conn.open_bi().await?;
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);

    ctrl_tx
        .send(&control(Msg::Hello(Hello {
            protocol_version: nuvira_proto::PROTOCOL_VERSION,
            device_name: nuvira_transport::device_name(),
            os: std::env::consts::OS.into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            // Prototype: video as datagrams with error correction (nuvira_transport::fec).
            video_datagrams: std::env::var_os("NUVIRA_VIDEO_FEC").is_some()
                && conn.max_datagram_size().is_some(),
            terminal_only,
            print: !terminal_only && crate::printing::available(),
            recording: !terminal_only,
            audio: audio && !terminal_only && conn.max_datagram_size().is_some(),
            // The remote pointer drawn here: its shape as this computer's pointer.
            cursor: !terminal_only && std::env::var_os("NUVIRA_NO_CURSOR").is_none(),
            // Shapes kept by serial and taken as PNG (NUVIRA_NO_CURSOR_CACHE: as a
            // viewer of 0.26, for tests).
            cursor_cache: std::env::var_os("NUVIRA_NO_CURSOR_CACHE").is_none(),
            // Files copied there offered on this clipboard, and the other way.
            clipboard_files: !terminal_only,
            // A session handed over to this viewer is taken over (it connects again).
            handover: !terminal_only,
            // Switch sides: this program can show its own screen (the app's).
            switch_sides: !terminal_only && crate::REVERSE_HOST.get().is_some(),
            // The two-factor code is asked for (CodeRequired).
            two_factor: true,
            // The technician's microphone can be sent (SetMicrophone).
            microphone: !terminal_only && nuvira_audio::microphone::available(),
            // The end by the person at the PC comes as "@agent.ended_at_computer".
            end_reason: true,
            ..Default::default()
        })))
        .await?;
    let Some(ControlMessage { msg: Some(Msg::Hello(agent)) }) = ctrl_rx.recv().await? else {
        bail!("the agent did not introduce itself");
    };
    if agent.password_required {
        let password = password.with_context(|| nuvira_i18n::t("viewer.password_required"))?;
        let auth = if let Some(ticket) = password.strip_prefix(crate::SUPPORT_TICKET) {
            nuvira_proto::v1::Auth { support_ticket: ticket.into(), ..Default::default() }
        } else if let Some(token) = password.strip_prefix(crate::HANDOVER_TOKEN) {
            nuvira_proto::v1::Auth { handover: token.into(), ..Default::default() }
        } else {
            nuvira_proto::v1::Auth { password, ..Default::default() }
        };
        ctrl_tx.send(&control(Msg::Auth(auth.clone()))).await?;
        let mut answer = ctrl_rx.recv().await?;
        // The password was right and the computer also wants a two-factor code.
        if let Some(ControlMessage { msg: Some(Msg::CodeRequired(_)) }) = answer {
            info!("the remote computer asks for the two-factor code");
            let code = match std::env::var("NUVIRA_CODE").ok().filter(|c| !c.is_empty()) {
                Some(code) => Some(code),
                None => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = proxy.send_event(NetEvent::CodeRequired(tx));
                    rx.await.ok().flatten()
                }
            };
            let Some(code) = code else {
                return Err(Denied("@viewer.code_cancelled".into()).into());
            };
            ctrl_tx.send(&control(Msg::Auth(nuvira_proto::v1::Auth { code, ..auth }))).await?;
            answer = ctrl_rx.recv().await?;
        }
        if let Some(ControlMessage { msg: Some(Msg::ConfirmationPending(_)) }) = answer {
            info!("waiting for the person at the remote computer to accept");
            let _ = proxy.send_event(NetEvent::ConfirmationPending);
            answer = ctrl_rx.recv().await?;
        }
        match answer {
            Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) if r.ok => {}
            Some(ControlMessage { msg: Some(Msg::AuthResult(r)) }) => return Err(Denied(r.message).into()),
            _ => bail!("no answer to the password"),
        }
    }
    // Terminal only: no video and no input channel.
    if terminal_only {
        if !agent.terminal {
            bail!("{}", nuvira_i18n::t("viewer.terminal_unavailable"));
        }
        info!(agent = agent.device_name, "connected (terminal)");
        let _ = proxy.send_event(NetEvent::Connected {
            agent: agent.device_name,
            direct,
            width: 0,
            height: 0,
            file_manager: false,
            secure_attention: false,
            remote_mac: agent.os == "macos",
            remote_windows: remote_is_windows(&agent.os),
            privacy: false,
            terminal: true,
            tunnel: agent.tunnel,
            restart: false,
            restart_users: false,
            print: false,
            recording: false,
            recording_required: false,
            resolution: false,
            audio: false,
            switch_sides: false,
            lock_on_end: false,
            wallpaper: false,
            restart_safe_mode: false,
            microphone: false,
        });
        return Ok((ctrl_tx, ctrl_rx, None, None, false, false));
    }
    let Some(ControlMessage { msg: Some(Msg::VideoFormat(format)) }) = ctrl_rx.recv().await? else {
        bail!("missing video format");
    };
    info!(
        agent = agent.device_name,
        w = format.width,
        h = format.height,
        video_datagrams = agent.video_datagrams,
        "connected"
    );
    let video_datagrams = agent.video_datagrams;
    let _ = proxy.send_event(NetEvent::Connected {
        agent: agent.device_name,
        direct,
        width: format.width,
        height: format.height,
        file_manager: agent.file_manager,
        // Older agents send no system: they are all Windows.
        secure_attention: agent.os.is_empty() || agent.os == "windows",
        remote_mac: agent.os == "macos",
        remote_windows: remote_is_windows(&agent.os),
        privacy: agent.privacy_mode,
        terminal: agent.terminal,
        tunnel: agent.tunnel,
        restart: agent.restart,
        restart_users: agent.restart && agent.restart_users,
        print: agent.print,
        recording: agent.recording,
        recording_required: agent.recording_required,
        resolution: agent.resolution,
        audio: audio && agent.audio,
        switch_sides: agent.switch_sides && crate::REVERSE_HOST.get().is_some(),
        lock_on_end: agent.lock_on_end,
        wallpaper: agent.wallpaper,
        restart_safe_mode: agent.restart && agent.restart_safe_mode,
        microphone: agent.microphone && !terminal_only && nuvira_audio::microphone::available(),
    });

    let input_tx = FramedSend::new(conn.open_uni().await?);
    tracing::debug!("input channel open");
    // With datagrams there is no video stream.
    let video_rx = match video_datagrams {
        true => None,
        false => Some(FramedRecv::new(conn.accept_uni().await?)),
    };
    tracing::debug!("video channel accepted");
    Ok((ctrl_tx, ctrl_rx, Some(input_tx), video_rx, agent.audio, agent.clipboard_files))
}
