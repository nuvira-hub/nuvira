//! Session toolbar.
//!
//! A WebView (WebView2 on Windows) child of the window, at the top centre: the
//! buttons are HTML (`toolbar.html`, reusable in the future web client) and work
//! with both drawing methods, wgpu and CPU. Clicks reach the event loop as
//! [`NetEvent::Toolbar`]; the state goes back to the page with
//! `nuvira.update({...})`. The mouse over the toolbar does not reach the remote PC.
//!
//! The chat panel ([`ChatPanel`], `chat.html`) is a second WebView, on the right
//! below the toolbar, visible only when needed.

use std::sync::Arc;

use anyhow::{Context, Result};
use nuvira_proto::v1::{DisplayInfo, Quality};
use winit::event_loop::EventLoopProxy;
use winit::window::Window;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::net::NetEvent;

const HTML: &str = include_str!("toolbar.html");
const CHAT_HTML: &str = include_str!("chat.html");
const NOTICE_HTML: &str = include_str!("notice.html");
/// Sizes in logical pixels: open toolbar and the tab to reopen it.
const BAR: (f64, f64) = (940.0, 38.0);
/// Chat panel: width and maximum height, distance from the edges.
const CHAT: (f64, f64) = (340.0, 440.0);
const CHAT_MARGIN: f64 = 12.0;
const TAB: (f64, f64) = (56.0, 14.0);
/// Message in the middle of the window, before the first image.
const NOTICE: (f64, f64) = (520.0, 76.0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SecureAttention,
    ViewOnly,
    /// Privacy mode on or off at the agent.
    Privacy,
    /// Starts or stops recording the session.
    Record,
    /// Opens a remote terminal window.
    Terminal,
    /// Opens the TCP tunnels window.
    Tunnels,
    /// Restart of the remote computer: asks for confirmation first.
    Restart,
    /// Restart confirmed in the dialog.
    RestartConfirmed,
    /// Real size (1:1) or fitted to the window.
    ActualSize,
    /// The remote screen takes the window's size (as close as its modes allow), or
    /// goes back to its own.
    AdaptResolution,
    Fullscreen,
    /// Stream the screen with this index.
    Display(u32),
    Quality(Quality),
    Collapse,
    Expand,
    Close,
    /// Opens or closes the chat panel.
    Chat,
    /// Message written in the chat panel.
    ChatSend(String),
    /// Picks files to send to the remote PC.
    SendFile,
    /// Opens the file manager window.
    FileManager,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        if let Some(index) = s.strip_prefix("display:") {
            return index.parse().ok().map(Self::Display);
        }
        if let Some(text) = s.strip_prefix("chat:") {
            return Some(Self::ChatSend(text.to_string()));
        }
        if let Some(name) = s.strip_prefix("quality:") {
            return Some(Self::Quality(match name {
                "sharp" => Quality::Sharp,
                "fast" => Quality::Fast,
                "balanced" => Quality::Balanced,
                _ => return None,
            }));
        }
        Some(match s {
            "sas" => Self::SecureAttention,
            "viewonly" => Self::ViewOnly,
            "privacy" => Self::Privacy,
            "record" => Self::Record,
            "terminal" => Self::Terminal,
            "tunnels" => Self::Tunnels,
            "restart" => Self::Restart,
            "actual" => Self::ActualSize,
            "resolution" => Self::AdaptResolution,
            "fullscreen" => Self::Fullscreen,
            "collapse" => Self::Collapse,
            "expand" => Self::Expand,
            "close" => Self::Close,
            "chat" => Self::Chat,
            "sendfile" => Self::SendFile,
            "files" => Self::FileManager,
            _ => return None,
        })
    }
}

pub struct Toolbar {
    webview: WebView,
    window: Arc<Window>,
    collapsed: bool,
    // Must live as long as the WebView.
    _context: WebContext,
}

impl Toolbar {
    pub fn new(window: Arc<Window>, proxy: EventLoopProxy<NetEvent>) -> Result<Self> {
        let (webview, context) =
            child_webview(&window, proxy, HTML, bounds(&window, BAR), true).context("toolbar WebView")?;
        Ok(Self { webview, window, collapsed: false, _context: context })
    }

    /// Repositions the toolbar (window resized or toolbar collapsed/expanded).
    pub fn layout(&self) {
        let size = if self.collapsed { TAB } else { BAR };
        let _ = self.webview.set_bounds(bounds(&self.window, size));
    }

    pub fn set_collapsed(&mut self, collapsed: bool) {
        self.collapsed = collapsed;
        self.layout();
        self.update(&format!("{{collapsed:{collapsed}}}"));
    }

