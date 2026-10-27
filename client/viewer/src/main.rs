//! NexoDesk viewer — la finestra della sessione remota.
//!
//! Prototipo della fase 0: si collega direttamente a un agent in LAN, mostra lo
//! schermo con wgpu e invia mouse e tastiera. Nel titolo della finestra:
//! frame al secondo, latenza cattura→visualizzazione, RTT e banda. In alto la
//! barra degli strumenti (`toolbar.rs`).

mod cpu;
mod net;
mod render;
mod report;
mod toolbar;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use nexodesk_proto::v1::{
    Button, InputEvent, Key, MouseButton, MouseMove, MouseWheel, SecureAttention, input_event::Event,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::net::{FrameSlot, NetEvent};
use crate::render::Renderer;
use crate::report::Report;

const COPYRIGHT: &str = "NexoDesk © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/alexbn71/NexoDesk";

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum TransportArg {
    Auto,
    Udp,
    Websocket,
}

impl From<TransportArg> for nexodesk_transport::signal::TransportMode {
    fn from(arg: TransportArg) -> Self {
        match arg {
            TransportArg::Auto => Self::Auto,
            TransportArg::Udp => Self::Udp,
            TransportArg::Websocket => Self::WebSocket,
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about = "NexoDesk viewer: controlla un PC con NexoDesk agent (prototipo in LAN)", after_help = COPYRIGHT)]
struct Args {
    /// ID dell'agent (con --server) oppure indirizzo in LAN, es. 192.168.1.20[:21500].
    agent: String,

    /// nexodesk-server da usare per collegarsi per ID (host[:porta]).
    #[arg(long, value_name = "HOST[:PORTA]")]
    server: Option<String>,

    /// Impronta del certificato del server. Senza, si memorizza quella vista al primo collegamento.
    #[arg(long, value_name = "IMPRONTA")]
    server_key: Option<String>,

    /// Trasporto verso il server: auto (UDP, poi WebSocket se UDP è bloccato), udp, websocket.
    #[arg(long, value_enum, default_value_t = TransportArg::Auto)]
    transport: TransportArg,

    /// URL WebSocket del server per il ripiego (predefinito: ws://<server>:<porta>/nexodesk),
    /// es. wss://nexodesk.example.it/nexodesk dietro un reverse proxy.
    #[arg(long, value_name = "URL")]
    server_ws: Option<String>,

    /// Password di sessione mostrata dall'agent (anche dalla variabile NEXODESK_PASSWORD,
    /// che a differenza della riga di comando non è visibile agli altri programmi).
    #[arg(long, env = "NEXODESK_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    /// File con la chiave del dispositivo (predefinito: viewer.key nella cartella dati;
    /// diversa da quella dell'agent, che altrimenti il server scollegherebbe).
    #[arg(long, value_name = "FILE")]
    identity: Option<PathBuf>,

    /// Impronta del certificato mostrata dall'agent (consigliata).
    #[arg(long)]
    fingerprint: Option<String>,

    /// Solo visualizzazione: non invia mouse e tastiera.
    #[arg(long)]
    view_only: bool,

    /// Chiude la sessione dopo questi secondi (test automatici).
    #[arg(long, value_name = "SECONDI")]
    duration: Option<u64>,

    /// Scrive i risultati della sessione in JSON all'uscita (test automatici).
    #[arg(long, value_name = "FILE")]
    stats_file: Option<PathBuf>,

