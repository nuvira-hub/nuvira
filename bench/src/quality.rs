//! Picture quality (plan 6.4): own BT.601 reference conversion, PSNR, SSIM, BD-rate.
//!
//! - The app encodes BT.601 limited range, 8-bit 4:2:0. [`Reference::from_bgra`]
//!   converts the source BGRA with this module's **own** float conversion (chroma =
//!   2x2 box average), not with the production `convert.rs`, so a change there that
//!   degrades colour is caught rather than hidden.
//! - [`psnr`]: PSNR-Y, -U, -V and PSNR-YUV = (6 Y + U + V) / 8 of the per-plane
//!   values (the JCT-VC convention). Identical planes give +inf; [`PSNR_CAP_DB`]
//!   caps values for aggregation and JSON.
//! - [`ssim_y`]: SSIM of the luma, Gaussian 11x11 window, sigma 1.5, K1 = 0.01,
//!   K2 = 0.03, L = 255, mean over every window position fully inside the picture
//!   ("valid"), no automatic downsampling. Every pixel up to 2560x1440; above that a
//!   stride-2 grid of window positions (the choice is fixed and recorded with
//!   [`ssim_step_for`]).
//! - [`bd_rate`]: Bjontegaard delta rate (VCEG-M33): cubic fit of log(rate) as a
//!   function of quality per curve, integrated over the overlapping quality range.

use nuvira_codec::{BgraImage, YuvFrame};

/// Cap of PSNR values in aggregates and JSON (identical planes give +inf).
pub const PSNR_CAP_DB: f64 = 100.0;

const SSIM_K1: f64 = 0.01;
const SSIM_K2: f64 = 0.03;
const SSIM_L: f64 = 255.0;
const SSIM_WIN: usize = 11;
const SSIM_SIGMA: f64 = 1.5;

/// Planar 4:2:0 image with float samples (8-bit scale), packed planes.
#[derive(Clone, Debug, PartialEq)]
pub struct Yuv420f {
    pub width: u32,
    pub height: u32,
    pub y: Vec<f32>,
    /// `chroma_size` samples each.
    pub u: Vec<f32>,
    pub v: Vec<f32>,
}

/// The reference a decoded frame is compared with.
pub type Reference = Yuv420f;

/// Chroma plane size of a `w x h` 4:2:0 picture (rounded up, as decoders give it).
pub fn chroma_size(w: u32, h: u32) -> (u32, u32) {
    (w.div_ceil(2), h.div_ceil(2))
}

impl Yuv420f {
    /// BT.601 limited range of the source, cropped to even size as the production
    /// conversion does (`convert::even_size`).
    pub fn from_bgra(img: &BgraImage) -> Yuv420f {
        let (w, h) = ((img.width & !1) as usize, (img.height & !1) as usize);
        let (cw, ch) = (w / 2, h / 2);
        let mut y = vec![0f32; w * h];
        let mut u = vec![0f32; cw * ch];
        let mut v = vec![0f32; cw * ch];
        // Y' = 16 + 65.481 R + 128.553 G + 24.966 B (R, G, B in 0..1), and so on.
        const KY: [f64; 3] = [65.481 / 255.0, 128.553 / 255.0, 24.966 / 255.0];
        const KU: [f64; 3] = [-37.797 / 255.0, -74.203 / 255.0, 112.0 / 255.0];
        const KV: [f64; 3] = [112.0 / 255.0, -93.786 / 255.0, -18.214 / 255.0];
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut su, mut sv) = (0f64, 0f64);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (px, py) = (cx * 2 + dx, cy * 2 + dy);
                        let p = &img.data[py * img.stride + px * 4..][..3];
                        let (b, g, r) = (p[0] as f64, p[1] as f64, p[2] as f64);
                        y[py * w + px] = (16.0 + KY[0] * r + KY[1] * g + KY[2] * b) as f32;
                        su += 128.0 + KU[0] * r + KU[1] * g + KU[2] * b;
                        sv += 128.0 + KV[0] * r + KV[1] * g + KV[2] * b;
                    }
                }
                u[cy * cw + cx] = (su / 4.0) as f32;
                v[cy * cw + cx] = (sv / 4.0) as f32;
            }
        }
        Yuv420f { width: w as u32, height: h as u32, y, u, v }
    }

    /// From an 8-bit I420 frame (e.g. the production conversion, for the diagnostic
    /// "codec only" PSNR). Planes on a GPU surface are read first.
    pub fn from_yuv(frame: &YuvFrame) -> Yuv420f {
        let f = frame.with_planes();
        let conv = |p: &[u8]| p.iter().map(|&x| x as f32).collect();
        Yuv420f { width: f.width, height: f.height, y: conv(&f.y), u: conv(&f.u), v: conv(&f.v) }
    }

    /// Rounded to an 8-bit I420 frame (an ideal converter).
    pub fn to_yuv(&self) -> YuvFrame {
        let conv = |p: &[f32]| p.iter().map(|&x| x.round().clamp(0.0, 255.0) as u8).collect();
        YuvFrame {
            width: self.width,
            height: self.height,
            y: conv(&self.y),
            u: conv(&self.u),
            v: conv(&self.v),
            ..Default::default()
        }
    }
}

