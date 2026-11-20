//! BGRA → YUV 4:2:0 conversion (BT.601, limited range) with integer arithmetic,
//! split over several cores by rows of 2x2 blocks (a pool of parked threads, made at
//! the first frame).
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

/// A small pool of parked threads for the conversion and scaling jobs (VID-7): the
/// threads are made once, at the first frame, instead of `threads()` new OS threads
/// (stack mapping, start, join) for every frame.
mod pool {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

    /// Jobs of one call still running, and whether one of them panicked.
    #[derive(Default)]
    struct Latch {
        state: Mutex<(usize, bool)>,
        done: Condvar,
    }

    impl Latch {
        fn finished(&self, panicked: bool) {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.0 -= 1;
            state.1 |= panicked;
            if state.0 == 0 {
                self.done.notify_all();
            }
        }
    }

    struct Task {
        /// The caller's job: valid until the latch reaches 0 (`run` waits for it).
        job: *const (dyn Fn(usize) + Sync + 'static),
        index: usize,
        latch: Arc<Latch>,
    }

    // SAFETY: the job is `Sync` and outlives the task (see `run`).
    unsafe impl Send for Task {}

    fn workers() -> &'static [mpsc::Sender<Task>] {
        static WORKERS: OnceLock<Vec<mpsc::Sender<Task>>> = OnceLock::new();
        WORKERS.get_or_init(|| {
            (1..super::threads())
                .filter_map(|n| {
                    let (tx, rx) = mpsc::channel::<Task>();
                    std::thread::Builder::new()
                        .name(format!("nuvira-convert-{n}"))
                        .spawn(move || {
                            for task in rx {
                                // SAFETY: `run` does not return before this task's `finished`.
                                let job = unsafe { &*task.job };
                                let panicked = catch_unwind(AssertUnwindSafe(|| job(task.index))).is_err();
                                task.latch.finished(panicked);
                            }
                        })
                        .ok()
                        .map(|_| tx)
                })
                .collect()
        })
    }

