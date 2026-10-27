//! Drawing without a GPU: YUV → RGB conversion and scaling on the CPU,
//! presentation with `softbuffer` (GDI on Windows).
//!
//! Used for graphics cards that wgpu reaches only through OpenGL: on an Intel
//! HD Graphics 2500 the OpenGL path cost ~23 ms per frame at 1080p.
//! Scaling is nearest-pixel: exact at 1:1, slightly jagged when the window is
//! smaller than the remote screen.

use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use nexodesk_codec::YuvFrame;
use winit::window::Window;

use crate::render::{Viewport, fit};

pub struct CpuRenderer {
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    width: u32,
    height: u32,
    /// Last frame received (used to redraw after a resize).
    last: Option<YuvFrame>,
    /// Ready image, the size of the window, in 0x00RRGGBB.
    pixels: Vec<u32>,
}

impl CpuRenderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let context = softbuffer::Context::new(window.clone()).map_err(|e| anyhow!("{e}"))?;
        let surface = softbuffer::Surface::new(&context, window).map_err(|e| anyhow!("{e}"))?;
        let mut r = Self { surface, width: 0, height: 0, last: None, pixels: Vec::new() };
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
            Some(f) => fit(f.width, f.height, self.width, self.height),
            None => Viewport::default(),
        }
    }

    fn convert(&mut self, frame: &YuvFrame) {
        let vp = fit(frame.width, frame.height, self.width, self.height);
        let (out_w, out_h) = (self.width as usize, self.height as usize);
        let (vx, vy) = (vp.x as usize, vp.y as usize);
        let (vw, vh) = ((vp.width as usize).min(out_w - vx), (vp.height as usize).min(out_h - vy));
        self.pixels.fill(0);
        if vw == 0 || vh == 0 {
            return;
        }
        // Source column for every destination column.
        let src_x: Vec<usize> = (0..vw).map(|x| x * frame.width as usize / vw).collect();
        yuv_to_rgb_scaled(frame, &src_x, vh, &mut self.pixels[vy * out_w..(vy + vh) * out_w], out_w, vx);
    }

    pub fn render(&mut self) -> Result<()> {
        let mut buffer = self.surface.buffer_mut().map_err(|e| anyhow!("{e}")).context("buffer")?;
        if buffer.len() == self.pixels.len() {
            buffer.copy_from_slice(&self.pixels);
        }
        buffer.present().map_err(|e| anyhow!("{e}"))
    }
}

/// Saves a frame as PNG at full resolution.
pub fn write_png(frame: &YuvFrame, path: &std::path::Path) -> Result<()> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let src_x: Vec<usize> = (0..w).collect();
    let mut pixels = vec![0u32; w * h];
    yuv_to_rgb_scaled(frame, &src_x, h, &mut pixels, w, 0);
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

/// Converts (BT.601, limited range) and scales `rows` destination rows, written to
/// `out` starting at column `x0` with pitch `stride`. Splits the work over several cores.
fn yuv_to_rgb_scaled(
    frame: &YuvFrame,
    src_x: &[usize],
    rows: usize,
    out: &mut [u32],
    stride: usize,
    x0: usize,
) {
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let cw = fw.div_ceil(2);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
    let rows_per_job = rows.div_ceil(threads);
    std::thread::scope(|s| {
        for (job, chunk) in out.chunks_mut(stride * rows_per_job).enumerate() {
            s.spawn(move || {
                for (i, line) in chunk.chunks_mut(stride).enumerate() {
                    let sy = (job * rows_per_job + i) * fh / rows;
                    let y_row = &frame.y[sy * fw..][..fw];
                    let u_row = &frame.u[(sy / 2) * cw..][..cw];
                    let v_row = &frame.v[(sy / 2) * cw..][..cw];
                    for (dst, &sx) in line[x0..x0 + src_x.len()].iter_mut().zip(src_x) {
                        let c = 298 * (y_row[sx] as i32 - 16);
                        let d = u_row[sx / 2] as i32 - 128;
                        let e = v_row[sx / 2] as i32 - 128;
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
        // 2x1 destination inside a buffer 3 wide starting at column 1.
        let src_x: Vec<usize> = (0..2).map(|x| x * 4 / 2).collect();
        let mut out = vec![0u32; 3];
        yuv_to_rgb_scaled(&frame, &src_x, 1, &mut out, 3, 1);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 0xffffff);
        let (r, g, b) = (out[2] >> 16, (out[2] >> 8) & 0xff, out[2] & 0xff);
        assert!(r > 250 && g < 5 && b < 5, "red: {r} {g} {b}");
    }
}
