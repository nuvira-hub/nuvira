//! File manager: a second window with this PC on the left and the remote one on
//! the right (`filemanager.html`). Remote folders are listed by the agent
//! (`FsRequest`), files go both ways on their own streams, and an interrupted
//! transfer can be resumed from where it stopped (the `.part` left on the
//! receiving side).
//!
//! The page sends JSON messages (`{"op": "list", ...}`), which reach the event loop
//! as [`NetEvent::FileManager`]; the state goes back with `fm.update({...})`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use nexodesk_proto::v1::control_message::Msg;
use nexodesk_proto::v1::{FsDownload, FsEntry, FsRequest, FsResponse, FsUploadOffset, fs_request};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::net::{Downloads, NetEvent, Outgoing, part_path};

const HTML: &str = include_str!("filemanager.html");

/// Channels of the session, set once connected.
pub struct Link {
    pub control: UnboundedSender<Msg>,
    pub files: UnboundedSender<Outgoing>,
    pub downloads: Downloads,
}

#[derive(Debug, Clone, Serialize)]
struct Transfer {
    id: u32,
    upload: bool,
    name: String,
    /// Local file: the one sent, or the one being written.
    local: PathBuf,
    /// Remote side: destination folder (upload) or file (download).
    remote: String,
    done: u64,
    total: u64,
    /// "running", "done", "failed".
    state: &'static str,
    /// Final path, or the reason of the failure.
    message: String,
}

/// What an answer of the agent is for.
enum Pending {
    List,
    UploadOffset(u32),
    Download(u32),
}

struct Ui {
    window: Arc<Window>,
    webview: WebView,
    _context: WebContext,
}

#[derive(Default)]
pub struct FileManager {
    ui: Option<Ui>,
    link: Option<Link>,
    next_id: u32,
    pending: HashMap<u32, Pending>,
    transfers: Vec<Transfer>,
}

impl FileManager {
    pub fn connect(&mut self, link: Link) {
        self.link = Some(link);
    }