    /// Salva l'ultimo frame mostrato in PNG all'uscita (verifica dei colori nei test).
    #[arg(long, value_name = "FILE")]
    snapshot: Option<PathBuf>,
}

/// Colori nei log solo se il terminale li interpreta (il vecchio prompt di Windows
/// li mostrerebbe come `←[2m`).
fn ansi_colors() -> bool {
    #[cfg(windows)]
    return std::io::IsTerminal::is_terminal(&std::io::stderr())
        && nu_ansi_term::enable_ansi_support().is_ok();
    #[cfg(not(windows))]
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(ansi_colors())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let target = match &args.server {
        Some(server_name) => net::Target::Server {
            server: nexodesk_transport::resolve_addr(server_name, nexodesk_transport::signal::DEFAULT_PORT)?,
            server_name: server_name.clone(),
            server_key: args.server_key.as_deref().map(nexodesk_transport::parse_fingerprint).transpose()?,
            transport: args.transport.into(),
            server_ws: args.server_ws.clone(),
            device_id: args.agent.chars().filter(|c| c.is_ascii_digit()).collect(),
            identity: args
                .identity
                .clone()
                .unwrap_or_else(|| nexodesk_transport::identity::data_dir().join("viewer.key")),
        },
        None => net::Target::Direct {
            addr: nexodesk_transport::resolve_addr(&args.agent, nexodesk_transport::DEFAULT_PORT)?,
            fingerprint: args.fingerprint.as_deref().map(nexodesk_transport::parse_fingerprint).transpose()?,
        },
    };
    println!("{COPYRIGHT}");

    let started = Instant::now();
    let event_loop = EventLoop::<NetEvent>::with_user_event().build()?;
    if let Some(secs) = args.duration {
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            let _ = proxy.send_event(NetEvent::TestFinished);
        });
    }
    let mut app = App {
        keep_last_frame: args.snapshot.is_some(),
        last_frame: None,
        slot: Arc::new(FrameSlot::default()),
        started,
        report: Report::default(),
        connect: Some((target, args.password.clone())),
        proxy: event_loop.create_proxy(),
        input: None,
        net_thread: None,
        control: None,
        feedback_window: (Instant::now(), Vec::new()),
        view_only: args.view_only,
        window: None,
        renderer: None,
        toolbar: None,
        quality: nexodesk_proto::v1::Quality::Balanced,
        agent: args.agent.clone(),
        stats: Stats::new(),
        clock_offset_us: None,
        best_rtt_us: u64::MAX,
        rtt_us: 0,
        pressed_keys: HashSet::new(),
        exit_reason: None,
    };
    event_loop.run_app(&mut app)?;
    // Chiudere il canale dell'input termina la sessione: si attende (poco) che la
    // chiusura raggiunga l'agent.
    app.input = None;
    if let Some(thread) = app.net_thread.take() {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if let Some(reason) = &app.exit_reason {
        println!("{reason}");
    }
    if let Some(path) = &args.stats_file {
        app.report.exit_reason = app.exit_reason.clone();
        app.report.dropped = app.slot.dropped();
        std::fs::write(path, app.report.to_json(app.started))
            .with_context(|| format!("scrittura di {}", path.display()))?;
    }
    if let (Some(path), Some(frame)) = (&args.snapshot, &app.last_frame) {
        cpu::write_png(frame, path).with_context(|| format!("scrittura di {}", path.display()))?;
    }
    // Chiusura in ordine: barra e superficie wgpu, poi la finestra, poi il resto.
    // Lasciata all'ordine implicito, il viewer andava talvolta in crash in uscita.
    drop(app.toolbar.take());
    drop(app.renderer.take());
    drop(app.window.take());
    drop(app);
    Ok(())
}

struct App {
    /// Conserva una copia dell'ultimo frame per `--snapshot`.
    keep_last_frame: bool,
    last_frame: Option<nexodesk_codec::YuvFrame>,
    slot: Arc<FrameSlot>,
    started: Instant,
    report: Report,
    /// Agent a cui collegarsi appena la finestra è pronta.
    connect: Option<(net::Target, Option<String>)>,
    proxy: EventLoopProxy<NetEvent>,
    input: Option<UnboundedSender<InputEvent>>,
    net_thread: Option<std::thread::JoinHandle<()>>,
    /// Verso l'agent, per il bitrate adattivo: latenze dell'intervallo in corso.
    /// Messaggi di controllo verso l'agent (feedback, schermo, qualità).
    control: Option<UnboundedSender<nexodesk_proto::v1::control_message::Msg>>,
    feedback_window: (Instant, Vec<u32>),
    view_only: bool,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    toolbar: Option<toolbar::Toolbar>,
    quality: nexodesk_proto::v1::Quality,
    agent: String,
    stats: Stats,
    /// Orologio dell'agent meno il nostro (dalla misura con RTT minore).
    clock_offset_us: Option<i64>,
    best_rtt_us: u64,
    rtt_us: u64,
    pressed_keys: HashSet<String>,
    exit_reason: Option<String>,
}

impl App {
    fn send(&self, event: Event) {
        if let (false, Some(input)) = (self.view_only, &self.input) {
            let _ = input.send(InputEvent { event: Some(event) });
        }
    }

