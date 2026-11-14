//! The remote pointer apart from the picture: its image and position, for the
//! viewers that draw it themselves (`CursorShape`, `CursorPosition` of the session
//! protocol): the pointer moves in the viewer without waiting for a video frame,
//! and a still screen with a moving pointer sends no video.
//!
//! The agent asks for it ([`set_wanted`]) and the capture publishes what it reads
//! ([`publish`]): on Linux the screen cast (portal, Mutter, KWin) or XFixes, on
//! Windows DXGI Desktop Duplication, which never draws the pointer in. When
//! someone needs the pointer in the picture after all (a recording, a viewer that
//! cannot draw it), the agent sets [`set_composite`] and the capture draws it in
//! itself.

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
static STATE: Mutex<(Option<Shape>, Position)> =
    Mutex::new((None, Position { x: 0, y: 0, visible: false, serial: 0 }));

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
    draw_region(data, 0, 0, width, height, stride, rgb);
}

/// The rectangle the pointer covers in the picture (left, top, width, height),
/// clipped to `width`x`height`; `None` when it is hidden or outside.
pub fn bounds(width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    let (shape, pos) = latest();
    bounds_of(shape.as_ref()?, pos, width, height)
}

fn bounds_of(shape: &Shape, pos: Position, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    if !pos.visible || shape.serial != pos.serial {
        return None;
    }
    let (x, y) = (pos.x - shape.hot_x as i32, pos.y - shape.hot_y as i32);
    let (left, top) = (x.max(0), y.max(0));
    let right = (x + shape.width as i32).min(width as i32);
    let bottom = (y + shape.height as i32).min(height as i32);
    (right > left && bottom > top)
        .then(|| (left as u32, top as u32, (right - left) as u32, (bottom - top) as u32))
}

/// Like [`draw`], onto a piece of the picture: `data` holds `width`x`height`
/// pixels whose top-left is at (`x0`, `y0`) of the picture (Windows: the piece of a
/// GPU texture under the pointer, read back, drawn on and written again).
pub fn draw_region(data: &mut [u8], x0: i32, y0: i32, width: u32, height: u32, stride: usize, rgb: bool) {
    let (shape, pos) = latest();
    if let Some(shape) = shape {
        draw_shape(&shape, pos, data, x0, y0, width, height, stride, rgb);
    }
}

/// The pointer drawn into a picture kept from one frame to the next, with the
/// pixels it covers: when only the pointer moves, [`Drawn::undo`] puts them back
/// and it is drawn again where it is now, without copying the whole picture.
#[derive(Default)]
pub struct Drawn {
    rect: Option<(usize, usize, usize, usize)>,
    under: Vec<u8>,
    generation: Option<u64>,
}

impl Drawn {
    /// The [`generation`] of the pointer drawn in; `None` when none is.
    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    /// Takes the pointer out of the picture again.
    pub fn undo(&mut self, data: &mut [u8], stride: usize) {
        if let Some((x, y, w, h)) = self.rect.take() {
            for r in 0..h {
                let d = (y + r) * stride + x * 4;
                if let (Some(dst), Some(src)) =
                    (data.get_mut(d..d + w * 4), self.under.get(r * w * 4..(r + 1) * w * 4))
                {
                    dst.copy_from_slice(src);
                }
            }
        }
        self.generation = None;
    }

    /// The picture's content changed under the pointer: what was kept is stale.
    pub fn forget(&mut self) {
        self.rect = None;
        self.generation = None;
    }

