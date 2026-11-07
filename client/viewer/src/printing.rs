//! Remote printing (docs/29-remote-printing.md): documents printed on the remote
//! computer's "NuviraHub" printer arrive as PDF (`NetEvent::PrintJob`) and are printed
//! here, on a printer of this computer.
//!
//! Every document opens the "Print" window (`print.html`): printer (the default one
//! chosen), print, save as PDF or not print. With "from now on without asking" the
//! choice is kept in `print.json` in the data folder and the next documents go to that
//! printer straight away; the app's settings ask again. `--print-to` and `--print-save`
//! (hidden, automated tests) do the same without the window.
//!
//! Printing: on Windows the PDF is drawn by Windows' own PDF renderer (Windows.Data.Pdf)
//! page by page at up to 300 dpi and sent to the printer through GDI, at real size when
//! the page fits the paper; on Linux and macOS it goes to CUPS (`lp`), which prints PDF.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::net::NetEvent;

const HTML: &str = include_str!("print.html");

/// Larger documents are refused (the agent does not send them either).
pub const MAX_DOCUMENT: u64 = 512 << 20;

/// This computer can print documents (the viewer says so to the agent).
pub fn available() -> bool {
    cfg!(windows) || unix::lp_available()
}

/// Printer chosen with "from now on without asking" (`print.json`, shared with the app).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub printer: String,
    #[serde(default)]
    pub automatic: bool,
}

impl Choice {
    pub fn path() -> PathBuf {
        nuvira_transport::identity::data_dir().join("print.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let path = Self::path();
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, serde_json::to_string(self).unwrap_or_default()));
        if let Err(e) = result {
            warn!("{} not saved: {e}", path.display());
        }
    }
}

/// Without the window, for automated tests.
pub enum Test {
    /// `--print-to PRINTER`.
    PrintTo(String),
    /// `--print-save DIR`.
    SaveIn(PathBuf),
}

/// A document waiting for an answer, in a temporary file deleted when it is done.
struct Job {
    id: u32,
    name: String,
    path: PathBuf,
    size: u64,
}

impl Drop for Job {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Ui {
    window: Arc<Window>,
    webview: WebView,
    _context: WebContext,
}

#[derive(Default)]
pub struct Printing {
    jobs: VecDeque<Job>,
    ui: Option<Ui>,
    next_id: u32,
    pub test: Option<Test>,
    /// Name of the remote computer, for the window.
    pub agent: String,
}

impl Printing {
    pub fn new(test: Option<Test>, agent: String) -> Self {
        Self { test, agent, ..Default::default() }
    }