    /// Updates the page; `state` is a JavaScript object, e.g. `{viewOnly:true}`.
    pub fn update(&self, state: &str) {
        let _ = self.webview.evaluate_script(&format!("window.nuvira && nuvira.update({state})"));
    }

    /// After a click on the toolbar the keyboard goes back to the session.
    pub fn focus_session(&self) {
        let _ = self.webview.focus_parent();
    }
}

/// Chat panel, hidden until needed.
pub struct ChatPanel {
    webview: WebView,
    window: Arc<Window>,
    visible: bool,
    _context: WebContext,
}

impl ChatPanel {
    pub fn new(window: Arc<Window>, proxy: EventLoopProxy<NetEvent>) -> Result<Self> {
        let (webview, context) =
            child_webview(&window, proxy, CHAT_HTML, chat_bounds(&window), false).context("chat WebView")?;
        // On Linux wry shows the GTK window and hides it again, but GTK maps it only
        // when it handles its events: the empty (black) container would stay above
        // the image. Hidden again once GTK is done.
        #[cfg(target_os = "linux")]
        {
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
            let _ = webview.set_visible(false);
        }
        Ok(Self { webview, window, visible: false, _context: context })
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        self.layout();
        let _ = self.webview.set_visible(visible);
        if visible {
            let _ = self.webview.focus();
            let _ = self.webview.evaluate_script("window.nuvira && nuvira.focus()");
        } else {
            let _ = self.webview.focus_parent();
        }
    }

    pub fn layout(&self) {
        let _ = self.webview.set_bounds(chat_bounds(&self.window));
    }

    /// Adds a message; `mine`: written by the controlling side.
    pub fn add(&self, mine: bool, text: &str) {
        let _ = self.webview.evaluate_script(&format!(
            "window.nuvira && nuvira.add({{mine:{mine},text:{}}})",
            js_string(text)
        ));
    }

    /// Viewer notice (e.g. file sent), not a message.
    pub fn notice(&self, text: &str) {
        let _ = self.webview.evaluate_script(&format!(
            "window.nuvira && nuvira.add({{system:true,text:{}}})",
            js_string(text)
        ));
    }

    /// Click on the image: the keyboard goes back to the session.
    pub fn focus_session(&self) {
        let _ = self.webview.focus_parent();
    }
}

/// Message in the middle of the window while there is no image yet (connecting,
/// waiting for the person at the remote computer to accept): without it the window
/// stays black. Dropped at the first image.
pub struct Notice {
    webview: WebView,
    window: Arc<Window>,
    _context: WebContext,
}

impl Notice {
    pub fn new(window: Arc<Window>, proxy: EventLoopProxy<NetEvent>, text: &str) -> Result<Self> {
        let html = NOTICE_HTML.replace("/*TEXT*/\"\"", &js_string(text));
        let (webview, context) =
            child_webview(&window, proxy, &html, notice_bounds(&window), true).context("notice WebView")?;
        Ok(Self { webview, window, _context: context })
    }

    pub fn set_text(&self, text: &str) {
        let _ = self.webview.evaluate_script(&format!("window.nuvira && nuvira.set({})", js_string(text)));
    }

    pub fn layout(&self) {
        let _ = self.webview.set_bounds(notice_bounds(&self.window));
    }
}

/// In the middle of the window.
fn notice_bounds(window: &Window) -> Rect {
    let inner = window.inner_size().to_logical::<f64>(window.scale_factor());
    let w = NOTICE.0.min(inner.width - 2.0 * CHAT_MARGIN).max(0.0);
    let h = NOTICE.1.min(inner.height).max(0.0);
    Rect {
        position: LogicalPosition::new(((inner.width - w) / 2.0).round(), ((inner.height - h) / 2.0).round())
            .into(),
        size: LogicalSize::new(w, h).into(),
    }
}

/// WebView child of the session window, with the page's messages forwarded to the viewer.
fn child_webview(
    window: &Window,
    proxy: EventLoopProxy<NetEvent>,
    html: &str,
    bounds: Rect,
    visible: bool,
) -> Result<(WebView, WebContext)> {
    // WebView2 data in the user's folder: next to the program (e.g. Program Files)
    // it often cannot write.
    let data_dir =
        std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("nuvira").join("WebView2"));
    let mut context = WebContext::new(data_dir);
    let html = html.to_string();
    let webview = retry_in_use(|| {
        let proxy = proxy.clone();
        WebViewBuilder::new_with_web_context(&mut context)
            .with_html(&html)
            // Same colour as the pages: no white flash while loading.
            .with_background_color((0x1f, 0x25, 0x2b, 0xff))
            .with_bounds(bounds)
            .with_visible(visible)
            .with_initialization_script(init_script())
            .with_ipc_handler(move |request| {
                if let Some(action) = Action::parse(request.body()) {
                    let _ = proxy.send_event(NetEvent::Toolbar(action));
                }
            })
            .build_as_child(window)
    })?;
    Ok((webview, context))
}

