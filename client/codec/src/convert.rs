//! Conversione BGRA → YUV 4:2:0 (BT.601, gamma limitata) con aritmetica intera,
//! divisa su più core per righe di blocchi 2x2.
//!
//! Formula (ITU-R BT.601, coefficienti a 8 bit):
//!   Y = ((66 R + 129 G + 25 B + 128) >> 8) + 16
//!   U = ((-38 R - 74 G + 112 B + 128) >> 8) + 128
//!   V = ((112 R - 94 G - 18 B + 128) >> 8) + 128
//! U e V sono calcolati sulla media del blocco 2x2.

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

/// Converte due righe BGRA in due righe di luma e una riga di crominanza.
/// `put_chroma(x, u, v)` scrive la crominanza della colonna di blocchi `x`.
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

/// Dimensioni pari usate dalla conversione (si scarta al massimo una riga/colonna).
pub fn even_size(image: &BgraImage) -> (usize, usize) {
    ((image.width & !1) as usize, (image.height & !1) as usize)
}

/// BGRA → I420 (tre piani: Y w×h, U e V w/2×h/2). I buffer vengono ridimensionati.
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

/// BGRA → NV12 (piano Y w×h seguito da UV interlacciati w×h/2) con passo di riga `pitch`
/// e `height_aligned` righe di luma prima del piano UV (allineamento richiesto da alcuni encoder).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((w * h) as usize)
    }

    #[test]
    fn known_colors() {
        // Bianco, nero e rosso puro in BT.601 a gamma limitata.
        for (bgra, yuv) in [
            ([255, 255, 255, 255], (235, 128, 128)),
            ([0, 0, 0, 255], (16, 128, 128)),
            ([0, 0, 255, 255], (82, 90, 240)),
        ] {
            let data = solid(4, 2, bgra);
            let img = BgraImage { width: 4, height: 2, stride: 16, data: &data };
            let (mut y, mut u, mut v) = (vec![], vec![], vec![]);
            bgra_to_i420(&img, &mut y, &mut u, &mut v);
            assert_eq!((y[0], u[0], v[0]), yuv, "colore {bgra:?}");

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

        // Righe NV12 con passo 16 e 8 righe allineate.
        let mut nv12 = vec![0u8; 16 * 8 + 16 * 4];
        bgra_to_nv12(&img, &mut nv12, 16, 8);
        assert_eq!(nv12[16 * 3 + 5], y[3 * 6 + 5]);
        assert_eq!(nv12[16 * 8 + 16 + 4], u[3 + 2]);
    }
}