    pub fn owns(&self, id: WindowId) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.window.id() == id)
    }

    /// A document arrived from the agent: printed at once (automatic choice, tests) or
    /// shown in the window. The text is for the notice in the session window.
    pub fn arrived(
        &mut self,
        name: String,
        data: Vec<u8>,
        event_loop: &ActiveEventLoop,
        proxy: &EventLoopProxy<NetEvent>,
    ) -> Option<String> {
        self.next_id += 1;
        let path = match save_temporary(self.next_id, &name, &data) {
            Ok(path) => path,
            Err(e) => {
                warn!("printed document not kept: {e:#}");
                return Some(nuvira_i18n::tf(
                    "viewer.print_failed",
                    &[("name", &name), ("reason", &format!("{e:#}"))],
                ));
            }
        };
        let job = Job { id: self.next_id, name, path, size: data.len() as u64 };
        info!(name = job.name, bytes = job.size, "document printed on the remote computer");
        match &self.test {
            Some(Test::PrintTo(printer)) => return Some(start_printing(job, printer.clone(), proxy)),
            Some(Test::SaveIn(dir)) => {
                let target = dir.join(file_name(&job.name));
                return Some(match std::fs::copy(&job.path, &target) {
                    Ok(_) => {
                        println!("@printed\t{}", target.display());
                        nuvira_i18n::tf(
                            "viewer.print_saved",
                            &[("name", &job.name), ("path", &target.display().to_string())],
                        )
                    }
                    Err(e) => nuvira_i18n::tf(
                        "viewer.print_failed",
                        &[("name", &job.name), ("reason", &e.to_string())],
                    ),
                });
            }
            None => {}
        }
        let choice = Choice::load();
        if choice.automatic && printers().0.contains(&choice.printer) {
            return Some(start_printing(job, choice.printer, proxy));
        }
        self.jobs.push_back(job);
        if let Err(e) = self.open(event_loop, proxy.clone()) {
            warn!("print window not opened: {e:#}");
        }
        self.push();
        None
    }

    fn open(&mut self, event_loop: &ActiveEventLoop, proxy: EventLoopProxy<NetEvent>) -> Result<()> {
        if let Some(ui) = &self.ui {
            ui.window.focus_window();
            return Ok(());
        }
        let attrs = crate::window_attributes()
            .with_title(nuvira_i18n::t("pr.title"))
            .with_inner_size(winit::dpi::LogicalSize::new(520.0, 330.0));
        let window = Arc::new(event_loop.create_window(attrs).context("print window")?);
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
                    let _ = proxy.send_event(NetEvent::PrintPage(request.body().clone()));
                })
                .build_as_child(&window)
        })
        .context("print WebView")?;
        window.focus_window();
        self.ui = Some(Ui { window, webview, _context: context });
        Ok(())
    }

    /// Message of the window's page; the text is for the notice in the session window.
    pub fn message(&mut self, body: &str, proxy: &EventLoopProxy<NetEvent>) -> Option<String> {
        let msg = serde_json::from_str::<Value>(body).ok()?;
        let id = msg["id"].as_u64().unwrap_or(0) as u32;
        let notice = match msg["op"].as_str() {
            Some("ready") => None,
            Some("print") => {
                let job = self.take(id)?;
                let printer = msg["printer"].as_str().unwrap_or_default().to_string();
                if msg["automatic"].as_bool() == Some(true) {
                    info!(printer, "documents of remote computers printed from now on without asking");
                    Choice { printer: printer.clone(), automatic: true }.save();
                }
                Some(start_printing(job, printer, proxy))
            }
            Some("save") => {
                let job = self.jobs.front().filter(|j| j.id == id)?;
                let target = rfd::FileDialog::new()
                    .set_title(nuvira_i18n::t("pr.save"))
                    .set_file_name(file_name(&job.name))
                    .add_filter("PDF", &["pdf"])
                    .save_file()?;
                let job = self.take(id)?;
                Some(match std::fs::copy(&job.path, &target) {
                    Ok(_) => nuvira_i18n::tf(
                        "viewer.print_saved",
                        &[("name", &job.name), ("path", &target.display().to_string())],
                    ),
                    Err(e) => nuvira_i18n::tf(
                        "viewer.print_failed",
                        &[("name", &job.name), ("reason", &e.to_string())],
                    ),
                })
            }
            Some("cancel") => {
                let job = self.take(id)?;
                info!(name = job.name, "remote document not printed");
                None
            }
            _ => None,
        };
        if self.jobs.is_empty() {
            self.ui = None;
        } else {
            self.push();
        }
        notice
    }

    /// Closing the window prints none of the documents waiting.
    pub fn window_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if !self.jobs.is_empty() {
                    info!(documents = self.jobs.len(), "print window closed: documents not printed");
                }
                self.jobs.clear();
                self.ui = None;
            }
            WindowEvent::Resized(_) => {
                if let Some(ui) = &self.ui {
                    let _ = ui.webview.set_bounds(full(&ui.window));
                }
            }
            _ => {}
        }
    }

    fn take(&mut self, id: u32) -> Option<Job> {
        let index = self.jobs.iter().position(|j| j.id == id)?;
        self.jobs.remove(index)
    }

    fn push(&self) {
        let Some(job) = self.jobs.front() else { return };
        let (list, default) = printers();
        let choice = Choice::load();
        // The printer chosen last time, if still there, otherwise the default one.
        let selected = Some(choice.printer).filter(|p| list.contains(p)).or(default);
        let state = json!({
            "id": job.id,
            "name": job.name,
            "size": job.size,
            "agent": self.agent,
            "waiting": self.jobs.len() - 1,
            "printers": list,
            "selected": selected,
        });
        if let Some(ui) = &self.ui {
            let _ = ui.webview.evaluate_script(&format!("pr.update({state})"));
        }
    }
}

/// Prints on a thread of its own and tells the event loop (`NetEvent::Printed`) how it went.
fn start_printing(job: Job, printer: String, proxy: &EventLoopProxy<NetEvent>) -> String {
    let text = nuvira_i18n::tf("viewer.print_printing", &[("name", &job.name), ("printer", &printer)]);
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        info!(name = job.name, printer, "printing the remote document");
        let result = print_file(&job.path, &printer, &job.name);
        if let Err(e) = &result {
            warn!(name = job.name, printer, "not printed: {e:#}");
        }
        let error = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
        if error.is_empty() {
            println!("@printed\t{printer}\t{}", job.name);
        }
        let _ = proxy.send_event(NetEvent::Printed { name: job.name.clone(), printer, error });
    });
    text
}

