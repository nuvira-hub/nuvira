//! Video scene generators S1-S6 (plan 5.1). Integer arithmetic only.
//!
//! Each scene precomputes canvases once ([`SceneGen::new`]) and composes a frame
//! from them in a few milliseconds ([`SceneGen::render`]): S1 is one canvas, S2
//! draws the visible terminal lines, S3 is a window into a tall page, S4 combines
//! 32 precomputed noise tiles with moving offsets, moving boxes and grain, S5 is
//! the desktop plus a window blit, S6 is S5 at a size that changes every quarter.

use super::font::{GLYPH_H, GLYPH_W, glyphs};
use super::{Frame, Scene};
use crate::rng::{Xoshiro256, mix64};

pub type Bgra = [u8; 4];

const fn bgra(r: u8, g: u8, b: u8) -> Bgra {
    [b, g, r, 255]
}

const WHITE: Bgra = bgra(255, 255, 255);
const TEXT_DARK: Bgra = bgra(32, 32, 32);
const SCENE_SEED: u64 = 0x5343_454e_4553_0001;

/// UI scale: 1 up to 1440p, 2 at 4K (as a high-DPI desktop).
pub fn ui_scale(h: usize) -> usize {
    (h / 1000).clamp(1, 4)
}

/// BGRA image with stride = width * 4.
#[derive(Clone)]
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Canvas {
    pub fn new(w: usize, h: usize, c: Bgra) -> Canvas {
        let mut data = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            data.extend_from_slice(&c);
        }
        Canvas { w, h, data }
    }

    /// Clipped rectangle `x..x+w`, `y..y+h`.
    fn clip(&self, x: i64, y: i64, w: i64, h: i64) -> Option<(usize, usize, usize, usize)> {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.w as i64);
        let y1 = (y + h).min(self.h as i64);
        (x1 > x0 && y1 > y0).then_some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
    }

    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: Bgra) {
        let Some((x0, y0, x1, y1)) = self.clip(x, y, w, h) else { return };
        let stride = self.w * 4;
        let first = &mut self.data[y0 * stride + x0 * 4..y0 * stride + x1 * 4];
        for px in first.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&c);
        }
        for yy in y0 + 1..y1 {
            self.data.copy_within(y0 * stride + x0 * 4..y0 * stride + x1 * 4, yy * stride + x0 * 4);
        }
    }

    /// 1 px frame around the rectangle.
    pub fn border(&mut self, x: i64, y: i64, w: i64, h: i64, c: Bgra) {
        self.fill_rect(x, y, w, 1, c);
        self.fill_rect(x, y + h - 1, w, 1, c);
        self.fill_rect(x, y, 1, h, c);
        self.fill_rect(x + w - 1, y, 1, h, c);
    }

    /// Rectangle with a 2D gradient: `a` at the top-left, `b` right, `c` bottom.
    #[allow(clippy::too_many_arguments)]
    pub fn gradient_rect(&mut self, x: i64, y: i64, w: i64, h: i64, a: Bgra, b: Bgra, c: Bgra) {
        let Some((x0, y0, x1, y1)) = self.clip(x, y, w, h) else { return };
        let (w, h) = (w.max(2), h.max(2));
        for yy in y0..y1 {
            let fy = (yy as i64 - y) * 256 / (h - 1);
            let row = &mut self.data[yy * self.w * 4..][..self.w * 4];
            for xx in x0..x1 {
                let fx = (xx as i64 - x) * 256 / (w - 1);
                for ch in 0..3 {
                    let v = a[ch] as i64 * 256
                        + (b[ch] as i64 - a[ch] as i64) * fx
                        + (c[ch] as i64 - a[ch] as i64) * fy;
                    row[xx * 4 + ch] = (v / 256).clamp(0, 255) as u8;
                }
                row[xx * 4 + 3] = 255;
            }
        }
    }

    /// Text in the pseudo-font at `scale`, top-left at (x, y). Returns the width drawn.
    pub fn text(&mut self, x: i64, y: i64, s: &str, c: Bgra, scale: usize) -> i64 {
        let g = glyphs();
        let mut cx = x;
        for ch in s.bytes() {
            let glyph = &g[(ch & 0x7f) as usize];
            for (gy, &bits) in glyph.iter().enumerate() {
                if bits == 0 {
                    continue;
                }
                for gx in 0..GLYPH_W {
                    if bits & (0x80 >> gx) != 0 {
                        self.fill_small(cx + (gx * scale) as i64, y + (gy * scale) as i64, scale, c);
                    }
                }
            }
            cx += (GLYPH_W * scale) as i64;
        }
        cx - x
    }

    #[inline]
    fn fill_small(&mut self, x: i64, y: i64, s: usize, c: Bgra) {
        if x < 0 || y < 0 || x as usize + s > self.w || y as usize + s > self.h {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        for yy in y..y + s {
            let o = (yy * self.w + x) * 4;
            for px in self.data[o..o + s * 4].as_chunks_mut::<4>().0 {
                px.copy_from_slice(&c);
            }
        }
    }

    /// Copies `src` with its top-left at (x, y), clipped.
    pub fn blit(&mut self, src: &Canvas, x: i64, y: i64) {
        let Some((x0, y0, x1, y1)) = self.clip(x, y, src.w as i64, src.h as i64) else { return };
        let sx = (x0 as i64 - x) as usize;
        let n = (x1 - x0) * 4;
        for yy in y0..y1 {
            let sy = (yy as i64 - y) as usize;
            let s = &src.data[(sy * src.w + sx) * 4..][..n];
            self.data[(yy * self.w + x0) * 4..][..n].copy_from_slice(s);
        }
    }
}