    /// Ogni 250 ms invia all'agent latenza mediana e minima dei frame mostrati.
    fn send_feedback(&mut self) {
        let (since, latencies) = &mut self.feedback_window;
        let window = since.elapsed();
        if window < Duration::from_millis(250) || latencies.is_empty() {
            return;
        }
        latencies.sort_unstable();
        let feedback = nexodesk_proto::v1::Feedback {
            latency_ms_p50: latencies[latencies.len() / 2],
            latency_ms_min: latencies[0],
            frames: latencies.len() as u32,
            window_ms: window.as_millis() as u32,
        };
        latencies.clear();
        *since = Instant::now();
        self.send_control(nexodesk_proto::v1::control_message::Msg::Feedback(feedback));
    }

    fn send_control(&self, msg: nexodesk_proto::v1::control_message::Msg) {
        if let Some(tx) = &self.control {
            let _ = tx.send(msg);
        }
    }

    fn ctrl_alt_held(&self) -> bool {
        let held = |a: &str, b: &str| self.pressed_keys.contains(a) || self.pressed_keys.contains(b);
        held("ControlLeft", "ControlRight") && held("AltLeft", "AltRight")
    }

    fn release_keys(&mut self) {
        for code in std::mem::take(&mut self.pressed_keys) {
            self.send(Event::Key(Key { code, down: false }));
        }
    }

    fn update_title(&self) {
        let Some(window) = &self.window else { return };
        let s = &self.stats;
        let latency = if self.clock_offset_us.is_some() && s.frames > 0 {
            format!("{:.0} ms", s.latency_us as f64 / s.frames as f64 / 1000.0)
        } else {
            "…".into()
        };
        let title = format!(
            "NexoDesk — {} · {:.0} fps · latenza {latency} · RTT {:.1} ms · {:.1} Mbit/s · comp. {:.1} ms · decomp. {:.1} ms",
            self.agent,
            s.fps(),
            self.rtt_us as f64 / 1000.0,
            s.mbps(),
            s.encode_us as f64 / s.frames.max(1) as f64 / 1000.0,
            s.decode_us as f64 / s.frames.max(1) as f64 / 1000.0,
        );
        debug!("{title}");
        window.set_title(&title);
        if let Some(toolbar) = &self.toolbar {
            let stats = format!("{:.0} fps · {latency}", s.fps());
            toolbar.update(&format!("{{stats:{}}}", toolbar::js_string(&stats)));
        }
    }

    fn handle_toolbar(&mut self, event_loop: &ActiveEventLoop, action: toolbar::Action) {
        use nexodesk_proto::v1::control_message::Msg;
        use toolbar::Action;
        match action {
            Action::SecureAttention => self.send(Event::SecureAttention(SecureAttention {})),
            Action::ViewOnly => {
                if !self.view_only {
                    // Prima di smettere di inviare, si rilascia ciò che è premuto.
                    self.release_keys();
                }
                self.view_only = !self.view_only;
                info!("solo visualizzazione: {}", self.view_only);
            }
            Action::Fullscreen => {
                if let Some(window) = &self.window {
                    let full = window.fullscreen().is_none();
                    window.set_fullscreen(full.then_some(winit::window::Fullscreen::Borderless(None)));
                }
            }
            Action::Display(index) => {
                self.send_control(Msg::SelectDisplay(nexodesk_proto::v1::SelectDisplay { index }));
            }
            Action::Quality(quality) => {
                self.quality = quality;
                info!("qualità {quality:?}");
                self.send_control(Msg::SetQuality(nexodesk_proto::v1::SetQuality { quality: quality as i32 }));
            }
            Action::Collapse | Action::Expand => {
                if let Some(toolbar) = self.toolbar.as_mut() {
                    toolbar.set_collapsed(action == Action::Collapse);
                }
            }
            Action::Close => {
                self.release_keys();
                self.exit_reason = Some("Sessione chiusa dall'utente".into());
                event_loop.exit();
                return;
            }
        }
        self.update_toolbar_state();
        if let Some(toolbar) = &self.toolbar {
            toolbar.focus_session();
        }
    }

