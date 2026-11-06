//! TCP tunnels (NuviraHub Pro): ports of this computer forwarded to addresses of the
//! remote computer's local network, e.g. a router's web panel or the Remote Desktop
//! of another PC. Each connection to the local port goes through the session on a
//! stream of its own (`net::TunnelCmd`); the agent connects only to local addresses.
//!
//! The "Tunnels" window (`tunnels.html`) adds and removes them and opens the browser
//! or Remote Desktop on the local port; `--tunnel LOCAL:HOST:PORT` adds them from the
//! command line. Its page sends JSON messages (`{"op": "add", ...}`), which reach the
//! event loop as [`NetEvent::TunnelPage`]; the list goes back with `tn.update(...)`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::net::{NetEvent, TunnelCmd};

const HTML: &str = include_str!("tunnels.html");

#[derive(Debug, Clone, Serialize)]
struct Entry {
    id: u32,
    host: String,
    port: u16,
    /// Asked for (0: any free port), then the one listening.
    local_port: u16,
    listening: bool,
    error: String,
}

struct Ui {
    window: Arc<Window>,
    webview: WebView,
    _context: WebContext,
}

#[derive(Default)]
pub struct Tunnels {
    entries: Vec<Entry>,
    /// Towards the network, once connected to an agent that has tunnels.
    link: Option<UnboundedSender<TunnelCmd>>,
    ui: Option<Ui>,
    next_id: u32,
}

