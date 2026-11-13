//! X11 capture: the root window read with `GetImage`, the cursor drawn on top
//! (XFixes). A first, simple version: no shared memory and no XDamage, so a
//! still screen is detected by comparing the image with the previous one.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tracing::info;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

use crate::{Capturer, Display, Frame};

/// The whole X screen, as one display.
pub fn displays() -> Result<Vec<Display>> {
    let (conn, screen) = x11rb::connect(None).context("X11 connection")?;
    let root = &conn.setup().roots[screen];
    Ok(vec![display(root.width_in_pixels.into(), root.height_in_pixels.into())])
}

fn display(width: u32, height: u32) -> Display {
    Display { index: 0, name: "Screen".into(), left: 0, top: 0, width, height, primary: true }
}

pub struct X11Capturer {
    conn: RustConnection,
    root: u32,
    display: Display,
    cursor: bool,
    frame: Option<Frame>,
    /// The image without the cursor, and the pointer position, to tell a still screen.
    last_screen: Vec<u8>,
    last_pointer: Option<(i32, i32)>,
    /// Next capture, at most 60 per second.
    next: Instant,
}

impl X11Capturer {
    pub fn new() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("X11 connection")?;
        let root = &conn.setup().roots[screen];
        if root.root_depth != 24 && root.root_depth != 32 {
            bail!("X screen with depth {} is not supported", root.root_depth);
        }
        let (root, w, h) = (root.root, root.width_in_pixels.into(), root.height_in_pixels.into());
        let cursor = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
        info!(width = w, height = h, cursor, "X11 capture");
        Ok(Self {
            conn,
            root,
            display: display(w, h),
            cursor,
            frame: None,
            last_screen: Vec::new(),
            last_pointer: None,
            next: Instant::now(),
        })
    }

    /// Draws the cursor (ARGB, premultiplied) onto the BGRA image.
    fn draw_cursor(&self, data: &mut [u8]) {
        let Some(cursor) = self.conn.xfixes_get_cursor_image().ok().and_then(|c| c.reply().ok()) else {
            return;
        };
        let (w, h) = (self.display.width as i32, self.display.height as i32);
        let left = i32::from(cursor.x) - i32::from(cursor.xhot);
        let top = i32::from(cursor.y) - i32::from(cursor.yhot);
        for cy in 0..i32::from(cursor.height) {
            let y = top + cy;
            if !(0..h).contains(&y) {
                continue;
            }
            for cx in 0..i32::from(cursor.width) {
                let x = left + cx;
                if !(0..w).contains(&x) {
                    continue;
                }
                let argb = cursor.cursor_image[(cy * i32::from(cursor.width) + cx) as usize];
                let alpha = argb >> 24;
                if alpha == 0 {
                    continue;
                }
                let i = ((y * w + x) * 4) as usize;
                let px = &mut data[i..i + 3];
                for (c, shift) in px.iter_mut().zip([0u32, 8, 16]) {
                    let src = (argb >> shift) & 0xFF;
                    *c = (src + u32::from(*c) * (255 - alpha) / 255).min(255) as u8;
                }
            }
        }
    }
}

impl Capturer for X11Capturer {
    fn display(&self) -> &Display {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<&Frame>> {
        let wait = self.next.saturating_duration_since(Instant::now());
        if wait > timeout {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        std::thread::sleep(wait);
        self.next = Instant::now() + Duration::from_millis(16);
        let (w, h) = (self.display.width as u16, self.display.height as u16);
        let image = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, !0)?
            .reply()
            .context("X11 GetImage")?;
        // Depth 24/32 in Z format: 4 bytes per pixel, B G R X on little-endian.
        let mut data = image.data;
        if data.len() < usize::from(w) * usize::from(h) * 4 {
            bail!("X11 image too small");
        }
        let cursor_moved = self.cursor && self.cursor_changed();
        if data == self.last_screen && !cursor_moved {
            return Ok(None);
        }
        self.last_screen.clone_from(&data);
        if self.cursor {
            self.draw_cursor(&mut data);
        }
        self.frame = Some(Frame {
            width: w.into(),
            height: h.into(),
            stride: usize::from(w) * 4,
            data,
            captured_us: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64),
            dmabuf: None,
        });
        Ok(self.frame.as_ref())
    }

    fn last_frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }
}

impl X11Capturer {
    /// Whether the pointer moved since the last image (it is not part of the
    /// root window's contents, so the image alone does not tell).
    fn cursor_changed(&mut self) -> bool {
        let Some(pointer) = self.conn.query_pointer(self.root).ok().and_then(|c| c.reply().ok()) else {
            return false;
        };
        let pos = (i32::from(pointer.root_x), i32::from(pointer.root_y));
        let moved = self.last_pointer != Some(pos);
        self.last_pointer = Some(pos);
        moved
    }
}

/// Opens capture of the whole X screen.
pub fn open() -> Result<Box<dyn Capturer>> {
    Ok(Box::new(X11Capturer::new()?))
}