/// Deterministic pseudo-words.
fn words(rng: &mut Xoshiro256, max_chars: usize) -> String {
    let mut s = String::new();
    loop {
        let len = 2 + rng.below(8) as usize;
        if s.len() + len + 1 > max_chars {
            return s;
        }
        if !s.is_empty() {
            s.push(' ');
        }
        for _ in 0..len {
            s.push((b'a' + rng.below(26) as u8) as char);
        }
        if rng.below(10) == 0 {
            s.push(if rng.below(2) == 0 { ',' } else { '.' });
        }
    }
}

/// Wallpaper gradient + desktop icons + taskbar (shared by S1 and S5).
fn desktop(w: usize, h: usize) -> Canvas {
    let s = ui_scale(h);
    let mut c = Canvas::new(w, h, bgra(0, 0, 0));
    for y in 0..h {
        let row = &mut c.data[y * w * 4..][..w * 4];
        for x in 0..w {
            let r = 30 + 40 * (x + y) / (w + h);
            let g = 60 + 80 * x / w;
            let b = 120 + 100 * y / h;
            row[x * 4..x * 4 + 4].copy_from_slice(&[b as u8, g as u8, r as u8, 255]);
        }
    }
    let mut rng = Xoshiro256::new(SCENE_SEED ^ 0xdec);
    let icon = (40 * s) as i64;
    for i in 0..6i64 {
        let y = (16 * s) as i64 + i * (icon + (28 * s) as i64);
        let col = bgra(rng.below(200) as u8 + 40, rng.below(200) as u8 + 40, rng.below(200) as u8 + 40);
        c.fill_rect((16 * s) as i64, y, icon, icon, col);
        let label = words(&mut rng, 8);
        c.text((12 * s) as i64, y + icon + 4, &label, WHITE, s);
    }
    let tb = (40 * s) as i64;
    let ty = h as i64 - tb;
    c.fill_rect(0, ty, w as i64, tb, bgra(32, 32, 36));
    c.fill_rect(0, ty, w as i64, 1, bgra(70, 70, 76));
    c.fill_rect((8 * s) as i64, ty + (8 * s) as i64, (24 * s) as i64, (24 * s) as i64, bgra(0, 120, 215));
    for i in 0..8i64 {
        let col = bgra(rng.below(256) as u8, rng.below(256) as u8, rng.below(256) as u8);
        c.fill_rect(
            (48 * s) as i64 + i * (36 * s) as i64,
            ty + (8 * s) as i64,
            (24 * s) as i64,
            (24 * s) as i64,
            col,
        );
    }
    c.text(w as i64 - (60 * s) as i64, ty + (12 * s) as i64, "15:04", WHITE, s);
    c
}

