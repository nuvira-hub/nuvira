//! Barra degli strumenti della sessione.
//!
//! Una WebView (WebView2 su Windows) figlia della finestra, in alto al centro:
//! i pulsanti sono HTML (`toolbar.html`, riusabile nel futuro client web) e
//! funzionano con entrambi i modi di disegno, wgpu e CPU. I clic arrivano al
//! ciclo degli eventi come [`NetEvent::Toolbar`]; lo stato torna alla pagina con
//! `nexodesk.update({...})`. Il mouse sopra la barra non arriva al PC remoto.

use std::sync::Arc;

use anyhow::{Context, Result};
use winit::event_loop::EventLoopProxy;
use winit::window::Window;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::net::NetEvent;

const HTML: &str = include_str!("toolbar.html");
/// Dimensioni in pixel logici: barra aperta e linguetta per riaprirla.
const BAR: (f64, f64) = (640.0, 38.0);
const TAB: (f64, f64) = (56.0, 14.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SecureAttention,
    ViewOnly,
    Fullscreen,
    Collapse,
    Expand,
    Close,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sas" => Self::SecureAttention,
            "viewonly" => Self::ViewOnly,
            "fullscreen" => Self::Fullscreen,
            "collapse" => Self::Collapse,
            "expand" => Self::Expand,
            "close" => Self::Close,
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
        // Dati di WebView2 nella cartella dell'utente: accanto al programma (es. Program Files)
        // spesso non si può scrivere.
        let data_dir = std::env::var_os("LOCALAPPDATA")
            .map(|d| std::path::PathBuf::from(d).join("NexoDesk").join("WebView2"));
        let mut context = WebContext::new(data_dir);
        let webview = WebViewBuilder::new_with_web_context(&mut context)
            .with_html(HTML)
            // Stesso colore della barra: niente lampo bianco al caricamento.
            .with_background_color((0x1f, 0x25, 0x2b, 0xff))
            .with_bounds(bounds(&window, BAR))
            .with_ipc_handler(move |request| {
                if let Some(action) = Action::parse(request.body()) {
                    let _ = proxy.send_event(NetEvent::Toolbar(action));
                }
            })
            .build_as_child(window.as_ref())
            .context("WebView della barra")?;
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

/// In alto al centro, larga al massimo quanto la finestra.
fn bounds(window: &Window, (w, h): (f64, f64)) -> Rect {
    let inner = window.inner_size().to_logical::<f64>(window.scale_factor());
    let w = w.min(inner.width);
    Rect {
        position: LogicalPosition::new(((inner.width - w) / 2.0).round(), 0.0).into(),
        size: LogicalSize::new(w, h).into(),
    }
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
        // "<" diventa <: il testo non può chiudere un tag <script>.
        let expected = concat!(r#""PC \"lab\"\u000a"#, r"\u", "003c", r#"/script>""#);
        assert_eq!(js_string("PC \"lab\"\n</script>"), expected);
    }
}
