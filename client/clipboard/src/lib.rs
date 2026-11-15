//! Clipboard shared during the session: text, images and files.
//!
//! Viewer and agent check at intervals whether their system has new content on the
//! clipboard and send it to the other side, which writes it to its own. Received
//! content is not sent back (no ping-pong).
//!
//! On Windows a change is detected from the clipboard sequence number, without
//! reading it; elsewhere the content is compared, and a new copy of the same
//! files is told from the pasteboard's change count (macOS) or the CLIPBOARD
//! owner changing (X11, XFixes). Images travel as PNG (a 1080p
//! screenshot is typically 0.2–2 MB instead of 8 MB of pixels).
//!
//! Files and folders: the list goes to the other side, which offers them on its
//! clipboard; their contents travel only when they are pasted there (`share`).

use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub mod list;
pub mod share;
#[cfg(windows)]
mod win_files;
#[cfg(target_os = "linux")]
mod fuse_files;
#[cfg(target_os = "linux")]
mod x11_files;
#[cfg(target_os = "macos")]
mod mac_files;

pub use share::{RemoteFiles, Sharing};
#[cfg(windows)]
pub use win_files::{allow_session_user_calls, running_as_system};

use anyhow::{Context, Result, bail};
use tracing::debug;

/// Longer text is not shared (it stays only on the local clipboard).
pub const MAX_TEXT_BYTES: usize = 1 << 20;
/// Larger images are not shared.
pub const MAX_IMAGE_PIXELS: usize = 8192 * 8192;
pub const MAX_IMAGE_PNG: usize = 32 << 20;

/// Something copied: text, an image encoded as PNG, files and folders copied
/// here (`Files`, outgoing) or on the other side (`RemoteFiles`, incoming).
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text(String),
    Image(Vec<u8>),
    Files(Vec<PathBuf>),
    RemoteFiles(RemoteFiles),
}

/// Reads the clipboard another way when this process cannot: on Windows a
/// SYSTEM process sees what Explorer (and Word…) copied only as an OLE object,
/// so the service's agent asks a helper started as the signed-in person.
pub type Fallback = Box<dyn FnMut() -> Option<Content> + Send>;

/// The files and, without them, the text on the clipboard (the helper's side).
pub fn peek() -> (Vec<PathBuf>, Option<String>) {
    let Ok(mut clipboard) = arboard::Clipboard::new() else { return (Vec::new(), None) };
    let files = clipboard.get().file_list().unwrap_or_default();
    let text = if files.is_empty() { clipboard.get_text().ok() } else { None };
    (files, text)
}

/// Content to write, and what to do once it is in the clipboard (with `true`) or
/// the write failed (`false`): the agent presses the paste keys only then.
pub struct Incoming {
    pub content: Content,
    pub then: Option<Box<dyn FnOnce(bool) + Send>>,
}

impl From<Content> for Incoming {
    fn from(content: Content) -> Self {
        Self { content, then: None }
    }
}

pub struct Clipboard {
    inner: arboard::Clipboard,
    /// Last state seen: sequence number (see `Sequence`), text and image (hash of the pixels).
    seq: Sequence,
    last_seq: u32,
    last_text: Option<String>,
    last_image: Option<u64>,
    last_files: Option<Vec<PathBuf>>,
    /// Asked when the clipboard changed but nothing of it could be read here
    /// (Windows, as SYSTEM: see `Fallback`).
    fallback: Option<Fallback>,
    /// The other side's files on this clipboard (Windows: their own thread).
    #[cfg(windows)]
    virtual_files: Option<win_files::VirtualClipboard>,
    #[cfg(target_os = "linux")]
    x11_files: Option<x11_files::X11Offer>,
    #[cfg(target_os = "macos")]
    mac_files: Option<mac_files::MacOffer>,
}

