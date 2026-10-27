//! Barra degli strumenti della sessione.
//!
//! Una WebView (WebView2 su Windows) figlia della finestra, in alto al centro:
//! i pulsanti sono HTML (`toolbar.html`, riusabile nel futuro client web) e
//! funzionano con entrambi i modi di disegno, wgpu e CPU. I clic arrivano al
//! ciclo degli eventi come [`NetEvent::Toolbar`]; lo stato torna alla pagina con
//! `nexodesk.update({...})`. Il mouse sopra la barra non arriva al PC remoto.

use std::sync::Arc;

use anyhow::{Context, Result};
use nexodesk_proto::v1::{DisplayInfo, Quality};
use winit::event_loop::EventLoopProxy;
use winit::window::Window;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::net::NetEvent;

const HTML: &str = include_str!("toolbar.html");
/// Dimensioni in pixel logici: barra aperta e linguetta per riaprirla.
const BAR: (f64, f64) = (780.0, 38.0);
const TAB: (f64, f64) = (56.0, 14.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        if let Some(index) = s.strip_prefix("display:") {
            return index.parse().ok().map(Self::Display);
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
        // "<" diventa <: il testo non può chiudere un tag <script>.
        let expected = concat!(r#""PC \"lab\"\u000a"#, r"\u", "003c", r#"/script>""#);
        assert_eq!(js_string("PC \"lab\"\n</script>"), expected);
    }
}