    /// Runs `job(0)` .. `job(count - 1)`, job 0 on the calling thread and the others on
    /// the pool, and returns when all have ended (a job's panic is raised again here).
    pub fn run(count: usize, job: &(dyn Fn(usize) + Sync)) {
        let workers = workers();
        if count <= 1 || workers.is_empty() {
            (0..count).for_each(job);
            return;
        }
        let latch = Arc::new(Latch::default());
        // SAFETY: only the lifetime is erased; this function waits below until every
        // task sent has run (the wait is reached even when job 0 panics).
        let erased: *const (dyn Fn(usize) + Sync + 'static) = unsafe { std::mem::transmute(job) };
        let mut inline = Vec::new();
        for index in 1..count {
            latch.state.lock().unwrap_or_else(|e| e.into_inner()).0 += 1;
            let task = Task { job: erased, index, latch: latch.clone() };
            if let Err(mpsc::SendError(task)) = workers[(index - 1) % workers.len()].send(task) {
                // A worker that is gone: the job runs here.
                task.latch.finished(false);
                inline.push(index);
            }
        }
        let mine = catch_unwind(AssertUnwindSafe(|| {
            job(0);
            inline.iter().for_each(|&i| job(i));
        }));
        let mut state = latch.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.0 > 0 {
            state = latch.done.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        let panicked = state.1;
        drop(state);
        if let Err(payload) = mine {
            resume_unwind(payload);
        }
        if panicked {
            panic!("a conversion job panicked");
        }
    }

    /// Runs `job` once for each item, as [`run`] does (one item per job).
    pub fn each<T: Send>(items: impl Iterator<Item = T>, job: impl Fn(usize, T) + Sync) {
        let items: Vec<Mutex<Option<T>>> = items.map(|t| Mutex::new(Some(t))).collect();
        run(items.len(), &|i| {
            if let Some(item) = items[i].lock().unwrap_or_else(|e| e.into_inner()).take() {
                job(i, item);
            }
        });
    }
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
    let jobs = y
        .chunks_mut(w * 2 * rows_per_job)
        .zip(u.chunks_mut(cw * rows_per_job))
        .zip(v.chunks_mut(cw * rows_per_job));
    pool::each(jobs, |job, ((y, u), v)| {
        for (i, ((yy, uu), vv)) in y.chunks_mut(w * 2).zip(u.chunks_mut(cw)).zip(v.chunks_mut(cw)).enumerate()
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
    let jobs = luma_plane[..pitch * h]
        .chunks_mut(pitch * 2 * rows_per_job)
        .zip(chroma_plane[..pitch * ch].chunks_mut(pitch * rows_per_job));
    pool::each(jobs, |job, (y, uv)| {
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

/// A column-sum row of [`scale_bgra`] (one per job), taken from a small store and given
/// back when the job ends: the rows are made once, not at every frame (a 4K row is 60 KiB).
struct ColumnSums(Vec<u32>);

static COLUMN_SUMS: std::sync::Mutex<Vec<Vec<u32>>> = std::sync::Mutex::new(Vec::new());

impl ColumnSums {
    fn take(len: usize) -> Self {
        let mut row = COLUMN_SUMS.lock().unwrap_or_else(|e| e.into_inner()).pop().unwrap_or_default();
        row.resize(len, 0);
        Self(row)
    }
}

impl Drop for ColumnSums {
    fn drop(&mut self) {
        let mut store = COLUMN_SUMS.lock().unwrap_or_else(|e| e.into_inner());
        // At most one row per job of two callers at once.
        if store.len() < 2 * threads() {
            store.push(std::mem::take(&mut self.0));
        }
    }
}

impl std::ops::Deref for ColumnSums {
    type Target = [u32];
    fn deref(&self) -> &[u32] {
        &self.0
    }
}

impl std::ops::DerefMut for ColumnSums {
    fn deref_mut(&mut self) -> &mut [u32] {
        &mut self.0
    }
}

/// Scales `image` down to `width`x`height` (clamped to the image size), each output
/// pixel being the average of the source pixels it covers: text stays readable
/// instead of losing strokes as with point sampling. `out` gets packed rows.
///
/// Separable: the source rows of an output row are first added column by column into
/// a `u32` row (a straight loop the compiler vectorises), then each output pixel adds
/// its columns of that row once. Same integer sums, count and rounding as adding the
/// whole box pixel by pixel, so the same bytes.
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
    let cols = &cols;
    pool::each(out.chunks_mut(width * 4 * rows_per_job), |job, chunk| {
        let mut column_sums = ColumnSums::take(sw * 4);
        for (i, row) in chunk.chunks_mut(width * 4).enumerate() {
            let y = job * rows_per_job + i;
            let (y0, y1) = (y * sh / height, (y + 1) * sh / height);
            let first = &image.data[y0 * image.stride..][..sw * 4];
            for (sum, &v) in column_sums.iter_mut().zip(first) {
                *sum = v as u32;
            }
            for sy in y0 + 1..y1 {
                let src = &image.data[sy * image.stride..][..sw * 4];
                for (sum, &v) in column_sums.iter_mut().zip(src) {
                    *sum += v as u32;
                }
            }
            for (x, dst) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (c0, c1) = (cols[x], cols[x + 1]);
                let mut sum = [0u32; 4];
                for px in column_sums[c0 * 4..c1 * 4].as_chunks::<4>().0 {
                    for c in 0..4 {
                        sum[c] += px[c];
                    }
                }
                let n = ((c1 - c0) * (y1 - y0)) as u32;
                for c in 0..4 {
                    dst[c] = ((sum[c] + n / 2) / n) as u8;
                }
            }
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

    /// The box average as it was written before the separable version (VID-8): every
    /// source pixel of the box added one by one. The reference of `scale_matches_reference`.
    fn scale_bgra_reference(image: &BgraImage, width: usize, height: usize) -> Vec<u8> {
        let (sw, sh) = (image.width as usize, image.height as usize);
        let (width, height) = (width.min(sw), height.min(sh));
        let mut out = vec![0u8; width * height * 4];
        if width == 0 || height == 0 {
            return out;
        }
        let cols: Vec<usize> = (0..=width).map(|x| x * sw / width).collect();
        for (y, row) in out.chunks_mut(width * 4).enumerate() {
            let (y0, y1) = (y * sh / height, (y + 1) * sh / height);
            let mut sums = vec![[0u32; 4]; width];
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
        out
    }

    #[test]
    fn scale_matches_reference() {
        // Random pixels (xorshift, fixed seed), padded rows, odd sizes and every kind of
        // ratio: identity, exact 2:1 and 3:1, fractional, one output row/column.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let sizes = [(1, 1), (2, 2), (7, 5), (64, 36), (97, 61), (320, 180), (643, 419)];
        let mut cases = 0;
        for &(sw, sh) in &sizes {
            let stride = sw * 4 + (next() % 3) as usize * 4;
            let data: Vec<u8> = (0..stride * sh).map(|_| next() as u8).collect();
            let img = BgraImage { width: sw as u32, height: sh as u32, stride, data: &data };
            let mut targets = vec![(sw, sh), (sw / 2, sh / 2), (sw / 3, sh / 3), (1, 1), (sw, 1), (1, sh)];
            for _ in 0..6 {
                targets.push((1 + next() as usize % sw, 1 + next() as usize % sh));
            }
            targets.push((sw + 5, sh + 3));
            for (dw, dh) in targets {
                let mut out = Vec::new();
                scale_bgra(&img, dw, dh, &mut out);
                assert_eq!(out, scale_bgra_reference(&img, dw, dh), "{sw}x{sh} -> {dw}x{dh}");
                cases += 1;
            }
        }
        // Large ratios as the agent meets them (4K and 1440p screens, small windows).
        for (sw, sh, dw, dh) in [(3840, 2160, 1920, 1080), (2560, 1440, 1920, 1080), (3840, 2160, 1366, 768)]
        {
            let data: Vec<u8> = (0..sw * sh * 4).map(|_| next() as u8).collect();
            let img = BgraImage { width: sw as u32, height: sh as u32, stride: sw * 4, data: &data };
            let mut out = Vec::new();
            scale_bgra(&img, dw, dh, &mut out);
            assert_eq!(out, scale_bgra_reference(&img, dw, dh), "{sw}x{sh} -> {dw}x{dh}");
            cases += 1;
        }
        assert!(cases > 90);
    }

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((w * h) as usize)
    }

    #[test]
    fn pool_runs_every_job_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for count in [0, 1, 2, threads(), threads() * 3 + 1] {
            let hits: Vec<AtomicUsize> = (0..count).map(|_| AtomicUsize::new(0)).collect();
            pool::run(count, &|i| {
                hits[i].fetch_add(1, Ordering::Relaxed);
            });
            assert!(hits.iter().all(|h| h.load(Ordering::Relaxed) == 1), "{count} jobs");
        }
        // Several callers at once (two sessions, all screens) share the pool.
        std::thread::scope(|s| {
            for t in 0..4 {
                s.spawn(move || {
                    for n in 0..200 {
                        let mut out = vec![0usize; 16];
                        pool::each(out.chunks_mut(2), |job, c| c.fill(job + t + n));
                        let want: Vec<usize> = (0..16).map(|i| i / 2 + t + n).collect();
                        assert_eq!(out, want);
                    }
                });
            }
        });
    }

    #[test]
    fn pool_raises_a_job_panic_and_keeps_working() {
        for bad in [0, 1] {
            let r = std::panic::catch_unwind(|| pool::run(threads().max(2), &|i| assert_ne!(i, bad)));
            assert!(r.is_err(), "job {bad} panicked");
        }
        let mut out = vec![0u8; 64];
        pool::each(out.chunks_mut(8), |job, c| c.fill(job as u8));
        assert_eq!(out[63], 7);
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
