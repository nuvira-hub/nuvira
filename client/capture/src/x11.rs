//! X11 capture: the root window, the cursor drawn on top (XFixes).
//!
//! - **MIT-SHM**: the image comes through shared memory (`ShmGetImage`) instead of
//!   the X socket (`GetImage`, which copies the whole screen through it each
//!   time). Without the extension (a remote X server, a container) `GetImage`.
//! - **XDamage**: the screen is read only when X says something changed on it, or
//!   the pointer moved (it is not part of the root window's contents). Without the
//!   extension the screen is read 60 times a second and compared with the last image.
//! - Once a second the screen is read anyway and compared: if it changed with no
//!   damage reported (some drivers or compositors), XDamage is left aside after
//!   three such times and the screen is polled as without it.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::shm::ConnectionExt as _;
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

/// A System V shared memory segment attached to the X server.
struct Shm {
    seg: u32,
    addr: *mut u8,
    size: usize,
}

impl Shm {
    fn new(conn: &RustConnection, size: usize) -> Result<Self> {
        let version = conn.shm_query_version()?.reply().context("MIT-SHM")?;
        info!(major = version.major_version, minor = version.minor_version, "MIT-SHM");
        // SAFETY: plain System V calls; the address is checked and detached in Drop.
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if id < 0 {
                bail!("shmget: {}", std::io::Error::last_os_error());
            }
            let addr = libc::shmat(id, std::ptr::null(), 0);
            if addr as isize == -1 {
                let e = std::io::Error::last_os_error();
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                bail!("shmat: {e}");
            }
            let shm = Shm { seg: conn.generate_id()?, addr: addr.cast(), size };
            let attached = conn.shm_attach(shm.seg, id as u32, false).map(|c| c.check());
            // Removed now: it goes away when the last of the X server and this
            // process detaches, also if the process is killed.
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            match attached {
                Ok(Ok(())) => Ok(shm),
                Ok(Err(e)) => bail!("ShmAttach: {e}"),
                Err(e) => bail!("ShmAttach: {e}"),
            }
        }
    }

    fn data(&self) -> &[u8] {
        // SAFETY: `size` bytes mapped at `addr` while self lives; the X server
        // writes them only during ShmGetImage, whose reply was awaited.
        unsafe { std::slice::from_raw_parts(self.addr, self.size) }
    }
}

// Only the capture thread uses it.
unsafe impl Send for Shm {}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: attached in new(), detached once.
        unsafe { libc::shmdt(self.addr.cast()) };
    }
}

pub struct X11Capturer {
    conn: RustConnection,
    root: u32,
    display: Display,
    cursor: bool,
    shm: Option<Shm>,
    /// The Damage object on the root window, while it is trusted.
    damage: Option<u32>,
    /// Damage reported since the last read of the screen.
    damaged: bool,
    /// Changes found by the check of every second with no damage reported.
    missed: u32,
    last_check: Instant,
    frame: Option<Frame>,
    /// The image without the cursor, and the pointer position, to tell a still screen.
    last_screen: Vec<u8>,
    last_pointer: Option<(i32, i32)>,
    /// Next capture, at most 60 per second.
    next: Instant,
    /// The pointer as metadata for the viewers (nuvira_pointer), its last
    /// XFixes serial, and the generation of the pointer state last drawn or sent.
    metadata: bool,
    cursor_serial: Option<u32>,
    cursor_generation: u64,
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
        let shm = if std::env::var_os("NUVIRA_X11_NO_SHM").is_some() {
            None
        } else {
            Shm::new(&conn, w as usize * h as usize * 4)
                .inspect_err(|e| info!("X11 capture without shared memory: {e:#}"))
                .ok()
        };
        let damage = if std::env::var_os("NUVIRA_X11_NO_DAMAGE").is_some() {
            None
        } else {
            create_damage(&conn, root).inspect_err(|e| info!("X11 capture without XDamage: {e:#}")).ok()
        };
        let metadata = cursor && nuvira_pointer::wanted();
        nuvira_pointer::set_active(metadata);
        info!(width = w, height = h, cursor, metadata, shm = shm.is_some(), damage = damage.is_some(), "X11 capture");
        Ok(Self {
            conn,
            root,
            display: display(w, h),
            cursor,
            shm,
            damage,
            damaged: true,
            missed: 0,
            last_check: Instant::now(),
            frame: None,
            last_screen: Vec::new(),
            last_pointer: None,
            next: Instant::now(),
            metadata,
            cursor_serial: None,
            cursor_generation: u64::MAX,
        })
    }

    /// The pointer for the viewers that draw it: its image when it changed (XFixes,
    /// premultiplied ARGB, made straight BGRA) and its position.
    fn publish_cursor(&mut self) {
        let Some(c) = self.conn.xfixes_get_cursor_image().ok().and_then(|c| c.reply().ok()) else {
            return;
        };
        let serial = c.cursor_serial;
        let shape = (self.cursor_serial != Some(serial)).then(|| {
            let (w, h) = (u32::from(c.width).min(256), u32::from(c.height).min(256));
            let mut bgra = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                for x in 0..w {
                    let argb = c.cursor_image[(y * u32::from(c.width) + x) as usize];
                    let a = argb >> 24;
                    let straight = |shift: u32| if a == 0 { 0 } else { (((argb >> shift) & 0xFF) * 255 / a).min(255) as u8 };
                    bgra.extend_from_slice(&[straight(0), straight(8), straight(16), a as u8]);
                }
            }
            nuvira_pointer::Shape {
                serial,
                width: w,
                height: h,
                hot_x: u32::from(c.xhot).min(w.saturating_sub(1)),
                hot_y: u32::from(c.yhot).min(h.saturating_sub(1)),
                bgra: std::sync::Arc::new(bgra),
            }
        });
        self.cursor_serial = Some(serial);
        nuvira_pointer::publish(
            shape,
            nuvira_pointer::Position { x: i32::from(c.x), y: i32::from(c.y), visible: true, serial },
        );
    }

    /// Takes the events waiting: any damage notice means the screen changed.
    fn drain_events(&mut self) -> Result<()> {
        while let Some(event) = self.conn.poll_for_event()? {
            if let Event::DamageNotify(_) = event {
                self.damaged = true;
            }
        }
        Ok(())
    }

    /// The screen into `last_screen` when it differs from it; whether it did.
    fn read_screen(&mut self) -> Result<bool> {
        let (w, h) = (self.display.width as u16, self.display.height as u16);
        let size = usize::from(w) * usize::from(h) * 4;
        // Depth 24/32 in Z format: 4 bytes per pixel, B G R X on little-endian.
        if let Some(shm) = &self.shm {
            self.conn
                .shm_get_image(self.root, 0, 0, w, h, !0, ImageFormat::Z_PIXMAP.into(), shm.seg, 0)?
                .reply()
                .context("X11 ShmGetImage")?;
            let data = &shm.data()[..size];
            if data == self.last_screen.as_slice() {
                return Ok(false);
            }
            self.last_screen.clear();
            self.last_screen.extend_from_slice(data);
            return Ok(true);
        }
        let image = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, !0)?
            .reply()
            .context("X11 GetImage")?;
        if image.data.len() < size {
            bail!("X11 image too small");
        }
        if image.data == self.last_screen {
            return Ok(false);
        }
        self.last_screen = image.data;
        Ok(true)
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

