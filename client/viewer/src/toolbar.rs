//! Barra degli strumenti della sessione.
//!
//! Una WebView (WebView2 su Windows) figlia della finestra, in alto al centro:
//! i pulsanti sono HTML (`toolbar.html`, riusabile nel futuro client web) e
//! funzionano con entrambi i modi di disegno, wgpu e CPU. I clic arrivano al
//! ciclo degli eventi come [`NetEvent::Toolbar`]; lo stato torna alla pagina con
//! `nexodesk.update({...})`. Il mouse sopra la barra non arriva al PC remoto.
//!
//! Il riquadro della chat ([`ChatPanel`], `chat.html`) è una seconda WebView,
//! a destra sotto la barra, visibile solo quando serve.

use std::sync::Arc;

use anyhow::{Context, Result};
use nexodesk_proto::v1::{DisplayInfo, Quality};
use winit::event_loop::EventLoopProxy;
use winit::window::Window;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::net::NetEvent;

const HTML: &str = include_str!("toolbar.html");
const CHAT_HTML: &str = include_str!("chat.html");
/// Dimensioni in pixel logici: barra aperta e linguetta per riaprirla.
const BAR: (f64, f64) = (860.0, 38.0);
/// Riquadro della chat: larghezza e altezza massima, distanza dai bordi.
const CHAT: (f64, f64) = (340.0, 440.0);
const CHAT_MARGIN: f64 = 12.0;
const TAB: (f64, f64) = (56.0, 14.0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SecureAttention,
    ViewOnly,
    Fullscreen,
    /// Trasmetti lo schermo con questo indice.
    Display(u32),
    Quality(Quality),
    Collapse,
    Expand,
    Close,
    /// Apre o chiude il riquadro della chat.
    Chat,
    /// Messaggio scritto nel riquadro della chat.
    ChatSend(String),
    /// Sceglie file da inviare al PC remoto.
    SendFile,
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
            "fullscreen" => Self::Fullscreen,
            "collapse" => Self::Collapse,
            "expand" => Self::Expand,
            "close" => Self::Close,
            "chat" => Self::Chat,
            "sendfile" => Self::SendFile,
            _ => return None,
        })
    }
}

pub struct Toolbar {
    webview: WebView,
    window: Arc<Window>,
    collapsed: bool,
    // Deve vivere quanto la WebView.
    _context: WebContext,
}

impl Toolbar {
    pub fn new(window: Arc<Window>, proxy: EventLoopProxy<NetEvent>) -> Result<Self> {
        let (webview, context) =
            child_webview(&window, proxy, HTML, bounds(&window, BAR), true).context("WebView della barra")?;
        Ok(Self { webview, window, collapsed: false, _context: context })
    }

    /// Riposiziona la barra (finestra ridimensionata o barra ridotta/aperta).
    pub fn layout(&self) {
        let size = if self.collapsed { TAB } else { BAR };
        let _ = self.webview.set_bounds(bounds(&self.window, size));
    }

    pub fn set_collapsed(&mut self, collapsed: bool) {
        self.collapsed = collapsed;
        self.layout();
        self.update(&format!("{{collapsed:{collapsed}}}"));
    }

    /// Aggiorna la pagina; `state` è un oggetto JavaScript, es. `{viewOnly:true}`.
    pub fn update(&self, state: &str) {
        let _ = self.webview.evaluate_script(&format!("window.nexodesk && nexodesk.update({state})"));
    }

    /// Dopo un clic sulla barra la tastiera torna alla sessione.
    pub fn focus_session(&self) {
        let _ = self.webview.focus_parent();
    }
}

/// Riquadro della chat, nascosto finché non serve.
pub struct ChatPanel {
    webview: WebView,
    window: Arc<Window>,
    visible: bool,
    _context: WebContext,
}