    fn update_toolbar_state(&self) {
        let (Some(toolbar), Some(window)) = (&self.toolbar, &self.window) else { return };
        toolbar.update(&format!(
            "{{viewOnly:{},fullscreen:{},quality:{}}}",
            self.view_only,
            window.fullscreen().is_some(),
            toolbar::js_string(toolbar::quality_name(self.quality))
        ));
    }
}

impl ApplicationHandler<NetEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("NexoDesk — connessione a {}…", self.agent))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!("finestra non creata: {e}");
                event_loop.exit();
                return;
            }
        };
        match Renderer::new(window.clone()) {
            Ok((r, info)) => {
                info!("rendering con {info}");
                self.report.renderer = Some(info);
                self.renderer = Some(r);
            }
            Err(e) => {
                error!("rendering non disponibile: {e:#}");
                self.exit_reason = Some(format!("rendering non disponibile: {e:#}"));
                event_loop.exit();
                return;
            }
        }
        match toolbar::Toolbar::new(window.clone(), self.proxy.clone()) {
            Ok(toolbar) => {
                toolbar.update(&format!("{{name:{}}}", toolbar::js_string(&self.agent)));
                self.toolbar = Some(toolbar);
            }
            // La sessione resta usabile (Ctrl+Alt+Fine, chiusura della finestra).
            Err(e) => error!("barra degli strumenti non disponibile: {e:#}"),
        }
        self.window = Some(window);
        self.update_toolbar_state();
        // La connessione parte solo a finestra e rendering pronti: se la rete
        // arrivava prima, il canale video a volte non veniva mai accettato.
        if let Some((target, password)) = self.connect.take() {
            let (input, control, thread) =
                net::start(target, password, self.proxy.clone(), self.slot.clone());
            self.input = Some(input);
            self.control = Some(control);
            self.net_thread = Some(thread);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: NetEvent) {
        match event {
            NetEvent::Connected { agent, direct, width, height } => {
                info!("schermo remoto {width}x{height}");
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
                let Some(frame) = self.slot.take() else { return };
                if self.keep_last_frame {
                    let last = self.last_frame.get_or_insert_with(Default::default);
                    last.width = frame.yuv.width;
                    last.height = frame.yuv.height;
                    last.y.clone_from(&frame.yuv.y);
                    last.u.clone_from(&frame.yuv.u);
                    last.v.clone_from(&frame.yuv.v);
                }
                let Some(renderer) = self.renderer.as_mut() else { return };
                let t = Instant::now();
                renderer.upload(&frame.yuv);
                renderer.render();
                let render_us = t.elapsed().as_micros() as u64;
                tracing::trace!("frame mostrato in {render_us} µs");
                self.report.render_us += render_us;
                if let Some(offset) = self.clock_offset_us {
                    // Momento della cattura riportato sul nostro orologio.
                    let captured_here = frame.capture_us as i64 - offset;
                    let latency = nexodesk_proto::now_us() as i64 - captured_here;
                    self.stats.latency_us += latency.max(0) as u64;
                    self.report.latencies_us.push(latency.clamp(0, u32::MAX as i64) as u32);
                    self.feedback_window.1.push((latency.max(0) / 1000) as u32);
                }
                self.send_feedback();
                let r = &mut self.report;
                r.first_frame_ms.get_or_insert(self.started.elapsed().as_millis() as u64);
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
            NetEvent::Clock { rtt_us, offset_us } => {
                self.rtt_us = rtt_us;
                self.report.rtt_us.push(rtt_us.min(u32::MAX as u64) as u32);
                // La misura con RTT minore è la meno disturbata dalle code di rete.
                if rtt_us <= self.best_rtt_us {
                    self.best_rtt_us = rtt_us;
                    self.clock_offset_us = Some(offset_us);
                }
            }
            NetEvent::TestFinished => {
                info!("durata del test terminata");
                self.report.completed = true;
                self.exit_reason = Some("durata del test terminata".into());
                self.release_keys();
                event_loop.exit();
            }
            NetEvent::Displays { displays, current } => {
                if let Some(toolbar) = &self.toolbar {
                    toolbar.update(&toolbar::displays_state(&displays, current));
                }
            }
            NetEvent::Toolbar(action) => self.handle_toolbar(event_loop, action),
            NetEvent::Disconnected(reason) => {
                info!("disconnesso: {reason}");
                self.exit_reason = Some(format!("Sessione chiusa: {reason}"));
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
                self.update_toolbar_state();
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    r.render();
                }
            }
            WindowEvent::Focused(false) => self.release_keys(),
            WindowEvent::CursorMoved { position, .. } => {
                let Some(r) = &self.renderer else { return };
                if let Some((x, y)) = r.viewport().normalize(position.x, position.y) {
                    self.send(Event::MouseMove(MouseMove { x, y }));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
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
                // Unità di Windows: 120 per scatto. I touchpad danno pixel: ~40 px per scatto.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 * 3.0, p.y as f32 * 3.0),
                };
                self.send(Event::MouseWheel(MouseWheel { dx: dx as i32, dy: dy as i32 }));
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let code = w3c_code(code);
                let down = event.state == ElementState::Pressed;
                // Ctrl+Alt+Fine (come in Desktop remoto) = Ctrl+Alt+Canc sul dispositivo
                // controllato: Ctrl+Alt+Canc vero lo intercetta il sistema locale.
                if code == "End" && self.ctrl_alt_held() {
                    if down && !event.repeat {
                        info!("Ctrl+Alt+Fine: invio Ctrl+Alt+Canc");
                        self.send(Event::SecureAttention(SecureAttention {}));
                    }
                    return;
                }
                if down {
                    self.pressed_keys.insert(code.clone());
                } else {
                    self.pressed_keys.remove(&code);
                }
                self.send(Event::Key(Key { code, down }));
            }
            _ => {}
        }
    }
}

