//! A session with a viewer already connected: the handshake that lets it in, then
//! the video, the input and every channel it opens until the session ends.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nuvira_proto::v1::{AuthResult, ControlMessage, Hello, InputEvent, Pong, VideoFrame, control_message::Msg};
use nuvira_transport::{Connection, FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::{info, warn};

#[cfg(windows)]
use crate::clip_user;
use crate::ending::video_stopped;
use crate::gate::{Gate, authorize_with_code};
use crate::handshake::{local_hello, read_auth, read_hello, watched};
use crate::input::{InputCmd, input_thread, paste_keys};
use crate::policy::{lock_wanted, reverse_policy, shorter_idle};
use crate::video::{
    NEXT_SESSION, RestoreScreens, RestoreWallpaper, VideoSource, WALLPAPER_STATE, displays_message,
    quality_limits, reopen, screen_rect, video_format,
};
use crate::{
    AgentConfig, AgentEvent, FEC_MAX, FEC_MIN, FEC_START, HANDSHAKE_MESSAGE_LIMIT, MIN_BITRATE,
    RECORDING_START, Reverse, SessionContext, WRONG_PASSWORD, abr, control, files, fsops, lock, pointer,
    printer, restart, reverse, same_secret, session_stats, shared, stats, switches, terminal, tunnel,
};

/// [`run_session`], ending its statistics log (if any) with the reason it ended.
pub(crate) async fn session(
    conn: Connection,
    streams: (quinn::SendStream, quinn::RecvStream),
    config: &AgentConfig,
    gate: Option<&Gate<'_>>,
    reverse: Option<&Reverse>,
) -> Result<()> {
    let recorder = Mutex::new(None);
    let result = watched(&conn.clone(), run_session(conn, streams, config, gate, reverse, &recorder)).await;
    if let Some(r) = recorder.into_inner().ok().flatten() {
        r.finish(nuvira_transport::stats::exit_reason(&result));
    }
    result
}

/// A session with a viewer already connected. With `gate` the viewer must
/// authenticate (and possibly be confirmed) before receiving any image; with
/// `reverse` (switch sides) it must give its password and gets the screen only.
pub(crate) async fn run_session(
    conn: Connection,
    (send, recv): (quinn::SendStream, quinn::RecvStream),
    config: &AgentConfig,
    gate: Option<&Gate<'_>>,
    reverse: Option<&Reverse>,
    recorder: &Mutex<Option<nuvira_transport::stats::Recorder>>,
) -> Result<()> {
    let mut ctrl_tx = FramedSend::new(send);
    // Small messages only until the viewer has authenticated.
    let mut ctrl_rx = FramedRecv::with_limit(recv, HANDSHAKE_MESSAGE_LIMIT);
    // Rules of the server at the start of the session, with its limits (none without
    // a server; the screen only when the sides are switched).
    let policy = gate.map(Gate::policy).or(reverse.map(|_| reverse_policy())).unwrap_or_default();
    // What the person at the PC switches off during the session.
    let (switches, off) = switches::channel();

    let hello = read_hello(&mut ctrl_rx, "viewer").await?;
    if let Some(gate) = gate {
        gate.refuse_reopen(&hello)?;
    }
    let viewer_protocol = hello.protocol_version;
    let terminal_only = hello.terminal_only;
    // The server's rules want every session recorded: by the viewer, which must be able to.
    let recording_required = policy.record_always && !terminal_only;
    let viewer_records = hello.recording;
    let viewer_two_factor = hello.two_factor;
    // The wallpaper can be hidden in sessions through a server, with a screen.
    let wallpaper_state = gate
        .filter(|g| !g.request.join && !terminal_only && config.synthetic.is_none())
        .filter(|_| nuvira_capture::wallpaper::available())
        .map(|g| g.ctx.access_dir.join(WALLPAPER_STATE));
    let _wallpaper = RestoreWallpaper(wallpaper_state.clone());
    // Minutes without activity after which the session ends: the computer's
    // setting and the server's rules, the shorter (0: never). Not for guests nor
    // terminal-only sessions.
    let idle_limit = match gate {
        Some(g) if !g.request.join && !terminal_only => {
            shorter_idle(g.ctx.idle_minutes.load(Ordering::Relaxed), policy.idle_minutes)
        }
        _ => 0,
    };
    if let Some(gate) = gate {
        gate.terminal_only.store(terminal_only, Ordering::Relaxed);
    }
    // Why a terminal is refused, if it is: checked at every terminal opened.
    let terminal_off = off.clone();
    let terminal_refused = move || -> Option<&'static str> {
        if config.view_only || config.synthetic.is_some() {
            Some("@agent.terminal_unavailable")
        } else if reverse.is_some() || gate.is_some_and(|g| g.policy().no_terminal) {
            Some("@agent.terminal_disabled")
        } else if gate.is_some_and(|g| !g.ctx.allow_terminal.load(Ordering::Relaxed)) {
            Some("@agent.terminal_not_allowed")
        } else if !terminal_off.is_on(switches::TERMINAL) {
            Some("@agent.terminal_switched_off")
        } else {
            None
        }
    };
    // Tunnels: only with a Pro server that offers them (never in a LAN session).
    let tunnel_off = off.clone();
    let tunnel_refused = move || -> Option<&'static str> {
        let Some(g) = gate else { return Some("@agent.tunnel_unavailable") };
        let licensed = g.ctx.features.lock().expect("features").iter().any(|f| f == "pro.tunnel");
        if config.view_only || config.synthetic.is_some() || !licensed {
            Some("@agent.tunnel_unavailable")
        } else if g.policy().no_tunnel {
            Some("@agent.tunnel_disabled")
        } else if !g.ctx.allow_tunnel.load(Ordering::Relaxed) {
            Some("@agent.tunnel_not_allowed")
        } else if !tunnel_off.is_on(switches::TUNNEL) {
            Some("@agent.tunnel_switched_off")
        } else {
            None
        }
    };
    // Privacy mode allowed by the person at the PC (always without the app: LAN tests).
    let privacy_allowed = || gate.is_none_or(|g| g.ctx.allow_privacy.load(Ordering::Relaxed));
    // Restart of the computer: only where the agent comes back by itself afterwards.
    let restart_available = gate.is_some_and(|g| g.ctx.unattended)
        && !policy.no_restart
        && !config.view_only
        && config.synthetic.is_none()
        && restart::available();
    // The technician's microphone: played here through a server session, with a
    // sound output, when the rules allow it.
    let mic_offered = gate.is_some_and(|g| !g.request.join)
        && !terminal_only
        && hello.microphone
        && !policy.no_microphone
        && config.synthetic.is_none()
        && nuvira_audio::player::available();
    // Safe mode with networking: Windows, the service's agent (it comes back).
    let safe_mode_offered = restart_available
        && !terminal_only
        && gate.is_some_and(|g| restart::safe_mode_available(g.ctx.unattended));
    // Remote printing, to a viewer that prints: allowed by the person at the PC (always
    // without the app: LAN tests) and by the server's rules, during the session only.
    let print_off = off.clone();
    let print_allowed = move || {
        print_off.is_on(switches::PRINT) && gate.is_none_or(|g| g.ctx.allow_print.load(Ordering::Relaxed))
    };
    // Recording of the session by the viewer, allowed by the person at the PC (always
    // without the app: LAN tests), who is told when it starts.
    // The server's rules come first (every session recorded, the person is told anyway).
    let recording_allowed = || {
        recording_required
            || (!policy.no_record && gate.is_none_or(|g| g.ctx.allow_recording.load(Ordering::Relaxed)))
    };
    let printer = match hello.print
        && !terminal_only
        && !config.view_only
        && config.synthetic.is_none()
        && !policy.no_print
        && gate.is_none_or(|g| g.ctx.allow_print.load(Ordering::Relaxed))
    {
        true => printer::listen(),
        false => None,
    };
    // Adapting the screen's resolution to the viewer's window.
    let resolution_offered = !terminal_only
        && !config.view_only
        && config.synthetic.is_none()
        && !policy.no_resolution
        && nuvira_capture::resolution::supported();
    // Video as datagrams with error correction, when the viewer asks for it.
    let video_datagrams = hello.video_datagrams && conn.max_datagram_size().is_some();
    // The pointer apart from the picture, to viewers that draw it (Linux, and not
    // with the DMA-BUF capture, which would lose it from recordings).
    let cursor_cache = hello.cursor_cache;
    // The viewer offers files copied here on its clipboard, and serves its own.
    let viewer_files = hello.clipboard_files;
    let cursor_offered = hello.cursor
        && !terminal_only
        && !recording_required
        && config.synthetic.is_none()
        && pointer::possible(config.plan);
    pointer::set_wanted(cursor_offered);
    let _pointer_reset = pointer::Reset;
    // The computer's sound, as datagrams, to viewers that play it.
    let audio_offered = !terminal_only
        && hello.audio
        && !policy.no_audio
        && config.synthetic.is_none()
        && conn.max_datagram_size().is_some()
        && nuvira_audio::capture::available();
    let hello = Hello {
        password_required: gate.is_some() || reverse.is_some(),
        file_manager: !config.view_only && config.synthetic.is_none() && !policy.no_files,
        video_datagrams,
        privacy_mode: !config.view_only
            && config.synthetic.is_none()
            && !policy.no_privacy
            && privacy_allowed()
            && nuvira_input::privacy::available(),
        terminal: terminal_refused().is_none(),
        tunnel: tunnel_refused().is_none(),
        restart: restart_available && !terminal_only,
        restart_users: restart_available && !terminal_only && restart::knows_users(),
        print: printer.is_some(),
        recording: !terminal_only && recording_allowed(),
        recording_required,
        resolution: resolution_offered,
        // Characters as they are (InputEvent.text), with the keyboard.
        text: !terminal_only && !config.view_only,
        // Clipboard.paste: with the clipboard and the keyboard.
        paste: !terminal_only && !config.view_only && config.synthetic.is_none() && !policy.no_clipboard,
        audio: audio_offered,
        cursor: cursor_offered,
        cursor_cache: cursor_offered && cursor_cache,
        // Others can join only sessions through a server: then it can be handed over.
        handover: gate.is_some() && !terminal_only,
        // Switch sides: a window with the viewer's screen, for the person signed in
        // (not inside a switched session, nor without a screen).
        switch_sides: gate.is_some()
            && reverse.is_none()
            && !terminal_only
            && config.synthetic.is_none()
            && reverse::viewer_available(),
        lock_on_end: gate.is_some() && lock::available(),
        idle_limit: idle_limit > 0,
        wallpaper: wallpaper_state.is_some(),
        restart_safe_mode: safe_mode_offered,
        microphone: mic_offered,
        // Files through the clipboard: with the clipboard and the files.
        clipboard_files: !terminal_only
            && !config.view_only
            && config.synthetic.is_none()
            && !policy.no_clipboard
            && !policy.no_files,
        ..local_hello()
    };
    // What the person at the PC can switch off: what this session has.
    let mut offered = 0;
    for (bit, has) in [
        (switches::INPUT, !terminal_only && !config.view_only && config.synthetic.is_none() && !policy.no_input),
        (switches::CLIPBOARD, !terminal_only && !config.view_only && config.synthetic.is_none() && !policy.no_clipboard),
        (switches::FILES, !terminal_only && hello.file_manager),
        (switches::TERMINAL, hello.terminal),
        (switches::TUNNEL, hello.tunnel),
        (switches::PRINT, hello.print),
        (switches::RESOLUTION, hello.resolution),
        (switches::AUDIO, hello.audio),
    ] {
        if has {
            offered |= bit;
        }
    }
    switches.set_offered(offered);
    let clipboard_files_offered = hello.clipboard_files;
    // Switch sides: what the window reports goes to the viewer on the control channel.
    let switch_offered = hello.switch_sides;
    let (switch_tx, mut switch_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::SwitchSides>();
    ctrl_tx.send(&control(Msg::Hello(hello))).await?;
    if let Some(gate) = gate {
        let given = read_auth(&mut ctrl_rx).await?;
        // A viewer that does not record cannot work here (older ones, the web client).
        if recording_required && !viewer_records {
            let message = "@agent.recording_required".to_string();
            ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok: false, message: message.clone() }))).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            bail!("{message}");
        }
        let (tx, rx) = (&mut ctrl_tx, &mut ctrl_rx);
        let verdict = authorize_with_code(gate, given, viewer_two_factor, viewer_protocol, tx, rx).await?;
        let (ok, message) = match &verdict {
            Ok(()) => (true, String::new()),
            Err(reason) => (false, reason.clone()),
        };
        ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok, message }))).await?;
        if let Err(reason) = verdict {
            // The reason must reach the viewer before the connection closes; with a
            // wrong password the pause is longer, against rapid-fire attempts (not when
            // refused without checking it: the attempt frees its slot at once).
            let checked = !gate.throttled.load(Ordering::Relaxed);
            let pause = if reason == WRONG_PASSWORD && checked { 2000 } else { 500 };
            tokio::time::sleep(Duration::from_millis(pause)).await;
            bail!("{reason}");
        }
        *gate.ctx.switches.lock().expect("switches") = Some(switches.clone());
        gate.ctx.lock_asked.store(false, Ordering::Relaxed);
        gate.ctx.mic_muted.send_replace(false);
        // The profile of the password given: what it does not allow is off for the
        // whole session, and the person at the PC cannot switch it back on.
        let profile_off = gate.profile_off.load(Ordering::Relaxed) & offered;
        if profile_off != 0 {
            for (bit, _) in switches::ALL {
                if profile_off & bit != 0 {
                    switches.set(bit, false);
                }
            }
            offered &= !profile_off;
            switches.set_offered(offered);
        }
    } else if let Some(reverse) = reverse {
        let given = read_auth(&mut ctrl_rx).await?;
        let ok = same_secret(&given.password, &reverse.password);
        let message = if ok { String::new() } else { WRONG_PASSWORD.to_string() };
        ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok, message }))).await?;
        if !ok {
            tokio::time::sleep(Duration::from_millis(2000)).await;
            bail!("{WRONG_PASSWORD}");
        }
    }
    ctrl_rx.set_limit(nuvira_proto::framing::MAX_MESSAGE_LEN);
    // Cleared at the end, however the session ends.
    let _clear = ClearSwitches(gate.map(|g| g.ctx));
    // The screens this session changed are put back however it ends.
    let owner = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let _screens = RestoreScreens(owner);
    let tell_permissions = |off: u8| {
        if let Some(g) = gate {
            let _ = g.ctx.events.send(AgentEvent::Permissions {
                offered: switches::names(offered),
                off: switches::names(off & offered),
            });
        }
        control(Msg::Permissions(permissions(offered, off)))
    };

    // Terminal only: no capture, no video, nothing on the screen.
    if terminal_only {
        ctrl_tx.send(&tell_permissions(0)).await?;
        let refused = (&terminal_refused, &tunnel_refused);
        return terminal_session(&conn, ctrl_tx, ctrl_rx, gate, refused, off, &tell_permissions).await;
    }

    let keyframe = Arc::new(AtomicBool::new(true));
    let max_bps = config.bitrate_kbps * 1000;
    // With error correction the parity shards travel too: the adaptive bitrate is what
    // goes on the network (parity included) and the encoder gets its share of it,
    // otherwise parity pushes a saturated link past its rate.
    // Parity per data shard, in thousandths: it follows the shards the viewer reports
    // lost (NUVIRA_FEC_RATIO fixes it, for tests).
    let fixed_ratio: Option<f64> = std::env::var("NUVIRA_FEC_RATIO").ok().and_then(|r| r.parse().ok());
    let fec_permille = Arc::new(AtomicU32::new(match video_datagrams {
        true => (fixed_ratio.unwrap_or(FEC_START) * 1000.0) as u32,
        false => 0,
    }));
    // What goes on the network for each byte of video, in thousandths: measured, since
    // small frames get at least two parity shards and weigh more than the ratio says.
    let overhead_permille = Arc::new(AtomicU32::new(1000 + fec_permille.load(Ordering::Relaxed)));
    let encoder_bps = {
        let overhead_permille = overhead_permille.clone();
        move |bps: u32| (bps as f64 * 1000.0 / overhead_permille.load(Ordering::Relaxed) as f64) as u32
    };
    // Share of shards lost, smoothed over the last reports.
    let mut shard_loss = 0.0f64;
    // Start from half the maximum: the unknown network might not take more.
    let mut abr = (!config.fixed_bitrate).then(|| abr::Abr::new(max_bps / 2, MIN_BITRATE, max_bps));
    let bitrate = Arc::new(AtomicU32::new(encoder_bps(abr.as_ref().map_or(max_bps, |a| a.bitrate()))));
    let fps = Arc::new(AtomicU32::new(config.fps));
    // Area the viewer draws the image in (0 = full size until it says otherwise).
    let view = Arc::new(AtomicU64::new(0));
    let session_start = Instant::now();
    // Last activity from the viewer (keyboard and mouse, files, its requests), in
    // ms since the start of the session: for the idle limit.
    let activity = Arc::new(AtomicU64::new(0));
    let mark_activity = {
        let activity = activity.clone();
        move || activity.store(session_start.elapsed().as_millis() as u64, Ordering::Relaxed)
    };
    // First frame sent after the last feedback (ms from the start, 0 = none): tells
    // whether feedback is missing. Measuring from the last frame instead would make a
    // still screen (one frame every few seconds) look like a stuck network.
    let unanswered_since_ms = Arc::new(AtomicU64::new(0));
    // Video bytes sent since the start: the adaptive bitrate needs the rate actually used.
    let sent_bytes = Arc::new(AtomicU64::new(0));
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    // The statistics log, when NUVIRA_STATS asks for it (read at every session).
    let stats_config = nuvira_transport::stats::Config::from_env();
    let stats = stats_config.as_ref().map(|_| Arc::new(session_stats::AgentStats::default()));
    if let (Some(s), Some(abr)) = (&stats, abr.as_ref()) {
        s.abr_target_bps.store(abr.bitrate(), Ordering::Relaxed);
    }
    let source = VideoSource {
        config: config.clone(),
        frames: frame_tx,
        keyframe: keyframe.clone(),
        bitrate: bitrate.clone(),
        fps: fps.clone(),
        view: view.clone(),
        stats: stats.clone(),
    };
    let (mut display, first) = source.start(config.display).await?;
    ctrl_tx.send(&video_format(&display)).await?;
    if let Some(msg) = displays_message(config, &display) {
        ctrl_tx.send(&msg).await?;
    }
    // After the video format: older viewers want it right after AuthResult.
    ctrl_tx.send(&tell_permissions(off.mask())).await?;
    // Whether the computer will be locked at the end (after VideoFormat: the viewer
    // reads that first).
    if let Some(g) = gate.filter(|g| !g.request.join && lock::available()) {
        let (on, forced) = lock_wanted(g.ctx, g.request);
        ctrl_tx.send(&control(Msg::LockOnEndState(nuvira_proto::v1::LockOnEndState { on, forced }))).await?;
    }
    // The statistics log starts once the viewer has what it waits for first.
    if let (Some(cfg), Some(s)) = (&stats_config, &stats) {
        let src = session_stats::AgentSource {
            stats: s.clone(),
            conn: conn.clone(),
            plan: format!("{:?}", config.plan),
            fps_max: fps.clone(),
            sent_bytes: sent_bytes.clone(),
            fec_permille: video_datagrams.then(|| fec_permille.clone()),
            adaptive: abr.is_some(),
            last: Default::default(),
        };
        let session =
            gate.map_or_else(nuvira_transport::stats::local_session_id, |g| g.session_id.to_string());
        let path = gate.map_or(if reverse.is_some() { "nested" } else { "direct" }, |g| g.path);
        *recorder.lock().expect("stats recorder") =
            Some(nuvira_transport::stats::Recorder::start(cfg, "agent", &session, path, Box::new(src)));
    }
    // Capture in progress: the control channel replaces it when the screen changes.
    let pipeline = std::sync::Mutex::new(Some(first));
    let (screen_tx, mut screen_rx) = tokio::sync::watch::channel(screen_rect(&display));
    // Other viewers can join this session (sessions from a server only): the same
    // picture, and their keyboard and mouse into this session's input.
    let (guest_input_tx, mut guest_input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let shared = gate.map(|g| {
        let s = Arc::new(shared::Shared::new(
            keyframe.clone(),
            display.clone(),
            guest_input_tx,
            offered & switches::INPUT != 0,
            g.ctx.events.clone(),
            audio_offered,
            cursor_offered,
        ));
        *g.ctx.shared.lock().expect("shared") = Some(s.clone());
        s
    });
    let _end_shared = shared::EndShared(gate.map(|g| g.ctx));

    // Chat: messages from the person at the PC come from the app through the context.
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<String>();
    let events = gate.map(|g| {
        *g.ctx.chat.lock().expect("chat") = Some(chat_tx.clone());
        g.ctx.events.clone()
    });
    // Outcome of the received files, to report to the viewer.
    let (file_tx, mut file_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::FileReceived>();
    // Answers of the file manager, computed off the control loop.
    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::FsResponse>();
    // Shared clipboard: what is copied here goes to the viewer, what is copied there arrives here.
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<nuvira_clipboard::Content>();
    // Clipboard.paste: the keys go to the input once the text is in the clipboard.
    let (paste_tx, mut paste_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::Paste>();
    let clipboard = if offered & switches::CLIPBOARD == 0 {
        None
    } else {
        // As SYSTEM, what cannot be read here is read by a helper started as the person.
        #[cfg(windows)]
        let fallback: Option<nuvira_clipboard::Fallback> =
            nuvira_clipboard::running_as_system().then(|| Box::new(clip_user::read_as_user) as nuvira_clipboard::Fallback);
        #[cfg(not(windows))]
        let fallback = None;
        match nuvira_clipboard::spawn_sync_with(move |content| clip_tx.send(content).is_ok(), fallback) {
            Ok(tx) => Some(tx),
            Err(e) => {
                warn!("clipboard not shared: {e:#}");
                None
            }
        }
    };
    // Images arrive on their own streams, received with the files.
    let clipboard_in = clipboard.clone();
    // Files copied through the clipboard, both ways (the list at once, the
    // contents when pasted): with the clipboard and the files, here and there.
    let files_by_clipboard = viewer_files && clipboard_files_offered && clipboard.is_some();
    let (sharing, mut paste_rx_files) = nuvira_clipboard::Sharing::new();
    let (listed_tx, mut listed_rx) = mpsc::unbounded_channel::<nuvira_proto::v1::ClipboardFiles>();
    let files_allowed = {
        let off = off.clone();
        move || files_by_clipboard && off.is_on(switches::CLIPBOARD) && off.is_on(switches::FILES)
    };

    // With datagrams there is no video stream (QUIC shows a stream to the other side
    // only once something is written on it: the viewer would wait for it forever).
    let mut video_tx = match video_datagrams {
        true => None,
        false => Some(FramedSend::new(conn.open_uni().await?)),
    };
    tracing::debug!(video_datagrams, "video channel open");
    let video = {
        let unanswered_since_ms = unanswered_since_ms.clone();
        let sent_bytes = sent_bytes.clone();
        let stats = stats.clone();
        let shared = shared.clone();
        let conn = conn.clone();
        let fec_permille = fec_permille.clone();
        let overhead_permille = overhead_permille.clone();
        async move {
            // Bytes on the network and bytes of video, decaying (the last few seconds).
            let (mut wire, mut video) = (0.0f64, 0.0f64);
            // Its Reed-Solomon codecs kept for the session (review NET-9).
            let mut fec = nuvira_transport::fec::Encoder::default();
            while let Some(frame) = frame_rx.recv().await {
                tracing::trace!(seq = frame.seq, bytes = frame.data.len(), "sending frame");
                let sent = match &mut video_tx {
                    Some(tx) => {
                        if let Err(e) = tx.send(&frame).await {
                            return Err(video_stopped(&conn, e).await);
                        }
                        frame.data.len()
                    }
                    None => {
                        let max = conn.max_datagram_size().context("datagrams no longer available")?;
                        let bytes = prost::Message::encode_to_vec(&frame);
                        let mut sent = 0;
                        let mut ratio = fec_permille.load(Ordering::Relaxed) as f64 / 1000.0;
                        // A keyframe lost freezes the image again: twice the parity.
                        if frame.keyframe {
                            ratio = (ratio * 2.0).max(FEC_MAX);
                        }
                        for packet in fec.encode(frame.seq, &bytes, max, ratio)? {
                            sent += packet.len();
                            conn.send_datagram_wait(packet.into()).await?;
                        }
                        wire = wire * 0.98 + sent as f64;
                        video = video * 0.98 + frame.data.len() as f64;
                        overhead_permille.store((wire / video.max(1.0) * 1000.0) as u32, Ordering::Relaxed);
                        sent
                    }
                };
                sent_bytes.fetch_add(sent as u64, Ordering::Relaxed);
                if let Some(s) = &stats {
                    s.frames_sent.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(shared) = &shared {
                    shared.frame(&frame);
                }
                let now_ms = session_start.elapsed().as_millis().max(1) as u64;
                let _ = unanswered_since_ms.compare_exchange(0, now_ms, Ordering::Relaxed, Ordering::Relaxed);
            }
            anyhow::Ok(())
        }
    };

    // The technician's microphone: datagrams from the viewer, played while it is on
    // (SetMicrophone) and the person at the PC did not mute it.
    let (mic_on_tx, mut mic_on_rx) = tokio::sync::watch::channel(false);
    let microphone = {
        let conn = conn.clone();
        let mut muted_rx = gate.map(|g| g.ctx.mic_muted.subscribe());
        let events = gate.map(|g| g.ctx.events.clone());
        async move {
            if !mic_offered {
                return std::future::pending::<Result<()>>().await;
            }
            let mut player: Option<nuvira_audio::player::Player> = None;
            let mut failed = false;
            loop {
                let datagram = tokio::select! {
                    d = conn.read_datagram() => match d {
                        Ok(d) => d,
                        // The connection ended: the other channels say why.
                        Err(_) => return std::future::pending::<Result<()>>().await,
                    },
                    Ok(()) = mic_on_rx.changed() => {
                        if !*mic_on_rx.borrow_and_update() && player.take().is_some() {
                            if let Some(events) = &events {
                                let _ = events.send(AgentEvent::Microphone { on: false });
                            }
                        }
                        continue;
                    }
                };
                let muted = muted_rx.as_mut().is_some_and(|m| *m.borrow_and_update());
                if !nuvira_audio::packet::is_audio(&datagram) || !*mic_on_rx.borrow() || muted || failed {
                    continue;
                }
                if player.is_none() {
                    // NUVIRA_MIC_DUMP (tests): written to that WAV file instead of played.
                    let dump = std::env::var_os("NUVIRA_MIC_DUMP").map(PathBuf::from);
                    match tokio::task::spawn_blocking(move || nuvira_audio::player::Player::start(dump)).await? {
                        Ok(p) => {
                            info!("technician's microphone played");
                            player = Some(p);
                            if let Some(events) = &events {
                                let _ = events.send(AgentEvent::Microphone { on: true });
                            }
                        }
                        Err(e) => {
                            warn!("technician's microphone not played: {e:#}");
                            failed = true;
                            continue;
                        }
                    }
                }
                if let (Some(p), Ok((seq, packet))) = (&player, nuvira_audio::packet::decode(&datagram)) {
                    p.push(seq, packet);
                }
            }
        }
    };
    // Ends with the session: the app's notice goes.
    struct MicNotice(Option<mpsc::UnboundedSender<AgentEvent>>);
    impl Drop for MicNotice {
        fn drop(&mut self) {
            if let Some(events) = &self.0 {
                let _ = events.send(AgentEvent::Microphone { on: false });
            }
        }
    }
    let _mic_notice = MicNotice(gate.filter(|_| mic_offered).map(|g| g.ctx.events.clone()));

    // The sound: captured while the viewer wants it (SetAudio), each packet a datagram.
    let (audio_on_tx, mut audio_on_rx) = tokio::sync::watch::channel(true);
    let audio = {
        let conn = conn.clone();
        // On while the viewer wants it (its mute button), or a guest of the session
        // does, and the person at the PC did not switch it off.
        let mut audio_off = off.clone();
        let shared = shared.clone();
        async move {
            if !audio_offered {
                return std::future::pending().await;
            }
            // Without a shared session no guest listens: a channel that never
            // changes (its sender kept here; a dropped one would wake the loop
            // at once, forever).
            let (_no_guests, idle) = tokio::sync::watch::channel(0);
            let mut listeners = match &shared {
                Some(s) => s.listeners(),
                None => idle,
            };
            let mut guests_gone = false;
            let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
            let mut capture: Option<nuvira_audio::capture::Capture> = None;
            let mut failed = false;
            let mut seq = 0u32;
            loop {
                let first = *audio_on_rx.borrow_and_update();
                let guests = *listeners.borrow_and_update() > 0;
                let want = (first || guests) && audio_off.is_on(switches::AUDIO);
                if want && capture.is_none() && !failed {
                    let tx = tx.clone();
                    match tokio::task::spawn_blocking(move || nuvira_audio::capture::Capture::start(move |p| {
                        let _ = tx.send(p);
                    }))
                    .await?
                    {
                        Ok(c) => capture = Some(c),
                        Err(e) => {
                            // No sound device: the session goes on without sound.
                            warn!("sound not sent: {e:#}");
                            failed = true;
                        }
                    }
                } else if !want && let Some(c) = capture.take() {
                    tokio::task::spawn_blocking(move || drop(c)).await?;
                }
                tokio::select! {
                    changed = audio_on_rx.changed() => {
                        if changed.is_err() {
                            return anyhow::Ok(());
                        }
                    }
                    _ = audio_off.changed() => {}
                    changed = listeners.changed(), if !guests_gone => {
                        if changed.is_err() {
                            guests_gone = true;
                        }
                    }
                    Some(packet) = rx.recv() => {
                        if capture.is_none() {
                            continue;
                        }
                        let datagram = Arc::new(nuvira_audio::packet::encode(seq, &packet));
                        seq = seq.wrapping_add(1);
                        if let Some(s) = &shared {
                            s.audio_packet(&datagram);
                        }
                        if !*audio_on_rx.borrow() {
                            continue;
                        }
                        // Not waiting for room: a packet that cannot leave now is late anyway.
                        match conn.send_datagram(datagram.as_ref().clone().into()) {
                            Ok(()) => {}
                            Err(quinn::SendDatagramError::ConnectionLost(e)) => return Err(e.into()),
                            Err(e) => tracing::debug!("audio packet not sent: {e}"),
                        }
                    }
                }
            }
        }
    };

    let control_loop = async {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        // The idle limit: checked every 5 s, a warning one minute before.
        let mut idle_tick = tokio::time::interval(Duration::from_secs(5));
        // The person at the PC mutes the microphone: the viewer is told.
        let (_never_muted, unmuted) = tokio::sync::watch::channel(false);
        let mut mic_muted_rx = match gate {
            Some(g) if mic_offered => g.ctx.mic_muted.subscribe(),
            _ => unmuted,
        };
        let mut idle_warned = false;
        let mut bytes_seen = 0u64;
        // Samples (time, bytes sent) of the last 2 s, for the rate actually used.
        let mut sent_samples = std::collections::VecDeque::<(Instant, u64)>::new();
        // Keyframes asked by the viewer: at most one a second. With the video as
        // datagrams on a 2 Mbit/s link every lost frame asked for one, each large and
        // with double parity, and they caused more losses: 2 Mbit/s went out while the
        // bitrate had come down to 0.5 (2026-11-01).
        let mut last_requested_key: Option<Instant> = None;
        // Privacy mode: on while this lives, at most until the end of the session.
        let mut privacy: Option<nuvira_input::privacy::Privacy> = None;
        // The viewer is recording (the person at the PC was told).
        let mut recording = false;
        let tell_app = |on: bool| {
            if let Some(g) = gate {
                let _ = g.ctx.events.send(AgentEvent::Recording { on });
            }
            // The recording is of the video: the pointer must be in it.
            pointer::set_recording(on);
        };
        // The pointer for this viewer, when it draws it.
        let mut pointer_sender = pointer::Sender::new(cursor_cache);
        let mut pointer_tick = tokio::time::interval(Duration::from_millis(16));
        pointer_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut changes = off.clone();
        // Lock state last told to the viewer (macOS only: elsewhere always false).
        let mut screen_locked = false;
        // Who joined the session, told to this viewer (none in LAN sessions).
        let mut guests_rx = match &shared {
            Some(s) => s.subscribe(),
            None => tokio::sync::watch::channel(nuvira_proto::v1::Guests::default()).1,
        };
        loop {
            let msg = tokio::select! {
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    Some(msg) => msg,
                    None => return anyhow::Ok(()),
                },
                Ok(()) = guests_rx.changed() => {
                    let guests = guests_rx.borrow_and_update().clone();
                    ctrl_tx.send(&control(Msg::Guests(guests))).await?;
                    continue;
                }
                Some(state) = switch_rx.recv() => {
                    ctrl_tx.send(&control(Msg::SwitchSides(state))).await?;
                    continue;
                }
                Ok(()) = mic_muted_rx.changed() => {
                    let muted = *mic_muted_rx.borrow_and_update();
                    let wanted = *mic_on_tx.borrow();
                    let error = if muted { "@agent.microphone_muted".to_string() } else { String::new() };
                    let state = nuvira_proto::v1::MicrophoneState { on: wanted && !muted, error };
                    ctrl_tx.send(&control(Msg::MicrophoneState(state))).await?;
                    continue;
                }
                _ = idle_tick.tick(), if idle_limit > 0 => {
                    // Files moving count as activity.
                    let bytes =
                        stats::BYTES_IN.load(Ordering::Relaxed) + stats::BYTES_OUT.load(Ordering::Relaxed);
                    if bytes != bytes_seen {
                        bytes_seen = bytes;
                        mark_activity();
                    }
                    let idle_ms = (session_start.elapsed().as_millis() as u64)
                        .saturating_sub(activity.load(Ordering::Relaxed));
                    let limit_ms = u64::from(idle_limit) * 60_000;
                    if idle_ms >= limit_ms {
                        info!(minutes = idle_limit, "session ended after a time without activity");
                        bail!("@agent.idle_ended");
                    }
                    let warn_from = limit_ms.saturating_sub(60_000);
                    if idle_ms >= warn_from && !idle_warned {
                        idle_warned = true;
                        let seconds = ((limit_ms - idle_ms) / 1000) as u32;
                        let warning = nuvira_proto::v1::IdleWarning { seconds };
                        ctrl_tx.send(&control(Msg::IdleWarning(warning))).await?;
                    } else if idle_ms < warn_from && idle_warned {
                        idle_warned = false;
                        let over = nuvira_proto::v1::IdleWarning { seconds: 0 };
                        ctrl_tx.send(&control(Msg::IdleWarning(over))).await?;
                    }
                    continue;
                }
                _ = pointer_tick.tick(), if cursor_offered => {
                    for msg in pointer_sender.poll() {
                        ctrl_tx.send(&control(msg)).await?;
                    }
                    continue;
                }
                now = changes.changed() => {
                    ctrl_tx.send(&tell_permissions(now)).await?;
                    // Files or clipboard switched off: the viewer's files leave this
                    // clipboard (a paste would only fail), and ours are not served.
                    if files_by_clipboard && now & (switches::FILES | switches::CLIPBOARD) != 0 {
                        sharing.forget();
                        if let Some(clipboard) = &clipboard {
                            let _ = clipboard.send(nuvira_clipboard::Content::RemoteFiles(nuvira_clipboard::RemoteFiles::none()).into());
                        }
                    }
                    // Switched off by the person at the PC: the screen goes back at once.
                    if now & switches::RESOLUTION != 0 && nuvira_capture::resolution::adapted(owner, &display) {
                        let d = display.clone();
                        tokio::task::spawn_blocking(move || nuvira_capture::resolution::restore(owner, Some(&d))).await?;
                        display = reopen(&source, &pipeline, display.index, display.index).await?;
                        ctrl_tx.send(&video_format(&display)).await?;
                        if let Some(shared) = &shared {
                            shared.set_display(&display);
                        }
                        if let Some(msg) = displays_message(config, &display) {
                            ctrl_tx.send(&msg).await?;
                        }
                        let _ = screen_tx.send(screen_rect(&display));
                        let state = nuvira_proto::v1::ResolutionState {
                            width: display.width,
                            height: display.height,
                            adapted: false,
                            error: "@agent.resolution_switched_off".into(),
                        };
                        ctrl_tx.send(&control(Msg::ResolutionState(state))).await?;
                    }
                    continue;
                }
                Some(content) = clip_rx.recv() => {
                    if !off.is_on(switches::CLIPBOARD) {
                        sharing.forget();
                        continue;
                    }
                    match content {
                        nuvira_clipboard::Content::Text(text) => {
                            sharing.forget();
                            ctrl_tx.send(&control(Msg::Clipboard(nuvira_proto::v1::Clipboard { text, ..Default::default() }))).await?;
                        }
                        // Files: listed as the user signed in, offered to the viewer.
                        nuvira_clipboard::Content::Files(paths) => {
                            sharing.forget();
                            tracing::debug!(files = paths.len(), allowed = files_allowed(), "files copied here");
                            if files_allowed() {
                                let (sharing, listed_tx) = (sharing.clone(), listed_tx.clone());
                                tokio::spawn(async move {
                                    match fsops::as_user(move || nuvira_clipboard::list::expand(&paths)).await {
                                        Ok(listed) => {
                                            let _ = listed_tx.send(sharing.announce(listed));
                                        }
                                        Err(e) => info!("copied files not offered: {e:#}"),
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
                                let header = nuvira_proto::v1::FileChunk { clipboard_image: true, ..Default::default() };
                                if let Err(e) = nuvira_transport::send_stream(&conn, header, &png).await {
                                    warn!("clipboard image not sent: {e:#}");
                                }
                            });
                        }
                    }
                    continue;
                }
                Some(list) = listed_rx.recv() => {
                    ctrl_tx.send(&control(Msg::ClipboardFiles(list))).await?;
                    continue;
                }
                // Files of the viewer pasted here: their contents asked for.
                Some(request) = paste_rx_files.recv() => {
                    if files_allowed() {
                        ctrl_tx.send(&control(Msg::ClipboardFileRequest(request))).await?;
                    } else {
                        sharing.refuse(request.id, "files switched off");
                    }
                    continue;
                }
                Some(text) = chat_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Chat(nuvira_proto::v1::Chat { text }))).await?;
                    continue;
                }
                Some(answer) = fs_rx.recv() => {
                    ctrl_tx.send(&control(Msg::FsResponse(answer))).await?;
                    continue;
                }
                Some(done) = file_rx.recv() => {
                    ctrl_tx.send(&control(Msg::FileReceived(done))).await?;
                    continue;
                }
                _ = tick.tick() => {
                    let locked = nuvira_capture::screen_locked();
                    if locked != screen_locked {
                        screen_locked = locked;
                        info!(locked, "screen lock state");
                        #[cfg(target_os = "macos")]
                        if locked {
                            nuvira_input::lockhid::prepare();
                        }
                        let input = nuvira_capture::lock_screen_takes_input();
                        let state = nuvira_proto::v1::ScreenState { locked, input };
                        ctrl_tx.send(&control(Msg::ScreenState(state))).await?;
                    }
                    let now = Instant::now();
                    sent_samples.push_back((now, sent_bytes.load(Ordering::Relaxed)));
                    while sent_samples.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(2)) {
                        sent_samples.pop_front();
                    }
                    // The person at the PC no longer allows it: it ends at once.
                    if privacy.is_some() && !privacy_allowed() {
                        if let Some(on) = privacy.take() {
                            tokio::task::spawn_blocking(move || on.stop()).await?;
                        }
                        let state = nuvira_proto::v1::PrivacyState { on: false, error: "@agent.privacy_not_allowed".into() };
                        ctrl_tx.send(&control(Msg::PrivacyState(state))).await?;
                    }
                    // Required by the rules: the viewer has a few seconds to start.
                    if recording_required && !recording && session_start.elapsed() > RECORDING_START {
                        warn!("session ended: not recorded as the server's rules want");
                        let state = nuvira_proto::v1::RecordingState { on: false, error: "@agent.recording_required".into() };
                        ctrl_tx.send(&control(Msg::RecordingState(state))).await?;
                        return anyhow::Ok(());
                    }
                    if recording && !recording_allowed() {
                        recording = false;
                        info!("recording stopped: no longer allowed by the person at the PC");
                        tell_app(false);
                        let state = nuvira_proto::v1::RecordingState { on: false, error: "@agent.recording_not_allowed".into() };
                        ctrl_tx.send(&control(Msg::RecordingState(state))).await?;
                    }
                    // The encoder's share follows the overhead measured.
                    if video_datagrams && let Some(abr) = abr.as_ref() {
                        bitrate.store(encoder_bps(abr.bitrate()), Ordering::Relaxed);
                    }
                    // A frame left long ago and no feedback since: the network is probably stuck.
                    let now_ms = session_start.elapsed().as_millis() as u64;
                    let since = unanswered_since_ms.load(Ordering::Relaxed);
                    if let Some(abr) = abr.as_mut()
                        && since != 0
                        && now_ms.saturating_sub(since) > abr::FEEDBACK_TIMEOUT.as_millis() as u64
                        && let Some(bps) = abr.on_silence(Instant::now())
                    {
                        bitrate.store(encoder_bps(bps), Ordering::Relaxed);
                        info!("bitrate {} kbit/s (no feedback from the viewer)", bps / 1000);
                    }
                    if let (Some(s), Some(abr)) = (&stats, abr.as_ref()) {
                        s.abr_target_bps.store(abr.bitrate(), Ordering::Relaxed);
                    }
                    continue;
                }
            };
            // What the viewer does counts as activity, not what it sends by itself.
            if !matches!(
                msg.msg,
                Some(Msg::Ping(_) | Msg::Feedback(_) | Msg::ViewSize(_) | Msg::RequestKeyframe(_)) | None
            ) {
                mark_activity();
            }
            match msg.msg {
                Some(Msg::Activity(_)) => {}
                Some(Msg::Ping(ping)) => {
                    let pong = Pong { ping_sent_us: ping.sent_us, agent_us: nuvira_proto::now_us() };
                    ctrl_tx.send(&control(Msg::Pong(pong))).await?;
                }
                Some(Msg::RequestKeyframe(_)) => {
                    if let Some(s) = &stats {
                        s.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    if last_requested_key.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                        last_requested_key = Some(Instant::now());
                        keyframe.store(true, Ordering::Relaxed);
                    }
                }
                Some(Msg::FsRequest(request)) => {
                    let fs_tx = fs_tx.clone();
                    if config.view_only || config.synthetic.is_some() || policy.no_files || !off.is_on(switches::FILES) {
                        let key = if policy.no_files {
                            "@agent.files_disabled"
                        } else if !off.is_on(switches::FILES) {
                            "@agent.files_switched_off"
                        } else {
                            "@agent.files_view_only"
                        };
                        let error = key.to_string();
                        let _ = fs_tx.send(nuvira_proto::v1::FsResponse {
                            id: request.id,
                            error,
                            ..Default::default()
                        });
                        continue;
                    }
                    let (conn, off) = (conn.clone(), off.clone());
                    tokio::spawn(async move {
                        let id = request.id;
                        let answer = switches::until_off(off, switches::FILES, fsops::handle(id, request.op, conn))
                            .await
                            .unwrap_or_else(|| nuvira_proto::v1::FsResponse {
                                id,
                                error: "@agent.files_switched_off".into(),
                                ..Default::default()
                            });
                        let _ = fs_tx.send(answer);
                    });
                }
                Some(Msg::Feedback(fb)) => {
                    unanswered_since_ms.store(0, Ordering::Relaxed);
                    if let Some(s) = &stats {
                        s.feedback(&fb);
                    }
                    // The network part when the viewer reports it: a slow encoder or
                    // graphics card is not congestion and lowering the bitrate would not help.
                    let (p50, min) = match (fb.transit_ms_p50, fb.transit_ms_min) {
                        (Some(p50), Some(min)) => (p50, min),
                        _ => (fb.latency_ms_p50, fb.latency_ms_min),
                    };
                    let sent_bps = match (sent_samples.front(), sent_samples.back()) {
                        (Some((t0, b0)), Some((t1, b1))) if t1 > t0 => {
                            (b1 - b0) as f64 * 8.0 / t1.duration_since(*t0).as_secs_f64()
                        }
                        // Start of the session: no measurement yet, latency counts.
                        _ => f64::INFINITY,
                    };
                    if video_datagrams {
                        if let (Some(shards), Some(lost)) = (fb.shards, fb.shards_lost)
                            && shards > 0
                        {
                            shard_loss = 0.7 * shard_loss + 0.3 * (lost as f64 / shards as f64);
                            if fixed_ratio.is_none() {
                                let ratio = (FEC_MIN + 3.0 * shard_loss).clamp(FEC_MIN, FEC_MAX);
                                let new = (ratio * 1000.0) as u32;
                                if fec_permille.swap(new, Ordering::Relaxed).abs_diff(new) >= 20 {
                                    tracing::debug!(
                                        "parity {ratio:.2} (shards lost {:.1}%)",
                                        shard_loss * 100.0
                                    );
                                    if let Some(abr) = abr.as_ref() {
                                        bitrate.store(encoder_bps(abr.bitrate()), Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                        if let Some(abr) = abr.as_mut()
                            && let Some(bps) =
                                abr.on_loss(Instant::now(), fb.frames_lost.unwrap_or(0), sent_bps)
                        {
                            bitrate.store(encoder_bps(bps), Ordering::Relaxed);
                            info!(
                                "bitrate {} kbit/s ({} frames lost; sending {} kbit/s)",
                                bps / 1000,
                                fb.frames_lost.unwrap_or(0),
                                (sent_bps.min(1e12) / 1000.0) as u64
                            );
                        }
                    }
                    if let Some(abr) = abr.as_mut()
                        && fb.frames > 0
                        && let Some(bps) = abr.on_feedback(Instant::now(), p50 as f64, min as f64, sent_bps)
                    {
                        bitrate.store(encoder_bps(bps), Ordering::Relaxed);
                        info!(
                            "bitrate {} kbit/s (network {p50} ms, minimum {min} ms; total latency {} ms; sending {} kbit/s)",
                            bps / 1000,
                            fb.latency_ms_p50,
                            (sent_bps.min(1e12) / 1000.0) as u64
                        );
                    }
                }
                Some(Msg::SelectDisplay(select)) if config.synthetic.is_none() => {
                    let index = select.index as usize;
                    if index == display.index {
                        continue;
                    }
                    // The screen left goes back to its own resolution.
                    let d = display.clone();
                    tokio::task::spawn_blocking(move || nuvira_capture::resolution::restore(owner, Some(&d))).await?;
                    display = reopen(&source, &pipeline, index, display.index).await?;
                    ctrl_tx.send(&video_format(&display)).await?;
                    if let Some(shared) = &shared {
                        shared.set_display(&display);
                    }
                    if let Some(msg) = displays_message(config, &display) {
                        ctrl_tx.send(&msg).await?;
                    }
                    let _ = screen_tx.send(screen_rect(&display));
                }
                Some(Msg::SetResolution(set)) => {
                    let refused = if !resolution_offered {
                        Some("@agent.resolution_disabled")
                    } else if !off.is_on(switches::RESOLUTION) {
                        Some("@agent.resolution_switched_off")
                    } else if display.index == nuvira_capture::ALL_DISPLAYS {
                        Some("@agent.resolution_all_screens")
                    } else {
                        None
                    };
                    let mut error = refused.map(String::from).unwrap_or_default();
                    if refused.is_none() {
                        let d = display.clone();
                        let target = nuvira_capture::resolution::Size { width: set.width, height: set.height };
                        let before = (display.width, display.height);
                        let result = tokio::task::spawn_blocking(move || match target.width == 0 || target.height == 0 {
                            true => Ok(nuvira_capture::resolution::restore(owner, Some(&d))),
                            false => nuvira_capture::resolution::fit(owner, &d, target).map(|s| (s.width, s.height) != before),
                        })
                        .await?;
                        match result {
                            // The capture starts again at the new size.
                            Ok(true) => {
                                display = reopen(&source, &pipeline, display.index, display.index).await?;
                                ctrl_tx.send(&video_format(&display)).await?;
                                if let Some(shared) = &shared {
                                    shared.set_display(&display);
                                }
                                if let Some(msg) = displays_message(config, &display) {
                                    ctrl_tx.send(&msg).await?;
                                }
                                let _ = screen_tx.send(screen_rect(&display));
                            }
                            Ok(false) => {}
                            Err(e) => {
                                warn!("screen resolution not changed: {e:#}");
                                error = e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@'))
                                    .unwrap_or_else(|| "@agent.resolution_failed".into());
                            }
                        }
                    }
                    let state = nuvira_proto::v1::ResolutionState {
                        width: display.width,
                        height: display.height,
                        adapted: nuvira_capture::resolution::adapted(owner, &display),
                        error,
                    };
                    ctrl_tx.send(&control(Msg::ResolutionState(state))).await?;
                }
                Some(Msg::Clipboard(clip)) => {
                    if let Some(clipboard) = clipboard.as_ref().filter(|_| off.is_on(switches::CLIPBOARD)) {
                        let paste = clip.paste();
                        let then: Option<Box<dyn FnOnce(bool) + Send>> = match paste {
                            nuvira_proto::v1::Paste::None => None,
                            _ => {
                                let paste_tx = paste_tx.clone();
                                Some(Box::new(move |written| {
                                    if written {
                                        let _ = paste_tx.send(paste);
                                    }
                                }))
                            }
                        };
                        let content = nuvira_clipboard::Content::Text(clip.text);
                        let _ = clipboard.send(nuvira_clipboard::Incoming { content, then });
                    }
                }
                // Files copied at the viewer: offered on this clipboard.
                Some(Msg::ClipboardFiles(list)) => {
                    if files_allowed() && let Some(clipboard) = &clipboard {
                        info!(entries = list.files.len(), "files copied at the viewer: on the clipboard");
                        let _ = clipboard.send(nuvira_clipboard::Content::RemoteFiles(sharing.remote(list)).into());
                    }
                }
                // Files copied here, pasted at the viewer: read as the user signed in.
                Some(Msg::ClipboardFileRequest(request)) => {
                    let allowed = files_allowed();
                    let (sharing, conn, off) = (sharing.clone(), conn.clone(), off.clone());
                    tokio::spawn(async move {
                        let opened = match (allowed, sharing.requested(&request)) {
                            (false, _) => Err(anyhow::anyhow!("@agent.files_switched_off")),
                            (true, Err(e)) => Err(e),
                            (true, Ok(path)) => fsops::as_user(move || {
                                std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))
                            })
                            .await,
                        };
                        let sent = nuvira_clipboard::Sharing::answer(&conn, &request, opened);
                        let sent = switches::until_off(off.clone(), switches::CLIPBOARD, sent);
                        match switches::until_off(off, switches::FILES, sent).await.flatten() {
                            Some(Err(e)) => warn!("pasted file not sent: {e:#}"),
                            Some(Ok(())) => {}
                            None => info!("pasted file cut: switched off at the PC"),
                        }
                    });
                }
                Some(Msg::Chat(chat)) => match &events {
                    Some(events) => {
                        let _ = events.send(AgentEvent::Chat { text: chat.text });
                    }
                    None => info!("chat from the viewer: {}", chat.text),
                },
                Some(Msg::ViewSize(size)) => {
                    let packed = crate::pipeline::pack_view(size.width, size.height);
                    if view.swap(packed, Ordering::Relaxed) != packed {
                        tracing::debug!("viewer area {}x{}", size.width, size.height);
                    }
                }
                Some(Msg::SetAudio(set)) => {
                    info!(on = set.on, "sound of the session");
                    let _ = audio_on_tx.send(set.on);
                }
                Some(Msg::SetMicrophone(set)) => {
                    let muted = gate.is_some_and(|g| *g.ctx.mic_muted.borrow());
                    let state = match (mic_offered, muted) {
                        (false, _) => nuvira_proto::v1::MicrophoneState {
                            on: false,
                            error: "@agent.microphone_unavailable".into(),
                        },
                        (true, true) if set.on => nuvira_proto::v1::MicrophoneState {
                            on: false,
                            error: "@agent.microphone_muted".into(),
                        },
                        _ => nuvira_proto::v1::MicrophoneState { on: set.on, error: String::new() },
                    };
                    info!(on = set.on, played = state.on, "technician's microphone asked");
                    let _ = mic_on_tx.send(set.on && mic_offered);
                    ctrl_tx.send(&control(Msg::MicrophoneState(state))).await?;
                }
                Some(Msg::SetWallpaper(set)) => {
                    if let Some(state) = wallpaper_state.clone() {
                        info!(hidden = set.hidden, "wallpaper asked by the viewer");
                        let done = tokio::task::spawn_blocking(move || match set.hidden {
                            true => nuvira_capture::wallpaper::hide(&state),
                            false => {
                                nuvira_capture::wallpaper::restore(&state);
                                Ok(())
                            }
                        })
                        .await?;
                        if let Err(e) = done {
                            warn!("wallpaper not hidden: {e:#}");
                        }
                    }
                }
                Some(Msg::SetLockOnEnd(set)) => {
                    if let Some(g) = gate.filter(|g| !g.request.join) {
                        info!(on = set.on, "lock at the end asked by the viewer");
                        g.ctx.lock_asked.store(set.on, Ordering::Relaxed);
                        let (on, forced) = lock_wanted(g.ctx, g.request);
                        let state = nuvira_proto::v1::LockOnEndState { on, forced };
                        ctrl_tx.send(&control(Msg::LockOnEndState(state))).await?;
                    }
                }
                // Handed over to a guest: its token goes to it, this session ends
                // (and the guests' with it), and it connects again as the first viewer.
                Some(Msg::HandOver(h)) => {
                    let from = gate.map(|g| match g.request.viewer_name.as_str() {
                        "" => g.request.viewer_id.clone(),
                        name => name.to_string(),
                    });
                    let handed = match (gate, &shared, from) {
                        (Some(g), Some(s), Some(from)) => s.hand_over(h.guest, &from).map(|t| (g, t)),
                        _ => None,
                    };
                    match handed {
                        Some((g, (token, viewer_id, name))) => {
                            *g.ctx.handover.lock().expect("handover") =
                                Some(shared::Handover { viewer_id, token, at: Instant::now() });
                            info!(to = name, "session handed over");
                            let told = nuvira_proto::v1::HandedOver { to: name, ..Default::default() };
                            ctrl_tx.send(&control(Msg::HandedOver(told))).await?;
                            // A moment for the guest to get its token before its session ends.
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            return anyhow::Ok(());
                        }
                        None => warn!(guest = h.guest, "session not handed over: no such guest, or it cannot take it"),
                    }
                }
                Some(Msg::SetPrivacy(set)) => {
                    let mut error = String::new();
                    if set.on && privacy.is_none() {
                        if policy.no_privacy || !off.is_on(switches::INPUT) {
                            error = "@agent.privacy_disabled".into();
                        } else if !privacy_allowed() {
                            error = "@agent.privacy_not_allowed".into();
                        } else if config.view_only || config.synthetic.is_some() {
                            error = "@agent.privacy_unsupported".into();
                        } else {
                            let message = nuvira_i18n::t("agent.privacy_screen");
                            match tokio::task::spawn_blocking(move || {
                                nuvira_input::privacy::start(&message, &nuvira_capture::hide_process)
                            }).await? {
                                Ok(on) => privacy = Some(on),
                                Err(e) => {
                                    warn!("privacy mode not turned on: {e:#}");
                                    error = e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@'))
                                        .unwrap_or_else(|| "@agent.privacy_failed".into());
                                }
                            }
                        }
                    } else if !set.on && let Some(on) = privacy.take() {
                        tokio::task::spawn_blocking(move || on.stop()).await?;
                    }
                    let state = nuvira_proto::v1::PrivacyState { on: privacy.is_some(), error };
                    ctrl_tx.send(&control(Msg::PrivacyState(state))).await?;
                }
                Some(Msg::SetRecording(set)) if !set.on && recording_required => {
                    warn!("session ended: the viewer stopped the recording the server's rules want");
                    tell_app(false);
                    let state = nuvira_proto::v1::RecordingState { on: false, error: "@agent.recording_required".into() };
                    ctrl_tx.send(&control(Msg::RecordingState(state))).await?;
                    return anyhow::Ok(());
                }
                Some(Msg::SetRecording(set)) => {
                    let mut error = String::new();
                    if set.on && !recording_allowed() {
                        error = "@agent.recording_not_allowed".into();
                    } else if set.on != recording {
                        recording = set.on;
                        info!(on = recording, "session recording");
                        tell_app(recording);
                    }
                    let state = nuvira_proto::v1::RecordingState { on: recording, error };
                    ctrl_tx.send(&control(Msg::RecordingState(state))).await?;
                }
                Some(Msg::RestartCheck(_)) => {
                    // Asked before the confirmation: the technician is told whose
                    // sessions a restart would close.
                    let others = if restart_available {
                        tokio::task::spawn_blocking(restart::other_users).await.unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    info!(others = others.len(), "other people signed in, asked before a restart");
                    let users = nuvira_proto::v1::RestartUsers { others };
                    ctrl_tx.send(&control(Msg::RestartUsers(users))).await?;
                }
                Some(Msg::Restart(asked)) => {
                    let mut state = nuvira_proto::v1::RestartState::default();
                    // Without keyboard and mouse (view only) no restart either.
                    let refused = !restart_available || (asked.safe_mode && !safe_mode_offered);
                    if refused || !off.is_on(switches::INPUT) {
                        state.error = "@agent.restart_unavailable".into();
                    } else {
                        info!(safe_mode = asked.safe_mode, "restart of the computer asked by the viewer");
                        let restart = move || {
                            if asked.safe_mode {
                                restart::prepare_safe_mode()?;
                            }
                            restart::restart()
                        };
                        match tokio::task::spawn_blocking(restart).await? {
                            Ok(()) => {
                                state.ok = true;
                                // Back without anyone at the computer: the permanent
                                // password, with no confirmation asked.
                                state.reconnect = gate.is_some_and(|g| {
                                    matches!(*g.access.lock().expect("access"), "permanent" | "profile")
                                });
                                if let Some(g) = gate {
                                    *g.ctx.restarting.lock().expect("restarting") = Some(Instant::now());
                                }
                            }
                            Err(e) => {
                                warn!("restart refused by the system: {e:#}");
                                state.error = e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@'))
                                    .unwrap_or_else(|| format!("{e:#}"));
                            }
                        }
                    }
                    let restarting = state.ok;
                    ctrl_tx.send(&control(Msg::RestartState(state))).await?;
                    if restarting {
                        // The answer leaves, then the session ends before the system stops it.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        info!("session ended: the computer is restarting");
                        return anyhow::Ok(());
                    }
                }
                Some(Msg::SetQuality(q)) => {
                    let (limit_bps, limit_fps) = quality_limits(config, q.quality());
                    fps.store(limit_fps, Ordering::Relaxed);
                    let bps = match abr.as_mut() {
                        Some(abr) => abr.set_max(limit_bps),
                        None => Some(limit_bps),
                    };
                    if let Some(bps) = bps {
                        bitrate.store(encoder_bps(bps), Ordering::Relaxed);
                    }
                    info!("quality {:?}: up to {} kbit/s and {limit_fps} fps", q.quality(), limit_bps / 1000);
                }
                _ => {}
            }
        }
    };

    let input = async {
        // Keyboard and mouse: while the session has them and they are not switched off.
        let open_injector = |screen| -> Result<_> { input_thread(nuvira_input::open(screen)?) };
        let closed = || anyhow!("input thread ended");
        let input_offered = offered & switches::INPUT != 0;
        // The viewer's input stream shows only with its first event; the events of
        // viewers who joined the session may come before.
        let mut injector = None;
        let stream = loop {
            tokio::select! {
                stream = conn.accept_uni() => break stream?,
                Some(event) = guest_input_rx.recv() => {
                    if !(input_offered && off.is_on(switches::INPUT)) {
                        // Ending the thread releases the keys still pressed.
                        injector = None;
                        continue;
                    }
                    if injector.is_none() {
                        injector = Some(open_injector(*screen_rx.borrow_and_update())?);
                    }
                    if let Some(injector) = &injector {
                        injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                    }
                }
                Ok(()) = screen_rx.changed() => if let Some(injector) = &injector {
                    let screen = *screen_rx.borrow_and_update();
                    injector.send(InputCmd::Screen(screen)).map_err(|_| closed())?;
                },
            }
        };
        let mut rx = FramedRecv::with_limit(stream, HANDSHAKE_MESSAGE_LIMIT);
        // Switched off before the viewer's first event (the stream shows only then): none.
        if injector.is_none() && input_offered && off.is_on(switches::INPUT) {
            injector = Some(open_injector(*screen_rx.borrow_and_update())?);
        } else if !off.is_on(switches::INPUT) {
            injector = None;
        }
        let mut input_changes = off.clone();
        loop {
            tokio::select! {
                event = rx.recv::<InputEvent>() => match event? {
                    Some(event) => {
                        mark_activity();
                        if let Some(injector) = &injector {
                            injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                        }
                    }
                    None => break,
                },
                now = input_changes.changed(), if offered & switches::INPUT != 0 => {
                    match (now & switches::INPUT == 0, injector.is_some()) {
                        // Ending the thread releases the keys still pressed.
                        (false, true) => {
                            injector = None;
                            info!("keyboard and mouse switched off by the person at the PC");
                        }
                        (true, false) => {
                            injector = Some(open_injector(*screen_rx.borrow_and_update())?);
                            info!("keyboard and mouse switched on again by the person at the PC");
                        }
                        _ => {}
                    }
                }
                // The keyboard and mouse of viewers who joined the session.
                Some(event) = guest_input_rx.recv() => if let Some(injector) = &injector {
                    injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                },
                Ok(()) = screen_rx.changed() => if let Some(injector) = &injector {
                    let screen = *screen_rx.borrow_and_update();
                    injector.send(InputCmd::Screen(screen)).map_err(|_| closed())?;
                },
                // The text of a Clipboard.paste is in the clipboard: its keys now.
                Some(paste) = paste_rx.recv() => if let Some(injector) = &injector {
                    tracing::debug!(?paste, "clipboard written: paste keys");
                    for event in paste_keys(paste) {
                        injector.send(InputCmd::Event(event)).map_err(|_| closed())?;
                    }
                },
                // After the input one, every unidirectional stream of the viewer is a file
                // or an image copied to the clipboard.
                stream = conn.accept_uni() => {
                    let mut stream = stream?;
                    if config.view_only || config.synthetic.is_some() {
                        // View only: no files.
                        let _ = stream.stop(0u32.into());
                        continue;
                    }
                    let (file_tx, events, clipboard) = (file_tx.clone(), events.clone(), clipboard_in.clone());
                    let no_files = policy.no_files;
                    let off = off.clone();
                    let sharing = sharing.clone();
                    tokio::spawn(async move {
                        let mut rx = FramedRecv::new(stream);
                        let first = match rx.recv::<nuvira_proto::v1::FileChunk>().await {
                            Ok(Some(first)) => first,
                            _ => return,
                        };
                        // The answer to a paste here (asked only while allowed): cut as
                        // soon as files or the clipboard are switched off (the paste
                        // then fails instead of finishing).
                        if first.clipboard_file {
                            let receiving = sharing.receive(first, rx);
                            let receiving = switches::until_off(off.clone(), switches::CLIPBOARD, receiving);
                            if switches::until_off(off, switches::FILES, receiving).await.flatten().is_none() {
                                info!("pasted file cut: switched off at the PC");
                            }
                            return;
                        }
                        // Rules of the server: no files (images of the clipboard are refused
                        // by having no clipboard, see above).
                        let switched_off = !off.is_on(switches::FILES);
                        if (no_files || switched_off) && !first.clipboard_image {
                            let message = if no_files { "@agent.files_disabled" } else { "@agent.files_switched_off" };
                            let _ = file_tx.send(nuvira_proto::v1::FileReceived {
                                name: first.name,
                                transfer_id: first.transfer_id,
                                message: message.into(),
                                ..Default::default()
                            });
                            return;
                        }
                        if first.clipboard_image && !off.is_on(switches::CLIPBOARD) {
                            return;
                        }
                        // Switched off during the transfer: it stops there.
                        let stopped = |name: String, transfer_id: u32| nuvira_proto::v1::FileReceived {
                            name,
                            transfer_id,
                            message: "@agent.files_switched_off".into(),
                            ..Default::default()
                        };
                        // File manager: into the chosen folder, with resume.
                        if first.transfer_id != 0 {
                            let (name, transfer_id) = (first.name.clone(), first.transfer_id);
                            let Some(received) = switches::until_off(off, switches::FILES, fsops::receive(first, rx)).await else {
                                let _ = file_tx.send(stopped(name, transfer_id));
                                return;
                            };
                            let done = match received {
                                Ok(path) => nuvira_proto::v1::FileReceived {
                                    name,
                                    ok: true,
                                    transfer_id,
                                    path: path.display().to_string(),
                                    ..Default::default()
                                },
                                Err(e) => {
                                    warn!("file {name} not received: {e:#}");
                                    nuvira_proto::v1::FileReceived { name, transfer_id, message: format!("{e:#}"), ..Default::default() }
                                }
                            };
                            let _ = file_tx.send(done);
                            return;
                        }
                        if first.clipboard_image {
                            match nuvira_transport::read_stream(&mut rx, &first, nuvira_clipboard::MAX_IMAGE_PNG as u64).await {
                                Ok(png) => if let Some(clipboard) = clipboard {
                                    let _ = clipboard.send(nuvira_clipboard::Content::Image(png).into());
                                },
                                Err(e) => warn!("clipboard image not received: {e:#}"),
                            }
                            return;
                        }
                        let name = first.name.clone();
                        let Some(received) = switches::until_off(off, switches::FILES, files::receive(first, rx)).await else {
                            let _ = file_tx.send(stopped(name, 0));
                            return;
                        };
                        let done = match received {
                            Ok((name, path)) => {
                                if let Some(events) = &events {
                                    let path = path.display().to_string();
                                    let _ = events.send(AgentEvent::FileReceived { name: name.clone(), path });
                                }
                                nuvira_proto::v1::FileReceived { name, ok: true, message: "@agent.downloads".into(), ..Default::default() }
                            }
                            Err((name, e)) => {
                                warn!("file {name} not received: {e:#}");
                                nuvira_proto::v1::FileReceived { name, ok: false, message: format!("{e:#}"), ..Default::default() }
                            }
                        };
                        let _ = file_tx.send(done);
                    });
                }
            }
        }
        anyhow::Ok(())
    };

    // A failure of remote printing does not end the session.
    let printing = async {
        if let Some(printer) = printer
            && let Err(e) = printer.serve(conn.clone(), print_allowed).await
        {
            warn!("remote printing stopped: {e:#}");
        }
        std::future::pending::<Result<()>>().await
    };

    let result = tokio::select! {
        r = video => r.context("sending video"),
        r = control_loop => r.context("control channel"),
        r = input => r.context("input channel"),
        r = accept_streams(&conn, (&terminal_refused, &tunnel_refused), &off, switch_offered.then_some(&switch_tx)) => r,
        r = printing => r,
        r = audio => r.context("sound"),
        r = microphone => r.context("microphone"),
    };
    let last = pipeline.lock().expect("pipeline").take();
    let _ = tokio::task::spawn_blocking(move || drop(last)).await;
    if let Some(gate) = gate {
        *gate.ctx.chat.lock().expect("chat") = None;
    }
    drop(chat_tx);
    result
}

/// A session with terminals only: the control channel carries ping and chat, every
/// other bidirectional stream of the viewer is a terminal.
async fn terminal_session(
    conn: &Connection,
    mut ctrl_tx: FramedSend,
    mut ctrl_rx: FramedRecv,
    gate: Option<&Gate<'_>>,
    refused: Refusals<'_, impl Fn() -> Option<&'static str>, impl Fn() -> Option<&'static str>>,
    off: switches::Off,
    tell_permissions: &impl Fn(u8) -> ControlMessage,
) -> Result<()> {
    info!("terminal session (no desktop)");
    let mut changes = off.clone();
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<String>();
    let events = gate.map(|g| {
        *g.ctx.chat.lock().expect("chat") = Some(chat_tx.clone());
        g.ctx.events.clone()
    });
    let control = async {
        loop {
            tokio::select! {
                msg = ctrl_rx.recv::<ControlMessage>() => match msg? {
                    None => return anyhow::Ok(()),
                    Some(ControlMessage { msg: Some(Msg::Ping(ping)) }) => {
                        let pong = Pong { ping_sent_us: ping.sent_us, agent_us: nuvira_proto::now_us() };
                        ctrl_tx.send(&control(Msg::Pong(pong))).await?;
                    }
                    Some(ControlMessage { msg: Some(Msg::Chat(chat)) }) => {
                        if let Some(events) = &events {
                            let _ = events.send(AgentEvent::Chat { text: chat.text });
                        }
                    }
                    Some(_) => {}
                },
                Some(text) = chat_rx.recv() => {
                    ctrl_tx.send(&control(Msg::Chat(nuvira_proto::v1::Chat { text }))).await?;
                }
                now = changes.changed() => {
                    ctrl_tx.send(&tell_permissions(now)).await?;
                }
            }
        }
    };
    let result = tokio::select! {
        r = control => r.context("control channel"),
        r = accept_streams(conn, refused, &off, None) => r,
    };
    if let Some(gate) = gate {
        *gate.ctx.chat.lock().expect("chat") = None;
    }
    result
}

/// Why a terminal and a tunnel are refused, checked when each one opens.
type Refusals<'a, T, U> = (&'a T, &'a U);

/// Every bidirectional stream of the viewer after the control one is a terminal or a
/// TCP tunnel (its first message, `StreamOpen`, says which), served by a task of its
/// own. It never ends by itself: the end of the connection is seen by the other
/// channels of the session.
async fn accept_streams(
    conn: &Connection,
    (terminal_refused, tunnel_refused): Refusals<'_, impl Fn() -> Option<&'static str>, impl Fn() -> Option<&'static str>>,
    off: &switches::Off,
    switch_sides: Option<&mpsc::UnboundedSender<nuvira_proto::v1::SwitchSides>>,
) -> Result<()> {
    use nuvira_proto::v1::{StreamOpen, stream_open::Kind};
    while let Ok((send, mut recv)) = conn.accept_bi().await {
        let (terminal, tunnel) = (terminal_refused(), tunnel_refused());
        let off = off.clone();
        let switch_sides = switch_sides.cloned();
        tokio::spawn(async move {
            let limit = nuvira_transport::STREAM_HEADER_LIMIT;
            let open = match nuvira_transport::recv_one_with_limit::<StreamOpen>(&mut recv, limit).await {
                Ok(Some(open)) => open,
                Ok(None) => return,
                Err(e) => return warn!("stream from the viewer: {e:#}"),
            };
            let result = match open.kind {
                Some(Kind::Terminal(t)) => {
                    let size = t.size.unwrap_or_default();
                    let stop = off.switched_off(switches::TERMINAL);
                    terminal::serve(FramedSend::new(send), FramedRecv::new(recv), size, terminal, stop).await
                }
                // Switched off: the connection is cut.
                Some(Kind::Tunnel(t)) => {
                    switches::until_off(off, switches::TUNNEL, tunnel::serve(send, recv, t, tunnel)).await.unwrap_or(Ok(()))
                }
                // Switch sides: the window with the viewer's screen (an agent that
                // does not offer it closes the stream).
                Some(Kind::Reverse(open)) => match switch_sides {
                    Some(report) => reverse::open_window(send, recv, open, report).await,
                    None => Ok(()),
                },
                None => Ok(()),
            };
            if let Err(e) = result {
                warn!("stream from the viewer: {e:#}");
            }
        });
    }
    std::future::pending().await
}

pub(crate) fn permissions(offered: u8, off: u8) -> nuvira_proto::v1::Permissions {
    let on = |bit: u8| offered & bit != 0 && off & bit == 0;
    nuvira_proto::v1::Permissions {
        input: on(switches::INPUT),
        clipboard: on(switches::CLIPBOARD),
        files: on(switches::FILES),
        terminal: on(switches::TERMINAL),
        tunnel: on(switches::TUNNEL),
        print: on(switches::PRINT),
        resolution: on(switches::RESOLUTION),
        audio: on(switches::AUDIO),
        switched_off: switches::names(off & offered),
    }
}

/// At the end of a session its switches are gone: the app's commands find nothing.
struct ClearSwitches<'a>(Option<&'a SessionContext>);

impl Drop for ClearSwitches<'_> {
    fn drop(&mut self) {
        if let Some(ctx) = self.0 {
            *ctx.switches.lock().expect("switches") = None;
        }
    }
}