impl Clipboard {
    /// Content already present when opened is not sent: only what is copied afterwards counts.
    pub fn new() -> Result<Self> {
        let mut inner = arboard::Clipboard::new().context("clipboard not available")?;
        let last_text = inner.get_text().ok();
        let last_image = inner.get_image().ok().map(|i| pixels_hash(&i.bytes));
        let last_files = inner.get().file_list().ok().filter(|f| !f.is_empty());
        let mut seq = Sequence::new();
        Ok(Self {
            inner,
            last_seq: seq.now(),
            seq,
            last_text,
            last_image,
            last_files,
            fallback: None,
            #[cfg(windows)]
            virtual_files: None,
            #[cfg(target_os = "linux")]
            x11_files: None,
            #[cfg(target_os = "macos")]
            mac_files: None,
        })
    }

    /// Content copied locally since the last call, if any.
    pub fn poll(&mut self) -> Option<Content> {
        let seq = self.seq.now();
        let copied_again = seq != self.last_seq;
        self.last_seq = seq;
        if cfg!(windows) && !copied_again {
            return None;
        }
        // Linux: still the other side's files offered here; reading them would
        // fetch them as if pasted and send them back.
        #[cfg(target_os = "linux")]
        if self.x11_files.as_ref().is_some_and(|x| x.is_owner()) {
            return None;
        }
        // Normal errors: clipboard held by another program, content of another kind.
        // Files first: a copied file often comes with its name as text too.
        match self.inner.get().file_list() {
            Ok(files) if !files.is_empty() => {
                // A new sequence number is a new copy, also of the same files
                // (copied again for a session started afterwards); without
                // one only different files are.
                if !copied_again && self.last_files.as_ref() == Some(&files) {
                    return None;
                }
                self.last_files = Some(files.clone());
                // The same text or image copied again afterwards is new again.
                self.last_text = None;
                self.last_image = None;
                return Some(Content::Files(files));
            }
            _ => self.last_files = None,
        }
        if let Ok(text) = self.inner.get_text() {
            if self.last_text.as_ref() == Some(&text) {
                return None;
            }
            self.last_text = Some(text.clone());
            if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                debug!(bytes = text.len(), "clipboard text not shared");
                return None;
            }
            return Some(Content::Text(text));
        }
        let Ok(image) = self.inner.get_image() else { return self.read_by_fallback() };
        let hash = pixels_hash(&image.bytes);
        if self.last_image == Some(hash) {
            return None;
        }
        self.last_image = Some(hash);
        match encode_png(image.width, image.height, &image.bytes) {
            Ok(png) => Some(Content::Image(png)),
            Err(e) => {
                debug!("clipboard image not shared: {e:#}");
                None
            }
        }
    }

    /// Writes content received from the other side, without reporting it as new afterwards.
    pub fn set(&mut self, content: Content) -> Result<()> {
        match content {
            Content::Text(text) => {
                self.inner.set_text(text.clone()).context("writing the clipboard")?;
                self.last_text = Some(text);
            }
            Content::Image(png) => {
                let (width, height, rgba) = decode_png(&png)?;
                self.last_image = Some(pixels_hash(&rgba));
                let image = arboard::ImageData { width, height, bytes: rgba.into() };
                self.inner.set_image(image).context("writing the clipboard")?;
            }
            Content::Files(_) => {}
            Content::RemoteFiles(files) => self.offer(files)?,
        }
        self.last_seq = self.seq.now();
        Ok(())
    }
}

impl Clipboard {
    /// Nothing readable here: what the fallback reads, as a new copy.
    fn read_by_fallback(&mut self) -> Option<Content> {
        let content = (self.fallback.as_mut()?)()?;
        match &content {
            Content::Files(files) => {
                self.last_files = Some(files.clone());
                self.last_text = None;
                self.last_image = None;
            }
            Content::Text(text) => {
                if self.last_text.as_ref() == Some(text) || text.len() > MAX_TEXT_BYTES {
                    return None;
                }
                self.last_text = Some(text.clone());
            }
            _ => {}
        }
        Some(content)
    }