/// PSNR of each plane and combined, in dB.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Psnr {
    pub y: f64,
    pub u: f64,
    pub v: f64,
    /// (6 Y + U + V) / 8, each capped at [`PSNR_CAP_DB`] (still +inf when all are).
    pub yuv: f64,
}

/// PSNR from a mean squared error (8-bit peak).
pub fn psnr_db(mse: f64) -> f64 {
    if mse <= 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

/// Caps +inf (and anything above) at [`PSNR_CAP_DB`].
pub fn cap(db: f64) -> f64 {
    if db.is_nan() { db } else { db.min(PSNR_CAP_DB) }
}

/// MSE over the common `w x h` top-left region of two planes.
fn mse(a: &[u8], a_stride: usize, b: &[f32], b_stride: usize, w: usize, h: usize) -> f64 {
    if w == 0 || h == 0 {
        return 0.0;
    }
    let mut sum = 0f64;
    for y in 0..h {
        let ra = &a[y * a_stride..][..w];
        let rb = &b[y * b_stride..][..w];
        let mut row = 0f64;
        for (&p, &q) in ra.iter().zip(rb) {
            let d = p as f64 - q as f64;
            row += d * d;
        }
        sum += row;
    }
    sum / (w * h) as f64
}

/// PSNR of a decoded frame against its reference, on the common region (the decoded
/// `width x height`, which `even_size` makes equal to the reference's).
pub fn psnr(decoded: &YuvFrame, reference: &Yuv420f) -> Psnr {
    let d = decoded.with_planes();
    let w = d.width.min(reference.width) as usize;
    let h = d.height.min(reference.height) as usize;
    let (dcw, _) = chroma_size(d.width, d.height);
    let (rcw, _) = chroma_size(reference.width, reference.height);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let y = psnr_db(mse(&d.y, d.width as usize, &reference.y, reference.width as usize, w, h));
    let u = psnr_db(mse(&d.u, dcw as usize, &reference.u, rcw as usize, cw, ch));
    let v = psnr_db(mse(&d.v, dcw as usize, &reference.v, rcw as usize, cw, ch));
    let yuv = if y.is_infinite() && u.is_infinite() && v.is_infinite() {
        f64::INFINITY
    } else {
        (6.0 * cap(y) + cap(u) + cap(v)) / 8.0
    };
    Psnr { y, u, v, yuv }
}

/// SSIM grid step used for a picture size (1 up to 2560x1440, then 2).
pub fn ssim_step_for(width: u32, height: u32) -> usize {
    if width as u64 * height as u64 > 2560 * 1440 { 2 } else { 1 }
}

/// SSIM of the luma of a decoded frame against its reference ([`ssim_step_for`]).
pub fn ssim_y(decoded: &YuvFrame, reference: &Yuv420f) -> f64 {
    let d = decoded.with_planes();
    let w = d.width.min(reference.width) as usize;
    let h = d.height.min(reference.height) as usize;
    let step = ssim_step_for(w as u32, h as u32);
    ssim_plane(&d.y, d.width as usize, &reference.y, reference.width as usize, w, h, step)
}

/// Normalised 11-tap Gaussian, sigma 1.5.
fn gaussian() -> [f64; SSIM_WIN] {
    let mut g = [0f64; SSIM_WIN];
    let c = (SSIM_WIN / 2) as f64;
    for (i, v) in g.iter_mut().enumerate() {
        let x = i as f64 - c;
        *v = (-(x * x) / (2.0 * SSIM_SIGMA * SSIM_SIGMA)).exp();
    }
    let s: f64 = g.iter().sum();
    g.map(|v| v / s)
}

/// Any 8-bit or float plane sample.
pub trait Sample: Copy {
    fn f(self) -> f64;
}
impl Sample for u8 {
    fn f(self) -> f64 {
        self as f64
    }
}
impl Sample for f32 {
    fn f(self) -> f64 {
        self as f64
    }
}

/// Mean SSIM of two planes over the `w x h` region, window positions every `step`
/// pixels in both directions (separable filter, 11 horizontal rows kept at a time).
/// Pictures smaller than the window give NaN.
pub fn ssim_plane<A: Sample, B: Sample>(
    a: &[A],
    a_stride: usize,
    b: &[B],
    b_stride: usize,
    w: usize,
    h: usize,
    step: usize,
) -> f64 {
    if w < SSIM_WIN || h < SSIM_WIN {
        return f64::NAN;
    }
    let step = step.max(1);
    let g = gaussian();
    let c1 = (SSIM_K1 * SSIM_L).powi(2);
    let c2 = (SSIM_K2 * SSIM_L).powi(2);
    let outs: Vec<usize> = (0..=w - SSIM_WIN).step_by(step).collect();
    let n = outs.len();
    // Horizontal sums of row y: [mu_a, mu_b, a^2, b^2, ab] at each output column.
    let mut ring = vec![[0f64; 5]; n * SSIM_WIN];
    let horizontal = |y: usize, slot: &mut [[f64; 5]]| {
        let ra = &a[y * a_stride..][..w];
        let rb = &b[y * b_stride..][..w];
        for (o, &x0) in outs.iter().enumerate() {
            let mut s = [0f64; 5];
            for k in 0..SSIM_WIN {
                let (p, q) = (ra[x0 + k].f(), rb[x0 + k].f());
                let gk = g[k];
                s[0] += gk * p;
                s[1] += gk * q;
                s[2] += gk * p * p;
                s[3] += gk * q * q;
                s[4] += gk * p * q;
            }
            slot[o] = s;
        }
    };
    let mut total = 0f64;
    let mut count = 0usize;
    for y in 0..h {
        let r = y % SSIM_WIN;
        horizontal(y, &mut ring[r * n..(r + 1) * n]);
        if y + 1 < SSIM_WIN {
            continue;
        }
        let y0 = y + 1 - SSIM_WIN; // top row of the window
        if !y0.is_multiple_of(step) {
            continue;
        }
        for o in 0..n {
            let mut s = [0f64; 5];
            for k in 0..SSIM_WIN {
                let row = &ring[((y0 + k) % SSIM_WIN) * n + o];
                for (acc, v) in s.iter_mut().zip(row) {
                    *acc += g[k] * v;
                }
            }
            let (ma, mb) = (s[0], s[1]);
            let va = s[2] - ma * ma;
            let vb = s[3] - mb * mb;
            let cov = s[4] - ma * mb;
            total += ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            count += 1;
        }
    }
    total / count as f64
}

/// Mean, minimum and 5th percentile of per-frame values (PSNR capped).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Aggregate {
    pub n: usize,
    pub mean: f64,
    pub min: f64,
    pub p5: f64,
}