    /// Draws the pointer in (after [`Drawn::undo`]), keeping what it covers.
    pub fn draw(&mut self, data: &mut [u8], width: u32, height: u32, stride: usize, rgb: bool) {
        let generation = generation();
        let (shape, pos) = latest();
        if let Some(shape) = shape
            && let Some((x, y, w, h)) = bounds_of(&shape, pos, width, height)
        {
            let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
            self.under.clear();
            for r in 0..h {
                let d = (y + r) * stride + x * 4;
                match data.get(d..d + w * 4) {
                    Some(src) => self.under.extend_from_slice(src),
                    None => return,
                }
            }
            self.rect = Some((x, y, w, h));
            draw_shape(&shape, pos, data, 0, 0, width, height, stride, rgb);
        }
        self.generation = Some(generation);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_shape(
    shape: &Shape,
    pos: Position,
    data: &mut [u8],
    x0: i32,
    y0: i32,
    width: u32,
    height: u32,
    stride: usize,
    rgb: bool,
) {
    if !pos.visible || shape.serial != pos.serial {
        return;
    }
    let left = pos.x - shape.hot_x as i32 - x0;
    let top = pos.y - shape.hot_y as i32 - y0;
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

/// The largest pointer sent or drawn, per side (bigger ones are cut).
pub const MAX_SIDE: u32 = 256;

/// A serial that depends only on the shape: the same pointer gets the same
/// number again (FNV-1a over size, hot spot and pixels; never 0).
pub fn serial_of(width: u32, height: u32, hot_x: u32, hot_y: u32, bgra: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in [width, height, hot_x, hot_y].iter().flat_map(|v| v.to_le_bytes()).chain(bgra.iter().copied()) {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    h.max(1)
}

/// The shape as an RGBA PNG (`CursorShape.png`): a 32x32 pointer takes some
/// hundred bytes instead of 4 KB.
pub fn png_of(shape: &Shape) -> Vec<u8> {
    let mut rgba = shape.bgra.as_ref().clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, shape.width, shape.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let written = encoder.write_header().and_then(|mut w| w.write_image_data(&rgba));
    if written.is_err() {
        out.clear();
    }
    out
}

/// Pointer shapes of Windows (`DXGI_OUTDUPL_POINTER_SHAPE_TYPE`).
pub const WIN_MONOCHROME: u32 = 1;
pub const WIN_COLOR: u32 = 2;
pub const WIN_MASKED_COLOR: u32 = 4;

/// A pointer shape of Windows (as DXGI Desktop Duplication gives it) as BGRA,
/// not premultiplied: `width` and `height` in pixels (a monochrome one has its
/// two masks one above the other, so twice the height), `pitch` bytes a row.
/// The pixels that invert the screen (the text cursor, often) cannot be drawn
/// apart from it: they become black with a white edge, visible on any background.
/// `None` if the buffer is too short or the kind unknown.
pub fn from_windows(
    kind: u32,
    width: u32,
    height: u32,
    pitch: u32,
    buf: &[u8],
) -> Option<(u32, u32, Vec<u8>)> {
    const CLEAR: [u8; 4] = [0, 0, 0, 0];
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    let pitch = pitch as usize;
    let rows = if kind == WIN_MONOCHROME { height / 2 } else { height };
    let (w, h) = (width.min(MAX_SIDE), rows.min(MAX_SIDE));
    if w == 0 || h == 0 {
        return None;
    }
    let mut out = vec![0u8; (w * h * 4) as usize];
    let mut invert = vec![false; (w * h) as usize];
    match kind {
        WIN_MONOCHROME => {
            let bit = |row: usize, x: usize| buf.get(row * pitch + x / 8).map(|b| (b >> (7 - x % 8)) & 1);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let and = bit(y, x)?;
                    let xor = bit(y + rows as usize, x)?;
                    let px = match (and, xor) {
                        (0, 0) => BLACK,
                        (0, _) => WHITE,
                        (_, 0) => CLEAR,
                        _ => {
                            invert[y * w as usize + x] = true;
                            BLACK
                        }
                    };
                    out[(y * w as usize + x) * 4..][..4].copy_from_slice(&px);
                }
            }
        }
        WIN_COLOR | WIN_MASKED_COLOR => {
            for y in 0..h as usize {
                let row = buf.get(y * pitch..y * pitch + w as usize * 4)?;
                for (x, px) in row.as_chunks::<4>().0.iter().enumerate() {
                    let i = y * w as usize + x;
                    let value = if kind == WIN_COLOR {
                        [px[0], px[1], px[2], px[3]]
                    } else if px[3] == 0 {
                        // Mask 0: the colour replaces the screen.
                        [px[0], px[1], px[2], 255]
                    } else if px[..3] == [0, 0, 0] {
                        // XOR with black: the screen as it is.
                        CLEAR
                    } else {
                        invert[i] = true;
                        BLACK
                    };
                    out[i * 4..][..4].copy_from_slice(&value);
                }
            }
        }
        _ => return None,
    }
    // A white edge around the inverting pixels, where the shape is clear.
    if invert.iter().any(|&v| v) {
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let i = (y * w as i32 + x) as usize;
                if out[i * 4 + 3] != 0 {
                    continue;
                }
                let near = (-1..=1).any(|dy| {
                    (-1..=1).any(|dx| {
                        let (nx, ny) = (x + dx, y + dy);
                        nx >= 0
                            && ny >= 0
                            && nx < w as i32
                            && ny < h as i32
                            && invert[(ny * w as i32 + nx) as usize]
                    })
                });
                if near {
                    out[i * 4..][..4].copy_from_slice(&WHITE);
                }
            }
        }
    }
    Some((w, h, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests that use the shared state, one at a time.
    static SHARED: Mutex<()> = Mutex::new(());

    #[test]
    fn windows_monochrome() {
        // 8x2 pointer: AND rows then XOR rows, one byte a row.
        // Row 0: AND 0b0000_1111, XOR 0b0101_0101 -> black, white, black, white, clear, invert, clear, invert.
        // Row 1: AND 0b1111_1111, XOR 0 -> clear.
        let buf = [0b0000_1111, 0b1111_1111, 0b0101_0101, 0];
        let (w, h, px) = from_windows(WIN_MONOCHROME, 8, 4, 1, &buf).unwrap();
        assert_eq!((w, h), (8, 2));
        let at = |x: usize, y: usize| &px[(y * 8 + x) * 4..][..4];
        assert_eq!(at(0, 0), &[0, 0, 0, 255]);
        assert_eq!(at(1, 0), &[255, 255, 255, 255]);
        assert_eq!(at(5, 0), &[0, 0, 0, 255]); // inverting: black
        assert_eq!(at(4, 0), &[255, 255, 255, 255]); // clear next to it: white edge
        assert_eq!(at(0, 1), &[0, 0, 0, 0]); // clear, not next to an inverting pixel
        assert_eq!(at(5, 1), &[255, 255, 255, 255]); // under an inverting pixel
    }

    #[test]
    fn windows_masked_color() {
        // 3x1: opaque red, XOR with black (clear), XOR with a colour (inverting).
        let buf = [0, 0, 255, 0, 0, 0, 0, 255, 10, 10, 10, 255];
        let (_, _, px) = from_windows(WIN_MASKED_COLOR, 3, 1, 12, &buf).unwrap();
        assert_eq!(&px[0..4], &[0, 0, 255, 255]);
        assert_eq!(&px[4..8], &[255, 255, 255, 255]); // clear next to the inverting pixel
        assert_eq!(&px[8..12], &[0, 0, 0, 255]);
        assert!(from_windows(WIN_COLOR, 4, 4, 16, &[0; 8]).is_none());
    }

    #[test]
    fn png_is_smaller_and_decodes() {
        let mut bgra = vec![0u8; 32 * 32 * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            if i % 32 < i / 32 {
                px.copy_from_slice(&[10, 20, 30, 255]);
            }
        }
        let shape =
            Shape { serial: 1, width: 32, height: 32, hot_x: 0, hot_y: 0, bgra: Arc::new(bgra.clone()) };
        let png = png_of(&shape);
        assert!(!png.is_empty() && png.len() < 1000, "{} bytes", png.len());
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
        let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut rgba).unwrap();
        assert_eq!(&rgba[(31 * 32) * 4..][..4], &[30, 20, 10, 255]);
        assert_eq!(&rgba[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn serial_follows_the_shape() {
        let a = serial_of(2, 2, 0, 0, &[1; 16]);
        assert_eq!(a, serial_of(2, 2, 0, 0, &[1; 16]));
        assert_ne!(a, serial_of(2, 2, 1, 0, &[1; 16]));
        assert_ne!(a, 0);
    }

    #[test]
    fn drawn_and_undone() {
        let _one = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        let shape =
            Shape { serial: 4, width: 1, height: 1, hot_x: 0, hot_y: 0, bgra: Arc::new(vec![9, 9, 9, 255]) };
        publish(Some(shape), Position { x: 1, y: 0, visible: true, serial: 4 });
        let mut img = vec![5u8; 3 * 4];
        let mut drawn = Drawn::default();
        drawn.draw(&mut img, 3, 1, 12, false);
        assert_eq!(&img[4..7], &[9, 9, 9]);
        assert_eq!(drawn.generation(), Some(generation()));
        publish(None, Position { x: 2, y: 0, visible: true, serial: 4 });
        drawn.undo(&mut img, 12);
        drawn.draw(&mut img, 3, 1, 12, false);
        assert_eq!(&img[4..7], &[5, 5, 5]);
        assert_eq!(&img[8..11], &[9, 9, 9]);
    }

    #[test]
    fn region_and_bounds() {
        let _one = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        let shape =
            Shape { serial: 9, width: 2, height: 2, hot_x: 0, hot_y: 0, bgra: Arc::new(vec![255; 16]) };
        publish(Some(shape), Position { x: 3, y: 1, visible: true, serial: 9 });
        assert_eq!(bounds(4, 4), Some((3, 1, 1, 2)));
        // The piece (2,0)-(4,3) of the picture: the pointer at its (1,1).
        let mut piece = vec![0u8; 2 * 3 * 4];
        draw_region(&mut piece, 2, 0, 2, 3, 8, false);
        assert_eq!(&piece[3 * 4..][..4], &[255, 255, 255, 0]);
        assert_eq!(&piece[0..4], &[0, 0, 0, 0]);
        publish(None, Position { x: 9, y: 9, visible: true, serial: 9 });
        assert_eq!(bounds(4, 4), None);
    }

    #[test]
    fn draws_with_alpha_and_hotspot() {
        let _one = SHARED.lock().unwrap_or_else(|e| e.into_inner());
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