/// Nome W3C (`KeyboardEvent.code`) di un tasto winit. I nomi coincidono tranne
/// per il tasto Windows/Cmd, che winit chiama "Super".
fn w3c_code(code: winit::keyboard::KeyCode) -> String {
    match format!("{code:?}").as_str() {
        "SuperLeft" => "MetaLeft".into(),
        "SuperRight" => "MetaRight".into(),
        other => other.into(),
    }
}

struct Stats {
    since: Instant,
    frames: u32,
    bytes: usize,
    latency_us: u64,
    encode_us: u64,
    decode_us: u64,
}

impl Stats {
    fn new() -> Self {
        Self { since: Instant::now(), frames: 0, bytes: 0, latency_us: 0, encode_us: 0, decode_us: 0 }
    }

    fn fps(&self) -> f64 {
        self.frames as f64 / self.since.elapsed().as_secs_f64().max(0.001)
    }

    fn mbps(&self) -> f64 {
        self.bytes as f64 * 8.0 / self.since.elapsed().as_secs_f64().max(0.001) / 1e6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn super_becomes_meta() {
        assert_eq!(w3c_code(winit::keyboard::KeyCode::SuperLeft), "MetaLeft");
        assert_eq!(w3c_code(winit::keyboard::KeyCode::KeyA), "KeyA");
    }
}
