//! What the toolbar buttons do (`toolbar::Action`).

use nuvira_i18n::{t, tf};
use nuvira_proto::v1::{SecureAttention, input_event::Event};
use tracing::{error, info};
use winit::event_loop::ActiveEventLoop;

use crate::net::NetEvent;
use crate::{App, prefs, toolbar};

impl App {
    pub(super) fn handle_toolbar(&mut self, event_loop: &ActiveEventLoop, action: toolbar::Action) {
        use nuvira_proto::v1::control_message::Msg;
        use toolbar::Action;
        match action {
            Action::SecureAttention => self.send(Event::SecureAttention(SecureAttention {})),
            Action::ViewOnly => {
                if !self.view_only {
                    // Before stopping sending, whatever is pressed is released.
                    self.release_keys();
                }
                self.view_only = !self.view_only;
                info!("view only: {}", self.view_only);
            }
            Action::ActualSize => {
                self.view.actual = !self.view.actual;
                info!("real size: {}", self.view.actual);
                self.send_view_size();
                if let Some(r) = self.renderer.as_mut() {
                    r.set_view(self.view);
                    r.render();
                }
                self.update_toolbar_state();
            }
            Action::AdaptResolution => {
                self.adapt.wanted = !self.adapt.wanted;
                info!("remote resolution adapted to the window: {}", self.adapt.wanted);
                prefs::save(&prefs::Prefs { adapt_resolution: self.adapt.wanted });
                self.adapt.asked_by_click = true;
                self.send_adapt();
                self.update_toolbar_state();
            }
            Action::Fullscreen => {
                if let Some(window) = &self.window {
                    let full = window.fullscreen().is_none();
                    window.set_fullscreen(full.then_some(winit::window::Fullscreen::Borderless(None)));
                }
            }
            Action::Tunnels => {
                if let Err(e) = self.tunnels.open(event_loop, self.proxy.clone()) {
                    error!("tunnels not available: {e:#}");
                }
                return;
            }
            Action::Terminal => {
                let title = tf("term.title", &[("agent", &self.agent)]);
                if let Err(e) = self.terminals.open(event_loop, self.proxy.clone(), &title, "") {
                    error!("terminal not available: {e:#}");
                }
                // The keyboard stays with the new window.
                return;
            }
            Action::Restart => self.ask_restart(),
            Action::RestartConfirmed => {
                info!("restart of the remote computer requested");
                self.send_control(Msg::Restart(nuvira_proto::v1::Restart { safe_mode: false }));
            }
            Action::RestartSafeModeConfirmed => {
                info!("restart of the remote computer into safe mode requested");
                self.send_control(Msg::Restart(nuvira_proto::v1::Restart { safe_mode: true }));
            }
            Action::HandOver(id) => self.confirm_handover(id),
            Action::SwitchSides if self.switch_offered => {
                let on = !self.switching;
                info!(on, "switch sides asked");
                self.send_control(Msg::SwitchSides(nuvira_proto::v1::SwitchSides { on, ..Default::default() }));
                if !on {
                    self.switch_ended(&t("viewer.switch_back"));
                }
            }
            Action::SwitchSides => {}
            Action::HandOverConfirmed(id) => {
                info!(guest = id, "handing the session over");
                self.send_control(Msg::HandOver(nuvira_proto::v1::HandOver { guest: id }));
            }
            // Required by the server's rules: it cannot be stopped (the button is disabled).
            Action::Record if self.recording.required => {}
            Action::Record => {
                info!("recording requested: {}", !self.recording.on);
                self.recording.asked = !self.recording.on;
                self.send_view_size();
                self.send_control(Msg::SetRecording(nuvira_proto::v1::SetRecording { on: !self.recording.on }));
            }
            Action::Screenshot => self.screenshot(),
            Action::Microphone if self.microphone.0 => {
                let on = !self.microphone.1;
                info!(on, "microphone asked");
                self.send_control(Msg::SetMicrophone(nuvira_proto::v1::SetMicrophone { on }));
            }
            Action::Microphone => {}
            Action::Code(code) => {
                if let Some(reply) = self.code_reply.take() {
                    info!("two-factor code typed");
                    let _ = reply.send(Some(code));
                    self.show_notice(t("viewer.connecting_code"));
                }
            }
            Action::CodeCancel => {
                if let Some(reply) = self.code_reply.take() {
                    info!("two-factor code not given");
                    let _ = reply.send(None);
                }
            }
            Action::LockOnEnd if self.lock.available && !self.lock.forced => {
                self.lock.wanted = !self.lock.on;
                info!(on = self.lock.wanted, "lock of the remote computer at the end asked");
                self.send_control(Msg::SetLockOnEnd(nuvira_proto::v1::SetLockOnEnd { on: self.lock.wanted }));
            }
            Action::LockOnEnd => {}
            Action::Sound => {
                if let Some(on) = self.sound {
                    info!(on = !on, "sound of the remote computer");
                    self.sound = Some(!on);
                    self.send_control(Msg::SetAudio(nuvira_proto::v1::SetAudio { on: !on }));
                    self.update_toolbar_state();
                }
            }
            Action::Privacy => {
                info!("privacy mode requested: {}", !self.privacy);
                self.send_control(Msg::SetPrivacy(nuvira_proto::v1::SetPrivacy { on: !self.privacy }));
            }
            Action::Display(index) => {
                self.send_control(Msg::SelectDisplay(nuvira_proto::v1::SelectDisplay { index }));
            }
            Action::Menu(kind, x) if kind == "handover" => {
                let items = self.handover_items();
                if let Some(menu) = self.menu.as_mut() {
                    let hint = t("tb.handover_hint");
                    let list: Vec<toolbar::MenuItem> = items
                        .iter()
                        .map(|(action, name)| toolbar::MenuItem { action, label: name, hint: &hint, checked: false })
                        .collect();
                    if menu.is_open() || list.is_empty() {
                        menu.hide();
                    } else {
                        menu.show(f64::from(x), &list);
                        return;
                    }
                }
            }
            Action::Menu(kind, x) => {
                if kind == "quality"
                    && let Some(menu) = self.menu.as_mut()
                {
                    let current = toolbar::quality_name(self.quality);
                    let items: Vec<toolbar::MenuItem> = [
                        ("balanced", "tb.quality_balanced", "tb.quality_balanced_hint"),
                        ("sharp", "tb.quality_sharp", "tb.quality_sharp_hint"),
                        ("fast", "tb.quality_fast", "tb.quality_fast_hint"),
                    ]
                    .iter()
                    .map(|&(name, label, hint)| toolbar::MenuItem {
                        action: match name {
                            "sharp" => "quality:sharp",
                            "fast" => "quality:fast",
                            _ => "quality:balanced",
                        },
                        label,
                        hint,
                        checked: name == current,
                    })
                    .collect();
                    // A second click on the button closes it.
                    if menu.is_open() {
                        menu.hide();
                    } else {
                        menu.show(f64::from(x), &items);
                        // The open menu keeps the keyboard.
                        return;
                    }
                }
            }
            Action::MenuClose => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.hide();
                }
            }
            Action::Quality(quality) => {
                self.quality = quality;
                info!("quality {quality:?}");
                self.send_control(Msg::SetQuality(nuvira_proto::v1::SetQuality { quality: quality as i32 }));
            }
            Action::Chat => {
                let open = !self.chat.as_ref().is_some_and(|c| c.visible());
                self.show_chat(open);
                // The open panel keeps the keyboard for typing.
                if open {
                    return;
                }
            }
            Action::ChatSend(text) => {
                if let Some(chat) = &self.chat {
                    chat.add(true, &text);
                }
                self.send_control(Msg::Chat(nuvira_proto::v1::Chat { text }));
                return;
            }
            Action::FileManager => {
                if let Err(e) = self.fm.open(event_loop, self.proxy.clone(), self.window.as_deref()) {
                    error!("file manager not available: {e:#}");
                }
                // The keyboard stays with the file manager: giving it back to the session
                // brought the session window in front of the new one.
                return;
            }
            Action::SendFile => {
                // The file picker blocks its own thread, not the session.
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let picked = rfd::FileDialog::new().set_title(t("viewer.file_dialog")).pick_files();
                    if let Some(paths) = picked {
                        let _ = proxy.send_event(NetEvent::FilesPicked(paths));
                    }
                });
            }
            Action::Collapse | Action::Expand => {
                if let Some(toolbar) = self.toolbar.as_mut() {
                    toolbar.set_collapsed(action == Action::Collapse);
                }
            }
            Action::Close => {
                self.release_keys();
                self.exit_reason = Some("session closed by the user".into());
                event_loop.exit();
                return;
            }
        }
        self.update_toolbar_state();
        if let Some(toolbar) = &self.toolbar {
            toolbar.focus_session();
        }
    }
}