impl Aggregate {
    pub fn of(values: &[f64]) -> Aggregate {
        let v: Vec<f64> = values.iter().map(|&x| cap(x)).filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return Aggregate { n: 0, mean: f64::NAN, min: f64::NAN, p5: f64::NAN };
        }
        Aggregate {
            n: v.len(),
            mean: v.iter().sum::<f64>() / v.len() as f64,
            min: v.iter().copied().fold(f64::INFINITY, f64::min),
            p5: crate::measure::percentile(&v, 5.0),
        }
    }
}

// ---------------------------------------------------------------------------
// Bjontegaard delta

/// Least-squares polynomial of degree `deg`: coefficients c0..c_deg.
fn polyfit(x: &[f64], y: &[f64], deg: usize) -> Option<Vec<f64>> {
    let m = deg + 1;
    // Normal equations A^T A c = A^T y.
    let mut a = vec![vec![0f64; m + 1]; m];
    for (&xi, &yi) in x.iter().zip(y) {
        let mut pw = vec![1f64; 2 * m];
        for k in 1..2 * m {
            pw[k] = pw[k - 1] * xi;
        }
        for r in 0..m {
            for c in 0..m {
                a[r][c] += pw[r + c];
            }
            a[r][m] += pw[r] * yi;
        }
    }
    // Gaussian elimination with partial pivoting.
    for col in 0..m {
        let piv = (col..m).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        for r in 0..m {
            if r != col {
                let f = a[r][col] / a[col][col];
                #[allow(clippy::needless_range_loop)] // row r and pivot row col of one matrix
                for c in col..=m {
                    a[r][c] -= f * a[col][c];
                }
            }
        }
    }
    Some((0..m).map(|r| a[r][m] / a[r][r]).collect())
}

