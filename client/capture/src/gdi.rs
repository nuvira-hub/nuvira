//! Capture with GDI (BitBlt of the screen), when DXGI Desktop Duplication is not
//! available: some Remote Desktop sessions, virtual machines or drivers without it.
//! Slower (a full copy of the screen through the CPU at every frame) and without
//! change notifications: the image is compared with the previous one and reported
//! only if it changed. `NUVIRA_CAPTURE=gdi` forces it (tests).
//!
//! `BitBlt` leaves the pointer out: it is drawn into the copy with the system's own
//! drawing (`DrawIconEx`, inverting pointers included), always: this capture does
//! not give it apart to the viewers.

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
    /// `frame.data` holds a copy of the screen (the first copy is always reported).
    copied: bool,
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
        // The viewers do not get the pointer apart from this capture.
        if nuvira_pointer::active() {
            nuvira_pointer::set_active(false);
        }
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
            copied: false,
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
        self.draw_pointer();
        // SAFETY: the DIB section holds w*h*4 bytes, rows from the top.
        let src = unsafe { std::slice::from_raw_parts(self.pixels, self.frame.data.len()) };
        if self.copied && same_colours(src, &self.frame.data) {
            return Ok(false);
        }
        self.copied = true;
        self.frame.data.copy_from_slice(src);
        // GDI leaves the alpha channel at 0: opaque, as DXGI gives it.
        for alpha in self.frame.data.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        self.frame.captured_us = nuvira_proto::now_us();
        Ok(true)
    }
}

/// Whether the new DIB shows the same picture as the last frame: compared in place, alpha
/// left out (0 in the DIB, 255 in the frame), 8 bytes at a time. Replaces a SipHash of the
/// whole DIB at every poll (VID-13): a compare stops at the first difference and costs
/// less than the hash even on a still screen.
fn same_colours(dib: &[u8], frame: &[u8]) -> bool {
    const COLOURS: u64 = 0x00ff_ffff_00ff_ffff;
    if dib.len() != frame.len() {
        return false;
    }
    let (dib_words, dib_rest) = dib.as_chunks::<8>();
    let (frame_words, frame_rest) = frame.as_chunks::<8>();
    dib_words
        .iter()
        .zip(frame_words)
        .all(|(a, b)| (u64::from_le_bytes(*a) ^ u64::from_le_bytes(*b)) & COLOURS == 0)
        && dib_rest
            .chunks(4)
            .zip(frame_rest.chunks(4))
            .all(|(a, b)| a[..a.len().min(3)] == b[..b.len().min(3)])
}

impl GdiCapturer {
    /// The system's pointer, drawn into the copy where it is on this screen.
    fn draw_pointer(&self) {
        use windows::Win32::UI::WindowsAndMessaging::{
            CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON, ICONINFO,
        };
        let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if unsafe { GetCursorInfo(&mut info) }.is_err()
            || info.flags.0 & CURSOR_SHOWING.0 == 0
            || info.hCursor.is_invalid()
        {
            return;
        }
        let icon = HICON(info.hCursor.0);
        let mut icon_info = ICONINFO::default();
        if unsafe { GetIconInfo(icon, &mut icon_info) }.is_err() {
            return;
        }
        // GetIconInfo hands over copies of the two bitmaps.
        unsafe {
            if !icon_info.hbmMask.is_invalid() {
                let _ = DeleteObject(icon_info.hbmMask.into());
            }
            if !icon_info.hbmColor.is_invalid() {
                let _ = DeleteObject(icon_info.hbmColor.into());
            }
        }
        let x = info.ptScreenPos.x - icon_info.xHotspot as i32 - self.display.left;
        let y = info.ptScreenPos.y - icon_info.yHotspot as i32 - self.display.top;
        let _ = unsafe { DrawIconEx(self.memory, x, y, icon, 0, 0, 0, None, DI_NORMAL) };
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

#[cfg(test)]
mod tests {
    use super::same_colours;

    #[test]
    fn same_colours_ignores_only_alpha() {
        // 3 pixels (one in the tail after the 8-byte words), DIB alpha 0, frame alpha 255.
        let dib = [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0];
        let frame = [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255];
        assert!(same_colours(&dib, &frame));
        for i in [0, 1, 2, 4, 5, 6, 8, 9, 10] {
            let mut other = frame;
            other[i] ^= 1;
            assert!(!same_colours(&dib, &other), "byte {i}");
        }
        assert!(!same_colours(&dib, &frame[..8]));
    }
}
