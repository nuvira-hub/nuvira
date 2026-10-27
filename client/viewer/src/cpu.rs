//! Drawing without a GPU: YUV → RGB conversion and scaling on the CPU,
//! presentation with `softbuffer` (GDI on Windows).
//!
//! Used for graphics cards that wgpu reaches only through OpenGL: on an Intel
//! HD Graphics 2500 the OpenGL path cost ~23 ms per frame at 1080p.
//! At 1:1 pixels are copied as they are. When the image is shrunk each window pixel
//! averages the brightness of the source pixels at the corners of the area it covers
//! (2x2 samples): picking a single pixel skipped strokes and broke up thin text.
//! Colour stays one sample (half resolution anyway, and the eye notices it less).

use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use nexodesk_codec::YuvFrame;
use winit::window::Window;

use crate::render::{View, Viewport, place};

pub struct CpuRenderer {
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    width: u32,
    height: u32,
    /// Last frame received (used to redraw after a resize).
    last: Option<YuvFrame>,
    /// Ready image, the size of the window, in 0x00RRGGBB.
    pixels: Vec<u32>,
    view: View,
}

impl CpuRenderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let context = softbuffer::Context::new(window.clone()).map_err(|e| anyhow!("{e}"))?;
        let surface = softbuffer::Surface::new(&context, window).map_err(|e| anyhow!("{e}"))?;
        let mut r =
            Self { surface, width: 0, height: 0, last: None, pixels: Vec::new(), view: View::default() };
        r.resize(size.width, size.height);
        Ok(r)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else { return };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        self.width = width;
        self.height = height;
        self.pixels = vec![0; width as usize * height as usize];
        if let Some(frame) = self.last.take() {
            self.convert(&frame);
            self.last = Some(frame);
        }
    }

    pub fn upload(&mut self, frame: &YuvFrame) {
        self.convert(frame);
        let last = self.last.get_or_insert_with(YuvFrame::default);
        last.width = frame.width;
        last.height = frame.height;
        last.y.clone_from(&frame.y);
        last.u.clone_from(&frame.u);
        last.v.clone_from(&frame.v);
    }

    pub fn viewport(&self) -> Viewport {
        match &self.last {
            Some(f) => place(f.width, f.height, self.width, self.height, self.view),
            None => Viewport::default(),
        }
    }

    /// New placement: the last frame is converted again.
    pub fn set_view(&mut self, view: View) {
        if view == self.view {
            return;
        }
        self.view = view;
        if let Some(frame) = self.last.take() {
            self.convert(&frame);
            self.last = Some(frame);
        }
    }

    fn convert(&mut self, frame: &YuvFrame) {
        let vp = place(frame.width, frame.height, self.width, self.height, self.view);
        let out_w = self.width as usize;
        self.pixels.fill(0);
        let Some((vis, _, _)) = vp.visible(self.width, self.height) else { return };
        let (vx, vy, vw, vh) = (vis.x as usize, vis.y as usize, vis.width as usize, vis.height as usize);
        let cols = footprints(vx, vw, vp.x, vp.width, frame.width as usize);
        let rows = footprints(vy, vh, vp.y, vp.height, frame.height as usize);
        yuv_to_rgb_scaled(frame, &cols, &rows, &mut self.pixels[vy * out_w..(vy + vh) * out_w], out_w, vx);
    }

    pub fn render(&mut self) -> Result<()> {
        let mut buffer = self.surface.buffer_mut().map_err(|e| anyhow!("{e}")).context("buffer")?;
        if buffer.len() == self.pixels.len() {
            buffer.copy_from_slice(&self.pixels);
        }
        buffer.present().map_err(|e| anyhow!("{e}"))
    }
}

