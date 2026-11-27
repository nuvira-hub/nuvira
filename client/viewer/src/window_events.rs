//! What the viewer does with the events of its window: mouse and keyboard, dropped
//! files, resizing, redrawing, focus and closing.

use std::time::Instant;

use nuvira_proto::v1::{Button, Key, MouseButton, MouseMove, MouseWheel, SecureAttention, input_event::Event};
use tracing::info;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::PhysicalKey;
use winit::window::WindowId;

use crate::App;
use crate::keys::{altgr_fix_needed, w3c_code};

impl App {
    pub(super) fn window_input(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // The person is there: the agent's idle warning is answered.
        if self.idle_warned
            && matches!(
                event,
                WindowEvent::CursorMoved { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::KeyboardInput { .. }
            )
        {
            self.still_here();
        }
        if self.terminals.owns(id) {
            self.terminals.window_event(id, &event);
            if self.terminal_only && self.terminals.is_empty() {
                self.exit_reason.get_or_insert_with(|| "session closed by the user".into());
                event_loop.exit();
            }
            return;
        }
        if self.fm.owns(id) {
            self.fm.window_event(&event);
            return;
        }
        if self.tunnels.owns(id) {
            self.tunnels.window_event(&event);
            return;
        }
        if self.printing.owns(id) {
            self.printing.window_event(&event);
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.release_keys();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                    r.render();
                }
                if let Some(toolbar) = &self.toolbar {
                    toolbar.layout();
                }
                if let Some(chat) = &self.chat {
                    chat.layout();
                }
                if let Some(menu) = self.menu.as_mut() {
                    menu.hide();
                }
                if let Some(notice) = &self.notice {
                    notice.layout();
                }
                self.update_toolbar_state();
                self.send_view_size();
                self.schedule_adapt();
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    r.render();
                }
            }
            WindowEvent::Focused(false) => self.release_keys(),
            WindowEvent::DroppedFile(path) => self.send_file(path),
            WindowEvent::CursorMoved { position, .. } => {
                // Real size: the image follows the mouse, so that moving it from one edge
                // of the window to the other crosses the whole remote screen.
                if self.view.actual
                    && let (Some(r), Some(window)) = (self.renderer.as_mut(), &self.window)
                {
                    let size = window.inner_size();
                    let pan = (
                        (position.x / size.width.max(1) as f64) as f32,
                        (position.y / size.height.max(1) as f64) as f32,
                    );
                    let before = r.viewport();
                    self.view.pan = pan;
                    r.set_view(self.view);
                    let after = r.viewport();
                    if (before.x, before.y) != (after.x, after.y) {
                        window.request_redraw();
                    }
                }
                let Some(r) = &self.renderer else { return };
                if let Some((x, y)) = r.viewport().normalize(position.x, position.y) {
                    self.send(Event::MouseMove(MouseMove { x, y }));
                    if !self.view_only {
                        self.pointer.local = Some((x, y));
                        self.update_pointer();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Click on the image while typing in the chat: the keyboard goes back to the session.
                if state == ElementState::Pressed
                    && let Some(chat) = &self.chat
                {
                    chat.focus_session();
                }
                // ...and an open menu closes.
                if state == ElementState::Pressed
                    && let Some(menu) = self.menu.as_mut()
                {
                    menu.hide();
                }
                let button = match button {
                    winit::event::MouseButton::Left => Button::Left,
                    winit::event::MouseButton::Right => Button::Right,
                    winit::event::MouseButton::Middle => Button::Middle,
                    winit::event::MouseButton::Back => Button::Back,
                    winit::event::MouseButton::Forward => Button::Forward,
                    winit::event::MouseButton::Other(_) => return,
                };
                self.send(Event::MouseButton(MouseButton {
                    button: button as i32,
                    down: state == ElementState::Pressed,
                }));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Windows units: 120 per notch. Touchpads give pixels: ~40 px per notch.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 * 3.0, p.y as f32 * 3.0),
                };
                self.send(Event::MouseWheel(MouseWheel { dx: dx as i32, dy: dy as i32 }));
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let code = w3c_code(code, self.remote_mac);
                let down = event.state == ElementState::Pressed;
                // Ctrl+Alt+End (as in Remote Desktop) = Ctrl+Alt+Del on the controlled
                // device: the real Ctrl+Alt+Del is caught by the local system.
                if code == "End" && self.ctrl_alt_held() {
                    if down && !event.repeat {
                        info!("Ctrl+Alt+End: sending Ctrl+Alt+Del");
                        self.send(Event::SecureAttention(SecureAttention {}));
                    }
                    return;
                }
                let keys = if altgr_fix_needed(self.remote_windows) {
                    self.altgr.filter(&code, down, Instant::now())
                } else {
                    vec![(code, down)]
                };
                for (code, down) in keys {
                    if down {
                        self.pressed_keys.insert(code.clone());
                    } else {
                        self.pressed_keys.remove(&code);
                    }
                    self.send(Event::Key(Key { code, down }));
                }
            }
            _ => {}
        }
    }
}