impl ChatPanel {
    pub fn new(window: Arc<Window>, proxy: EventLoopProxy<NetEvent>) -> Result<Self> {
        let (webview, context) = child_webview(&window, proxy, CHAT_HTML, chat_bounds(&window), false)
            .context("WebView della chat")?;
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
            let _ = self.webview.evaluate_script("window.nexodesk && nexodesk.focus()");
        } else {
            let _ = self.webview.focus_parent();
        }
    }

    pub fn layout(&self) {
        let _ = self.webview.set_bounds(chat_bounds(&self.window));
    }

    /// Aggiunge un messaggio; `mine`: scritto da chi controlla.
    pub fn add(&self, mine: bool, text: &str) {
        let _ = self
            .webview
            .evaluate_script(&format!("window.nexodesk && nexodesk.add({{mine:{mine},text:{}}})", js_string(text)));
    }

    /// Avviso del viewer (es. file inviato), non un messaggio.
    pub fn notice(&self, text: &str) {
        let _ = self
            .webview
            .evaluate_script(&format!("window.nexodesk && nexodesk.add({{system:true,text:{}}})", js_string(text)));
    }

    /// Clic sull'immagine: la tastiera torna alla sessione.
    pub fn focus_session(&self) {
        let _ = self.webview.focus_parent();
    }
}

/// WebView figlia della finestra della sessione, con i messaggi della pagina inoltrati al viewer.
fn child_webview(
    window: &Window,
    proxy: EventLoopProxy<NetEvent>,
    html: &str,
    bounds: Rect,
    visible: bool,
) -> Result<(WebView, WebContext)> {
    // Dati di WebView2 nella cartella dell'utente: accanto al programma (es. Program Files)
    // spesso non si può scrivere.
    let data_dir =
        std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("NexoDesk").join("WebView2"));
    let mut context = WebContext::new(data_dir);
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_html(html)
        // Stesso colore delle pagine: niente lampo bianco al caricamento.
        .with_background_color((0x1f, 0x25, 0x2b, 0xff))
        .with_bounds(bounds)
        .with_visible(visible)
        .with_ipc_handler(move |request| {
            if let Some(action) = Action::parse(request.body()) {
                let _ = proxy.send_event(NetEvent::Toolbar(action));
            }
        })
        .build_as_child(window)?;
    Ok((webview, context))
}

/// A destra, sotto la barra.
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

/// In alto al centro, larga al massimo quanto la finestra.
fn bounds(window: &Window, (w, h): (f64, f64)) -> Rect {
    let inner = window.inner_size().to_logical::<f64>(window.scale_factor());
    let w = w.min(inner.width);
    Rect {
        position: LogicalPosition::new(((inner.width - w) / 2.0).round(), 0.0).into(),
        size: LogicalSize::new(w, h).into(),
    }
}

/// Nome della qualità usato dalla pagina.
pub fn quality_name(quality: Quality) -> &'static str {
    match quality {
        Quality::Sharp => "sharp",
        Quality::Fast => "fast",
        Quality::Balanced | Quality::Unspecified => "balanced",
    }
}

/// Stato degli schermi per `nexodesk.update`: i pulsanti compaiono solo se sono più di uno.
pub fn displays_state(displays: &[DisplayInfo], current: u32) -> String {
    let items: Vec<String> = displays
        .iter()
        .map(|d| {
            let title = format!(
                "Schermo {} · {}x{}{}",
                d.index + 1,
                d.width,
                d.height,
                if d.primary { " · principale" } else { "" }
            );
            format!("{{index:{},title:{}}}", d.index, js_string(&title))
        })
        .collect();
    format!("{{displays:[{}],display:{current}}}", items.join(","))
}

/// Stringa JavaScript con le virgolette e i caratteri speciali protetti.
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
        assert_eq!(Action::parse("altro"), None);
        assert_eq!(Action::parse("display:1"), Some(Action::Display(1)));
        assert_eq!(Action::parse("quality:sharp"), Some(Action::Quality(Quality::Sharp)));
        assert_eq!(Action::parse("quality:x"), None);
        assert_eq!(Action::parse("chat:ciao: a te"), Some(Action::ChatSend("ciao: a te".into())));
        // "<" diventa <: il testo non può chiudere un tag <script>.
        let expected = concat!(r#""PC \"lab\"\u000a"#, r"\u", "003c", r#"/script>""#);
        assert_eq!(js_string("PC \"lab\"\n</script>"), expected);
    }
}
