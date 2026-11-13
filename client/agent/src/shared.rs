//! Shared sessions: more viewers on the computer of a session in progress.
//!
//! A second technician (at most [`MAX_GUESTS`]) reaches the computer while someone
//! is already connected: they go through the same access control (password,
//! confirmation by the person at the PC), then see the same picture and use the
//! same keyboard and mouse, and hear the same sound (each with its own mute
//! button). The rest of the session (files, clipboard, terminal, tunnels,
//! printing, recording, privacy mode, the screen and its resolution) stays with
//! the first viewer, and the agent says so in its Hello:
//! any viewer works as a guest, older ones too. The guests leave when the first
//! viewer's session ends.
//!
//! The encoder runs once: its frames are copied to each guest. A guest that cannot
//! keep up loses frames, waits for the next keyframe and asks for one (every 2 s at
//! most); the bitrate follows the first viewer's network.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use nuvira_proto::v1::{
    AuthResult, ControlMessage, Hello, InputEvent, Pong, VideoFrame, control_message::Msg,
};
use tokio::sync::{broadcast, watch};
use nuvira_transport::{Connection, FramedRecv, FramedSend};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{AgentEvent, Gate, SessionContext, WRONG_PASSWORD, control, local_hello, video_format};

/// Viewers that can join a session besides the one who started it.
pub const MAX_GUESTS: usize = 3;

/// What goes to a guest from the session.
enum ToGuest {
    Frame(VideoFrame),
    /// The first viewer chose another screen, or its resolution changed.
    Format(nuvira_capture::Display),
}

struct Guest {
    id: u64,
    name: String,
    tx: mpsc::Sender<ToGuest>,
    /// Frames were dropped: the next ones are useless until a keyframe.
    lost: Arc<AtomicBool>,
}

/// The session in progress, as guests see it (`SessionContext::shared`).
pub struct Shared {
    /// The keyframe request of the session's encoder.
    keyframe: Arc<AtomicBool>,
    last_key: Mutex<Option<Instant>>,
    display: Mutex<nuvira_capture::Display>,
    guests: Mutex<Vec<Guest>>,
    next_id: AtomicU64,
    /// The guests' keyboard and mouse, into the session's input (its switch and
    /// the first viewer's injector apply).
    input: mpsc::UnboundedSender<InputEvent>,
    /// The keyboard and mouse can be used in this session at all.
    input_offered: bool,
    events: mpsc::UnboundedSender<AgentEvent>,
    /// The guests' names, for the first viewer (`Guests` control message).
    names: tokio::sync::watch::Sender<Vec<String>>,
    /// The computer's sound is sent in this session (the first viewer plays it).
    pub audio_offered: bool,
    /// The sound packets as sent to the first viewer (datagrams), for the guests.
    audio: broadcast::Sender<Arc<Vec<u8>>>,
    /// Guests that want the sound: the capture runs while the first viewer or one
    /// of them does.
    listeners: watch::Sender<usize>,
    /// The pointer comes apart from the picture in this session (Hello.cursor).
    cursor_offered: bool,
}

impl Shared {
    pub fn new(
        keyframe: Arc<AtomicBool>,
        display: nuvira_capture::Display,
        input: mpsc::UnboundedSender<InputEvent>,
        input_offered: bool,
        events: mpsc::UnboundedSender<AgentEvent>,
        audio_offered: bool,
        cursor_offered: bool,
    ) -> Self {
        Self {
            keyframe,
            last_key: Mutex::new(None),
            display: Mutex::new(display),
            guests: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            input,
            input_offered,
            events,
            names: tokio::sync::watch::channel(Vec::new()).0,
            audio_offered,
            audio: broadcast::channel(64).0,
            listeners: watch::channel(0).0,
            cursor_offered,
        }
    }

    /// A sound packet as sent to the first viewer, for the guests.
    pub fn audio_packet(&self, datagram: &Arc<Vec<u8>>) {
        let _ = self.audio.send(datagram.clone());
    }

    /// How many guests want the sound, at every change.
    pub fn listeners(&self) -> watch::Receiver<usize> {
        self.listeners.subscribe()
    }

    fn listen(&self, on: bool) {
        self.listeners.send_modify(|n| *n = if on { *n + 1 } else { n.saturating_sub(1) });
    }