/// A window: border, title bar with buttons, white body with text lines.
fn window(ww: usize, wh: usize, scale: usize, seed: u64) -> Canvas {
    let mut c = Canvas::new(ww, wh, WHITE);
    let mut rng = Xoshiro256::new(SCENE_SEED ^ seed);
    let title = (30 * scale) as i64;
    c.fill_rect(0, 0, ww as i64, title, bgra(230, 230, 232));
    let t = words(&mut rng, 24);
    c.text((10 * scale) as i64, (7 * scale) as i64, &t, TEXT_DARK, scale);
    for (i, col) in [bgra(200, 60, 60), bgra(220, 180, 40), bgra(60, 170, 70)].into_iter().enumerate() {
        let x = ww as i64 - (30 * scale) as i64 * (i as i64 + 1);
        c.fill_rect(
            x + (8 * scale) as i64,
            (9 * scale) as i64,
            (12 * scale) as i64,
            (12 * scale) as i64,
            col,
        );
    }
    let lh = (18 * scale) as i64;
    let max_chars = (ww.saturating_sub(24 * scale)) / (GLYPH_W * scale);
    let mut y = title + (10 * scale) as i64;
    while y + lh < wh as i64 - (8 * scale) as i64 {
        if rng.below(6) == 0 {
            y += lh / 2; // paragraph gap
        }
        let line = words(&mut rng, max_chars.max(4));
        c.text((12 * scale) as i64, y, &line, TEXT_DARK, scale);
        y += lh;
    }
    c.border(0, 0, ww as i64, wh as i64, bgra(90, 90, 96));
    c
}

/// Triangle wave in `0..=range` (bouncing motion).
fn bounce(t: usize, range: usize) -> usize {
    if range == 0 {
        return 0;
    }
    let p = t % (2 * range);
    if p <= range { p } else { 2 * range - p }
}

fn even(v: usize) -> usize {
    (v & !1).max(2)
}

/// Frame-level generator of one scene at one (base) size.
pub struct SceneGen {
    pub scene: Scene,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    kind: Kind,
}

enum Kind {
    Static(Canvas),
    Terminal(Terminal),
    Scroll(Canvas),
    Video(Video),
    Drag(Drag),
    /// Segment length, then S5 at each of the four sizes.
    Resize(u32, Vec<Drag>),
}

impl SceneGen {
    /// `frames` = sequence length (S6 splits it into four size segments).
    pub fn new(scene: Scene, width: u32, height: u32, frames: u32) -> SceneGen {
        let (w, h) = (width as usize, height as usize);
        let kind = match scene {
            Scene::Static => {
                let mut c = desktop(w, h);
                let s = ui_scale(h);
                let wins = [
                    (w * 5 / 100, h * 6 / 100, w * 45 / 100, h * 55 / 100, 1),
                    (w * 40 / 100, h * 15 / 100, w * 50 / 100, h * 45 / 100, 2),
                    (w * 20 / 100, h * 50 / 100, w * 40 / 100, h * 38 / 100, 3),
                ];
                for (x, y, ww, wh, seed) in wins {
                    c.blit(&window(ww, wh, s, seed), x as i64, y as i64);
                }
                Kind::Static(c)
            }
            Scene::Terminal => Kind::Terminal(Terminal::new(w, h)),
            Scene::Scroll => Kind::Scroll(page(w, h)),
            Scene::Video => Kind::Video(Video::new(w, h)),
            Scene::Drag => Kind::Drag(Drag::new(w, h)),
            Scene::Resize => {
                let sizes = resize_sizes(width, height);
                Kind::Resize(
                    frames.div_ceil(4).max(1),
                    sizes.iter().map(|&(w, h)| Drag::new(w as usize, h as usize)).collect(),
                )
            }
        };
        SceneGen { scene, width, height, frames, kind }
    }

    /// Size of frame `i` (constant except for S6).
    pub fn size_of(&self, i: u32) -> (u32, u32) {
        match &self.kind {
            Kind::Resize(seg, drags) => {
                let d = &drags[((i / seg) as usize).min(3)];
                (d.bg.w as u32, d.bg.h as u32)
            }
            _ => (self.width, self.height),
        }
    }

