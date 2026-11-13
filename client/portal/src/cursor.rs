//! The remote pointer as metadata (Linux): instead of being drawn into the picture
//! by the compositor, its image and position come apart, and the viewers that can
//! draw it themselves get them (`CursorShape`, `CursorPosition` of the session
//! protocol): the pointer moves in the viewer without waiting for a video frame,
//! and a still screen with a moving pointer sends no video.
//!
//! The agent asks for it before the capture opens ([`set_wanted`]); the screen
//! cast (portal, Mutter, KWin) asks the compositor for the cursor as metadata and
//! the capture publishes what it reads ([`publish`]). When someone needs the
//! pointer in the picture after all (a recording, a viewer that cannot draw it),
//! the agent sets [`set_composite`] and the capture draws it in itself.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The pointer's image: BGRA, not premultiplied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shape {
    pub serial: u32,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub bgra: Arc<Vec<u8>>,
}

/// Where the pointer is, in pixels of the captured picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// The serial of its [`Shape`].
    pub serial: u32,
}

static WANTED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static COMPOSITE: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static STATE: Mutex<(Option<Shape>, Position)> = Mutex::new((None, Position { x: 0, y: 0, visible: false, serial: 0 }));

/// Before the capture opens: ask the compositor for the pointer as metadata.
pub fn set_wanted(on: bool) {
    WANTED.store(on, Ordering::Relaxed);
}

/// The screen cast asks for the pointer as metadata.
pub fn wanted() -> bool {
    WANTED.load(Ordering::Relaxed)
}

/// The capture in use gives the pointer apart from the picture (set by the capture).
pub fn set_active(on: bool) {
    ACTIVE.store(on, Ordering::Relaxed);
    if !on {
        *STATE.lock().expect("cursor") = (None, Position::default());
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The pointer comes apart from the picture: the viewers draw it.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Draw the pointer into the picture anyway (a recording, a viewer that cannot).
pub fn set_composite(on: bool) {
    if COMPOSITE.swap(on, Ordering::Relaxed) != on {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn composite() -> bool {
    COMPOSITE.load(Ordering::Relaxed)
}

/// The capture's latest pointer: a new shape (`None` keeps the last one) and
/// position.
pub fn publish(shape: Option<Shape>, position: Position) {
    let mut state = STATE.lock().expect("cursor");
    let changed = shape.as_ref().is_some_and(|s| state.0.as_ref() != Some(s)) || state.1 != position;
    if let Some(shape) = shape {
        state.0 = Some(shape);
    }
    state.1 = position;
    drop(state);
    if changed {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// A number that changes with the pointer, its mode or the composite switch.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// The latest shape and position.
pub fn latest() -> (Option<Shape>, Position) {
    STATE.lock().expect("cursor").clone()
}

/// Draws the pointer onto a BGRA (or RGBA, `rgb`) picture of `width`x`height`.
pub fn draw(data: &mut [u8], width: u32, height: u32, stride: usize, rgb: bool) {
    let (shape, pos) = latest();
    let Some(shape) = shape else { return };
    if !pos.visible || shape.serial != pos.serial {
        return;
    }
    let left = pos.x - shape.hot_x as i32;
    let top = pos.y - shape.hot_y as i32;
    for cy in 0..shape.height as i32 {
        let y = top + cy;
        if y < 0 || y >= height as i32 {
            continue;
        }
        for cx in 0..shape.width as i32 {
            let x = left + cx;
            if x < 0 || x >= width as i32 {
                continue;
            }
            let s = ((cy as u32 * shape.width + cx as u32) * 4) as usize;
            let Some(src) = shape.bgra.get(s..s + 4) else { return };
            let a = u32::from(src[3]);
            if a == 0 {
                continue;
            }
            let d = y as usize * stride + x as usize * 4;
            let Some(dst) = data.get_mut(d..d + 4) else { continue };
            let (b, g, r) = (u32::from(src[0]), u32::from(src[1]), u32::from(src[2]));
            let (b, r) = if rgb { (r, b) } else { (b, r) };
            for (i, c) in [b, g, r].into_iter().enumerate() {
                dst[i] = ((c * a + u32::from(dst[i]) * (255 - a)) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_with_alpha_and_hotspot() {
        let mut bgra = vec![0u8; 2 * 2 * 4];
        bgra[..4].copy_from_slice(&[255, 0, 0, 255]); // blue, opaque
        bgra[4..8].copy_from_slice(&[0, 0, 255, 128]); // red, half
        let shape = Shape { serial: 7, width: 2, height: 2, hot_x: 1, hot_y: 1, bgra: Arc::new(bgra) };
        publish(Some(shape), Position { x: 2, y: 2, visible: true, serial: 7 });
        let (w, h) = (4u32, 4u32);
        let mut img = vec![100u8; (w * h * 4) as usize];
        draw(&mut img, w, h, w as usize * 4, false);
        // Hot spot (1,1) at (2,2): the shape's top-left at (1,1).
        assert_eq!(&img[(4 + 1) * 4..][..3], &[255, 0, 0]);
        assert_eq!(&img[(4 + 2) * 4..][..3], &[49, 49, 177]);
        assert_eq!(&img[0..3], &[100, 100, 100]);
        // Hidden: nothing drawn.
        publish(None, Position { x: 2, y: 2, visible: false, serial: 7 });
        let mut img2 = vec![100u8; (w * h * 4) as usize];
        draw(&mut img2, w, h, w as usize * 4, false);
        assert!(img2.iter().all(|&v| v == 100));
    }
}
