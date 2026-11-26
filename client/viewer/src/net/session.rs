//! The session with the agent, from the connection to its end: video, input, control
//! channel, clipboard, files, terminals and tunnels, all served together.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nuvira_proto::v1::{ControlMessage, FileChunk, Ping, RequestKeyframe, VideoFrame, control_message::Msg};
use nuvira_transport::FramedRecv;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use winit::event_loop::EventLoopProxy;

use super::audio::{receive_audio, start_player};
use super::connect::{agent_reason, close, handshake, open_connection, viewer_gone};
use super::streams::{ReverseHosting, forward, run_terminal};
use super::transfer::{file_name, receive_download, send_file, send_upload};
use super::video::{VideoLoss, cursor_png, decode_loop, receive_datagram_video, to_decoder};
use super::{Denied, FrameSlot, NetEvent, Outgoing, SessionChannels, Target, TermCmd, TermIn, TunnelCmd, control};

/// [`run_session`], ending its statistics log (if any) with the reason it ended.
pub(super) async fn session(
    target: Target,
    password: Option<String>,
    proxy: EventLoopProxy<NetEvent>,
    windows: (Arc<FrameSlot>, Arc<crate::recorder::Switch>),
    channels: SessionChannels,
    terminal_only: bool,
) -> Result<()> {
    let recorder = Mutex::new(None);
    let result = run_session(target, password, proxy, windows, channels, terminal_only, &recorder).await;
    if let Some(r) = recorder.into_inner().ok().flatten() {
        r.finish(nuvira_transport::stats::exit_reason(&result));
    }
    result
}

