//! Remote terminal windows: each one shows a shell of the remote computer with
//! xterm.js (`terminal.html`, files in `xterm/`) and has a stream of its own to the
//! agent (`net::TermCmd`). A session can be a terminal only (`--terminal`: the first
//! window opens at once and closing the last one ends the session) or a desktop
//! session with terminals opened from the toolbar.
//!
//! The page sends JSON messages (`{"op": "size", "cols": 80, "rows": 24}`,
//! `{"op": "input", "data": "..."}`), which reach the event loop as
//! [`NetEvent::TerminalPage`]; the output goes back base64-encoded with `term.output(...)`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::net::{NetEvent, TermCmd};

const HTML: &str = include_str!("terminal.html");
const XTERM_JS: &str = include_str!("xterm/xterm.js");
const XTERM_CSS: &str = include_str!("xterm/xterm.css");
const FIT_JS: &str = include_str!("xterm/addon-fit.js");

struct TermWindow {
    id: u32,
    window: Arc<Window>,
    webview: WebView,
    _context: WebContext,
    /// Size in characters, once the page measured it.
    size: Option<(u16, u16)>,
    /// The stream is open (or has been, and the shell ended).
    started: bool,
}

#[derive(Default)]
pub struct Terminals {
    windows: HashMap<WindowId, TermWindow>,
    /// Towards the network, once the session is connected and the agent has terminals.
    link: Option<UnboundedSender<TermCmd>>,
    next_id: u32,
}

impl Terminals {
    pub fn owns(&self, id: WindowId) -> bool {
        self.windows.contains_key(&id)
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// The session is connected: terminals open on the agent from now on.
    pub fn connect(&mut self, link: UnboundedSender<TermCmd>) {
        self.link = Some(link);
        let ready: Vec<WindowId> = self.windows.keys().copied().collect();
        for window in ready {
            self.start(window);
        }
    }

    /// A new terminal window; `status` is shown until the shell starts.
    pub fn open(
        &mut self,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<NetEvent>,
        title: &str,
        status: &str,
    ) -> Result<()> {
        self.next_id += 1;
        let id = self.next_id;
        let attrs = crate::window_attributes()
            .with_title(title)
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 560.0));
        let window = Arc::new(event_loop.create_window(attrs).context("terminal window")?);
        let data_dir =
            std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("nuvira").join("WebView2"));
        let mut context = WebContext::new(data_dir);
        let html = HTML
            .replace("/*XTERM_CSS*/", XTERM_CSS)
            .replace("/*XTERM_JS*/", XTERM_JS)
            .replace("/*FIT_JS*/", FIT_JS)
            .replace("\"/*STATUS*/\"", &crate::toolbar::js_string(status));
        let window_id = window.id();
        let webview = crate::toolbar::retry_in_use(|| {
            let proxy = proxy.clone();
            WebViewBuilder::new_with_web_context(&mut context)
                .with_html(&html)
                .with_background_color((0x16, 0x1a, 0x1f, 0xff))
                .with_bounds(full(&window))
                .with_initialization_script(crate::toolbar::init_script())
                .with_ipc_handler(move |request| {
                    let body = request.body().clone();
                    let _ = proxy.send_event(NetEvent::TerminalPage { window: window_id, body });
                })
                .build_as_child(&window)
        })
        .context("terminal WebView")?;
        let _ = webview.focus();
        window.focus_window();
        self.windows.insert(
            window_id,
            TermWindow { id, window, webview, _context: context, size: None, started: false },
        );
        Ok(())
    }

    /// Opens the terminal on the agent when both the page and the session are ready.
    fn start(&mut self, window: WindowId) {
        let (Some(link), Some(term)) = (&self.link, self.windows.get_mut(&window)) else { return };
        let Some((cols, rows)) = term.size else { return };
        if term.started {
            return;
        }
        term.started = true;
        let _ = link.send(TermCmd::Open { id: term.id, cols, rows });
    }

    /// Message from a page.
    pub fn message(&mut self, window: WindowId, body: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(body) else { return };
        let Some(term) = self.windows.get_mut(&window) else { return };
        let dim = |key: &str| msg[key].as_u64().unwrap_or(0).clamp(1, 1000) as u16;
        match msg["op"].as_str() {
            Some("size") => {
                let size = (dim("cols"), dim("rows"));
                let first = term.size.is_none();
                term.size = Some(size);
                if first {
                    self.start(window);
                } else if term.started
                    && let Some(link) = &self.link
                {
                    let _ = link.send(TermCmd::Resize { id: term.id, cols: size.0, rows: size.1 });
                }
            }
            Some("input") => {
                if let (true, Some(link), Some(data)) = (term.started, &self.link, msg["data"].as_str()) {
                    let _ = link.send(TermCmd::Input { id: term.id, data: data.as_bytes().to_vec() });
                }
            }
            _ => {}
        }
    }

    pub fn output(&self, id: u32, data: &[u8]) {
        if let Some(term) = self.by_id(id) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(data);
            let _ = term.webview.evaluate_script(&format!("term.output(\"{encoded}\")"));
        }
    }

    /// The shell ended (or did not start): the window stays open with the reason.
    pub fn exited(&self, id: u32, code: i32, error: &str) {
        if let Some(term) = self.by_id(id) {
            info!(id, code, error, "terminal ended");
            let text = match error.is_empty() {
                true => nuvira_i18n::tf("term.ended", &[("code", &code.to_string())]),
                false => nuvira_i18n::tf("term.failed", &[("reason", error)]),
            };
            let _ =
                term.webview.evaluate_script(&format!("term.ended({})", crate::toolbar::js_string(&text)));
        }
    }

    /// Title of every window (the name of the computer is known once connected).
    pub fn set_title(&self, title: &str) {
        for term in self.windows.values() {
            term.window.set_title(title);
        }
    }

    /// A message for every window (connection lost, waiting for the confirmation...).
    pub fn status(&self, text: &str) {
        for term in self.windows.values() {
            let _ =
                term.webview.evaluate_script(&format!("term.status({})", crate::toolbar::js_string(text)));
        }
    }

    pub fn window_event(&mut self, window: WindowId, event: &WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(term) = self.windows.remove(&window) {
                    if let Some(link) = &self.link {
                        let _ = link.send(TermCmd::Close { id: term.id });
                    }
                    info!(id = term.id, "terminal window closed");
                }
            }
            WindowEvent::Resized(_) => {
                if let Some(term) = self.windows.get(&window) {
                    let _ = term.webview.set_bounds(full(&term.window));
                }
            }
            WindowEvent::Focused(true) => {
                if let Some(term) = self.windows.get(&window) {
                    let _ = term.webview.focus();
                }
            }
            _ => {}
        }
    }

    fn by_id(&self, id: u32) -> Option<&TermWindow> {
        let term = self.windows.values().find(|t| t.id == id);
        if term.is_none() {
            tracing::debug!(id, "terminal window already closed");
        }
        term
    }
}

fn full(window: &Window) -> Rect {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(size.width, size.height).into(),
    }
}