/// One rate-quality curve fitted as `value = p(t)`, `t = (q - centre) / scale`.
struct Fit {
    centre: f64,
    scale: f64,
    coef: Vec<f64>,
}

impl Fit {
    fn new(q: &[f64], value: &[f64]) -> Option<Fit> {
        if q.len() < 2 {
            return None;
        }
        let lo = q.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = q.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let centre = (lo + hi) / 2.0;
        let scale = if hi > lo { (hi - lo) / 2.0 } else { 1.0 };
        let t: Vec<f64> = q.iter().map(|&v| (v - centre) / scale).collect();
        let coef = polyfit(&t, value, (q.len() - 1).min(3))?;
        Some(Fit { centre, scale, coef })
    }

    /// Integral of p over q in [a, b].
    fn integral(&self, a: f64, b: f64) -> f64 {
        let prim = |q: f64| {
            let t = (q - self.centre) / self.scale;
            self.coef.iter().enumerate().map(|(k, c)| c * t.powi(k as i32 + 1) / (k + 1) as f64).sum::<f64>()
        };
        (prim(b) - prim(a)) * self.scale
    }
}

fn range(points: &[(f64, f64)], f: impl Fn(&(f64, f64)) -> f64) -> (f64, f64) {
    points.iter().map(f).fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)))
}

/// BD-rate in percent of `new` against `base`; points are (bitrate, quality) with
/// quality in dB (PSNR) or SSIM. Positive = more bits for the same quality
/// (regression). NaN when undefined (fewer than 2 points, no overlap, bad rates).
pub fn bd_rate(base: &[(f64, f64)], new: &[(f64, f64)]) -> f64 {
    if base
        .iter()
        .chain(new)
        .any(|p| p.0.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) || !p.1.is_finite())
    {
        return f64::NAN;
    }
    let fit = |pts: &[(f64, f64)]| {
        let q: Vec<f64> = pts.iter().map(|p| p.1).collect();
        let r: Vec<f64> = pts.iter().map(|p| p.0.ln()).collect();
        Fit::new(&q, &r)
    };
    let (Some(fa), Some(fb)) = (fit(base), fit(new)) else { return f64::NAN };
    let (alo, ahi) = range(base, |p| p.1);
    let (blo, bhi) = range(new, |p| p.1);
    let (lo, hi) = (alo.max(blo), ahi.min(bhi));
    if hi.partial_cmp(&lo) != Some(std::cmp::Ordering::Greater) {
        return f64::NAN;
    }
    let avg = (fb.integral(lo, hi) - fa.integral(lo, hi)) / (hi - lo);
    (avg.exp() - 1.0) * 100.0
}

