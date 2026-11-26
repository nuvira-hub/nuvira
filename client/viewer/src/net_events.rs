//! What the viewer does with the events of the network task (`NetEvent`).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use nuvira_i18n::{t, tf};
use nuvira_proto::v1::{Button, InputEvent, Key, MouseButton, MouseMove, input_event::Event};
use tracing::{debug, error, info, warn};
use winit::event_loop::ActiveEventLoop;

use crate::keys::test_key_code;
use crate::net::NetEvent;
use crate::{
    App, HANDOVER_TOKEN, Pointer, RESTART_FIRST_TRY, RESTART_RETRY, RESTART_WAIT, Stats, TAKEOVER_WAIT,
    closed_text, net, recorder, render, toolbar,
};

impl App {
    pub(super) fn net_event(&mut self, event_loop: &ActiveEventLoop, event: NetEvent) {
        match event {
            NetEvent::CodeRequired(reply) => {
                self.show_notice(t("viewer.code_title"));
                match &self.notice {
                    Some(notice) => {
                        notice.ask_code();
                        self.code_reply = Some(reply);
                    }
                    None => {
                        warn!("two-factor code asked, but there is no window to ask it in (NUVIRA_CODE)");
                        let _ = reply.send(None);
                    }
                }
            }
            NetEvent::ConfirmationPending => {
                // For the NuviraHub app, which shows it too.
                println!("@pending");
                if let Some(window) = &self.window {
                    window.set_title(&tf("viewer.title_pending", &[("agent", &self.agent)]));
                }
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{name:{}}}", toolbar::js_string(&t("viewer.pending"))));
                }
                if let Some(notice) = &self.notice {
                    notice.set_text(&t("viewer.pending"));
                }
            }
            NetEvent::Connected {
                agent,
                direct,
                width,
                height,
                file_manager,
                secure_attention,
                remote_mac,
                remote_windows,
                privacy,
                terminal,
                tunnel,
                restart,
                restart_users,
                print,
                recording,
                recording_required,
                resolution,
                audio,
                switch_sides,
                lock_on_end,
                wallpaper,
                restart_safe_mode,
                microphone,
            } => {
                self.restart_safe_mode = restart_safe_mode;
                // Off at every connection: the technician turns it on.
                self.microphone = (microphone, false);
                if microphone && self.microphone_at_start {
                    self.send_control(nuvira_proto::v1::control_message::Msg::SetMicrophone(
                        nuvira_proto::v1::SetMicrophone { on: true },
                    ));
                }
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{micAvailable:{microphone},micOn:false}}"));
                }
                if wallpaper && self.hide_wallpaper {
                    let set = nuvira_proto::v1::SetWallpaper { hidden: true };
                    self.send_control(nuvira_proto::v1::control_message::Msg::SetWallpaper(set));
                }
                self.remote_mac = remote_mac;
                self.remote_windows = remote_windows;
                self.lock.available = lock_on_end;
                self.lock.on = false;
                self.lock.forced = false;
                if lock_on_end && self.lock.wanted {
                    let set = nuvira_proto::v1::SetLockOnEnd { on: true };
                    self.send_control(nuvira_proto::v1::control_message::Msg::SetLockOnEnd(set));
                }
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{lockAvailable:{lock_on_end},lockOn:false,lockForced:false}}"));
                }
                self.switch_offered = switch_sides;
                self.switching = false;
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{switchSides:{switch_sides},switching:false}}"));
                }
                if let Some(name) = self.reverse_of.clone() {
                    if let Some(window) = &self.window {
                        window.set_title(&tf("viewer.reverse_title", &[("name", &name)]));
                    }
                    self.notice(&tf("viewer.reverse_notice", &[("name", &name)]), false);
                }
                self.pointer = Pointer { screen: (width, height), ..Pointer::default() };
                if let Some(window) = &self.window {
                    window.set_cursor(winit::window::CursorIcon::Default);
                }
                // Every session starts with the sound on.
                self.sound = audio.then_some(true);
                self.restart_users = restart_users;
                // A new session: the agent put the screen back when the last one ended.
                self.adapt.available = resolution;
                self.adapt.allowed = resolution;
                self.adapt.sent = None;
                self.adapt.display = 0;
                if self.restarting.take().is_some() {
                    info!("connected again after the restart");
                    self.notice(&t("viewer.restart_back"), false);
                }
                if let Some((_, started)) = self.handover_after.as_mut() {
                    started.get_or_insert_with(Instant::now);
                }
                if let Some(test) = self.switch_test.as_mut() {
                    test.since.get_or_insert_with(Instant::now);
                }
                if self.taking_over.take().is_some() {
                    info!("session taken over");
                    self.notice(&t("viewer.took_over"), false);
                }
                if terminal && let Some(link) = self.term_link.clone() {
                    self.terminals.connect(link);
                }
                match (tunnel, self.tunnel_link.clone()) {
                    (true, Some(link)) => self.tunnels.connect(link),
                    _ if self.tunnels.has_any() => warn!("tunnels not available on this agent"),
                    _ => {}
                }
                if self.terminal_only {
                    // For the NuviraHub app, which reads the output: the session started (address book).
                    println!("@connected\t{agent}");
                    if !agent.is_empty() {
                        self.agent = agent;
                    }
                    self.terminals.set_title(&tf("term.title", &[("agent", &self.agent)]));
                    return;
                }
                info!("remote screen {width}x{height}");
                if print {
                    info!("remote printing: what is printed on the NuviraHub printer there arrives here");
                }
                if !agent.is_empty() {
                    self.printing.agent = agent.clone();
                }
                // After a reconnection the recording goes on in a new file.
                self.recording.available = recording;
                self.recording.required = recording_required;
                if recording_required {
                    info!("the server's rules want this session recorded");
                }
                if std::mem::take(&mut self.recording.at_start) || self.recording.on || recording_required {
                    // Nothing is written before the agent says the person there was told.
                    self.recording.switch.stop();
                    self.recording.on = false;
                    match recording {
                        true => {
                            self.recording.asked = true;
                            self.send_control(nuvira_proto::v1::control_message::Msg::SetRecording(
                                nuvira_proto::v1::SetRecording { on: true },
                            ));
                        }
                        false => self.notice(&t("viewer.recording_not_allowed"), true),
                    }
                }
                self.sent_view_size = None;
                self.send_view_size();
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!(
                        "{{fileManager:{file_manager},secureAttention:{secure_attention},privacyAvailable:{privacy},terminal:{terminal},tunnel:{tunnel},restart:{restart},recordingAvailable:{recording},soundAvailable:{audio}}}"
                    ));
                }
                self.update_toolbar_state();
                self.schedule_adapt();

                if std::mem::take(&mut self.restart_at_start) {
                    match restart {
                        true => self.send_control(nuvira_proto::v1::control_message::Msg::Restart(
                            nuvira_proto::v1::Restart { safe_mode: self.restart_safe_mode_at_start },
                        )),
                        false => warn!("restart not available on this agent"),
                    }
                }
                if std::mem::take(&mut self.restart_check_at_start) && restart {
                    self.ask_restart();
                }
                if std::mem::take(&mut self.privacy_at_start) {
                    match privacy {
                        true => self.send_control(nuvira_proto::v1::control_message::Msg::SetPrivacy(
                            nuvira_proto::v1::SetPrivacy { on: true },
                        )),
                        false => warn!("privacy mode not available on this agent"),
                    }
                }
                let (downloads, download_dir, uploads, upload_dir, resume) =
                    std::mem::take(&mut self.cli_transfers);
                self.cli_started = !downloads.is_empty() || !uploads.is_empty();
                for remote in downloads {
                    // The size comes with the data.
                    self.fm.download(remote, 0, download_dir.clone(), resume);
                }
                for local in uploads {
                    self.fm.upload(local, upload_dir.clone(), resume);
                }
                // For the NuviraHub app, which reads the output: the session started (address book).
                println!("@connected\t{agent}");
                for path in std::mem::take(&mut self.pending_files) {
                    self.send_file(path);
                }
                if let Some(secs) = self.screenshot_after.take() {
                    let proxy = self.proxy.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(secs));
                        let _ = proxy.send_event(NetEvent::Toolbar(toolbar::Action::Screenshot));
                    });
                }
                if let Some(toolbar) = &self.toolbar {
                    let name = if agent.is_empty() { &self.agent } else { &agent };
                    let path = if direct { "direct" } else { "relay" };
                    toolbar.update(&format!("{{name:{},path:\"{path}\"}}", toolbar::js_string(name)));
                }
                self.report.agent = Some(agent.clone());
                self.report.resolution = Some((width, height));
                if !agent.is_empty() {
                    self.agent = agent;
                }
                self.update_title();
            }
            NetEvent::FrameReady => {
                let Some(mut frame) = self.slot.take() else { return };
                if self.keep_last_frame {
                    let planes = frame.yuv.with_planes();
                    let last = self.last_frame.get_or_insert_with(Default::default);
                    last.width = planes.width;
                    last.height = planes.height;
                    last.y.clone_from(&planes.y);
                    last.u.clone_from(&planes.u);
                    last.v.clone_from(&planes.v);
                }
                // The image is there: no more waiting message.
                self.notice = None;
                let Some(renderer) = self.renderer.as_mut() else { return };
                let t = Instant::now();
                renderer.upload(&frame.yuv);
                renderer.render();
                let render_us = t.elapsed().as_micros() as u64;
                self.shown = Some(std::mem::take(&mut frame.yuv));
                tracing::trace!("frame shown in {render_us} µs");
                if let Some(s) = &self.session_stats {
                    s.frames_shown.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    s.render.record_us(render_us);
                }
                self.report.render_us += render_us;
                if let Some(offset) = self.clock_offset_us {
                    // Capture time translated to our clock.
                    let captured_here = frame.capture_us as i64 - offset;
                    let latency = nuvira_proto::now_us() as i64 - captured_here;
                    self.stats.latency_us += latency.max(0) as u64;
                    self.report.latency(latency.clamp(0, u32::MAX as i64) as u32);
                    self.feedback_window.1.push((latency.max(0) / 1000) as u32);
                    // From the end of encoding to arrival: the network alone.
                    let transit = frame.received_us as i64 - (frame.encoded_us as i64 - offset);
                    self.feedback_window.2.push((transit.max(0) / 1000) as u32);
                    if let Some(s) = &self.session_stats {
                        s.latency.record_us(latency.max(0) as u64);
                        s.transit.record_us(transit.max(0) as u64);
                    }
                }
                self.send_feedback();
                let r = &mut self.report;
                r.first_frame_ms.get_or_insert(self.started.elapsed().as_millis() as u64);
                if let Some(gap) = r.shown(Instant::now()) {
                    if let Some(s) = &self.session_stats {
                        s.freezes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        s.freeze_ms.fetch_add(gap.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    tracing::debug!(
                        gap_ms = gap.as_millis() as u64,
                        at_ms = self.started.elapsed().as_millis() as u64,
                        "freeze"
                    );
                }
                r.frames += 1;
                r.bytes += frame.bytes as u64;
                r.encode_us += frame.encode_us as u64;
                r.decode_us += frame.decode_us as u64;
                self.stats.frames += 1;
                self.stats.bytes += frame.bytes;
                self.stats.encode_us += frame.encode_us as u64;
                self.stats.decode_us += frame.decode_us as u64;
                if self.stats.since.elapsed().as_secs_f64() >= 1.0 {
                    self.update_title();
                    self.stats = Stats::new();
                }
            }
            NetEvent::Stats(s) => self.session_stats = Some(s),
            NetEvent::Clock { rtt_us, offset_us } => {
                if let Some(s) = &self.session_stats {
                    s.rtt(rtt_us);
                }
                self.send_feedback();
                if let Some(test) = &self.switch_test
                    && let Some(since) = test.since
                {
                    let wait = if self.switching { test.back_after } else { Some(test.after) };
                    if wait.is_some_and(|w| since.elapsed() >= w) && self.switch_offered {
                        let back = self.switching;
                        info!(back, "switch sides (test)");
                        if back {
                            self.switch_test = None;
                        } else if let Some(t) = self.switch_test.as_mut() {
                            t.since = Some(Instant::now());
                            // Once switched, the next step is taking it back (or nothing).
                            t.after = Duration::MAX;
                        }
                        self.handle_toolbar(event_loop, toolbar::Action::SwitchSides);
                    }
                }
                if let Some((wait, Some(started))) = self.handover_after
                    && started.elapsed() >= wait
                    && let Some(id) = self.handover_items().first().and_then(|(a, _)| a.strip_prefix("handover:")?.parse().ok())
                {
                    self.handover_after = None;
                    info!(guest = id, "handing the session over (test)");
                    self.send_control(nuvira_proto::v1::control_message::Msg::HandOver(nuvira_proto::v1::HandOver { guest: id }));
                }
                if let Some(n) = self.input_test.as_mut() {
                    *n += 1;
                    let x = if *n % 2 == 0 { 0.25 } else { 0.75 };
                    self.send(Event::MouseMove(MouseMove { x, y: 0.5 }));
                }
                if let Some((seen, _)) = self.type_keys.as_mut() {
                    *seen += 1;
                    if *seen >= 3
                        && let Some((_, text)) = self.type_keys.take()
                    {
                        if let Some((x, y)) = self.click_at.take() {
                            self.send(Event::MouseMove(MouseMove { x, y }));
                            for down in [true, false] {
                                self.send(Event::MouseButton(MouseButton { button: Button::Left as i32, down }));
                            }
                        }
                        // At a person's pace (40 ms a key): in a burst the input method of
                        // a sign-in screen (ibus) mixed up the order of the characters.
                        if let Some(input) = self.input.clone() {
                            std::thread::spawn(move || {
                                let key = |code: &str, down| {
                                    let event = Event::Key(Key { code: code.into(), down });
                                    let _ = input.send(InputEvent { event: Some(event) });
                                    std::thread::sleep(Duration::from_millis(20));
                                };
                                for c in text.chars() {
                                    if c == '\u{17}' {
                                        std::thread::sleep(Duration::from_secs(1));
                                        continue;
                                    }
                                    let Some((code, modifier)) = test_key_code(c) else { continue };
                                    if let Some(m) = modifier {
                                        key(m, true);
                                    }
                                    key(&code, true);
                                    key(&code, false);
                                    if let Some(m) = modifier {
                                        key(m, false);
                                    }
                                }
                                info!("test text typed");
                            });
                        }
                    }
                }
                self.rtt_us = rtt_us;
                self.report.rtt(rtt_us.min(u32::MAX as u64) as u32);
                // The measurement with the lowest RTT is the least disturbed by network queues.
                if rtt_us <= self.best_rtt_us {
                    self.best_rtt_us = rtt_us;
                    self.clock_offset_us = Some(offset_us);
                }
            }
            NetEvent::TestFinished => {
                info!("test duration over");
                self.report.completed = true;
                self.exit_reason = Some("test duration over".into());
                self.release_keys();
                event_loop.exit();
            }
            NetEvent::Displays { displays, current, all_available } => {
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&toolbar::displays_state(&displays, current, all_available));
                }
                // Another screen: the agent put the previous one back; this one is adapted now.
                if current != self.adapt.display {
                    self.adapt.display = current;
                    self.adapt.sent = None;
                    self.schedule_adapt();
                }
            }
            NetEvent::Fs(answer) => self.fm.response(answer),
            NetEvent::Transfer { id, done, total } => self.fm.progress(id, done, total),
            NetEvent::TransferDone { id, ok, message } => {
                self.fm.finished(id, ok, message);
                // --download/--upload: done when nothing is running any more.
                if self.cli_started && self.fm.idle() {
                    info!("transfers over");
                    self.exit_reason = Some("transfers over".into());
                    event_loop.exit();
                }
            }
            NetEvent::FileManager(body) => self.fm.message(&body),
            NetEvent::FilesPicked(paths) => {
                for path in paths {
                    self.send_file(path);
                }
            }
            NetEvent::FileProgress { name, sent, total } => {
                let percent = (sent * 100).checked_div(total).unwrap_or(100);
                let text = if sent < total {
                    tf("viewer.file_sending", &[("name", &name), ("percent", &percent.to_string())])
                } else {
                    tf("viewer.file_sent_waiting", &[("name", &name)])
                };
                self.notice(&text, true);
            }
            NetEvent::FileDone { name, ok, message } => {
                let text = if ok {
                    tf("viewer.file_arrived", &[("name", &name), ("folder", &nuvira_i18n::message(&message))])
                } else {
                    tf("viewer.file_failed", &[("name", &name), ("reason", &nuvira_i18n::message(&message))])
                };
                info!("{text}");
                self.notice(&text, false);
                if let Some(chat) = &self.chat {
                    chat.notice(&text);
                }
            }
            NetEvent::TerminalPage { window, body } => self.terminals.message(window, &body),
            NetEvent::TunnelPage(body) => self.tunnels.message(&body),
            NetEvent::PrintJob { name, data } => {
                if let Some(text) = self.printing.arrived(name, data, event_loop, &self.proxy) {
                    self.notice(&text, false);
                }
            }
            NetEvent::PrintPage(body) => {
                if let Some(text) = self.printing.message(&body, &self.proxy) {
                    self.notice(&text, false);
                }
            }
            NetEvent::Printed { name, printer, error } => {
                let text = match error.is_empty() {
                    true => tf("viewer.print_done", &[("name", &name), ("printer", &printer)]),
                    false => tf("viewer.print_failed", &[("name", &name), ("reason", &error)]),
                };
                self.notice(&text, false);
            }
            NetEvent::TunnelListening { id, local_port, error } => self.tunnels.listening(id, local_port, error),
            NetEvent::TunnelConnection { id, error } => self.tunnels.connection_error(id, error),
            NetEvent::TerminalOutput { id, data } => {
                if let Some(log) = self.terminal_log.as_mut() {
                    use std::io::Write;
                    let _ = log.write_all(&data);
                }
                self.terminals.output(id, &data);
                // `--terminal-type`: after the first output (the prompt) of the first terminal.
                if id == 1
                    && let (Some(text), Some(link)) = (self.terminal_type.take(), &self.term_link)
                {
                    let _ = link.send(net::TermCmd::Input { id, data: text });
                }
            }
            NetEvent::TerminalExit { id, code, error } => {
                self.terminals.exited(id, code, &error);
                if let Some(log) = self.terminal_log.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(log, "\n@exit {code} {error}");
                }
            }
            NetEvent::Recording { on, error } => {
                info!(on, error, "recording");
                if on && !self.recording.on {
                    let stem = recorder::file_stem(&self.printing.agent);
                    self.recording.switch.start(self.recording.dir.clone(), stem);
                    let dir = self.recording.dir.display().to_string();
                    self.notice(&tf("viewer.recording_on", &[("dir", &dir)]), false);
                } else if !on && self.recording.on {
                    self.recording.switch.stop();
                }
                if !error.is_empty() {
                    let reason = nuvira_i18n::message(&error);
                    self.notice(&tf("viewer.recording_failed", &[("reason", &reason)]), true);
                }
                self.recording.on = on;
                self.recording.asked = false;
                self.send_view_size();
                self.update_toolbar_state();
            }
            NetEvent::Microphone { on, error } => {
                info!(on, error, "microphone at the remote computer");
                self.microphone.1 = on;
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{micOn:{on}}}"));
                }
                if !error.is_empty() {
                    self.notice(&nuvira_i18n::message(&error), true);
                }
            }
            NetEvent::IdleWarning(seconds) => {
                self.idle_warned = seconds > 0;
                if seconds > 0 {
                    info!(seconds, "the session ends soon without activity");
                    self.notice(&tf("viewer.idle_warning", &[("seconds", &seconds.to_string())]), true);
                    if let Some(window) = &self.window {
                        window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                    }
                } else {
                    self.notice(&t("viewer.idle_over"), false);
                }
            }
            NetEvent::LockOnEnd { on, forced } => {
                info!(on, forced, "lock of the remote computer at the end");
                self.lock.on = on;
                self.lock.forced = forced;
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{lockOn:{on},lockForced:{forced}}}"));
                }
            }
            NetEvent::ScreenshotSaved(result) => {
                self.saving_screenshot = false;
                match result {
                    Ok(path) => {
                        info!("screenshot saved: {}", path.display());
                        let dir = path.parent().unwrap_or(&path).display().to_string();
                        self.notice(&tf("viewer.screenshot_saved", &[("dir", &dir)]), false);
                    }
                    Err(e) => {
                        error!("screenshot: {e}");
                        self.notice(&tf("viewer.screenshot_failed", &[("reason", &e)]), true);
                    }
                }
            }
            NetEvent::RecordingSaved { files, error } => {
                match error {
                    Some(e) => {
                        error!("recording: {e}");
                        self.notice(&tf("viewer.recording_failed", &[("reason", &e)]), true);
                        // The person at the remote computer is no longer told it is recorded.
                        if self.recording.on {
                            self.recording.on = false;
                            self.send_view_size();
                            self.send_control(nuvira_proto::v1::control_message::Msg::SetRecording(
                                nuvira_proto::v1::SetRecording { on: false },
                            ));
                            self.update_toolbar_state();
                        }
                    }
                    None if !files.is_empty() => {
                        let dir = self.recording.dir.display().to_string();
                        self.notice(&tf("viewer.recording_saved", &[("dir", &dir)]), false);
                    }
                    None => {}
                }
            }
            NetEvent::Permissions { input, files, terminal, tunnel, resolution, audio, switched_off } => {
                // Switched off there: the agent put the screen back; on again: asked again.
                if resolution != self.adapt.allowed && self.adapt.available {
                    self.adapt.allowed = resolution;
                    if resolution {
                        self.adapt.sent = None;
                        self.schedule_adapt();
                    } else if self.adapt.sent.is_some() {
                        self.adapt.sent = Some((0, 0));
                    }
                }
                if !input && !self.input_blocked {
                    // Whatever is pressed is released before sending stops.
                    self.release_keys();
                }
                self.input_blocked = !input;
                let names = |list: Vec<&String>| {
                    list.iter().map(|n| t(&format!("perm.{n}"))).collect::<Vec<_>>().join(", ")
                };
                let off: Vec<&String> = switched_off.iter().filter(|n| !self.switched_off.contains(n)).collect();
                let on: Vec<&String> = self.switched_off.iter().filter(|n| !switched_off.contains(n)).collect();
                // What went off stays on screen; what came back only for a while.
                let sticky = !off.is_empty();
                let mut said = Vec::new();
                if !off.is_empty() {
                    let what = names(off);
                    info!("switched off at the remote computer: {what}");
                    said.push(tf("viewer.switched_off", &[("what", &what)]));
                }
                if !on.is_empty() {
                    let what = names(on);
                    info!("switched on again at the remote computer: {what}");
                    said.push(tf("viewer.switched_on", &[("what", &what)]));
                }
                if !said.is_empty() {
                    self.notice(&said.join(" · "), sticky);
                }
                self.switched_off = switched_off;
                if let Some(toolbar) = &self.toolbar {
                    // The sound button only while the person there lets the sound through.
                    let sound = self.sound.is_some() && audio;
                    toolbar.update(&format!(
                        "{{fileManager:{files},terminal:{terminal},tunnel:{tunnel},soundAvailable:{sound}}}"
                    ));
                }
                self.update_toolbar_state();
            }
            NetEvent::ScreenLocked { locked, input } => {
                info!(locked, input, "remote screen lock state");
                // Kept on screen while locked: on a Mac the lock screen is seen (or a dark
                // placeholder when there is no picture); the password is typed there.
                let text = match (locked, input) {
                    (true, true) => "viewer.screen_locked",
                    (true, false) => "viewer.screen_locked_no_input",
                    (false, _) => "viewer.screen_unlocked",
                };
                self.notice(&t(text), locked);
            }
            NetEvent::AdaptSettled(generation) => {
                if generation == self.adapt.generation {
                    self.send_adapt();
                }
            }
            NetEvent::Resolution { width, height, adapted, error } => {
                info!(width, height, adapted, error, "remote resolution");
                let clicked = std::mem::take(&mut self.adapt.asked_by_click);
                let waiting = std::mem::take(&mut self.adapt.waiting);
                if !error.is_empty() {
                    // Switched off there: the permissions already said so.
                    if waiting && error != "@agent.resolution_switched_off" || clicked {
                        let reason = nuvira_i18n::message(&error);
                        self.notice(&tf("viewer.resolution_failed", &[("reason", &reason)]), false);
                    }
                } else if self.adapt.sent == Some((0, 0)) {
                    if clicked {
                        self.notice(&t("viewer.resolution_back"), false);
                    }
                } else if waiting && (adapted || clicked) {
                    // Also when the screen already had the best size for the window.
                    self.notice(&tf("viewer.resolution_now", &[("size", &format!("{width}x{height}"))]), false);
                }
            }
            NetEvent::ScreenSize { width, height } => {
                self.pointer.screen = (width, height);
                self.update_pointer();
            }
            NetEvent::CursorShape { serial, width, height, hot_x, hot_y, rgba } => {
                // As this computer's pointer while it controls (not when only watching).
                let os = match winit::window::CustomCursor::from_rgba(
                    rgba.as_ref().clone(),
                    width as u16,
                    height as u16,
                    hot_x as u16,
                    hot_y as u16,
                ) {
                    Ok(source) => Some(event_loop.create_custom_cursor(source)),
                    Err(e) => {
                        debug!("remote pointer not usable here: {e}");
                        None
                    }
                };
                if self.pointer.shape.is_none() {
                    info!(width, height, "remote pointer drawn here");
                }
                let shape = render::CursorShape { serial, width, height, hot_x, hot_y, rgba };
                let kept = &mut self.pointer.kept;
                kept.retain(|(s, _)| s.serial != serial);
                kept.push_back((shape.clone(), os.clone()));
                if kept.len() > 32 {
                    kept.pop_front();
                }
                self.pointer.os = os;
                self.pointer.shape = Some(shape);
                self.update_pointer();
            }
            NetEvent::CursorPosition { x, y, visible, serial } => {
                // A shape sent before, named by its serial.
                if self.pointer.shape.as_ref().is_none_or(|s| s.serial != serial)
                    && let Some((shape, os)) = self.pointer.kept.iter().find(|(s, _)| s.serial == serial)
                {
                    debug!(serial, "pointer shape kept");
                    self.pointer.shape = Some(shape.clone());
                    self.pointer.os = os.clone();
                }
                self.pointer.position = Some((x, y, visible, serial));
                self.update_pointer();
            }
            NetEvent::Privacy { on, error } => {
                info!(on, error, "privacy mode");
                let changed = on != self.privacy;
                self.privacy = on;
                if !error.is_empty() {
                    let reason = nuvira_i18n::message(&error);
                    self.notice(&tf("viewer.privacy_failed", &[("reason", &reason)]), false);
                } else if changed {
                    self.notice(&t(if on { "viewer.privacy_on" } else { "viewer.privacy_off" }), false);
                }
                self.update_toolbar_state();
            }
            NetEvent::SwitchSides { on: true, .. } => {
                info!("switch sides: the other computer shows this screen");
                self.switching = true;
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update("{switching:true}");
                }
                self.notice(&tf("viewer.switching", &[("agent", &self.agent)]), true);
                // Out of the way: the other computer would see this window showing
                // its own screen; the taskbar brings it back, with the button.
                if let Some(window) = &self.window {
                    window.set_minimized(true);
                }
            }
            NetEvent::SwitchSides { on: false, error } => {
                if self.switching || !error.is_empty() {
                    let text = match error.as_str() {
                        "" => tf("viewer.switch_closed", &[("agent", &self.agent)]),
                        e => tf("viewer.switch_failed", &[("reason", e)]),
                    };
                    self.switch_ended(&text);
                }
            }
            NetEvent::HandedOver(h) if h.token.is_empty() => {
                info!(to = h.to, "session handed over");
                self.handed_to = Some(h.to);
            }
            NetEvent::HandedOver(h) => {
                info!(from = h.from, "the session was handed over to this viewer");
                // Tests of the agent's checks (another device, after the minute): the
                // token is written to the file and not used; the window closes.
                if let Some(path) = std::env::var_os("NUVIRA_TEST_HANDOVER_TOKEN") {
                    let _ = std::fs::write(&path, &h.token);
                    info!("handover token kept for a test, not used");
                    event_loop.exit();
                    return;
                }
                if let Some((target, _)) = self.connect.take() {
                    self.connect = Some((target, Some(format!("{HANDOVER_TOKEN}{}", h.token))));
                }
                self.taking_over = Some(Instant::now());
                self.notice(&tf("viewer.taking_over", &[("name", &h.from)]), true);
            }
            NetEvent::Guests(guests) => {
                let names = guests.names;
                self.guest_ids = guests.ids;
                self.guest_can_take_over = guests.can_take_over;
                // Agents that do not tell: nobody can take it over.
                self.guest_can_take_over.resize(names.len(), false);
                self.guest_ids.resize(names.len(), 0);
                let handover = self.guest_can_take_over.iter().any(|c| *c);
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&format!("{{handover:{handover}}}"));
                }
                // Another technician joined or left: this one is told. Counted by
                // name, since two can have the same (two windows of one computer).
                let count = |list: &[String], name: &str| list.iter().filter(|n| *n == name).count();
                let mut seen = HashSet::new();
                for name in names.iter().chain(&self.guests) {
                    if !seen.insert(name.clone()) {
                        continue;
                    }
                    let (now, before) = (count(&names, name), count(&self.guests, name));
                    for _ in before..now {
                        info!(name, "joined the session");
                        self.notice(&tf("viewer.guest_joined", &[("name", name)]), false);
                    }
                    for _ in now..before {
                        info!(name, "left the session");
                        self.notice(&tf("viewer.guest_left", &[("name", name)]), false);
                    }
                }
                self.guests = names;
            }
            NetEvent::Chat(text) => {
                info!("chat message received");
                if !self.chat.as_ref().is_some_and(|c| c.visible()) {
                    self.show_chat(true);
                }
                if let Some(chat) = &self.chat {
                    chat.add(false, &text);
                }
            }
            NetEvent::Toolbar(action) => {
                // A click on the bar is the person there too.
                if self.idle_warned {
                    self.still_here();
                }
                self.handle_toolbar(event_loop, action)
            }
            NetEvent::RestartUsers { others } => {
                info!(?others, "other people signed in on the remote computer");
                if std::mem::take(&mut self.restart_check_pending) {
                    self.confirm_restart(&others);
                }
            }
            NetEvent::Restart { ok, error, reconnect } => {
                info!(ok, error, reconnect, "restart");
                if !ok {
                    let reason = nuvira_i18n::message(&error);
                    self.notice(&tf("viewer.restart_failed", &[("reason", &reason)]), false);
                } else if reconnect {
                    self.restarting = Some(Instant::now());
                    self.notice(&t("viewer.restarting"), true);
                } else {
                    self.restarted = true;
                }
            }
            NetEvent::Reconnect => {
                if (self.restarting.is_some() || self.taking_over.is_some()) && self.input.is_none() {
                    self.start_session();
                }
            }
            NetEvent::Disconnected { reason, .. } if self.handed_to.is_some() => {
                let to = self.handed_to.take().unwrap_or_default();
                info!("disconnected after handing the session over: {reason}");
                self.exit_reason = Some(format!("session handed over to {to}"));
                self.closed_message = Some(tf("viewer.handed_over", &[("name", &to)]));
                event_loop.exit();
            }
            // Handed over to this viewer: the session ends for everyone, and it
            // connects again with the token (the agent may need a moment).
            NetEvent::Disconnected { reason, .. } if self.taking_over.is_some_and(|t| t.elapsed() < TAKEOVER_WAIT) => {
                info!("taking the session over ({reason}): connecting again");
                self.drop_session();
                self.reconnect_after(Duration::from_millis(if self.input.is_none() { 800 } else { 300 }));
            }
            NetEvent::Disconnected { reason, retry }
                if retry && self.restarting.is_some_and(|since| since.elapsed() < RESTART_WAIT) =>
            {
                // The first time the computer has only just started going down.
                let first = self.input.is_some() && self.notice.is_none();
                self.wait_for_restart(&reason, if first { RESTART_FIRST_TRY } else { RESTART_RETRY });
            }
            NetEvent::Disconnected { reason, .. } if self.restarting.is_some() || self.restarted => {
                info!("disconnected after the restart: {reason}");
                self.exit_reason = Some(format!("disconnected after the restart: {reason}"));
                let message = match self.restarting {
                    Some(_) => tf("viewer.restart_not_back", &[("agent", &self.agent), ("reason", &reason)]),
                    None => tf("viewer.restarted", &[("agent", &self.agent)]),
                };
                self.close_saying(event_loop, message, false);
            }
            NetEvent::Disconnected { reason, .. } if self.terminal_only && !self.terminals.is_empty() => {
                // The windows stay open with the reason; closing them exits.
                info!("disconnected: {reason}");
                self.exit_reason = Some(format!("disconnected: {reason}"));
                self.terminals.status(&closed_text(&reason));
            }
            NetEvent::Disconnected { reason, .. } => {
                info!("disconnected: {reason}");
                self.exit_reason = Some(format!("disconnected: {reason}"));
                let by_the_computer = reason == t("agent.ended_at_computer");
                self.close_saying(event_loop, closed_text(&reason), by_the_computer);
            }
            NetEvent::ClosedDialogAnswered => event_loop.exit(),
        }
    }
}