    /// The other side's files, offered on this clipboard; their contents come
    /// when something pastes them. With no files ([`RemoteFiles::none`]) the
    /// ones offered before are taken off, if the clipboard still has them.
    #[cfg(windows)]
    fn offer(&mut self, files: RemoteFiles) -> Result<()> {
        if files.files.is_empty() {
            // Its thread empties the clipboard if it is still the owner.
            self.virtual_files = None;
            return Ok(());
        }
        if self.virtual_files.is_none() {
            self.virtual_files = Some(win_files::VirtualClipboard::spawn()?);
        }
        self.virtual_files.as_ref().expect("spawned").set(files);
        self.last_files = None;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn offer(&mut self, files: RemoteFiles) -> Result<()> {
        if files.files.is_empty() {
            // Lets the clipboard go if still its owner, and unmounts the files.
            self.x11_files = None;
            return Ok(());
        }
        if self.x11_files.is_none() {
            self.x11_files = Some(x11_files::X11Offer::spawn()?);
        }
        self.x11_files.as_ref().expect("spawned").set(files);
        self.last_files = None;
        Ok(())
    }

    /// macOS: placeholders on the pasteboard, filled where Finder pastes them
    /// (`mac_files`). They read back as these files: not a new local copy.
    #[cfg(target_os = "macos")]
    fn offer(&mut self, files: RemoteFiles) -> Result<()> {
        if files.files.is_empty() {
            if self.mac_files.take().is_some() && self.inner.get().file_list().ok() == self.last_files {
                let _ = self.inner.clear();
            }
            self.last_files = None;
            return Ok(());
        }
        if self.mac_files.is_none() {
            self.mac_files = Some(mac_files::MacOffer::spawn()?);
        }
        let top = self.mac_files.as_mut().expect("spawned").set(files)?;
        self.inner.set().file_list(&top[..]).context("writing the pasteboard")?;
        self.last_files = self.inner.get().file_list().ok().or(Some(top));
        self.last_text = None;
        self.last_image = None;
        Ok(())
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    fn offer(&mut self, files: RemoteFiles) -> Result<()> {
        debug!(entries = files.files.len(), "files of the other side: not offered on this system yet");
        Ok(())
    }
}

fn pixels_hash(rgba: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rgba.hash(&mut hasher);
    hasher.finish()
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    if width == 0 || height == 0 || width * height > MAX_IMAGE_PIXELS || rgba.len() != width * height * 4 {
        bail!("image {width}x{height} not shared");
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header()?.write_image_data(rgba)?;
    if png.len() > MAX_IMAGE_PNG {
        bail!("image of {} bytes not shared", png.len());
    }
    Ok(png)
}

/// PNG to RGBA, refusing images too large to be a clipboard's.
pub fn decode_png(png: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("not a PNG image")?;
    let (width, height) = (reader.info().width as usize, reader.info().height as usize);
    if width * height > MAX_IMAGE_PIXELS {
        bail!("image {width}x{height} too large");
    }
    let mut buffer = vec![0; reader.output_buffer_size().context("image size")?];
    let frame = reader.next_frame(&mut buffer)?;
    buffer.truncate(frame.buffer_size());
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => {
            buffer.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect()
        }
        png::ColorType::GrayscaleAlpha => {
            buffer.as_chunks::<2>().0.iter().flat_map(|&[g, a]| [g, g, g, a]).collect()
        }
        png::ColorType::Grayscale => buffer.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("palette images are expanded by the decoder"),
    };
    Ok((width, height, rgba))
}

/// Thread that checks the clipboard every 400 ms. Content copied locally goes to
/// `outgoing` (if it returns `false` the thread ends); content sent on the returned
/// channel is written to the clipboard. Closing the channel ends the thread.
pub fn spawn_sync(
    outgoing: impl FnMut(Content) -> bool + Send + 'static,
) -> Result<std::sync::mpsc::Sender<Incoming>> {
    spawn_sync_with(outgoing, None)
}

/// [`spawn_sync`], reading the clipboard through `fallback` when this process
/// cannot ([`Fallback`]).
pub fn spawn_sync_with(
    mut outgoing: impl FnMut(Content) -> bool + Send + 'static,
    fallback: Option<Fallback>,
) -> Result<std::sync::mpsc::Sender<Incoming>> {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let (incoming_tx, incoming_rx) = channel::<Incoming>();
    let (ready_tx, ready_rx) = channel();
    std::thread::Builder::new().name("clipboard".into()).spawn(move || {
        // The clipboard is opened in the thread that uses it.
        let mut clipboard = match Clipboard::new() {
            Ok(mut c) => {
                c.fallback = fallback;
                let _ = ready_tx.send(Ok(()));
                c
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        loop {
            match incoming_rx.recv_timeout(std::time::Duration::from_millis(400)) {
                Ok(Incoming { content, then }) => {
                    let written = clipboard.set(content).inspect_err(|e| debug!("{e:#}")).is_ok();
                    if let Some(then) = then {
                        then(written);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(content) = clipboard.poll()
                        && !outgoing(content)
                    {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    })?;
    ready_rx.recv().context("clipboard thread ended")??;
    Ok(incoming_tx)
}

/// A number that changes with every copy, whatever is copied: the clipboard
/// sequence number (Windows), the pasteboard's change count (macOS), how many
/// times the CLIPBOARD selection changed owner (X11, through XFixes; also
/// Wayland's clipboard, which XWayland mirrors). 0 when not available.
struct Sequence {
    #[cfg(target_os = "linux")]
    x11: Option<(x11rb::rust_connection::RustConnection, u32)>,
}

impl Sequence {
    fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            x11: x11_owner_changes().inspect_err(|e| debug!("clipboard owner changes not watched: {e:#}")).ok(),
        }
    }

    #[cfg(windows)]
    fn now(&mut self) -> u32 {
        unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
    }

    #[cfg(target_os = "macos")]
    fn now(&mut self) -> u32 {
        use objc2::rc::Retained;
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};
        // SAFETY: +[NSPasteboard generalPasteboard] and -changeCount, as arboard uses them.
        unsafe {
            let pasteboard: Option<Retained<AnyObject>> = msg_send![class!(NSPasteboard), generalPasteboard];
            let Some(pasteboard) = pasteboard else { return 0 };
            let count: isize = msg_send![&*pasteboard, changeCount];
            count as u32
        }
    }

    #[cfg(target_os = "linux")]
    fn now(&mut self) -> u32 {
        use x11rb::connection::Connection;
        let Some((conn, count)) = self.x11.as_mut() else { return 0 };
        while let Ok(Some(event)) = conn.poll_for_event() {
            if let x11rb::protocol::Event::XfixesSelectionNotify(_) = event {
                *count = count.wrapping_add(1);
            }
        }
        *count
    }
}

/// An X connection told by XFixes whenever the CLIPBOARD selection changes owner.
#[cfg(target_os = "linux")]
fn x11_owner_changes() -> Result<(x11rb::rust_connection::RustConnection, u32)> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
    use x11rb::protocol::xproto::ConnectionExt as _;
    let (conn, screen) = x11rb::connect(None).context("no X display")?;
    conn.xfixes_query_version(5, 0)?.reply().context("no XFixes")?;
    let root = conn.setup().roots[screen].root;
    let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    conn.xfixes_select_selection_input(root, clipboard, SelectionEventMask::SET_SELECTION_OWNER)?;
    conn.flush()?;
    Ok((conn, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let (w, h) = (3, 2);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7) as u8).collect();
        let png = encode_png(w, h, &rgba).expect("encode");
        assert_eq!(decode_png(&png).expect("decode"), (w, h, rgba));
        assert!(encode_png(0, 0, &[]).is_err());
        assert!(encode_png(2, 2, &[0; 3]).is_err());
        assert!(decode_png(b"not a png").is_err());
    }
}