/// For each of `count` window pixels starting at `from`, the first and last source
/// pixel of the area it covers, when the image starts at `start` and is `size`
/// window pixels long for `src` source pixels. At 1:1 or when enlarging they coincide.
fn footprints(from: usize, count: usize, start: f32, size: f32, src: usize) -> Vec<(usize, usize)> {
    let scale = src as f64 / size as f64;
    (from..from + count)
        .map(|w| {
            let at = (w as f64 - start as f64) * scale;
            let first = (at.floor().max(0.0) as usize).min(src - 1);
            let last = (((at + scale).ceil() as usize).saturating_sub(1)).clamp(first, src - 1);
            (first, last)
        })
        .collect()
}

/// Saves a frame as PNG at full resolution.
pub fn write_png(frame: &YuvFrame, path: &std::path::Path) -> Result<()> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let cols: Vec<(usize, usize)> = (0..w).map(|x| (x, x)).collect();
    let rows: Vec<(usize, usize)> = (0..h).map(|y| (y, y)).collect();
    let mut pixels = vec![0u32; w * h];
    yuv_to_rgb_scaled(frame, &cols, &rows, &mut pixels, w, 0);
    let rgb: Vec<u8> = pixels.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.write_header()?.write_image_data(&rgb)?;
    Ok(())
}

#[inline(always)]
fn clamp8(v: i32) -> u32 {
    v.clamp(0, 255) as u32
}

/// Converts (BT.601, limited range) and scales: one destination row per `rows`
/// entry and one column per `cols` entry, each the (first, last) source pixel it
/// covers (see `footprints`). Written to `out` from column `x0` with pitch `stride`.
/// Splits the work over several cores.
fn yuv_to_rgb_scaled(
    frame: &YuvFrame,
    cols: &[(usize, usize)],
    rows: &[(usize, usize)],
    out: &mut [u32],
    stride: usize,
    x0: usize,
) {
    let fw = frame.width as usize;
    let cw = fw.div_ceil(2);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
    let rows_per_job = rows.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for (job, chunk) in out.chunks_mut(stride * rows_per_job).enumerate() {
            s.spawn(move || {
                for (i, line) in chunk.chunks_mut(stride).enumerate() {
                    let Some(&(ya, yb)) = rows.get(job * rows_per_job + i) else { break };
                    let row_a = &frame.y[ya * fw..][..fw];
                    let row_b = &frame.y[yb * fw..][..fw];
                    let u_row = &frame.u[(ya / 2) * cw..][..cw];
                    let v_row = &frame.v[(ya / 2) * cw..][..cw];
                    for (dst, &(xa, xb)) in line[x0..x0 + cols.len()].iter_mut().zip(cols) {
                        let luma =
                            (row_a[xa] as i32 + row_a[xb] as i32 + row_b[xa] as i32 + row_b[xb] as i32 + 2)
                                >> 2;
                        let c = 298 * (luma - 16);
                        let d = u_row[xa / 2] as i32 - 128;
                        let e = v_row[xa / 2] as i32 - 128;
                        let r = clamp8((c + 409 * e + 128) >> 8);
                        let g = clamp8((c - 100 * d - 208 * e + 128) >> 8);
                        let b = clamp8((c + 516 * d + 128) >> 8);
                        *dst = (r << 16) | (g << 8) | b;
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_and_scaling() {
        // 4x2: left half white, right half pure red (limited BT.601 values).
        let frame = YuvFrame {
            width: 4,
            height: 2,
            y: vec![235, 235, 82, 82, 235, 235, 82, 82],
            u: vec![128, 90],
            v: vec![128, 240],
        };
        // 2x1 destination inside a buffer 3 wide starting at column 1: each pixel
        // covers two source columns of the same colour.
        let cols = footprints(1, 2, 1.0, 2.0, 4);
        assert_eq!(cols, [(0, 1), (2, 3)]);
        let rows = footprints(0, 1, 0.0, 1.0, 2);
        let mut out = vec![0u32; 3];
        yuv_to_rgb_scaled(&frame, &cols, &rows, &mut out, 3, 1);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 0xffffff);
        let (r, g, b) = (out[2] >> 16, (out[2] >> 8) & 0xff, out[2] & 0xff);
        assert!(r > 250 && g < 5 && b < 5, "red: {r} {g} {b}");
    }
}