fn save_temporary(id: u32, name: &str, data: &[u8]) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("nuvira-print");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-{id}-{}", std::process::id(), file_name(name)));
    std::fs::write(&path, data).with_context(|| path.display().to_string())?;
    Ok(path)
}

/// `name` as a file name ending in .pdf, without the characters a system refuses.
fn file_name(name: &str) -> String {
    let clean: String =
        name.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let clean = clean.trim().trim_end_matches('.').chars().take(120).collect::<String>();
    let clean = if clean.is_empty() { "document".to_string() } else { clean };
    if clean.to_ascii_lowercase().ends_with(".pdf") { clean } else { format!("{clean}.pdf") }
}

fn full(window: &Window) -> Rect {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(size.width, size.height).into(),
    }
}

/// The printers of this computer and the default one.
pub fn printers() -> (Vec<String>, Option<String>) {
    #[cfg(windows)]
    return win::printers();
    #[cfg(not(windows))]
    return unix::printers();
}

fn print_file(path: &Path, printer: &str, title: &str) -> Result<()> {
    #[cfg(windows)]
    return win::print(path, printer, title);
    #[cfg(not(windows))]
    return unix::print(path, printer, title);
}

#[cfg(windows)]
mod win {
    use std::path::Path;

    use anyhow::{Context, Result, bail};
    use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
    use windows::Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
        ExifOrientationMode,
    };
    use windows::Storage::StorageFile;
    use windows::Storage::Streams::InMemoryRandomAccessStream;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDCW, DEVMODEW, DIB_RGB_COLORS, DM_ORIENTATION, DeleteDC,
        GET_DEVICE_CAPS_INDEX, GetDeviceCaps, HALFTONE, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, PHYSICALHEIGHT,
        PHYSICALOFFSETX, PHYSICALOFFSETY, PHYSICALWIDTH, ResetDCW, SRCCOPY, SetBrushOrgEx, SetStretchBltMode,
        StretchDIBits, VERTRES,
    };
    use windows::Win32::Graphics::Printing::{
        ClosePrinter, DocumentPropertiesW, EnumPrintersW, GetDefaultPrinterW, OpenPrinterW,
        PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL, PRINTER_HANDLE, PRINTER_INFO_5W,
    };
    use windows::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::{HSTRING, PCWSTR, PWSTR, w};

    /// Pages are drawn at most at this resolution (A4 at 300 dpi: 35 MB per page).
    const MAX_DPI: f64 = 300.0;
    const DMORIENT_PORTRAIT: i16 = 1;
    const DMORIENT_LANDSCAPE: i16 = 2;
    const DM_OUT_BUFFER: u32 = 2;

    pub fn printers() -> (Vec<String>, Option<String>) {
        let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
        let (mut needed, mut count) = (0u32, 0u32);
        let _ = unsafe { EnumPrintersW(flags, PCWSTR::null(), 5, None, &mut needed, &mut count) };
        let mut list = Vec::new();
        if needed > 0 {
            let mut buffer = vec![0u8; needed as usize];
            if unsafe { EnumPrintersW(flags, PCWSTR::null(), 5, Some(&mut buffer), &mut needed, &mut count) }
                .is_ok()
            {
                // SAFETY: the buffer holds `count` PRINTER_INFO_5W, followed by their strings.
                let infos = unsafe {
                    std::slice::from_raw_parts(buffer.as_ptr().cast::<PRINTER_INFO_5W>(), count as usize)
                };
                list = infos
                    .iter()
                    .filter_map(|p| unsafe {
                        Some((p.pPrinterName.to_string().ok()?, p.pPortName.to_string().ok()?))
                    })
                    // Printers that ask for a file name or a fax number would open their
                    // dialog from the printing thread ("Save as PDF" is in the window), and
                    // this computer's own NuviraHub printer would print nowhere.
                    .filter(|(name, port)| {
                        !matches!(port.as_str(), "PORTPROMPT:" | "SHRFAX:") && name != "NuviraHub"
                    })
                    .map(|(name, _)| name)
                    .collect();
            }
        }
        list.sort_by_key(|p| p.to_lowercase());
        let default = default_printer().filter(|d| list.contains(d));
        (list, default)
    }

    fn default_printer() -> Option<String> {
        let mut size = 0u32;
        let _ = unsafe { GetDefaultPrinterW(None, &mut size) };
        if size == 0 {
            return None;
        }
        let mut buffer = vec![0u16; size as usize];
        unsafe { GetDefaultPrinterW(Some(PWSTR(buffer.as_mut_ptr())), &mut size) }.ok().ok()?;
        Some(String::from_utf16_lossy(&buffer[..size.saturating_sub(1) as usize]))
    }

    /// The printer's default settings (paper, quality), to change the orientation.
    fn devmode(printer: &HSTRING) -> Option<Vec<u8>> {
        let mut handle = PRINTER_HANDLE::default();
        unsafe { OpenPrinterW(printer, &mut handle, None) }.ok()?;
        let size = unsafe { DocumentPropertiesW(None, handle, printer, None, None, 0) };
        let mut buffer = vec![0u8; size.max(0) as usize];
        let ok = size > 0
            && unsafe {
                DocumentPropertiesW(
                    None,
                    handle,
                    printer,
                    Some(buffer.as_mut_ptr().cast()),
                    None,
                    DM_OUT_BUFFER,
                )
            } >= 0;
        unsafe {
            let _ = ClosePrinter(handle);
        }
        ok.then_some(buffer)
    }

    struct Dc(HDC);

    impl Drop for Dc {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }

    pub fn print(path: &Path, printer: &str, title: &str) -> Result<()> {
        // A thread of its own: WinRT in the multithreaded apartment.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.join()?;
        let document = PdfDocument::LoadFromFileAsync(&file)?.join().context("reading the PDF")?;
        let pages = document.PageCount()?;
        let name = HSTRING::from(printer);
        let mut devmode = devmode(&name);
        let dm = |d: &Option<Vec<u8>>| d.as_ref().map(|b| b.as_ptr().cast::<DEVMODEW>());
        let dc = Dc(unsafe { CreateDCW(w!("WINSPOOL"), &name, PCWSTR::null(), dm(&devmode)) });
        if dc.0.is_invalid() {
            bail!("printer not available");
        }
        let title = HSTRING::from(title);
        // Tests: the printer's output into a file (e.g. "Microsoft Print to PDF" without its dialog).
        let output = std::env::var_os("NUVIRA_PRINT_OUTPUT").map(HSTRING::from);
        let info = DOCINFOW {
            cbSize: size_of::<DOCINFOW>() as i32,
            lpszDocName: PCWSTR(title.as_ptr()),
            lpszOutput: output.as_ref().map_or(PCWSTR::null(), |o| PCWSTR(o.as_ptr())),
            ..Default::default()
        };
        if unsafe { StartDocW(dc.0, &info) } <= 0 {
            bail!("the printer did not accept the document: {}", windows::core::Error::from_thread());
        }
        let result = (|| -> Result<()> {
            for index in 0..pages {
                let page = document.GetPage(index)?;
                let size = page.Size()?;
                // The orientation of the paper follows the page's.
                if let Some(buffer) = devmode.as_mut() {
                    let landscape = size.Width > size.Height;
                    // SAFETY: the buffer holds a DEVMODEW from DocumentPropertiesW.
                    let mode = unsafe { &mut *buffer.as_mut_ptr().cast::<DEVMODEW>() };
                    mode.dmFields |= DM_ORIENTATION;
                    unsafe {
                        mode.Anonymous1.Anonymous1.dmOrientation =
                            if landscape { DMORIENT_LANDSCAPE } else { DMORIENT_PORTRAIT };
                        ResetDCW(dc.0, mode);
                    }
                }
                let caps = |i: GET_DEVICE_CAPS_INDEX| unsafe { GetDeviceCaps(Some(dc.0), i) } as f64;
                let (dpi_x, dpi_y) = (caps(LOGPIXELSX), caps(LOGPIXELSY));
                let (paper_w, paper_h) = (caps(PHYSICALWIDTH), caps(PHYSICALHEIGHT));
                let (offset_x, offset_y) = (caps(PHYSICALOFFSETX), caps(PHYSICALOFFSETY));
                let (area_w, area_h) = (caps(HORZRES), caps(VERTRES));
                // Page size in printer pixels (PDF sizes are in 1/96 inch).
                let (page_w, page_h) = (size.Width as f64 / 96.0 * dpi_x, size.Height as f64 / 96.0 * dpi_y);
                // Real size when the page fits the paper, aligned to it; otherwise
                // smaller, to fit the printable area.
                let (x, y, w, h) = if page_w <= paper_w * 1.02 && page_h <= paper_h * 1.02 {
                    ((paper_w - page_w) / 2.0 - offset_x, (paper_h - page_h) / 2.0 - offset_y, page_w, page_h)
                } else {
                    let scale = (area_w / page_w).min(area_h / page_h);
                    let (w, h) = (page_w * scale, page_h * scale);
                    ((area_w - w) / 2.0, (area_h - h) / 2.0, w, h)
                };
                let render_dpi = dpi_x.min(MAX_DPI);
                let (bitmap_w, bitmap_h) = (
                    (size.Width as f64 / 96.0 * render_dpi) as u32,
                    (size.Height as f64 / 96.0 * render_dpi) as u32,
                );
                let (pixels, bw, bh) = render(&page, bitmap_w.max(1), bitmap_h.max(1))?;
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: bw as i32,
                        // Top-down rows.
                        biHeight: -(bh as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                if unsafe { StartPage(dc.0) } <= 0 {
                    bail!("page {}: {}", index + 1, windows::core::Error::from_thread());
                }
                unsafe {
                    SetStretchBltMode(dc.0, HALFTONE);
                    let _ = SetBrushOrgEx(dc.0, 0, 0, None);
                    StretchDIBits(
                        dc.0,
                        x.round() as i32,
                        y.round() as i32,
                        w.round() as i32,
                        h.round() as i32,
                        0,
                        0,
                        bw as i32,
                        bh as i32,
                        Some(pixels.as_ptr().cast()),
                        &bmi,
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    );
                }
                if unsafe { EndPage(dc.0) } <= 0 {
                    bail!("page {}: {}", index + 1, windows::core::Error::from_thread());
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                if unsafe { EndDoc(dc.0) } <= 0 {
                    bail!("{}", windows::core::Error::from_thread());
                }
                Ok(())
            }
            Err(e) => {
                unsafe { AbortDoc(dc.0) };
                Err(e)
            }
        }
    }

    /// A page as BGRA pixels, top-down, `width` x `height` (or what the renderer made).
    fn render(page: &windows::Data::Pdf::PdfPage, width: u32, height: u32) -> Result<(Vec<u8>, u32, u32)> {
        let options = PdfPageRenderOptions::new()?;
        options.SetDestinationWidth(width)?;
        options.SetDestinationHeight(height)?;
        let stream = InMemoryRandomAccessStream::new()?;
        page.RenderWithOptionsToStreamAsync(&stream, &options)?.join().context("drawing the page")?;
        let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
        let (w, h) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
        let provider = decoder
            .GetPixelDataTransformedAsync(
                BitmapPixelFormat::Bgra8,
                BitmapAlphaMode::Ignore,
                &BitmapTransform::new()?,
                ExifOrientationMode::IgnoreExifOrientation,
                ColorManagementMode::DoNotColorManage,
            )?
            .join()?;
        let pixels = provider.DetachPixelData()?;
        Ok((pixels.to_vec(), w, h))
    }
}

/// CUPS (Linux, macOS): it prints PDF itself.
mod unix {
    #[cfg(not(windows))]
    use std::path::Path;
    #[cfg(not(windows))]
    use std::process::Command;

    #[cfg(not(windows))]
    use anyhow::{Result, bail};

    #[cfg(windows)]
    pub fn lp_available() -> bool {
        false
    }

    #[cfg(not(windows))]
    pub fn lp_available() -> bool {
        Command::new("lpstat").arg("-r").env("LC_ALL", "C").output().is_ok()
    }

    #[cfg(not(windows))]
    pub fn printers() -> (Vec<String>, Option<String>) {
        let run = |args: &[&str]| {
            Command::new("lpstat")
                .args(args)
                .env("LC_ALL", "C")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        };
        // This computer's own NuviraHub printer would print nowhere.
        let ours = |p: &str| p == nuvira_cups_printer();
        let mut list: Vec<String> = run(&["-e"])
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !ours(l))
            .map(String::from)
            .collect();
        list.sort_by_key(|p| p.to_lowercase());
        // "system default destination: NAME"
        let default =
            run(&["-d"]).lines().find_map(|l| l.split_once(": ").map(|(_, n)| n.trim().to_string()));
        (list, default.filter(|d| !d.is_empty() && !ours(d)))
    }

    /// Same name as `nuvira_cups::PRINTER`.
    #[cfg(not(windows))]
    fn nuvira_cups_printer() -> &'static str {
        "NuviraHub"
    }

    #[cfg(not(windows))]
    pub fn print(path: &Path, printer: &str, title: &str) -> Result<()> {
        let output = Command::new("lp")
            .args(["-d", printer, "-t", title, "--"])
            .arg(path)
            .env("LC_ALL", "C")
            .output()?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(file_name("Report: Q3/2026"), "Report_ Q3_2026.pdf");
        assert_eq!(file_name("invoice.PDF"), "invoice.PDF");
        assert_eq!(file_name("  "), "document.pdf");
        assert_eq!(file_name("notes."), "notes.pdf");
    }

    #[test]
    fn choice_defaults_to_asking() {
        let c: Choice = serde_json::from_str("{}").unwrap();
        assert!(!c.automatic && c.printer.is_empty());
    }
}