    /// The guests' names at every change, for the first viewer.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Vec<String>> {
        self.names.subscribe()
    }

    /// The app and the first viewer learn who is in the session.
    fn tell(&self, names: Vec<String>) {
        self.names.send_replace(names.clone());
        let _ = self.events.send(AgentEvent::Guests { names });
    }

    /// A frame sent to the first viewer: a copy to every guest.
    pub fn frame(&self, frame: &VideoFrame) {
        let mut guests = self.guests.lock().expect("guests");
        if guests.is_empty() {
            return;
        }
        let before = guests.len();
        guests.retain(|g| match g.tx.try_send(ToGuest::Frame(frame.clone())) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                g.lost.store(true, Ordering::Relaxed);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        let lost = guests.iter().any(|g| g.lost.load(Ordering::Relaxed));
        let changed = guests.len() != before;
        let names = changed.then(|| names(&guests));
        drop(guests);
        if lost {
            self.request_keyframe();
        }
        if let Some(names) = names {
            self.tell(names);
        }
    }

    /// The picture changed size or screen.
    pub fn set_display(&self, display: &nuvira_capture::Display) {
        *self.display.lock().expect("display") = display.clone();
        for g in self.guests.lock().expect("guests").iter() {
            // A new stream starts with a keyframe: what was waiting is useless.
            g.lost.store(true, Ordering::Relaxed);
            let _ = g.tx.try_send(ToGuest::Format(display.clone()));
        }
    }

    /// A keyframe for a guest that joined or lost frames: every 2 s at most, so that
    /// a slow guest does not fill the first viewer's network with keyframes.
    fn request_keyframe(&self) {
        let mut last = self.last_key.lock().expect("keyframe");
        if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
            *last = Some(Instant::now());
            self.keyframe.store(true, Ordering::Relaxed);
        }
    }

    fn join(&self, name: String) -> (u64, mpsc::Receiver<ToGuest>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel(8);
        let lost = Arc::new(AtomicBool::new(true));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let names = {
            let mut guests = self.guests.lock().expect("guests");
            guests.push(Guest { id, name, tx, lost: lost.clone() });
            names(&guests)
        };
        self.tell(names);
        *self.last_key.lock().expect("keyframe") = None;
        self.request_keyframe();
        (id, rx, lost)
    }

    fn leave(&self, id: u64) {
        let names = {
            let mut guests = self.guests.lock().expect("guests");
            guests.retain(|g| g.id != id);
            names(&guests)
        };
        self.tell(names);
    }

    fn full(&self) -> bool {
        self.guests.lock().expect("guests").len() >= MAX_GUESTS
    }

    /// The session ends: the guests' channels close, and their sessions with them.
    pub fn end(&self) {
        self.guests.lock().expect("guests").clear();
    }
}

fn names(guests: &[Guest]) -> Vec<String> {
    guests.iter().map(|g| g.name.clone()).collect()
}

/// Ends the sharing of the session however it ends (`SessionContext::shared`).
pub struct EndShared<'a>(pub Option<&'a SessionContext>);

impl Drop for EndShared<'_> {
    fn drop(&mut self) {
        if let Some(ctx) = self.0
            && let Some(shared) = ctx.shared.lock().expect("shared").take()
        {
            shared.end();
        }
    }
}

/// Whether a request can join the session in progress.
pub fn can_join(ctx: &SessionContext) -> bool {
    ctx.shared.lock().expect("shared").as_ref().is_some_and(|s| !s.full())
}