async fn run_session(
    target: Target,
    password: Option<String>,
    proxy: EventLoopProxy<NetEvent>,
    (slot, recording): (Arc<FrameSlot>, Arc<crate::recorder::Switch>),
    (mut input_rx, mut control_rx, mut files_rx, downloads, (mut terminals_rx, mut tunnels_rx)): SessionChannels,
    terminal_only: bool,
    recorder: &Mutex<Option<nuvira_transport::stats::Recorder>>,
) -> Result<()> {
    // Until the session starts the viewer may close (window closed while connecting
    // or waiting for the confirmation): then nothing more is done, and a connection
    // already open is closed, or the agent would stay "busy" until the timeout.
    let (conn, direct, upload, path) = tokio::select! {
        r = open_connection(target) => r?,
        () = viewer_gone(&mut input_rx) => return Ok(()),
    };
    recording.set_upload(upload);
    // The remote computer's sound, played here (none without an output device).
    let mut player = start_player(terminal_only);
    let (mut ctrl_tx, mut ctrl_rx, mut input_tx, video_rx, agent_audio, agent_files) = tokio::select! {
        r = handshake(&conn, direct, password, &proxy, terminal_only, player.is_some()) => match r {
            Ok(channels) => channels,
            Err(e) => {
                // The agent's own reason, unless it already gave it in its answer.
                let e = match agent_reason(&conn).await {
                    Some(reason) if e.downcast_ref::<Denied>().is_none() => anyhow::anyhow!(reason),
                    _ => e,
                };
                close(&conn).await;
                return Err(e);
            }
        },
        () = viewer_gone(&mut input_rx) => {
            info!("viewer closed while connecting");
            close(&conn).await;
            return Ok(());
        }
    };

    if !agent_audio {
        // An older agent, or one that cannot capture its sound.
        player = None;
    }
    let player = player.map(Arc::new);
    // Decoding happens on a separate thread so as not to block the network.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<()>();
    let key_tx_for_datagrams = key_tx.clone();
    // Losses of the video as datagrams, reported to the agent with the feedback.
    let datagrams = video_rx.is_none() && !terminal_only;
    let loss = Arc::new(VideoLoss::default());
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<(VideoFrame, u64)>(4);
    // The statistics log, when NUVIRA_STATS asks for it (read at every session).
    let stats = match nuvira_transport::stats::Config::from_env() {
        Some(config) if !terminal_only => {
            let stats = Arc::new(crate::session_stats::ViewerStats::default());
            let source = crate::session_stats::ViewerSource {
                stats: stats.clone(),
                conn: conn.clone(),
                slot: slot.clone(),
                datagrams,
                last: Default::default(),
            };
            let session = path.session_id.unwrap_or_else(nuvira_transport::stats::local_session_id);
            *recorder.lock().expect("stats recorder") = Some(nuvira_transport::stats::Recorder::start(
                &config,
                "viewer",
                &session,
                path.path,
                Box::new(source),
            ));
            let _ = proxy.send_event(NetEvent::Stats(stats.clone()));
            Some(stats)
        }
        _ => None,
    };
    let decoder_stats = stats.clone();
    let decoder_proxy = proxy.clone();
    // The sound also goes into recordings.
    let audio_tap = Arc::new(crate::recorder::AudioTap::default());
    let decoder_tap = player.is_some().then(|| audio_tap.clone());
    let decoder = std::thread::Builder::new().name("decoding".into()).spawn(move || {
        decode_loop(frame_rx, decoder_proxy, slot, key_tx, &recording, decoder_tap, decoder_stats)
    })?;
    let sound = player.as_deref().map(|p| (p, &*audio_tap));

    let video = async {
        if terminal_only {
            return std::future::pending().await;
        }
        let Some(mut video_rx) = video_rx else {
            let stats = stats.as_deref();
            return receive_datagram_video(&conn, &frame_tx, &key_tx_for_datagrams, &loss, sound, stats)
                .await;
        };
        let stream = async {
            while let Some(frame) = video_rx.recv::<VideoFrame>().await? {
                if !to_decoder(&frame_tx, frame, stats.as_deref()) {
                    break;
                }
            }
            anyhow::Ok(())
        };
        // The video on its stream: the datagrams carry only the sound.
        match sound {
            Some(sound) => tokio::select! {
                r = stream => r,
                r = receive_audio(&conn, sound) => r,
            },
            None => stream.await,
        }
    };

    // Closing the input channel (the viewer exits) ends the session, also terminal only.
    let input = async {
        while let Some(event) = input_rx.recv().await {
            if let Some(tx) = input_tx.as_mut() {
                tx.send(&event).await?;
            }
        }
        anyhow::Ok(())
    };

    // Each terminal on a stream of its own, served by a task.
    let terminals = {
        let conn = conn.clone();
        let proxy = proxy.clone();
        async move {
            let mut open = std::collections::HashMap::<u32, mpsc::UnboundedSender<TermIn>>::new();
            while let Some(cmd) = terminals_rx.recv().await {
                match cmd {
                    TermCmd::Open { id, cols, rows } => {
                        let (tx, rx) = mpsc::unbounded_channel();
                        open.insert(id, tx);
                        let (conn, proxy) = (conn.clone(), proxy.clone());
                        tokio::spawn(async move {
                            let (code, error) = match run_terminal(&conn, id, cols, rows, rx, &proxy).await {
                                Ok(exit) => exit,
                                Err(e) => (0, format!("{e:#}")),
                            };
                            let _ = proxy.send_event(NetEvent::TerminalExit { id, code, error });
                        });
                    }
                    TermCmd::Input { id, data } => {
                        if let Some(tx) = open.get(&id) {
                            let _ = tx.send(TermIn::Data(data));
                        }
                    }
                    TermCmd::Resize { id, cols, rows } => {
                        if let Some(tx) = open.get(&id) {
                            let _ = tx.send(TermIn::Resize(cols, rows));
                        }
                    }
                    TermCmd::Close { id } => {
                        open.remove(&id);
                    }
                }
            }
            std::future::pending::<anyhow::Result<()>>().await
        }
    };

    // TCP tunnels: a listener on this computer for each one, a stream for each connection.
    let tunnels = {
        let conn = conn.clone();
        let proxy = proxy.clone();
        async move {
            let mut open = std::collections::HashMap::<u32, tokio::task::JoinHandle<()>>::new();
            while let Some(cmd) = tunnels_rx.recv().await {
                match cmd {
                    TunnelCmd::Add { id, local_port, host, port } => {
                        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", local_port)).await {
                            Ok(listener) => listener,
                            Err(e) => {
                                warn!(id, local_port, "tunnel not opened: {e}");
                                let error = format!("{e}");
                                let _ = proxy.send_event(NetEvent::TunnelListening { id, local_port, error });
                                continue;
                            }
                        };
                        let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(local_port);
                        info!(id, local_port, host, port, "tunnel listening");
                        let _ = proxy.send_event(NetEvent::TunnelListening { id, local_port, error: String::new() });
                        let (conn, proxy) = (conn.clone(), proxy.clone());
                        open.insert(id, tokio::spawn(async move {
                            while let Ok((socket, _)) = listener.accept().await {
                                let (conn, proxy, host) = (conn.clone(), proxy.clone(), host.clone());
                                tokio::spawn(async move {
                                    let error = match forward(&conn, socket, &host, port).await {
                                        Ok(()) => return,
                                        Err(e) => format!("{e:#}"),
                                    };
                                    let _ = proxy.send_event(NetEvent::TunnelConnection { id, error });
                                });
                            }
                        }));
                    }
                    TunnelCmd::Remove { id } => {
                        if let Some(task) = open.remove(&id) {
                            info!(id, "tunnel closed");
                            task.abort();
                        }
                    }
                }
            }
            std::future::pending::<anyhow::Result<()>>().await
        }
    };

    // Shared clipboard: what is copied here goes to the agent, what is copied there arrives here.
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<nuvira_clipboard::Content>();
    let clipboard = match terminal_only {
        true => None,
        false => match nuvira_clipboard::spawn_sync(move |content| clip_tx.send(content).is_ok()) {
            Ok(tx) => Some(tx),
            Err(e) => {
                warn!("clipboard not shared: {e:#}");
                None
            }
        },
    };

    // Files copied through the clipboard, both ways (the list at once, the contents
    // when pasted), with agents that do it and while the session allows it.
    let files_by_clipboard = agent_files && clipboard.is_some();
    let (sharing, mut paste_rx_files) = nuvira_clipboard::Sharing::new();
    let (listed_tx, mut listed_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::ClipboardFiles>();

    // After the video one, the agent's unidirectional streams carry images copied to
    // its clipboard, the files downloaded with the file manager and printed documents.
    let streams = {
        let clipboard = clipboard.clone();
        let sharing = sharing.clone();
        let conn = conn.clone();
        let proxy = proxy.clone();
        async move {
            loop {
                let mut rx = FramedRecv::new(conn.accept_uni().await?);
                let (clipboard, proxy, downloads) = (clipboard.clone(), proxy.clone(), downloads.clone());
                let sharing = sharing.clone();
                tokio::spawn(async move {
                    let Ok(Some(first)) = rx.recv::<FileChunk>().await else { return };
                    // The answer to a paste here of files copied there.
                    if first.clipboard_file {
                        sharing.receive(first, rx).await;
                        return;
                    }
                    if first.transfer_id != 0 {
                        let id = first.transfer_id;
                        let target = downloads.lock().expect("downloads").get(&id).cloned();
                        let Some(target) = target else { return };
                        let outcome = receive_download(first, rx, &target, &proxy).await;
                        downloads.lock().expect("downloads").remove(&id);
                        let (ok, message) = match outcome {
                            Ok(path) => (true, path.display().to_string()),
                            Err(e) => (false, format!("{e:#}")),
                        };
                        let _ = proxy.send_event(NetEvent::TransferDone { id, ok, message });
                        return;
                    }
                    if first.print {
                        match nuvira_transport::read_stream(&mut rx, &first, crate::printing::MAX_DOCUMENT).await {
                            Ok(data) => {
                                let _ = proxy.send_event(NetEvent::PrintJob { name: first.name, data });
                            }
                            Err(e) => warn!("printed document not received: {e:#}"),
                        }
                        return;
                    }
                    if !first.clipboard_image {
                        return;
                    }
                    match nuvira_transport::read_stream(
                        &mut rx,
                        &first,
                        nuvira_clipboard::MAX_IMAGE_PNG as u64,
                    )
                    .await
                    {
                        Ok(png) => {
                            if let Some(clipboard) = clipboard {
                                let _ = clipboard.send(nuvira_clipboard::Content::Image(png).into());
                            }
                        }
                        Err(e) => warn!("clipboard image not received: {e:#}"),
                    }
                });
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        }
    };

    let control_loop = async {
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        // While frames are lost none is shown and the usual feedback stops: the losses
        // are then reported on their own, which is when the agent needs them most.
        let mut loss_tick = tokio::time::interval(Duration::from_millis(250));
        let mut last_feedback = Instant::now();
        // What is copied here goes to the agent while the session allows it (older
        // agents send no Permissions: always).
        let mut clipboard_allowed = true;
        // Switch sides: this computer's screen served to the other one, while on.
        let mut reverse = ReverseHosting::default();
        // The technician's microphone, while on: its packets go out as datagrams.
        let mut microphone: Option<nuvira_audio::microphone::Microphone> = None;
        let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut mic_seq = 0u32;
        let mut files_allowed = true;
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    ctrl_tx.send(&control(Msg::Ping(Ping { sent_us: nuvira_proto::now_us() }))).await?;
                }
                _ = loss_tick.tick(), if datagrams => {
                    if last_feedback.elapsed() >= Duration::from_millis(250)
                        && loss.frames_lost.load(Ordering::Relaxed) > 0
                    {
                        let mut f = nuvira_proto::v1::Feedback {
                            window_ms: last_feedback.elapsed().as_millis() as u32,
                            ..Default::default()
                        };
                        loss.fill(&mut f);
                        last_feedback = Instant::now();
                        ctrl_tx.send(&control(Msg::Feedback(f))).await?;
                    }
                }
                Some(mut msg) = control_rx.recv() => {
                    // Switch sides: served here, not a message for the agent.
                    if let Msg::SwitchSides(s) = &msg {
                        if s.on {
                            if let Err(e) = reverse.start(&conn, &proxy).await {
                                warn!("switch sides not started: {e:#}");
                                let error = format!("{e:#}");
                                let _ = proxy.send_event(NetEvent::SwitchSides { on: false, error });
                            }
                        } else {
                            reverse.stop();
                        }
                        continue;
                    }
                    // The microphone starts and stops here, with the message to the agent.
                    if let Msg::SetMicrophone(m) = &msg {
                        if m.on && microphone.is_none() {
                            let tx = mic_tx.clone();
                            match nuvira_audio::microphone::Microphone::start(move |p| {
                                let _ = tx.send(p);
                            }) {
                                Ok(mic) => microphone = Some(mic),
                                Err(e) => {
                                    warn!("microphone not started: {e:#}");
                                    // A key the toolbar translates; a system's error as it is.
                                    let error = match format!("{e:#}") {
                                        e if e.starts_with('@') => e,
                                        e => nuvira_i18n::tf("viewer.mic_failed", &[("e", &e)]),
                                    };
                                    let _ = proxy.send_event(NetEvent::Microphone { on: false, error });
                                    continue;
                                }
                            }
                        } else if !m.on {
                            microphone = None;
                        }
                    }
                    if let Msg::Feedback(f) = &mut msg
                        && datagrams
                    {
                        loss.fill(f);
                        last_feedback = Instant::now();
                    }
                    ctrl_tx.send(&control(msg)).await?;
                }
                Some(packet) = mic_rx.recv() => {
                    if microphone.is_some() {
                        let datagram = nuvira_audio::packet::encode(mic_seq, &packet);
                        mic_seq = mic_seq.wrapping_add(1);
                        if let Err(e) = conn.send_datagram(datagram.into()) {
                            tracing::debug!("microphone datagram not sent: {e}");
                        }
                    }
                }
                Some(content) = clip_rx.recv(), if clipboard_allowed => match content {
                    nuvira_clipboard::Content::Text(text) => {
                        sharing.forget();
                        ctrl_tx.send(&control(Msg::Clipboard(nuvira_proto::v1::Clipboard { text, ..Default::default() }))).await?;
                    }
                    // Files: listed off the loop, offered to the agent.
                    nuvira_clipboard::Content::Files(paths) => {
                        sharing.forget();
                        debug!(files = paths.len(), files_by_clipboard, files_allowed, "files copied here");
                        if files_by_clipboard && files_allowed {
                            let (sharing, listed_tx) = (sharing.clone(), listed_tx.clone());
                            tokio::spawn(async move {
                                match tokio::task::spawn_blocking(move || nuvira_clipboard::list::expand(&paths)).await {
                                    Ok(Ok(listed)) => {
                                        let _ = listed_tx.send(sharing.announce(listed));
                                    }
                                    Ok(Err(e)) => info!("copied files not offered: {e:#}"),
                                    Err(e) => warn!("listing copied files: {e}"),
                                }
                            });
                        }
                    }
                    nuvira_clipboard::Content::RemoteFiles(_) => {}
                    // On its own stream, not to hold up the control channel.
                    nuvira_clipboard::Content::Image(png) => {
                        sharing.forget();
                        let conn = conn.clone();
                        tokio::spawn(async move {
                            let header = FileChunk { clipboard_image: true, ..Default::default() };
                            if let Err(e) = nuvira_transport::send_stream(&conn, header, &png).await {
                                warn!("clipboard image not sent: {e:#}");
                            }
                        });
                    }
                },
                Some(list) = listed_rx.recv() => {
                    ctrl_tx.send(&control(Msg::ClipboardFiles(list))).await?;
                }
                // Files of the agent pasted here: their contents asked for.
                Some(request) = paste_rx_files.recv() => {
                    if files_by_clipboard && clipboard_allowed && files_allowed {
                        ctrl_tx.send(&control(Msg::ClipboardFileRequest(request))).await?;
                    } else {
                        sharing.refuse(request.id, "files switched off");
                    }
                }
                Some(()) = key_rx.recv() => {
                    ctrl_tx.send(&control(Msg::RequestKeyframe(RequestKeyframe {}))).await?;
                }
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(ControlMessage { msg: Some(Msg::Pong(pong)) }) => {
                        let now = nuvira_proto::now_us();
                        let rtt_us = now.saturating_sub(pong.ping_sent_us);
                        // Agent clock minus ours, assuming a symmetric round trip.
                        let midpoint = (pong.ping_sent_us + now) / 2;
                        let offset_us = pong.agent_us as i64 - midpoint as i64;
                        let _ = proxy.send_event(NetEvent::Clock { rtt_us, offset_us });
                    }
                    Some(ControlMessage { msg: Some(Msg::Displays(d)) }) => {
                        let _ = proxy.send_event(NetEvent::Displays {
                            displays: d.displays,
                            current: d.current,
                            all_available: d.all_available,
                        });
                    }
                    Some(ControlMessage { msg: Some(Msg::FileReceived(f)) }) if f.transfer_id != 0 => {
                        let message = if f.ok { f.path } else { f.message };
                        let _ = proxy.send_event(NetEvent::TransferDone { id: f.transfer_id, ok: f.ok, message });
                    }
                    Some(ControlMessage { msg: Some(Msg::FileReceived(f)) }) => {
                        let _ = proxy.send_event(NetEvent::FileDone { name: f.name, ok: f.ok, message: f.message });
                    }
                    Some(ControlMessage { msg: Some(Msg::FsResponse(answer)) }) => {
                        let _ = proxy.send_event(NetEvent::Fs(answer));
                    }
                    Some(ControlMessage { msg: Some(Msg::Chat(chat)) }) => {
                        let _ = proxy.send_event(NetEvent::Chat(chat.text));
                    }
                    Some(ControlMessage { msg: Some(Msg::Guests(g)) }) => {
                        let _ = proxy.send_event(NetEvent::Guests(g));
                    }
                    Some(ControlMessage { msg: Some(Msg::HandedOver(h)) }) => {
                        let _ = proxy.send_event(NetEvent::HandedOver(h));
                    }
                    Some(ControlMessage { msg: Some(Msg::SwitchSides(s)) }) => {
                        if !s.on {
                            reverse.stop();
                        }
                        let error = nuvira_i18n::message(&s.error);
                        let _ = proxy.send_event(NetEvent::SwitchSides { on: s.on, error });
                    }
                    Some(ControlMessage { msg: Some(Msg::ScreenState(s)) }) => {
                        let _ = proxy.send_event(NetEvent::ScreenLocked { locked: s.locked, input: s.input });
                    }
                    Some(ControlMessage { msg: Some(Msg::ResolutionState(r)) }) => {
                        let _ = proxy.send_event(NetEvent::Resolution {
                            width: r.width,
                            height: r.height,
                            adapted: r.adapted,
                            error: r.error,
                        });
                    }
                    Some(ControlMessage { msg: Some(Msg::PrivacyState(p)) }) => {
                        let _ = proxy.send_event(NetEvent::Privacy { on: p.on, error: p.error });
                    }
                    Some(ControlMessage { msg: Some(Msg::Permissions(p)) }) => {
                        info!(?p, "permissions of the session");
                        clipboard_allowed = p.clipboard;
                        files_allowed = p.files;
                        if !(p.clipboard && p.files) {
                            sharing.forget();
                            // The agent's files leave this clipboard: a paste would only fail.
                            if files_by_clipboard && let Some(clipboard) = &clipboard {
                                let _ = clipboard.send(nuvira_clipboard::Content::RemoteFiles(nuvira_clipboard::RemoteFiles::none()).into());
                            }
                        }
                        let _ = proxy.send_event(NetEvent::Permissions {
                            input: p.input,
                            files: p.files,
                            terminal: p.terminal,
                            tunnel: p.tunnel,
                            resolution: p.resolution,
                            audio: p.audio,
                            switched_off: p.switched_off,
                        });
                    }
                    Some(ControlMessage { msg: Some(Msg::MicrophoneState(m)) }) => {
                        let _ = proxy.send_event(NetEvent::Microphone { on: m.on, error: m.error });
                    }
                    Some(ControlMessage { msg: Some(Msg::IdleWarning(w)) }) => {
                        let _ = proxy.send_event(NetEvent::IdleWarning(w.seconds));
                    }
                    Some(ControlMessage { msg: Some(Msg::LockOnEndState(l)) }) => {
                        let _ = proxy.send_event(NetEvent::LockOnEnd { on: l.on, forced: l.forced });
                    }
                    Some(ControlMessage { msg: Some(Msg::RecordingState(r)) }) => {
                        let _ = proxy.send_event(NetEvent::Recording { on: r.on, error: r.error });
                    }
                    Some(ControlMessage { msg: Some(Msg::RestartUsers(u)) }) => {
                        let _ = proxy.send_event(NetEvent::RestartUsers { others: u.others });
                    }
                    Some(ControlMessage { msg: Some(Msg::RestartState(r)) }) => {
                        let _ = proxy.send_event(NetEvent::Restart { ok: r.ok, error: r.error, reconnect: r.reconnect });
                    }
                    Some(ControlMessage { msg: Some(Msg::Clipboard(clip)) }) => {
                        if let Some(clipboard) = &clipboard {
                            let _ = clipboard.send(nuvira_clipboard::Content::Text(clip.text).into());
                        }
                    }
                    // Files copied on the remote computer: offered on this clipboard.
                    Some(ControlMessage { msg: Some(Msg::ClipboardFiles(list)) }) => {
                        if files_by_clipboard
                            && clipboard_allowed
                            && files_allowed
                            && let Some(clipboard) = &clipboard
                        {
                            info!(entries = list.files.len(), "files copied there: on the clipboard");
                            let _ = clipboard.send(nuvira_clipboard::Content::RemoteFiles(sharing.remote(list)).into());
                        }
                    }
                    // Files copied here, pasted there.
                    Some(ControlMessage { msg: Some(Msg::ClipboardFileRequest(request)) }) => {
                        let allowed = files_by_clipboard && clipboard_allowed && files_allowed;
                        let (sharing, conn) = (sharing.clone(), conn.clone());
                        tokio::spawn(async move {
                            let opened = match (allowed, sharing.requested(&request)) {
                                (false, _) => Err(anyhow::anyhow!("files switched off")),
                                (true, Err(e)) => Err(e),
                                (true, Ok(path)) => std::fs::File::open(&path)
                                    .with_context(|| format!("opening {}", path.display())),
                            };
                            if let Err(e) = nuvira_clipboard::Sharing::answer(&conn, &request, opened).await {
                                warn!("pasted file not sent: {e:#}");
                            }
                        });
                    }
                    Some(ControlMessage { msg: Some(Msg::VideoFormat(f)) }) => {
                        info!(w = f.width, h = f.height, "new video format");
                        let _ = proxy.send_event(NetEvent::ScreenSize { width: f.width, height: f.height });
                    }
                    Some(ControlMessage { msg: Some(Msg::CursorShape(s)) }) => {
                        let (w, h) = (s.width.min(256), s.height.min(256));
                        debug!(
                            serial = s.serial,
                            png = !s.png.is_empty(),
                            bytes = s.png.len().max(s.bgra.len()),
                            "pointer shape"
                        );
                        let rgba = if s.png.is_empty() {
                            let mut rgba = s.bgra;
                            for px in rgba.chunks_exact_mut(4) {
                                px.swap(0, 2);
                            }
                            Some(rgba)
                        } else {
                            cursor_png(&s.png, s.width, s.height)
                        };
                        if let Some(mut rgba) = rgba
                            && w > 0
                            && h > 0
                            && rgba.len() >= (w * h * 4) as usize
                            && w == s.width
                            && h == s.height
                        {
                            rgba.truncate((w * h * 4) as usize);
                            let _ = proxy.send_event(NetEvent::CursorShape {
                                serial: s.serial,
                                width: w,
                                height: h,
                                hot_x: s.hot_x.min(w - 1),
                                hot_y: s.hot_y.min(h - 1),
                                rgba: std::sync::Arc::new(rgba),
                            });
                        }
                    }
                    Some(ControlMessage { msg: Some(Msg::CursorPosition(p)) }) => {
                        tracing::trace!(x = p.x, y = p.y, visible = p.visible, "remote pointer position");
                        let _ = proxy.send_event(NetEvent::CursorPosition { x: p.x, y: p.y, visible: p.visible, serial: p.serial });
                    }
                    Some(_) => {}
                    None => return anyhow::Ok(()),
                },
            }
        }
    };

    // Outgoing files, one at a time, each on its own stream.
    let files = async {
        while let Some(outgoing) = files_rx.recv().await {
            match outgoing {
                Outgoing::File(path) => {
                    if let Err(e) = send_file(&conn, &path, &proxy).await {
                        warn!("sending {} failed: {e:#}", path.display());
                        let name = file_name(&path);
                        let _ = proxy.send_event(NetEvent::FileDone {
                            name,
                            ok: false,
                            message: format!("{e:#}"),
                        });
                    }
                }
                Outgoing::Upload { id, local, dir, offset } => {
                    // The outcome comes from the agent (FileReceived with the id).
                    if let Err(e) = send_upload(&conn, id, &local, dir, offset, &proxy).await {
                        warn!("sending {} failed: {e:#}", local.display());
                        let _ = proxy.send_event(NetEvent::TransferDone {
                            id,
                            ok: false,
                            message: format!("{e:#}"),
                        });
                    }
                }
            }
        }
        // The channel closes only on exit: the input ends the session.
        std::future::pending::<anyhow::Result<()>>().await
    };

    let result = tokio::select! {
        r = files => r.context("sending files"),
        r = video => r.context("receiving video"),
        r = input => r.context("sending input"),
        r = control_loop => r.context("control channel"),
        r = streams => r.context("streams from the agent"),
        r = terminals => r,
        r = tunnels => r,
    };
    // The agent ended the session with a reason ("@key"): e.g. a time without activity.
    let result = match agent_reason(&conn).await {
        Some(reason) => Err(anyhow::anyhow!(reason)),
        None => result,
    };
    close(&conn).await;
    // The decoder ends when its channel closes: it is waited for, so that on exit
    // none of our threads is still working.
    drop(frame_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || decoder.join()))
        .await;
    result
}
