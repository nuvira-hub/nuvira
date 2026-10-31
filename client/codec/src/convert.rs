//! BGRA → YUV 4:2:0 conversion (BT.601, limited range) with integer arithmetic,
//! split over several cores by rows of 2x2 blocks.
//!
//! Formula (ITU-R BT.601, 8-bit coefficients):
//!   Y = ((66 R + 129 G + 25 B + 128) >> 8) + 16
//!   U = ((-38 R - 74 G + 112 B + 128) >> 8) + 128
//!   V = ((112 R - 94 G - 18 B + 128) >> 8) + 128
//! U and V are computed on the average of the 2x2 block.

use crate::BgraImage;

#[inline(always)]
fn luma(b: i32, g: i32, r: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

#[inline(always)]
fn chroma(b: i32, g: i32, r: i32) -> (u8, u8) {
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

/// Converts two BGRA rows into two luma rows and one chroma row.
/// `put_chroma(x, u, v)` writes the chroma of block column `x`.
#[inline(always)]
fn convert_row_pair(
    src0: &[u8],
    src1: &[u8],
    y0: &mut [u8],
    y1: &mut [u8],
    width: usize,
    mut put_chroma: impl FnMut(usize, u8, u8),
) {
    for x in 0..width / 2 {
        let p = x * 8;
        let (a, b) = (&src0[p..p + 8], &src1[p..p + 8]);
        let px = |s: &[u8], o: usize| (s[o] as i32, s[o + 1] as i32, s[o + 2] as i32);
        let (b00, g00, r00) = px(a, 0);
        let (b01, g01, r01) = px(a, 4);
        let (b10, g10, r10) = px(b, 0);
        let (b11, g11, r11) = px(b, 4);
        y0[x * 2] = luma(b00, g00, r00);
        y0[x * 2 + 1] = luma(b01, g01, r01);
        y1[x * 2] = luma(b10, g10, r10);
        y1[x * 2 + 1] = luma(b11, g11, r11);
        let (u, v) = chroma(
            (b00 + b01 + b10 + b11 + 2) >> 2,
            (g00 + g01 + g10 + g11 + 2) >> 2,
            (r00 + r01 + r10 + r11 + 2) >> 2,
        );
        put_chroma(x, u, v);
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
}

/// Even sizes used by the conversion (at most one row/column is dropped).
pub fn even_size(image: &BgraImage) -> (usize, usize) {
    ((image.width & !1) as usize, (image.height & !1) as usize)
}

/// BGRA → I420 (three planes: Y w×h, U and V w/2×h/2). The buffers are resized.
pub fn bgra_to_i420(image: &BgraImage, y: &mut Vec<u8>, u: &mut Vec<u8>, v: &mut Vec<u8>) {
    let (w, h) = even_size(image);
    let (cw, ch) = (w / 2, h / 2);
    y.resize(w * h, 0);
    u.resize(cw * ch, 0);
    v.resize(cw * ch, 0);
    if ch == 0 {
        return;
    }
    let rows_per_job = ch.div_ceil(threads());
    std::thread::scope(|s| {
        let jobs = y
            .chunks_mut(w * 2 * rows_per_job)
            .zip(u.chunks_mut(cw * rows_per_job))
            .zip(v.chunks_mut(cw * rows_per_job))
            .enumerate();
        for (job, ((y, u), v)) in jobs {
            s.spawn(move || {
                for (i, ((yy, uu), vv)) in
                    y.chunks_mut(w * 2).zip(u.chunks_mut(cw)).zip(v.chunks_mut(cw)).enumerate()
                {
                    let row = (job * rows_per_job + i) * 2;
                    let src0 = &image.data[row * image.stride..];
                    let src1 = &image.data[(row + 1) * image.stride..];
                    let (y0, y1) = yy.split_at_mut(w);
                    convert_row_pair(src0, src1, y0, y1, w, |x, cu, cv| {
                        uu[x] = cu;
                        vv[x] = cv;
                    });
                }
            });
        }
    });
}

/// BGRA → NV12 (Y plane w×h followed by interleaved UV w×h/2) with row pitch `pitch`
/// and `height_aligned` luma rows before the UV plane (alignment required by some encoders).
pub fn bgra_to_nv12(image: &BgraImage, out: &mut [u8], pitch: usize, height_aligned: usize) {
    let (w, h) = even_size(image);
    let ch = h / 2;
    if ch == 0 {
        return;
    }
    let (luma_plane, chroma_plane) = out.split_at_mut(pitch * height_aligned);
    let rows_per_job = ch.div_ceil(threads());
    std::thread::scope(|s| {
        let jobs = luma_plane[..pitch * h]
            .chunks_mut(pitch * 2 * rows_per_job)
            .zip(chroma_plane[..pitch * ch].chunks_mut(pitch * rows_per_job))
            .enumerate();
        for (job, (y, uv)) in jobs {
            s.spawn(move || {
                for (i, (yy, uv)) in y.chunks_mut(pitch * 2).zip(uv.chunks_mut(pitch)).enumerate() {
                    let row = (job * rows_per_job + i) * 2;
                    let src0 = &image.data[row * image.stride..];
                    let src1 = &image.data[(row + 1) * image.stride..];
                    let (y0, y1) = yy.split_at_mut(pitch);
                    convert_row_pair(src0, src1, y0, y1, w, |x, cu, cv| {
                        uv[x * 2] = cu;
                        uv[x * 2 + 1] = cv;
                    });
                }
            });
        }
    });
}

/// Scales `image` down to `width`x`height` (clamped to the image size), each output
/// pixel being the average of the source pixels it covers: text stays readable
/// instead of losing strokes as with point sampling. `out` gets packed rows.
pub fn scale_bgra(image: &BgraImage, width: usize, height: usize, out: &mut Vec<u8>) {
    let (sw, sh) = (image.width as usize, image.height as usize);
    let (width, height) = (width.min(sw), height.min(sh));
    out.resize(width * height * 4, 0);
    if width == 0 || height == 0 {
        return;
    }
    // Output column x averages source columns cols[x]..cols[x + 1] (never empty: width <= sw).
    let cols: Vec<usize> = (0..=width).map(|x| x * sw / width).collect();
    let rows_per_job = height.div_ceil(threads());
    std::thread::scope(|s| {
        for (job, chunk) in out.chunks_mut(width * 4 * rows_per_job).enumerate() {
            let cols = &cols;
            s.spawn(move || {
                let mut sums = vec![[0u32; 4]; width];
                for (i, row) in chunk.chunks_mut(width * 4).enumerate() {
                    let y = job * rows_per_job + i;
                    let (y0, y1) = (y * sh / height, (y + 1) * sh / height);
                    sums.fill([0; 4]);
                    for sy in y0..y1 {
                        let src = &image.data[sy * image.stride..][..sw * 4];
                        for (x, sum) in sums.iter_mut().enumerate() {
                            for px in src[cols[x] * 4..cols[x + 1] * 4].as_chunks::<4>().0 {
                                for c in 0..4 {
                                    sum[c] += px[c] as u32;
                                }
                            }
                        }
                    }
                    for (x, (sum, dst)) in sums.iter().zip(row.as_chunks_mut::<4>().0).enumerate() {
                        let n = ((cols[x + 1] - cols[x]) * (y1 - y0)) as u32;
                        for c in 0..4 {
                            dst[c] = ((sum[c] + n / 2) / n) as u8;
                        }
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
    fn scale_averages() {
        // 4x2 → 2x1: each output pixel averages a 2x2 block.
        let mut data = Vec::new();
        for v in [0u8, 100, 200, 40, 20, 120, 220, 60] {
            data.extend_from_slice(&[v, v, v, 255]);
        }
        let img = BgraImage { width: 4, height: 2, stride: 16, data: &data };
        let mut out = Vec::new();
        scale_bgra(&img, 2, 1, &mut out);
        assert_eq!(out, [60, 60, 60, 255, 130, 130, 130, 255]);
        // Never enlarged.
        scale_bgra(&img, 8, 4, &mut out);
        assert_eq!(out.len(), 4 * 2 * 4);
    }

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((w * h) as usize)
    }

    #[test]
    fn known_colors() {
        // White, black and pure red in limited-range BT.601.
        for (bgra, yuv) in [
            ([255, 255, 255, 255], (235, 128, 128)),
            ([0, 0, 0, 255], (16, 128, 128)),
            ([0, 0, 255, 255], (82, 90, 240)),
        ] {
            let data = solid(4, 2, bgra);
            let img = BgraImage { width: 4, height: 2, stride: 16, data: &data };
            let (mut y, mut u, mut v) = (vec![], vec![], vec![]);
            bgra_to_i420(&img, &mut y, &mut u, &mut v);
            assert_eq!((y[0], u[0], v[0]), yuv, "colour {bgra:?}");

            let mut nv12 = vec![0u8; 4 * 2 + 4];
            bgra_to_nv12(&img, &mut nv12, 4, 2);
            assert_eq!((nv12[0], nv12[8], nv12[9]), yuv, "NV12 {bgra:?}");
        }
    }

    #[test]
    fn odd_size_and_padding() {
        let (w, h, stride) = (7u32, 5u32, 7 * 4 + 12);
        let data = vec![128u8; stride * h as usize];
        let img = BgraImage { width: w, height: h, stride, data: &data };
        let (mut y, mut u, mut v) = (vec![], vec![], vec![]);
        bgra_to_i420(&img, &mut y, &mut u, &mut v);
        assert_eq!((y.len(), u.len(), v.len()), (6 * 4, 3 * 2, 3 * 2));

        // NV12 rows with pitch 16 and 8 aligned rows.
        let mut nv12 = vec![0u8; 16 * 8 + 16 * 4];
        bgra_to_nv12(&img, &mut nv12, 16, 8);
        assert_eq!(nv12[16 * 3 + 5], y[3 * 6 + 5]);
        assert_eq!(nv12[16 * 8 + 16 + 4], u[3 + 2]);
    }
}
