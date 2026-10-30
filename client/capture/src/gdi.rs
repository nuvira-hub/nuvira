//! Capture with GDI (BitBlt of the screen), when DXGI Desktop Duplication is not
//! available: some Remote Desktop sessions, virtual machines or drivers without it.
//! Slower (a full copy of the screen through the CPU at every frame) and without
//! change notifications: the image is compared with the previous one and reported
//! only if it changed. `NUVIRA_CAPTURE=gdi` forces it (tests).

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tracing::info;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};

use crate::{Capturer, Display, Frame};

/// GDI gives no notifications: the screen is copied at most this often.
const INTERVAL: Duration = Duration::from_millis(33);

pub struct GdiCapturer {
    display: Display,
    frame: Frame,
    memory: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pixels: *mut u8,
    last_hash: u64,
    next: Instant,
}

// The GDI handles are used by one thread at a time.
unsafe impl Send for GdiCapturer {}

impl GdiCapturer {
    pub fn new(display: Display) -> Result<Self> {
        let (w, h) = (display.width as i32, display.height as i32);
        if w <= 0 || h <= 0 {
            bail!("screen of size {w}x{h}");
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative: rows from the top, as the encoders expect.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = std::ptr::null_mut();
        let memory = unsafe { CreateCompatibleDC(None) };
        let bitmap = unsafe { CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut pixels, None, 0) }?;
        let old = unsafe { SelectObject(memory, bitmap.into()) };
        let name = &display.name;
        info!("GDI capture of {name} {w}x{h}");
        let stride = w as usize * 4;
        let frame = Frame {
            width: w as u32,
            height: h as u32,
            stride,
            data: vec![0; stride * h as usize],
            captured_us: 0,
            texture: None,
        };
        Ok(Self {
            display,
            frame,
            memory,
            bitmap,
            old,
            pixels: pixels.cast(),
            last_hash: 0,
            next: Instant::now(),
        })
    }

    fn copy(&mut self) -> Result<bool> {
        let screen = unsafe { GetDC(None) };
        let (w, h) = (self.frame.width as i32, self.frame.height as i32);
        let copied = unsafe {
            BitBlt(
                self.memory,
                0,
                0,
                w,
                h,
                Some(screen),
                self.display.left,
                self.display.top,
                SRCCOPY | CAPTUREBLT,
            )
        };
        unsafe { ReleaseDC(None, screen) };
        if copied.is_err() {
            // Desktop switch (UAC, lock screen): the copy works again on the new one.
            let _ = crate::desktop::follow_input_desktop();
            return Ok(false);
        }
        // SAFETY: the DIB section holds w*h*4 bytes, rows from the top.
        let src = unsafe { std::slice::from_raw_parts(self.pixels, self.frame.data.len()) };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        src.hash(&mut hasher);
        let hash = hasher.finish();
        if hash == self.last_hash {
            return Ok(false);
        }
        self.last_hash = hash;
        self.frame.data.copy_from_slice(src);
        // GDI leaves the alpha channel at 0: opaque, as DXGI gives it.
        for alpha in self.frame.data.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        self.frame.captured_us = nuvira_proto::now_us();
        Ok(true)
    }
}

impl Drop for GdiCapturer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memory, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.memory);
        }
    }
}

impl Capturer for GdiCapturer {
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
        self.next = Instant::now() + INTERVAL;
        Ok(self.copy()?.then_some(&self.frame))
    }

    fn last_frame(&self) -> Option<&Frame> {
        (self.frame.captured_us != 0).then_some(&self.frame)
    }
}

/// Screens as GDI sees them (when DXGI cannot list them).
pub fn displays() -> Vec<Display> {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HMONITOR};
    use windows::core::BOOL;

    unsafe extern "system" fn add(_: HMONITOR, _: HDC, rect: *mut RECT, list: LPARAM) -> BOOL {
        let list = unsafe { &mut *(list.0 as *mut Vec<Display>) };
        let r = unsafe { *rect };
        list.push(Display {
            index: list.len(),
            name: format!("screen {}", list.len() + 1),
            left: r.left,
            top: r.top,
            width: (r.right - r.left) as u32,
            height: (r.bottom - r.top) as u32,
            primary: r.left == 0 && r.top == 0,
        });
        true.into()
    }
    let mut list: Vec<Display> = Vec::new();
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(add), LPARAM(&mut list as *mut _ as isize)) };
    list
}