impl Tunnels {
    pub fn owns(&self, id: WindowId) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.window.id() == id)
    }

    pub fn has_any(&self) -> bool {
        !self.entries.is_empty()
    }

    /// The agent has tunnels: those added before connecting open now.
    pub fn connect(&mut self, link: UnboundedSender<TunnelCmd>) {
        for e in &self.entries {
            let _ = link.send(TunnelCmd::Add {
                id: e.id,
                local_port: e.local_port,
                host: e.host.clone(),
                port: e.port,
            });
        }
        self.link = Some(link);
    }

    /// `LOCAL:HOST:PORT` (`--tunnel`), e.g. `13389:192.168.1.20:3389`; LOCAL 0 = any free port.
    pub fn add_spec(&mut self, spec: &str) -> Result<()> {
        let mut parts = spec.splitn(2, ':');
        let local: u16 = parts.next().unwrap_or_default().parse().context("local port")?;
        let rest = parts.next().context("LOCAL:HOST:PORT")?;
        let (host, port) = rest.rsplit_once(':').context("LOCAL:HOST:PORT")?;
        let port: u16 = port.parse().context("remote port")?;
        self.add(host.trim_matches(['[', ']']).to_string(), port, local)
    }

    pub fn add(&mut self, host: String, port: u16, local_port: u16) -> Result<()> {
        if host.is_empty() || port == 0 {
            bail!("{}", nuvira_i18n::t("tn.invalid"));
        }
        self.next_id += 1;
        let entry =
            Entry { id: self.next_id, host, port, local_port, listening: false, error: String::new() };
        if let Some(link) = &self.link {
            let cmd = TunnelCmd::Add { id: entry.id, local_port, host: entry.host.clone(), port };
            let _ = link.send(cmd);
        }
        self.entries.push(entry);
        self.push();
        Ok(())
    }

    pub fn listening(&mut self, id: u32, local_port: u16, error: String) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.listening = error.is_empty();
            if e.listening {
                e.local_port = local_port;
                // For scripts and tests reading the output.
                println!("@tunnel\t{local_port}\t{}:{}", e.host, e.port);
            }
            e.error = error;
        }
        self.push();
    }

    /// A connection was refused by the agent (destination not local, unreachable...).
    pub fn connection_error(&mut self, id: u32, error: String) {
        warn!(id, error, "tunnel connection failed");
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.error = error;
        }
        self.push();
    }

    pub fn open(&mut self, event_loop: &ActiveEventLoop, proxy: EventLoopProxy<NetEvent>) -> Result<()> {
        if let Some(ui) = &self.ui {
            ui.window.focus_window();
            return Ok(());
        }
        let attrs = crate::window_attributes()
            .with_title(nuvira_i18n::t("tn.title"))
            .with_inner_size(winit::dpi::LogicalSize::new(640.0, 460.0));
        let window = Arc::new(event_loop.create_window(attrs).context("tunnels window")?);
        let data_dir =
            std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("nuvira").join("WebView2"));
        let mut context = WebContext::new(data_dir);
        let webview = crate::toolbar::retry_in_use(|| {
            let proxy = proxy.clone();
            WebViewBuilder::new_with_web_context(&mut context)
                .with_html(HTML)
                .with_background_color((0x1f, 0x25, 0x2b, 0xff))
                .with_bounds(full(&window))
                .with_initialization_script(crate::toolbar::init_script())
                .with_ipc_handler(move |request| {
                    let _ = proxy.send_event(NetEvent::TunnelPage(request.body().clone()));
                })
                .build_as_child(&window)
        })
        .context("tunnels WebView")?;
        window.focus_window();
        self.ui = Some(Ui { window, webview, _context: context });
        Ok(())
    }

    pub fn message(&mut self, body: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(body) else { return };
        let id = msg["id"].as_u64().unwrap_or(0) as u32;
        match msg["op"].as_str() {
            Some("ready") => self.push(),
            Some("add") => {
                let host = msg["host"].as_str().unwrap_or_default().trim().to_string();
                let port = msg["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
                let local = msg["local_port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
                if let Err(e) = self.add(host, port, local) {
                    self.script(&format!("tn.error({})", crate::toolbar::js_string(&format!("{e:#}"))));
                }
            }
            Some("remove") => {
                self.entries.retain(|e| e.id != id);
                if let Some(link) = &self.link {
                    let _ = link.send(TunnelCmd::Remove { id });
                }
                self.push();
            }
            Some("launch") => {
                let Some(e) = self.entries.iter().find(|e| e.id == id && e.listening) else { return };
                let kind = msg["kind"].as_str().unwrap_or_default();
                if let Err(e) = launch(kind, e.local_port) {
                    warn!("tunnel: {e:#}");
                }
            }
            _ => {}
        }
    }

    pub fn window_event(&mut self, event: &WindowEvent) {
        match event {
            // Closing the window keeps the tunnels: they end with the session.
            WindowEvent::CloseRequested => self.ui = None,
            WindowEvent::Resized(_) => {
                if let Some(ui) = &self.ui {
                    let _ = ui.webview.set_bounds(full(&ui.window));
                }
            }
            _ => {}
        }
    }

    fn push(&self) {
        let state = json!({ "tunnels": self.entries, "connected": self.link.is_some() });
        self.script(&format!("tn.update({state})"));
    }

    fn script(&self, js: &str) {
        if let Some(ui) = &self.ui {
            let _ = ui.webview.evaluate_script(js);
        }
    }
}

/// Opens the browser (`web`, `https`) or Remote Desktop (`rdp`, Windows) on the local port.
fn launch(kind: &str, port: u16) -> Result<()> {
    info!(kind, port, "opening the tunnel");
    let target = match kind {
        "web" => format!("http://localhost:{port}/"),
        "https" => format!("https://localhost:{port}/"),
        #[cfg(windows)]
        "rdp" => {
            std::process::Command::new("mstsc").arg(format!("/v:localhost:{port}")).spawn()?;
            return Ok(());
        }
        _ => bail!("unknown kind {kind}"),
    };
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{HSTRING, w};
        unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(target), None, None, SW_SHOWNORMAL) };
    }
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&target).spawn()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open").arg(&target).spawn()?;
    Ok(())
}

fn full(window: &Window) -> Rect {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(size.width, size.height).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        let mut t = Tunnels::default();
        t.add_spec("13389:192.168.1.20:3389").unwrap();
        t.add_spec("0:router.lan:80").unwrap();
        t.add_spec("8443:[fd00::1]:443").unwrap();
        let e: Vec<_> = t.entries.iter().map(|e| (e.local_port, e.host.as_str(), e.port)).collect();
        assert_eq!(e, [(13389, "192.168.1.20", 3389), (0, "router.lan", 80), (8443, "fd00::1", 443)]);
        assert!(t.add_spec("x:host:1").is_err());
        assert!(t.add_spec("1:host").is_err());
    }
}
