//! The system's pointer on macOS: its position (`CGEventCreate`, global points
//! with the origin at the top-left of the main display) and its shape
//! (`NSCursor.currentSystemCursor`, an image in points with its hot spot).
//!
//! `currentSystemCursor` is deprecated in favour of ScreenCaptureKit's
//! `showsCursor`, which only draws it into the picture; on macOS 27 it still
//! answers, also from a process without windows (tried 2026-11-14). When it does
//! not (`None`), the capture lets ScreenCaptureKit draw the pointer in.

use std::ffi::c_void;

use core_graphics::geometry::CGPoint;
use nuvira_pointer as cursor;
use objc2_app_kit::NSCursor;
use objc2_core_foundation::{CGRect, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little: BGRA in memory.
const BGRA_PREMULTIPLIED: u32 = 2 | (2 << 12);

/// Where the pointer is, in global points.
pub fn location() -> Option<(f64, f64)> {
    // SAFETY: a NULL source is allowed; the event is released below.
    let event = unsafe { CGEventCreate(std::ptr::null()) };
    if event.is_null() {
        return None;
    }
    // SAFETY: a valid event, released once.
    let point = unsafe { CGEventGetLocation(event) };
    unsafe { CFRelease(event) };
    Some((point.x, point.y))
}

/// The pointer's shape drawn at `scale` pixels per point (2 on Retina screens):
/// width, height, hot spot and straight (not premultiplied) BGRA.
pub fn shape(scale: f64) -> Option<(u32, u32, u32, u32, Vec<u8>)> {
    #[allow(deprecated)]
    let pointer = NSCursor::currentSystemCursor()?;
    let image = pointer.image();
    let size = image.size();
    let hot = pointer.hotSpot();
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    // Large pointers (accessibility sizes) are drawn smaller to fit.
    let side = f64::from(cursor::MAX_SIDE);
    let scale = scale.min(side / size.width).min(side / size.height);
    let (w, h) = ((size.width * scale).round().max(1.0) as usize, (size.height * scale).round().max(1.0) as usize);
    // SAFETY: NULL rect and context, no hints: the best representation.
    let cg = unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
    let space = CGColorSpace::new_device_rgb()?;
    let mut bgra = vec![0u8; w * h * 4];
    // SAFETY: `bgra` holds w*h*4 bytes and outlives the context, dropped below.
    let context = unsafe {
        CGBitmapContextCreate(bgra.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), BGRA_PREMULTIPLIED)
    }?;
    let rect = CGRect { origin: Default::default(), size: CGSize { width: w as f64, height: h as f64 } };
    CGContext::draw_image(Some(&context), rect, Some(&cg));
    drop(context);
    // The bitmap's first row is the top one. Premultiplied to straight alpha.
    for px in bgra.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let hot_x = ((hot.x * scale).round().max(0.0) as u32).min(w as u32 - 1);
    let hot_y = ((hot.y * scale).round().max(0.0) as u32).min(h as u32 - 1);
    Some((w as u32, h as u32, hot_x, hot_y, bgra))
}