    pub fn owns(&self, id: WindowId) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.window.id() == id)
    }

    /// Opens the window, or brings it to the front.
    pub fn open(&mut self, event_loop: &ActiveEventLoop, proxy: EventLoopProxy<NetEvent>) -> Result<()> {
        if let Some(ui) = &self.ui {
            ui.window.focus_window();
            return Ok(());
        }
        let attrs = Window::default_attributes()
            .with_title(nexodesk_i18n::t("fm.title"))
            .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 640.0));
        let window = Arc::new(event_loop.create_window(attrs).context("file manager window")?);
        let data_dir =
            std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("NexoDesk").join("WebView2"));
        let mut context = WebContext::new(data_dir);
        let webview = WebViewBuilder::new_with_web_context(&mut context)
            .with_html(HTML)
            .with_background_color((0x1f, 0x25, 0x2b, 0xff))
            .with_bounds(full(&window))
            .with_initialization_script(format!("window.I18N = {};", nexodesk_i18n::json()))
            .with_ipc_handler(move |request| {
                let _ = proxy.send_event(NetEvent::FileManager(request.body().clone()));
            })
            .build_as_child(&window)
            .context("file manager WebView")?;
        self.ui = Some(Ui { window, webview, _context: context });
        Ok(())
    }

    pub fn window_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.ui = None,
            WindowEvent::Resized(_) => {
                if let Some(ui) = &self.ui {
                    let _ = ui.webview.set_bounds(full(&ui.window));
                }
            }
            _ => {}
        }
    }

    /// Message from the page.
    pub fn message(&mut self, body: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(body) else { return };
        let text = |key: &str| msg[key].as_str().unwrap_or_default().to_string();
        match msg["op"].as_str() {
            // The page is ready: current transfers (after a reopening).
            Some("ready") => self.push_transfers(),
            Some("list") if msg["side"] == "local" => {
                let path = text("path");
                let update = match list_local(&path) {
                    Ok(entries) => json!({ "local": { "path": path, "entries": entries } }),
                    Err(e) => json!({ "local": { "path": path, "error": format!("{e:#}") } }),
                };
                self.push(&update);
            }
            Some("list") => {
                let path = text("path");
                self.request(fs_request::Op::List(path), Pending::List);
            }
            Some("upload") => {
                let local = PathBuf::from(text("local"));
                self.upload(local, text("dir"), false);
            }
            Some("download") => {
                let size = msg["size"].as_u64().unwrap_or(0);
                self.download(text("remote"), size, PathBuf::from(text("dir")), false);
            }
            Some("resume") => {
                let id = msg["id"].as_u64().unwrap_or(0) as u32;
                if let Some(pos) = self.transfers.iter().position(|t| t.id == id && t.state == "failed") {
                    let old = self.transfers.remove(pos);
                    if old.upload {
                        self.upload(old.local, old.remote, true);
                    } else {
                        let dir = old.local.parent().map(Path::to_path_buf).unwrap_or_default();
                        self.download(old.remote, old.total, dir, true);
                    }
                }
            }
            Some("clear") => {
                self.transfers.retain(|t| t.state == "running");
                self.push_transfers();
            }
            _ => {}
        }
    }

    /// Sends `local` into the remote folder `dir`; with `resume` it continues what an
    /// earlier attempt left there.
    pub fn upload(&mut self, local: PathBuf, dir: String, resume: bool) {
        let id = self.next();
        let total = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
        let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        info!("upload {} to {dir} (resume: {resume})", local.display());
        self.transfers.push(Transfer {
            id,
            upload: true,
            name: name.clone(),
            local: local.clone(),
            remote: dir.clone(),
            done: 0,
            total,
            state: "running",
            message: String::new(),
        });
        if resume {
            self.request_as(id, fs_request::Op::UploadOffset(FsUploadOffset { dir, name }), Pending::UploadOffset(id));
        } else {
            self.start_upload(id, 0);
        }
        self.push_transfers();
    }

    fn start_upload(&mut self, id: u32, offset: u64) {
        let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) else { return };
        t.done = offset;
        let outgoing = Outgoing::Upload { id, local: t.local.clone(), dir: t.remote.clone(), offset };
        if let Some(link) = &self.link {
            let _ = link.files.send(outgoing);
        }
    }

    /// Downloads the remote file `remote` into the local folder `dir`; with `resume` it
    /// continues the `.part` an earlier attempt left there.
    pub fn download(&mut self, remote: String, total: u64, dir: PathBuf, resume: bool) {
        let id = self.next();
        let name = remote.rsplit(['\\', '/']).next().unwrap_or_default().to_string();
        let local = dir.join(&name);
        let offset = if resume { std::fs::metadata(part_path(&local)).map(|m| m.len()).unwrap_or(0) } else { 0 };
        info!("download {remote} to {} (from {offset})", local.display());
        self.transfers.push(Transfer {
            id,
            upload: false,
            name,
            local: local.clone(),
            remote: remote.clone(),
            done: offset,
            total,
            state: "running",
            message: String::new(),
        });
        if let Some(link) = &self.link {
            link.downloads.lock().expect("downloads").insert(id, local);
        }
        self.request_as(id, fs_request::Op::Download(FsDownload { path: remote, offset }), Pending::Download(id));
        self.push_transfers();
    }

    /// Answer of the agent.
    pub fn response(&mut self, answer: FsResponse) {
        let Some(pending) = self.pending.remove(&answer.id) else { return };
        let error = (!answer.error.is_empty()).then(|| nexodesk_i18n::message(&answer.error));
        match pending {
            Pending::List => {
                let update = match error {
                    Some(error) => json!({ "remote": { "path": answer.path, "error": error } }),
                    None => json!({ "remote": { "path": answer.path, "entries": entries_json(&answer.entries) } }),
                };
                self.push(&update);
            }
            Pending::UploadOffset(id) => match error {
                Some(error) => self.finished(id, false, error),
                None => self.start_upload(id, answer.offset),
            },
            Pending::Download(id) => {
                if let Some(error) = error {
                    if let Some(link) = &self.link {
                        link.downloads.lock().expect("downloads").remove(&id);
                    }
                    self.finished(id, false, error);
                }
            }
        }
    }

    pub fn progress(&mut self, id: u32, done: u64, total: u64) {
        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
            t.done = done;
            t.total = total;
        }
        self.push_transfers();
    }

    pub fn finished(&mut self, id: u32, ok: bool, message: String) {
        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
            t.state = if ok { "done" } else { "failed" };
            if ok {
                t.done = t.total;
            }
            t.message = message;
            if ok {
                info!("transfer {id} done: {}", t.message);
            } else {
                warn!("transfer {id} failed: {}", t.message);
            }
        }
        self.push_transfers();
    }

    /// No transfer running (automated tests wait for this).
    pub fn idle(&self) -> bool {
        self.transfers.iter().all(|t| t.state != "running")
    }

    fn next(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn request(&mut self, op: fs_request::Op, pending: Pending) {
        let id = self.next();
        self.request_as(id, op, pending);
    }

    fn request_as(&mut self, id: u32, op: fs_request::Op, pending: Pending) {
        self.pending.insert(id, pending);
        if let Some(link) = &self.link {
            let _ = link.control.send(Msg::FsRequest(FsRequest { id, op: Some(op) }));
        }
    }

    fn push_transfers(&self) {
        self.push(&json!({ "transfers": self.transfers }));
    }

    fn push(&self, update: &Value) {
        if let Some(ui) = &self.ui {
            let _ = ui.webview.evaluate_script(&format!("window.fm && fm.update({update})"));
        }
    }
}

fn full(window: &Window) -> Rect {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    Rect { position: LogicalPosition::new(0.0, 0.0).into(), size: LogicalSize::new(size.width, size.height).into() }
}

fn entries_json(entries: &[FsEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|e| json!({ "name": e.name, "path": e.path, "dir": e.dir, "size": e.size, "modified": e.modified }))
            .collect(),
    )
}

/// This PC: "" = known folders and drives, otherwise the folder, folders first.
fn list_local(path: &str) -> Result<Value> {
    let mut entries = Vec::new();
    if path.is_empty() {
        if let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
            for name in ["Desktop", "Documents", "Downloads"] {
                let p = home.join(name);
                if p.is_dir() {
                    entries.push(FsEntry { name: name.into(), path: p.to_string_lossy().into(), dir: true, ..Default::default() });
                }
            }
        }
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if Path::new(&drive).exists() {
                entries.push(FsEntry { name: drive.trim_end_matches('\\').into(), path: drive, dir: true, ..Default::default() });
            }
        }
        return Ok(entries_json(&entries));
    }
    for item in std::fs::read_dir(path).with_context(|| format!("reading {path}"))?.flatten() {
        let Ok(meta) = item.metadata() else { continue };
        let modified =
            meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        entries.push(FsEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: item.path().to_string_lossy().into_owned(),
            dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: modified as i64,
        });
    }
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries_json(&entries))
}