    /// Draws frame `i` into `out` (resized as needed).
    pub fn render(&mut self, i: u32, out: &mut Frame) {
        let (w, h) = self.size_of(i);
        out.index = i;
        out.width = w;
        out.height = h;
        out.data.resize(w as usize * h as usize * 4, 0);
        match &mut self.kind {
            Kind::Static(c) => out.data.copy_from_slice(&c.data),
            Kind::Terminal(t) => t.render(i as usize, &mut out.data),
            Kind::Scroll(page) => {
                let n = w as usize * h as usize * 4;
                let range = page.h - h as usize;
                let off = (i as usize * 4) % (range + 1);
                out.data.copy_from_slice(&page.data[off * w as usize * 4..][..n]);
            }
            Kind::Video(v) => v.render(i as usize, &mut out.data),
            Kind::Drag(d) => d.render(i as usize, &mut out.data),
            Kind::Resize(seg, drags) => {
                let d = &drags[((i / *seg) as usize).min(3)];
                d.render(i as usize, &mut out.data);
            }
        }
    }

    /// A new frame `i`.
    pub fn frame(&mut self, i: u32) -> Frame {
        let mut f = Frame::default();
        self.render(i, &mut f);
        f
    }
}

/// S6 sizes: base, 4/3 of it, 2/3 of it, base (1080p: 1440p, 720p, 1080p). The big
/// one is capped as the agent caps what it encodes (VID-3): at a 4K base
/// 4096x2304, not 5120x2880, which the agent never gives an encoder.
pub fn resize_sizes(w: u32, h: u32) -> [(u32, u32); 4] {
    let (w, h) = (w as usize, h as usize);
    let big = agent_cap(even(w * 4 / 3) as u32, even(h * 4 / 3) as u32);
    let small = (even(w * 2 / 3) as u32, even(h * 2 / 3) as u32);
    [(w as u32, h as u32), big, small, (w as u32, h as u32)]
}

/// The agent's cap on the encoded size with the hardware encoders' default limit
/// (`nuvira_codec::encoded_size` with `SizeLimit::DEFAULT`: 4096 on a side, H.264
/// level 5.1's area), written out here because side A of a comparison (the
/// baseline) builds this bench/ folder against code that does not export it.
fn agent_cap(width: u32, height: u32) -> (u32, u32) {
    const SIDE: f64 = 4096.0;
    const AREA: f64 = 36_864.0 * 256.0;
    let (w, h) = (f64::from(width), f64::from(height));
    let scale = (SIDE / w.max(h)).min((AREA / (w * h)).sqrt());
    if scale >= 1.0 {
        return (width, height);
    }
    let fit = |n: u32| ((f64::from(n) * scale).round() as u32) & !1;
    (fit(width), fit(height))
}

// --- S2 terminal ------------------------------------------------------------

struct Terminal {
    scale: usize,
    rows: usize,
    cols: usize,
    lines: Vec<(String, Bgra)>,
    bg: Vec<u8>,
    canvas: Canvas,
}

const TERM_BG: Bgra = bgra(12, 12, 14);

impl Terminal {
    fn new(w: usize, h: usize) -> Terminal {
        let scale = ui_scale(h);
        let lh = (GLYPH_H + 2) * scale;
        let rows = (h - 8 * scale) / lh;
        let cols = (w - 8 * scale) / (GLYPH_W * scale);
        let canvas = Canvas::new(w, h, TERM_BG);
        Terminal { scale, rows, cols, lines: Vec::new(), bg: canvas.data.clone(), canvas }
    }

    /// Line `k` of the endless output (generated on demand, then kept).
    fn line(&mut self, k: usize) -> &(String, Bgra) {
        while self.lines.len() <= k {
            let n = self.lines.len();
            let mut rng = Xoshiro256::new(SCENE_SEED ^ 0x7e57 ^ (n as u64).wrapping_mul(0x9e37_79b9));
            let width = 10 + rng.below(self.cols.saturating_sub(10).max(1) as u64) as usize;
            let (text, c) = if n.is_multiple_of(6) {
                (format!("user@bench:~/src$ {}", words(&mut rng, width.min(60))), bgra(90, 220, 90))
            } else if n.is_multiple_of(13) {
                (format!("error: {}", words(&mut rng, width)), bgra(230, 70, 60))
            } else {
                (words(&mut rng, width), bgra(204, 204, 204))
            };
            self.lines.push((text, c));
        }
        &self.lines[k]
    }