/// Run by every page of the viewer before its own script: the strings in the
/// session's language, and no context menu (on Linux WebKitGTK's stays open
/// above every window when the WebView is a child of the session window).
pub fn init_script() -> String {
    format!(
        "window.I18N = {}; document.addEventListener('contextmenu', e => e.preventDefault());",
        nuvira_i18n::json()
    )
}

/// Builds a WebView, trying again for a couple of seconds while WebView2 answers
/// "resource in use" (0x800700AA): the data folder is shared, and a session window
/// closed just before is still shutting down its WebView2 processes.
pub fn retry_in_use(mut build: impl FnMut() -> wry::Result<WebView>) -> wry::Result<WebView> {
    let mut attempts = 0;
    loop {
        match build() {
            Err(e) if attempts < 8 && format!("{e:?}").contains("800700AA") => {
                attempts += 1;
                tracing::debug!("WebView2 data folder in use, trying again");
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            result => return result,
        }
    }
}

/// On the right, below the toolbar.
fn chat_bounds(window: &Window) -> Rect {
    let inner = window.inner_size().to_logical::<f64>(window.scale_factor());
    let w = CHAT.0.min(inner.width - 2.0 * CHAT_MARGIN).max(0.0);
    let top = BAR.1 + CHAT_MARGIN;
    let h = CHAT.1.min(inner.height - top - CHAT_MARGIN).max(0.0);
    Rect {
        position: LogicalPosition::new((inner.width - w - CHAT_MARGIN).round(), top).into(),
        size: LogicalSize::new(w, h).into(),
    }
}

/// At the top centre, at most as wide as the window.
fn bounds(window: &Window, (w, h): (f64, f64)) -> Rect {
    let inner = window.inner_size().to_logical::<f64>(window.scale_factor());
    let w = w.min(inner.width);
    Rect {
        position: LogicalPosition::new(((inner.width - w) / 2.0).round(), 0.0).into(),
        size: LogicalSize::new(w, h).into(),
    }
}

/// Quality name used by the page.
pub fn quality_name(quality: Quality) -> &'static str {
    match quality {
        Quality::Sharp => "sharp",
        Quality::Fast => "fast",
        Quality::Balanced | Quality::Unspecified => "balanced",
    }
}

/// Screens state for `nuvira.update`: the buttons appear only if there is more than one.
pub fn displays_state(displays: &[DisplayInfo], current: u32, all_available: bool) -> String {
    let items: Vec<String> = displays
        .iter()
        .map(|d| {
            let key = if d.primary { "tb.display_primary" } else { "tb.display" };
            let title = nuvira_i18n::tf(
                key,
                &[
                    ("n", &(d.index + 1).to_string()),
                    ("w", &d.width.to_string()),
                    ("h", &d.height.to_string()),
                ],
            );
            format!("{{index:{},title:{}}}", d.index, js_string(&title))
        })
        .collect();
    format!("{{displays:[{}],display:{current},all:{all_available}}}", items.join(","))
}

/// JavaScript string with quotes and special characters escaped.
pub fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_and_escaping() {
        assert_eq!(Action::parse("sas"), Some(Action::SecureAttention));
        assert_eq!(Action::parse("privacy"), Some(Action::Privacy));
        assert_eq!(Action::parse("resolution"), Some(Action::AdaptResolution));
        assert_eq!(Action::parse("record"), Some(Action::Record));
        assert_eq!(Action::parse("terminal"), Some(Action::Terminal));
        assert_eq!(Action::parse("altro"), None);
        assert_eq!(Action::parse("display:1"), Some(Action::Display(1)));
        assert_eq!(Action::parse("quality:sharp"), Some(Action::Quality(Quality::Sharp)));
        assert_eq!(Action::parse("quality:x"), None);
        assert_eq!(Action::parse("chat:hello: to you"), Some(Action::ChatSend("hello: to you".into())));
        // "<" is escaped: the text cannot close a <script> tag.
        let expected = concat!(r#""PC \"lab\"\u000a"#, r"\u", "003c", r#"/script>""#);
        assert_eq!(js_string("PC \"lab\"\n</script>"), expected);
    }
}