/// A Damage object on the root window: one notice each time its damaged region
/// goes from empty to not empty (emptied again with DamageSubtract).
fn create_damage(conn: &RustConnection, root: u32) -> Result<u32> {
    conn.damage_query_version(1, 1)?.reply().context("XDamage")?;
    let damage = conn.generate_id()?;
    conn.damage_create(damage, root, ReportLevel::NON_EMPTY)?.check().context("DamageCreate")?;
    Ok(damage)
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
        self.drain_events()?;
        let mut cursor_moved = self.cursor && self.cursor_changed();
        // The viewers draw the pointer: it goes to them apart, the picture does not
        // change for it. When it must be in the picture again, a new picture.
        let apart = self.metadata && !nuvira_pointer::composite();
        if apart && cursor_moved {
            self.publish_cursor();
        }
        let generation = nuvira_pointer::generation();
        let switched = self.metadata && !apart && generation != self.cursor_generation;
        if apart {
            cursor_moved = false;
        }
        cursor_moved |= switched;
        let check = self.last_check.elapsed() >= Duration::from_secs(1);
        if self.damage.is_some() && !self.damaged && !cursor_moved && !check && self.frame.is_some() {
            return Ok(None);
        }
        if check {
            self.last_check = Instant::now();
        }
        let damaged = std::mem::take(&mut self.damaged);
        if let Some(damage) = self.damage {
            // Emptied before reading: what changes from here is reported again.
            self.conn.damage_subtract(damage, x11rb::NONE, x11rb::NONE)?;
        }
        let changed = self.read_screen()?;
        if changed && !damaged && self.damage.is_some() && self.frame.is_some() {
            // A change while reading may be reported just now.
            self.drain_events()?;
            if !std::mem::take(&mut self.damaged) {
                self.missed += 1;
                if self.missed >= 3 {
                    warn!("XDamage does not report the changes of this screen: polling it instead");
                    if let Some(damage) = self.damage.take() {
                        let _ = self.conn.damage_destroy(damage);
                    }
                }
            }
        }
        if !changed && !cursor_moved && self.frame.is_some() {
            return Ok(None);
        }
        let mut data = self.frame.take().map(|f| f.data).unwrap_or_default();
        data.clone_from(&self.last_screen);
        if self.cursor && !apart {
            self.draw_cursor(&mut data);
            self.cursor_generation = generation;
        } else if apart && self.cursor_serial.is_none() {
            // The first picture: the pointer goes to the viewers at once.
            self.publish_cursor();
        }
        let (w, h) = (self.display.width, self.display.height);
        self.frame = Some(Frame {
            width: w,
            height: h,
            stride: w as usize * 4,
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

impl Drop for X11Capturer {
    fn drop(&mut self) {
        if let Some(damage) = self.damage.take() {
            let _ = self.conn.damage_destroy(damage);
        }
        if let Some(shm) = &self.shm {
            let _ = self.conn.shm_detach(shm.seg);
        }
        let _ = self.conn.flush();
    }
}

/// Opens capture of the whole X screen.
pub fn open() -> Result<Box<dyn Capturer>> {
    Ok(Box::new(X11Capturer::new()?))
}