    fn render(&mut self, i: usize, out: &mut [u8]) {
        let printed = self.rows / 2 + i / 4;
        let first = (printed + 1).saturating_sub(self.rows);
        let scale = self.scale;
        let lh = ((GLYPH_H + 2) * scale) as i64;
        for k in first..printed {
            self.line(k);
        }
        let c = &mut self.canvas;
        c.data.copy_from_slice(&self.bg);
        for (row, k) in (first..printed).enumerate() {
            let (text, col) = &self.lines[k];
            c.text((4 * scale) as i64, (4 * scale) as i64 + row as i64 * lh, text, *col, scale);
        }
        // Prompt and blinking block cursor on the line after the output.
        let y = (4 * scale) as i64 + (printed - first) as i64 * lh;
        let x = c.text((4 * scale) as i64, y, "user@bench:~/src$ ", bgra(90, 220, 90), scale);
        if (i / 15).is_multiple_of(2) {
            let (gw, gh) = ((GLYPH_W * scale) as i64, (GLYPH_H * scale) as i64);
            c.fill_rect((4 * scale) as i64 + x, y, gw, gh, bgra(220, 220, 220));
        }
        out.copy_from_slice(&c.data);
    }
}

// --- S3 scroll ----------------------------------------------------------------

/// A tall page (3 screens) of text blocks, flat boxes and gradient "images".
fn page(w: usize, h: usize) -> Canvas {
    let s = ui_scale(h);
    let tall = h * 3;
    let mut c = Canvas::new(w, tall, bgra(250, 250, 250));
    let mut rng = Xoshiro256::new(SCENE_SEED ^ 0x5c);
    c.fill_rect(0, 0, w as i64, (56 * s) as i64, bgra(40, 70, 140));
    c.text((24 * s) as i64, (20 * s) as i64, "bench page title", WHITE, s);
    let left = (w / 8) as i64;
    let width = (w * 3 / 4) as i64;
    let lh = (20 * s) as i64;
    let max_chars = width as usize / (GLYPH_W * s);
    let mut y = (80 * s) as i64;
    while y < tall as i64 - (40 * s) as i64 {
        match rng.below(4) {
            0 | 1 => {
                for _ in 0..3 + rng.below(6) {
                    let line = words(&mut rng, max_chars);
                    c.text(left, y, &line, TEXT_DARK, s);
                    y += lh;
                }
            }
            2 => {
                let bh = (60 + rng.below(140)) as i64 * s as i64;
                let col = bgra(rng.below(256) as u8, rng.below(256) as u8, rng.below(256) as u8);
                c.fill_rect(left, y, width, bh, col);
                y += bh;
            }
            _ => {
                let ih = (150 + rng.below(150)) as i64 * s as i64;
                let iw = width / 2 + rng.below((width / 2) as u64) as i64;
                let mut r = || bgra(rng.below(256) as u8, rng.below(256) as u8, rng.below(256) as u8);
                let (a, b, cc) = (r(), r(), r());
                c.gradient_rect(left, y, iw, ih, a, b, cc);
                y += ih;
            }
        }
        y += (16 * s) as i64;
    }
    c
}

// --- S4 video -----------------------------------------------------------------

const TILE: usize = 512;
const TILES: usize = 32;
const GRAIN: usize = 1024;

struct Video {
    w: usize,
    h: usize,
    tiles: Vec<Vec<u8>>,
    grain: Vec<u8>,
}

/// Smooth value noise: random lattice every 64 px, bilinear (integer) interpolation,
/// plus a finer octave every 16 px at a quarter of the amplitude. Wraps at TILE.
fn noise_tile(seed: u64) -> Vec<u8> {
    let lattice = |cell: usize, x: usize, y: usize| -> i64 {
        let n = TILE / cell;
        let (x, y) = (x % n, y % n);
        (mix64(seed ^ ((cell as u64) << 40) ^ ((y * n + x) as u64)) & 0xff) as i64
    };
    let mut t = vec![0u8; TILE * TILE];
    for y in 0..TILE {
        for x in 0..TILE {
            let mut v = 0i64;
            for (cell, weight) in [(64usize, 3i64), (16, 1)] {
                let (cx, cy) = (x / cell, y / cell);
                let (fx, fy) = ((x % cell) as i64, (y % cell) as i64);
                let c = cell as i64;
                let a = lattice(cell, cx, cy);
                let b = lattice(cell, cx + 1, cy);
                let d = lattice(cell, cx, cy + 1);
                let e = lattice(cell, cx + 1, cy + 1);
                let top = a * (c - fx) + b * fx;
                let bot = d * (c - fx) + e * fx;
                v += weight * (top * (c - fy) + bot * fy) / (c * c);
            }
            t[y * TILE + x] = (v / 4).clamp(0, 255) as u8;
        }
    }
    t
}

