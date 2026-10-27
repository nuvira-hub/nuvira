//! Clipboard shared during the session: text and images.
//!
//! Viewer and agent check at intervals whether their system has new content on the
//! clipboard and send it to the other side, which writes it to its own. Received
//! content is not sent back (no ping-pong).
//!
//! On Windows a change is detected from the clipboard sequence number, without
//! reading it; elsewhere the content is compared. Images travel as PNG (a 1080p
//! screenshot is typically 0.2–2 MB instead of 8 MB of pixels).

use std::hash::{Hash, Hasher};

use anyhow::{Context, Result, bail};
use tracing::debug;

/// Longer text is not shared (it stays only on the local clipboard).
pub const MAX_TEXT_BYTES: usize = 1 << 20;
/// Larger images are not shared.
pub const MAX_IMAGE_PIXELS: usize = 8192 * 8192;
pub const MAX_IMAGE_PNG: usize = 32 << 20;

/// Something copied: text, or an image encoded as PNG.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text(String),
    Image(Vec<u8>),
}

pub struct Clipboard {
    inner: arboard::Clipboard,
    /// Last state seen: sequence number (Windows), text and image (hash of the pixels).
    last_seq: u32,
    last_text: Option<String>,
    last_image: Option<u64>,
}

impl Clipboard {
    /// Content already present when opened is not sent: only what is copied afterwards counts.
    pub fn new() -> Result<Self> {
        let mut inner = arboard::Clipboard::new().context("clipboard not available")?;
        let last_text = inner.get_text().ok();
        let last_image = inner.get_image().ok().map(|i| pixels_hash(&i.bytes));
        Ok(Self { inner, last_seq: sequence(), last_text, last_image })
    }

    /// Content copied locally since the last call, if any.
    pub fn poll(&mut self) -> Option<Content> {
        if cfg!(windows) {
            let seq = sequence();
            if seq == self.last_seq {
                return None;
            }
            self.last_seq = seq;
        }
        // Normal errors: clipboard held by another program, content of another kind.
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
        let image = self.inner.get_image().ok()?;
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
        }
        self.last_seq = sequence();
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
    mut outgoing: impl FnMut(Content) -> bool + Send + 'static,
) -> Result<std::sync::mpsc::Sender<Content>> {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let (incoming_tx, incoming_rx) = channel::<Content>();
    let (ready_tx, ready_rx) = channel();
    std::thread::Builder::new().name("clipboard".into()).spawn(move || {
        // The clipboard is opened in the thread that uses it.
        let mut clipboard = match Clipboard::new() {
            Ok(c) => {
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
                Ok(content) => {
                    if let Err(e) = clipboard.set(content) {
                        debug!("{e:#}");
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

#[cfg(windows)]
fn sequence() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
fn sequence() -> u32 {
    0
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