/// A guest's session, its connection open: access control, then the picture and
/// the input until either side ends it or the first viewer's session ends.
pub async fn guest_session(
    conn: Connection,
    (send, recv): (quinn::SendStream, quinn::RecvStream),
    gate: &Gate<'_>,
) -> Result<()> {
    let mut ctrl_tx = FramedSend::new(send);
    let mut ctrl_rx = FramedRecv::new(recv);
    let Some(ControlMessage { msg: Some(Msg::Hello(hello)) }) = ctrl_rx.recv().await? else {
        bail!("the viewer did not introduce itself");
    };
    info!(
        device = hello.device_name,
        os = hello.os,
        version = hello.app_version,
        "viewer joining the session in progress"
    );
    if hello.protocol_version < nuvira_proto::MIN_PROTOCOL_VERSION {
        bail!("viewer protocol version not supported: {}", hello.protocol_version);
    }
    let policy = gate.policy();
    let Some(shared) = gate.ctx.shared.lock().expect("shared").clone() else {
        bail!("@agent.busy");
    };
    let input = shared.input_offered && !policy.no_input;
    // The sound where the first viewer has it, and this one plays it.
    let audio = shared.audio_offered && hello.audio && !policy.no_audio && conn.max_datagram_size().is_some();
    // The pointer apart, to a guest that draws it; one that cannot gets it in the
    // picture (for everyone, while it stays).
    let cursor = shared.cursor_offered && hello.cursor;
    let answer = Hello { password_required: true, text: input, audio, cursor, ..local_hello() };
    ctrl_tx.send(&control(Msg::Hello(answer))).await?;
    let given = match ctrl_rx.recv::<ControlMessage>().await? {
        Some(ControlMessage { msg: Some(Msg::Auth(auth)) }) => auth,
        _ => bail!("the viewer did not send the password"),
    };
    // Only the first viewer has a terminal; a recording required by the rules cannot
    // be made by a guest.
    let refused = if hello.terminal_only {
        Some("@agent.busy")
    } else if policy.record_always {
        Some("@agent.recording_required")
    } else {
        None
    };
    let verdict = match refused {
        Some(reason) => Err(reason.to_string()),
        None => {
            let pending = (hello.protocol_version
                >= nuvira_proto::v1::ProtocolVersion::ConfirmationPending as u32)
                .then_some(&mut ctrl_tx);
            gate.authorize(&given, pending).await
        }
    };
    // The session may have ended while the person at the PC was asked.
    let verdict = verdict.and_then(|()| match gate.ctx.shared.lock().expect("shared").as_ref() {
        Some(s) if Arc::ptr_eq(s, &shared) && !s.full() => Ok(()),
        _ => Err("@agent.busy".to_string()),
    });
    let (ok, message) = match &verdict {
        Ok(()) => (true, String::new()),
        Err(reason) => (false, reason.clone()),
    };
    ctrl_tx.send(&control(Msg::AuthResult(AuthResult { ok, message }))).await?;
    if let Err(reason) = verdict {
        let pause = if reason == WRONG_PASSWORD { 2000 } else { 500 };
        tokio::time::sleep(Duration::from_millis(pause)).await;
        bail!("{reason}");
    }

    let name = match gate.request.viewer_name.as_str() {
        "" => gate.request.viewer_id.clone(),
        name => name.to_string(),
    };
    let (id, mut frames, lost) = shared.join(name);
    struct Leave<'a>(&'a Shared, u64);
    impl Drop for Leave<'_> {
        fn drop(&mut self) {
            self.0.leave(self.1);
        }
    }
    let _leave = Leave(&shared, id);
    struct PlainGuest(bool);
    impl Drop for PlainGuest {
        fn drop(&mut self) {
            if self.0 {
                crate::plain_guest(false);
            }
        }
    }
    let plain = shared.cursor_offered && !cursor;
    if plain {
        crate::plain_guest(true);
    }
    let _plain = PlainGuest(plain);
    info!(guests = shared.guests.lock().expect("guests").len(), "viewer joined the session");

    let display = shared.display.lock().expect("display").clone();
    ctrl_tx.send(&video_format(&display)).await?;
    // What the guest's session has: the keyboard and mouse, or nothing.
    let mut permissions = crate::permissions(0, 0);
    permissions.input = input;
    permissions.audio = audio;
    ctrl_tx.send(&control(Msg::Permissions(permissions))).await?;
    let mut video_tx = FramedSend::new(conn.open_uni().await?);

    // Answers to the viewer's pings, sent by the loop that owns the control channel.
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<Pong>();
    let mut pointer = crate::PointerSender::default();
    let mut pointer_tick = tokio::time::interval(Duration::from_millis(16));
    pointer_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let video = async {
        loop {
            tokio::select! {
                _ = pointer_tick.tick(), if cursor => {
                    for msg in pointer.poll() {
                        ctrl_tx.send(&control(msg)).await?;
                    }
                }
                message = frames.recv() => match message {
                    Some(ToGuest::Frame(frame)) => {
                        if lost.load(Ordering::Relaxed) {
                            if !frame.keyframe {
                                continue;
                            }
                            lost.store(false, Ordering::Relaxed);
                        }
                        video_tx.send(&frame).await?;
                    }
                    Some(ToGuest::Format(display)) => ctrl_tx.send(&video_format(&display)).await?,
                    // The first viewer's session ended.
                    None => return anyhow::Ok(()),
                },
                Some(pong) = pong_rx.recv() => ctrl_tx.send(&control(Msg::Pong(pong))).await?,
            }
        }
    };
    let input_loop = async {
        let mut rx = FramedRecv::new(conn.accept_uni().await?);
        while let Some(event) = rx.recv::<InputEvent>().await? {
            if input {
                let _ = shared.input.send(event);
            }
        }
        anyhow::Ok(())
    };
    // The sound, while this guest's mute button is not pressed.
    let (audio_on_tx, mut audio_on_rx) = watch::channel(audio);
    let sound = async {
        if !audio {
            return std::future::pending().await;
        }
        struct Listening<'a>(&'a Shared, bool);
        impl Listening<'_> {
            fn set(&mut self, on: bool) {
                if on != self.1 {
                    self.0.listen(on);
                    self.1 = on;
                }
            }
        }
        impl Drop for Listening<'_> {
            fn drop(&mut self) {
                self.set(false);
            }
        }
        let mut listening = Listening(&shared, false);
        let mut packets = shared.audio.subscribe();
        loop {
            listening.set(*audio_on_rx.borrow_and_update());
            tokio::select! {
                changed = audio_on_rx.changed() => {
                    if changed.is_err() {
                        return anyhow::Ok(());
                    }
                }
                packet = packets.recv() => match packet {
                    Ok(packet) if listening.1 => match conn.send_datagram(packet.as_ref().clone().into()) {
                        Ok(()) => {}
                        Err(quinn::SendDatagramError::ConnectionLost(e)) => return Err(e.into()),
                        Err(e) => tracing::debug!("audio packet not sent to a guest: {e}"),
                    },
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return anyhow::Ok(()),
                },
            }
        }
    };
    let control_loop = async {
        while let Some(msg) = ctrl_rx.recv::<ControlMessage>().await? {
            match msg.msg {
                Some(Msg::Ping(ping)) => {
                    let _ =
                        pong_tx.send(Pong { ping_sent_us: ping.sent_us, agent_us: nuvira_proto::now_us() });
                }
                Some(Msg::RequestKeyframe(_)) => {
                    lost.store(true, Ordering::Relaxed);
                    shared.request_keyframe();
                }
                Some(Msg::SetAudio(set)) => {
                    let _ = audio_on_tx.send(set.on && audio);
                }
                _ => {}
            }
        }
        anyhow::Ok(())
    };
    let result = tokio::select! {
        r = video => r,
        r = input_loop => r,
        r = control_loop => r,
        r = sound => r,
    };
    if let Err(e) = &result {
        warn!("guest's session ended: {e:#}");
    }
    info!("viewer left the session");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display() -> nuvira_capture::Display {
        nuvira_capture::Display {
            index: 0,
            name: "Screen".into(),
            left: 0,
            top: 0,
            width: 1280,
            height: 720,
            primary: true,
        }
    }

    fn frame(seq: u64, keyframe: bool) -> VideoFrame {
        VideoFrame { seq, keyframe, data: vec![0; 10], ..Default::default() }
    }

    /// A guest that does not read (a slow network): its queue fills up, the frames
    /// after are dropped, it is marked as waiting for a keyframe and one is asked
    /// for, at most every 2 s; the names go to the first viewer and to the app.
    #[test]
    fn slow_guest_waits_for_a_keyframe() {
        let keyframe = Arc::new(AtomicBool::new(false));
        let (input, _input_rx) = mpsc::unbounded_channel();
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let shared = Shared::new(keyframe.clone(), display(), input, true, events, true, false);
        let mut names = shared.subscribe();

        let (id, mut rx, lost) = shared.join("NuviraHub Web".into());
        assert!(keyframe.swap(false, Ordering::Relaxed), "a keyframe for the new guest");
        assert!(lost.load(Ordering::Relaxed), "nothing shown before a keyframe");
        assert_eq!(*names.borrow_and_update(), ["NuviraHub Web"]);
        assert!(matches!(events_rx.try_recv(), Ok(AgentEvent::Guests { names }) if names == ["NuviraHub Web"]));

        // The guest's queue (8) fills up: the next frame is dropped and a keyframe
        // asked for, but not twice within 2 s.
        lost.store(false, Ordering::Relaxed);
        *shared.last_key.lock().unwrap() = None;
        for seq in 1..=8 {
            shared.frame(&frame(seq, false));
        }
        assert!(!lost.load(Ordering::Relaxed) && !keyframe.load(Ordering::Relaxed));
        shared.frame(&frame(9, false));
        assert!(lost.load(Ordering::Relaxed) && keyframe.swap(false, Ordering::Relaxed));
        shared.frame(&frame(10, false));
        assert!(!keyframe.load(Ordering::Relaxed), "a second keyframe within 2 s");
        // What it had queued it still gets; it shows nothing until the keyframe.
        let mut got = 0;
        while let Ok(ToGuest::Frame(_)) = rx.try_recv() {
            got += 1;
        }
        assert_eq!(got, 8);
        shared.frame(&frame(11, true));
        assert!(matches!(rx.try_recv(), Ok(ToGuest::Frame(f)) if f.keyframe));

        // The sound: guests that want it keep the capture going; the packets sent to
        // the first viewer reach them.
        let listeners = shared.listeners();
        shared.listen(true);
        shared.listen(true);
        shared.listen(false);
        assert_eq!(*listeners.borrow(), 1);
        let mut packets = shared.audio.subscribe();
        shared.audio_packet(&Arc::new(vec![0xA0, 1, 0, 0, 0, 9]));
        assert_eq!(packets.try_recv().unwrap().as_slice(), [0xA0, 1, 0, 0, 0, 9]);

        shared.leave(id);
        assert!(names.borrow_and_update().is_empty());
        // When the first viewer's session ends the guests' queues close.
        let (_, mut rx2, _) = shared.join("PC-B".into());
        shared.end();
        assert!(rx2.try_recv().is_err() && rx2.is_closed());
    }
}