impl Video {
    fn new(w: usize, h: usize) -> Video {
        let tiles = (0..TILES).map(|k| noise_tile(SCENE_SEED ^ 0x7111e ^ k as u64)).collect();
        let grain =
            Xoshiro256::new(SCENE_SEED ^ 0x6a).bytes(GRAIN * GRAIN).into_iter().map(|b| b & 31).collect();
        Video { w, h, tiles, grain }
    }

    fn render(&self, i: usize, out: &mut [u8]) {
        let (w, h) = (self.w, self.h);
        // A "cut" every 60 frames: other tiles.
        let k = (i / 60) * 3;
        let (ta, tb, tc) =
            (&self.tiles[k % TILES], &self.tiles[(k + 1) % TILES], &self.tiles[(k + 2) % TILES]);
        let m = TILE - 1;
        let (ax, ay) = (i * 3, i * 2);
        let (bx, by) = (TILE * 64 - i * 2, i);
        let (cx, cy) = (i * 5, TILE * 64 - i * 4);
        let g = (mix64(SCENE_SEED ^ i as u64) as usize) % (GRAIN * GRAIN);
        let (gx, gy) = (g % GRAIN, g / GRAIN);
        for y in 0..h {
            let ra = &ta[((y + ay) & m) * TILE..][..TILE];
            let rb = &tb[((y + by) & m) * TILE..][..TILE];
            let rc = &tc[((y + cy) & m) * TILE..][..TILE];
            let rg = &self.grain[((y + gy) % GRAIN) * GRAIN..][..GRAIN];
            let row = &mut out[y * w * 4..][..w * 4];
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let gr = rg[(x + gx) & (GRAIN - 1)];
                px[0] = (ra[(x + ax) & m] as u16 + gr as u16).min(255) as u8;
                px[1] = (rb[(x + bx) & m] as u16 + gr as u16).min(255) as u8;
                px[2] = ((rc[(x + cx) & m] as u16 + ra[(x + bx) & m] as u16) / 2 + gr as u16).min(255) as u8;
                px[3] = 255;
            }
        }
        // Moving flat boxes over the field.
        for b in 0..5usize {
            let bw = w / 8 + (b * w) / 40;
            let bh = h / 8 + (b * h) / 30;
            let x = bounce(i * (4 + 3 * b) + b * 97, w - bw);
            let y = bounce(i * (3 + 2 * b) + b * 53, h - bh);
            let col = bgra((40 * b + 60) as u8, (200 - 30 * b) as u8, (90 + 25 * b) as u8);
            for yy in y..y + bh {
                for px in out[(yy * w + x) * 4..(yy * w + x + bw) * 4].as_chunks_mut::<4>().0 {
                    px.copy_from_slice(&col);
                }
            }
        }
    }
}

// --- S5 drag ------------------------------------------------------------------

struct Drag {
    bg: Canvas,
    win: Canvas,
    range: (usize, usize),
}

impl Drag {
    fn new(w: usize, h: usize) -> Drag {
        let s = ui_scale(h);
        let bg = desktop(w, h);
        let ww = (800 * s).min(w - 2);
        let wh = (600 * s).min(h - 2 - 40 * s);
        let win = window(ww, wh, s, 5);
        Drag { range: (w - ww, h - 40 * s - wh), bg, win }
    }

    /// Window moved 6 px right and 6 px down per frame, bouncing at the edges.
    fn render(&self, i: usize, out: &mut [u8]) {
        out.copy_from_slice(&self.bg.data);
        let x = bounce(i * 6, self.range.0);
        let y = bounce(i * 6, self.range.1);
        let w = self.bg.w;
        let n = self.win.w * 4;
        for r in 0..self.win.h {
            out[((y + r) * w + x) * 4..][..n].copy_from_slice(&self.win.data[r * n..][..n]);
        }
    }
}