/// BD-quality (BD-PSNR when quality is PSNR): mean quality difference of `new`
/// against `base` at the same bitrate. Positive = better.
pub fn bd_quality(base: &[(f64, f64)], new: &[(f64, f64)]) -> f64 {
    if base
        .iter()
        .chain(new)
        .any(|p| p.0.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) || !p.1.is_finite())
    {
        return f64::NAN;
    }
    let fit = |pts: &[(f64, f64)]| {
        let r: Vec<f64> = pts.iter().map(|p| p.0.ln()).collect();
        let q: Vec<f64> = pts.iter().map(|p| p.1).collect();
        Fit::new(&r, &q)
    };
    let (Some(fa), Some(fb)) = (fit(base), fit(new)) else { return f64::NAN };
    let (alo, ahi) = range(base, |p| p.0.ln());
    let (blo, bhi) = range(new, |p| p.0.ln());
    let (lo, hi) = (alo.max(blo), ahi.min(bhi));
    if hi.partial_cmp(&lo) != Some(std::cmp::Ordering::Greater) {
        return f64::NAN;
    }
    (fb.integral(lo, hi) - fa.integral(lo, hi)) / (hi - lo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Xoshiro256;

    fn yuv(w: u32, h: u32, seed: u64) -> YuvFrame {
        let mut r = Xoshiro256::new(seed);
        let (cw, ch) = chroma_size(w, h);
        YuvFrame {
            width: w,
            height: h,
            y: r.bytes((w * h) as usize),
            u: r.bytes((cw * ch) as usize),
            v: r.bytes((cw * ch) as usize),
            ..Default::default()
        }
    }

    #[test]
    fn identical_images_are_inf_db_and_ssim_one() {
        let f = yuv(64, 48, 1);
        let r = Yuv420f::from_yuv(&f);
        let p = psnr(&f, &r);
        assert!(p.y.is_infinite() && p.u.is_infinite() && p.v.is_infinite() && p.yuv.is_infinite());
        assert!((ssim_y(&f, &r) - 1.0).abs() < 1e-12);
        assert_eq!(cap(p.y), PSNR_CAP_DB);
    }

    #[test]
    fn psnr_known_value() {
        // Every luma sample off by 1, chroma off by 2: MSE 1 and 4.
        let f = yuv(32, 32, 2);
        let mut r = Yuv420f::from_yuv(&f);
        r.y.iter_mut().for_each(|v| *v = if *v < 255.0 { *v + 1.0 } else { *v - 1.0 });
        r.u.iter_mut().for_each(|v| *v = if *v < 254.0 { *v + 2.0 } else { *v - 2.0 });
        r.v.iter_mut().for_each(|v| *v = if *v < 254.0 { *v + 2.0 } else { *v - 2.0 });
        let p = psnr(&f, &r);
        let p1 = 20.0 * 255f64.log10();
        let p4 = 10.0 * (255.0f64 * 255.0 / 4.0).log10();
        assert!((p.y - p1).abs() < 1e-9, "{}", p.y);
        assert!((p.u - p4).abs() < 1e-9 && (p.v - p4).abs() < 1e-9);
        assert!((p.yuv - (6.0 * p1 + 2.0 * p4) / 8.0).abs() < 1e-9);
        assert!((p1 - 48.130_803_608_679_1).abs() < 1e-9);
    }

    #[test]
    fn bt601_reference_known_colours() {
        // White -> (235, 128, 128), black -> (16, 128, 128), pure red -> (81.481, 90.203, 240).
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend_from_slice(&[255, 255, 255, 255]);
        }
        let img = BgraImage { width: 2, height: 2, stride: 8, data: &data };
        let r = Yuv420f::from_bgra(&img);
        assert!(
            (r.y[0] - 235.0).abs() < 1e-3 && (r.u[0] - 128.0).abs() < 1e-3 && (r.v[0] - 128.0).abs() < 1e-3
        );
        let red: Vec<u8> = (0..4).flat_map(|_| [0u8, 0, 255, 255]).collect();
        let r = Yuv420f::from_bgra(&BgraImage { width: 2, height: 2, stride: 8, data: &red });
        assert!((r.y[0] - 81.481).abs() < 1e-3, "{}", r.y[0]);
        assert!((r.u[0] - 90.203).abs() < 1e-3, "{}", r.u[0]);
        assert!((r.v[0] - 240.0).abs() < 1e-3, "{}", r.v[0]);
        // Odd sizes are cropped to even.
        let odd = vec![0u8; 5 * 3 * 4];
        let r = Yuv420f::from_bgra(&BgraImage { width: 5, height: 3, stride: 20, data: &odd });
        assert_eq!((r.width, r.height, r.u.len()), (4, 2, 2));
    }

    #[test]
    fn ssim_constant_images_closed_form() {
        // Constant planes a and b: variances and covariance 0, so
        // SSIM = (2ab + C1) / (a^2 + b^2 + C1).
        let (w, h) = (24usize, 20usize);
        let a = vec![100u8; w * h];
        let b = vec![140f32; w * h];
        let c1 = (0.01f64 * 255.0).powi(2);
        let want = (2.0 * 100.0 * 140.0 + c1) / (100.0f64.powi(2) + 140.0f64.powi(2) + c1);
        let got = ssim_plane(&a, w, &b, w, w, h, 1);
        assert!((got - want).abs() < 1e-12, "{got} {want}");
    }

    /// Direct (non-separable) SSIM, as Wang et al.'s ssim_index.m computes it.
    fn ssim_direct(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
        let g1 = gaussian();
        let c1 = (0.01f64 * 255.0).powi(2);
        let c2 = (0.03f64 * 255.0).powi(2);
        let mut total = 0.0;
        let mut n = 0;
        for y0 in 0..=h - 11 {
            for x0 in 0..=w - 11 {
                let (mut ma, mut mb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
                for j in 0..11 {
                    for i in 0..11 {
                        let g = g1[i] * g1[j];
                        let p = a[(y0 + j) * w + x0 + i] as f64;
                        let q = b[(y0 + j) * w + x0 + i] as f64;
                        ma += g * p;
                        mb += g * q;
                        saa += g * p * p;
                        sbb += g * q * q;
                        sab += g * p * q;
                    }
                }
                let (va, vb, cov) = (saa - ma * ma, sbb - mb * mb, sab - ma * mb);
                total +=
                    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2));
                n += 1;
            }
        }
        total / n as f64
    }

    #[test]
    fn ssim_matches_direct_implementation() {
        let (w, h) = (37usize, 29usize);
        let mut r = Xoshiro256::new(9);
        let a: Vec<u8> = (0..w * h).map(|i| ((i % w) * 5 + (i / w) * 3) as u8).collect();
        let b: Vec<u8> = a.iter().map(|&x| x.saturating_add(r.below(40) as u8).saturating_sub(20)).collect();
        let direct = ssim_direct(&a, &b, w, h);
        let fast = ssim_plane(&a, w, &b, w, w, h, 1);
        assert!((direct - fast).abs() < 1e-10, "{direct} {fast}");
        assert!(fast < 0.999 && fast > 0.3, "{fast}");
        // Stride 2 samples a subset of the same windows.
        let s2 = ssim_plane(&a, w, &b, w, w, h, 2);
        assert!((s2 - fast).abs() < 0.05);
        // Gaussian weights: symmetric, sum 1, centre weight known (sigma 1.5).
        let g = gaussian();
        assert!((g.iter().sum::<f64>() - 1.0).abs() < 1e-15);
        assert!((g[5] - 0.266_011_724_861_6).abs() < 1e-9, "{}", g[5]);
    }

    #[test]
    fn bd_rate_known_shifts() {
        let base = [(1000.0, 30.0), (2000.0, 33.1), (4000.0, 35.9), (8000.0, 38.2)];
        assert!(bd_rate(&base, &base).abs() < 1e-9);
        // Same quality with 10% more bits everywhere: +10%.
        let more: Vec<(f64, f64)> = base.iter().map(|&(r, q)| (r * 1.1, q)).collect();
        assert!((bd_rate(&base, &more) - 10.0).abs() < 1e-9, "{}", bd_rate(&base, &more));
        let less: Vec<(f64, f64)> = base.iter().map(|&(r, q)| (r * 0.8, q)).collect();
        assert!((bd_rate(&base, &less) + 20.0).abs() < 1e-9);
        // Better quality at the same rates: negative BD-rate, positive BD-PSNR.
        let better: Vec<(f64, f64)> = base.iter().map(|&(r, q)| (r, q + 0.5)).collect();
        assert!(bd_rate(&base, &better) < 0.0);
        assert!((bd_quality(&base, &better) - 0.5).abs() < 1e-9);
        // Two points (quick tier ladder): linear fit, same exact shift.
        let two = [(1000.0, 30.0), (4000.0, 36.0)];
        let two_more = [(1100.0, 30.0), (4400.0, 36.0)];
        assert!((bd_rate(&two, &two_more) - 10.0).abs() < 1e-9);
        // No overlap or invalid input.
        assert!(bd_rate(&[(1.0, 1.0), (2.0, 2.0)], &[(1.0, 5.0), (2.0, 6.0)]).is_nan());
        assert!(bd_rate(&[(0.0, 30.0), (1.0, 31.0)], &base).is_nan());
    }

    #[test]
    fn aggregate_caps_inf() {
        let a = Aggregate::of(&[f64::INFINITY, 40.0, 30.0]);
        assert_eq!(a.n, 3);
        assert_eq!(a.min, 30.0);
        assert!((a.mean - (100.0 + 70.0) / 3.0).abs() < 1e-9);
    }
}
