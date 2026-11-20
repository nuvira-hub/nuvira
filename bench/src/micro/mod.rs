//! Micro benchmarks B-MICRO-01..18 (plan section 7.1), work package WP2.
//!
//! Files: [`convert`] (01-03), [`codec_sw`] (04 and the shared encode harness),
//! [`codec_hw`] (05), [`gpu_win`] (06-07), [`decode`] (08-10), [`proto`] (11),
//! [`fec`] (12-13), [`crypto`] (14 password hash, 16 QUIC/TLS throughput),
//! [`audio`] (15), [`capture`] (17), [`clipboard`] (18, added in Phase 6 lot 23).
//!
//! Every case is a [`MicroCase`]. Cases whose hardware or OS is missing are always
//! registered (the same keys on every machine) and report `not-runnable` with the
//! reason; hardware probed only at run time reports `not-runnable` from `run`.
//! Cases marked Q in the plan run in both tiers at their quick parameters, the rest
//! only in `full`.

pub mod audio;
pub mod capture;
pub mod clipboard;
pub mod codec_hw;
pub mod codec_sw;
pub mod convert;
pub mod crypto;
pub mod decode;
pub mod fec;
pub mod gpu_win;
pub mod proto;

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use nuvira_codec::YuvFrame;

use crate::alloc::AllocSnapshot;
use crate::fixtures::Frame;
use crate::measure::{Better, Gate, MAX_NOISY_RETRIES, Metric, NoiseGuard, Reps, Sampler};
use crate::quality::{self, Aggregate, Psnr, Reference};
use crate::registry::{Case, CaseResult, Level, Registry, RunCtx, Status, Tier};

pub fn register(reg: &mut Registry) {
    convert::register(reg);
    codec_sw::register(reg);
    codec_hw::register(reg);
    gpu_win::register(reg);
    decode::register(reg);
    proto::register(reg);
    fec::register(reg);
    crypto::register(reg);
    audio::register(reg);
    capture::register(reg);
    clipboard::register(reg);
}

// ---------------------------------------------------------------------------
// The case type

type RunFn = Box<dyn FnMut(&mut RunCtx, CaseResult) -> Result<CaseResult> + Send>;

/// A micro case: identity, tiers, a lazy runnability check and the run closure,
/// which gets a fresh `ok` result with the identity filled in.
pub struct MicroCase {
    id: &'static str,
    name: String,
    tiers: &'static [Tier],
    config_ids: &'static [&'static str],
    runnable: fn() -> Result<(), String>,
    env: Vec<(String, String)>,
    isolated: bool,
    /// Reps cap in tier `full` (time budget, plan "Changes during implementation").
    full_reps: Option<u32>,
    run: RunFn,
}

fn always() -> Result<(), String> {
    Ok(())
}

impl MicroCase {
    pub fn new(
        id: &'static str,
        name: impl Into<String>,
        tiers: &'static [Tier],
        run: impl FnMut(&mut RunCtx, CaseResult) -> Result<CaseResult> + Send + 'static,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            tiers,
            config_ids: os_config_ids(),
            runnable: always,
            env: Vec::new(),
            isolated: false,
            full_reps: None,
            run: Box::new(run),
        }
    }

    pub fn config(mut self, ids: &'static [&'static str]) -> Self {
        self.config_ids = ids;
        self
    }

    pub fn runnable(mut self, f: fn() -> Result<(), String>) -> Self {
        self.runnable = f;
        self
    }

    /// A process-wide variable the case needs (it then runs in a child process).
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// At most `reps` measured reps in tier `full` (never fewer than
    /// [`MIN_REPS`]); the quick tier keeps the run's default.
    pub fn full_reps(mut self, reps: u32) -> Self {
        self.full_reps = Some(reps.max(MIN_REPS));
        self
    }

    /// Runs in a child process of its own.
    pub fn isolated(mut self) -> Self {
        self.isolated = true;
        self
    }
}

impl Case for MicroCase {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn level(&self) -> Level {
        Level::Micro
    }
    fn tiers(&self) -> &'static [Tier] {
        self.tiers
    }
    fn config_ids(&self) -> &'static [&'static str] {
        self.config_ids
    }
    fn runnable(&self) -> Result<(), String> {
        (self.runnable)()
    }
    fn run(&mut self, ctx: &mut RunCtx) -> Result<CaseResult> {
        let r = CaseResult::new(&*self);
        (self.run)(ctx, r)
    }
    fn env(&self) -> Vec<(String, String)> {
        self.env.clone()
    }
    fn isolated(&self) -> bool {
        self.isolated || !self.env.is_empty()
    }
    fn reps_for(&self, tier: Tier, default: u32) -> u32 {
        match (tier, self.full_reps) {
            (Tier::Full, Some(cap)) => default.min(cap),
            _ => default,
        }
    }
}

/// Fewest measured reps a case may be cut to (median + stddev need >= 5,
/// `REVISIONE_COMPLETA.md`).
pub const MIN_REPS: u32 = 5;

/// Turns a result into `not-runnable` with `reason` (hardware probed at run time).
pub fn not_runnable(mut r: CaseResult, reason: impl Into<String>) -> CaseResult {
    r.status = Status::NotRunnable;
    r.metrics.clear();
    r.notes.push(reason.into());
    r
}

/// Config IDs of `hw-matrix.md` for a CPU-only case on this OS.
pub fn os_config_ids() -> &'static [&'static str] {
    static IDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    IDS.get_or_init(|| {
        let mut v = Vec::new();
        if cfg!(windows) {
            let build: u32 = sysinfo::System::kernel_version()
                .and_then(|k| k.split('.').next_back().and_then(|b| b.parse().ok()))
                .unwrap_or(0);
            v.push(if build >= 22000 { "WIN11" } else { "WIN10" });
            if cfg!(target_arch = "aarch64") {
                v.push("WIN-ARM64");
            }
        } else if cfg!(target_os = "macos") {
            v.push(if cfg!(target_arch = "aarch64") { "MAC-ARM" } else { "MAC-INTEL" });
        } else if cfg!(target_os = "linux") {
            let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
            let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_uppercase();
            if session == "x11" {
                v.push("LINUX-X11");
            } else if session == "wayland" {
                v.push(if desktop.contains("KDE") { "LINUX-WAYLAND-KDE" } else { "LINUX-WAYLAND-GNOME" });
            }
        }
        v
    })
    .as_slice()
}

pub fn only_windows() -> Result<(), String> {
    if cfg!(windows) { Ok(()) } else { Err("needs Windows".into()) }
}

pub fn only_linux() -> Result<(), String> {
    if cfg!(target_os = "linux") { Ok(()) } else { Err("needs Linux".into()) }
}

pub fn only_macos() -> Result<(), String> {
    if cfg!(target_os = "macos") { Ok(()) } else { Err("needs macOS".into()) }
}

// ---------------------------------------------------------------------------
// Small helpers

pub fn res(w: u32, h: u32) -> String {
    format!("{w}x{h}")
}

/// Bitrate for `w x h` from one given at 1920x1080, scaled by pixel count
/// (rounded to 10 kbit/s).
pub fn scaled_kbps(kbps_1080p: u32, w: u32, h: u32) -> u32 {
    let k = kbps_1080p as u64 * w as u64 * h as u64 / (1920 * 1080);
    ((k + 5) / 10 * 10).max(10) as u32
}

pub fn sha_hex(bytes: &[u8]) -> String {
    crate::fixtures::hex(&crate::fixtures::sha256(bytes))
}

pub fn metric(unit: &str, better: Better, gate: Gate, values: Vec<f64>) -> Metric {
    Metric::from_values(unit, better, gate, values)
}

/// CPU ms per unit of work, one value per rep; `info` when a rep's CPU time
/// (`rep_cpu_ms`) is too short for the CPU clock ([`crate::measure::cpu_gate`]).
pub fn cpu_metric(values: Vec<f64>, rep_cpu_ms: impl IntoIterator<Item = f64>) -> Metric {
    let rep = rep_cpu_ms.into_iter().map(|ms| std::time::Duration::from_secs_f64(ms.max(0.0) / 1e3));
    metric("ms", Better::Lower, crate::measure::cpu_gate(Gate::Micro, rep), values)
}

pub fn single(unit: &str, better: Better, gate: Gate, value: f64) -> Metric {
    Metric::single(unit, better, gate, value)
}

/// Process-wide allocations per call of `f` over `n` calls (for code that spawns
/// threads, where the per-thread counter of the [`Sampler`] misses their work).
pub fn process_allocs(n: u64, mut f: impl FnMut()) -> (f64, f64) {
    let a0 = AllocSnapshot::take();
    for _ in 0..n {
        f();
    }
    let d = AllocSnapshot::take().since(&a0).total();
    (d.allocs as f64 / n.max(1) as f64, d.bytes as f64 / n.max(1) as f64)
}

/// The standard time metrics of a [`Sampler`] run: wall time per call, CPU of the
/// calling thread and of the whole process per call, and the thread's allocations.
pub fn record_sampler(r: &mut CaseResult, reps: &Reps, time_name: &str, unit_ns: bool, gate: Gate) {
    r.reps(reps);
    if unit_ns {
        r.metric(time_name, reps.ns_per_iter(gate));
    } else {
        r.metric(time_name, reps.ms_per_iter(gate));
    }
    r.metric("cpu_ms_op", reps.cpu_ms_per_iter(gate));
    r.metric("cpu_process_ms_op", reps.cpu_process_ms_per_iter(gate));
    r.metric("allocs_op", reps.allocs_per_iter(Gate::Micro));
    r.metric("alloc_bytes_op", reps.bytes_per_iter(Gate::Micro));
}

/// A [`Sampler`] for the context.
pub fn sampler(ctx: &RunCtx) -> Sampler {
    Sampler::new(ctx)
}

// ---------------------------------------------------------------------------
// Rep loop for cases that time parts of each rep themselves (codecs, GPU)

/// Warm-up reps (discarded) then measured reps, each observed by the noise guard;
/// a noisy rep is repeated up to [`MAX_NOISY_RETRIES`] times per case.
pub struct RepLoop {
    pub warmup: u32,
    pub reps: u32,
    guard: NoiseGuard,
    pub retries: u32,
    pub noisy: bool,
    /// Cores used by other processes during each kept rep.
    other: Vec<f64>,
}

impl RepLoop {
    pub fn new(ctx: &RunCtx) -> Self {
        Self {
            warmup: ctx.warmup,
            reps: ctx.reps.max(1),
            guard: NoiseGuard::new(),
            retries: 0,
            noisy: false,
            other: Vec::new(),
        }
    }

    /// `f(attempt, measured)`; returns the measured reps' values.
    pub fn run<T>(&mut self, mut f: impl FnMut(u32, bool) -> Result<T>) -> Result<Vec<T>> {
        let mut attempt = 0;
        for _ in 0..self.warmup {
            f(attempt, false)?;
            attempt += 1;
        }
        let mut out = Vec::new();
        while out.len() < self.reps as usize {
            self.guard.begin();
            let v = f(attempt, true)?;
            attempt += 1;
            let other = self.guard.end();
            let noisy = self.guard.is_noisy(other);
            if noisy && self.retries < MAX_NOISY_RETRIES {
                self.retries += 1;
                continue;
            }
            self.noisy |= noisy;
            self.other.extend(other);
            out.push(v);
        }
        Ok(out)
    }

    /// Records `iterations` (work per rep), warm-up, reps and noise as
    /// [`CaseResult::reps`] does.
    pub fn record(&self, r: &mut CaseResult, iterations: u64) {
        r.param("iterations", iterations);
        r.param("warmup_reps", self.warmup);
        r.param("reps", self.reps);
        if self.retries > 0 {
            r.param("noise_retries", self.retries);
        }
        if !self.other.is_empty() {
            r.param("other_cores_median", (crate::measure::median(&self.other) * 1000.0).round() / 1000.0);
        }
        if self.noisy && r.status == Status::Ok {
            r.status = Status::Noisy;
        }
    }
}

// ---------------------------------------------------------------------------
// Quality of decoded frames (plan 6.4), computed in parallel outside any timing

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobKind {
    /// The decoded frame of this source frame (also its displayed quality).
    CodedShown,
    /// The decoded frame of this source frame, shown later (async encoder).
    Coded,
    /// A source frame whose own picture was not shown yet: compared with the last
    /// decoded one.
    Stale,
}

struct Job {
    index: u32,
    kind: JobKind,
    src: Arc<Frame>,
    dec: Arc<YuvFrame>,
}

struct JobOut {
    index: u32,
    kind: JobKind,
    psnr: Psnr,
    ssim: f64,
    /// PSNR-YUV against the production `bgra_to_i420` of the source (diagnostic).
    vs_prod: f64,
    size: (u32, u32),
}

/// Collects (source, decoded) pairs and scores them: coded quality (each decoded
/// frame against its source) and displayed quality (each source frame against the
/// last decoded one at that moment).
pub struct QualityAcc {
    jobs: Vec<Job>,
    out: Vec<JobOut>,
    chunk: usize,
    /// Source frames before the first decoded picture.
    pub unshown: u32,
}

impl Default for QualityAcc {
    fn default() -> Self {
        Self::new()
    }
}

pub fn quality_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 8)
}

impl QualityAcc {
    pub fn new() -> Self {
        Self { jobs: Vec::new(), out: Vec::new(), chunk: quality_threads() * 2, unshown: 0 }
    }

    /// Decoded picture of source frame `index`; `shown_now` = it was available
    /// when that source frame was submitted (then it is also its displayed picture).
    pub fn coded(&mut self, index: u32, src: Arc<Frame>, dec: Arc<YuvFrame>, shown_now: bool) {
        let kind = if shown_now { JobKind::CodedShown } else { JobKind::Coded };
        self.push(Job { index, kind, src, dec });
    }

    /// Source frame `index` had no picture of its own when submitted: `last` is
    /// what the viewer showed (None = nothing yet).
    pub fn stale(&mut self, index: u32, src: Arc<Frame>, last: Option<Arc<YuvFrame>>) {
        match last {
            Some(dec) => self.push(Job { index, kind: JobKind::Stale, src, dec }),
            None => self.unshown += 1,
        }
    }

    fn push(&mut self, job: Job) {
        self.jobs.push(job);
        if self.jobs.len() >= self.chunk {
            self.flush();
        }
    }

    fn flush(&mut self) {
        let jobs = std::mem::take(&mut self.jobs);
        if jobs.is_empty() {
            return;
        }
        let per = jobs.len().div_ceil(quality_threads());
        let results: Vec<JobOut> = std::thread::scope(|s| {
            let handles: Vec<_> = jobs
                .chunks(per)
                .map(|part| {
                    s.spawn(move || {
                        part.iter()
                            .map(|j| {
                                let img = j.src.image();
                                let reference = Reference::from_bgra(&img);
                                let psnr = quality::psnr(&j.dec, &reference);
                                let ssim = quality::ssim_y(&j.dec, &reference);
                                let vs_prod = if j.kind == JobKind::Stale {
                                    f64::NAN
                                } else {
                                    let (mut y, mut u, mut v) = (Vec::new(), Vec::new(), Vec::new());
                                    nuvira_codec::convert::bgra_to_i420(&img, &mut y, &mut u, &mut v);
                                    let (w, h) = nuvira_codec::convert::even_size(&img);
                                    let prod = YuvFrame {
                                        width: w as u32,
                                        height: h as u32,
                                        y,
                                        u,
                                        v,
                                        ..Default::default()
                                    };
                                    quality::psnr(&j.dec, &quality::Yuv420f::from_yuv(&prod)).yuv
                                };
                                JobOut {
                                    index: j.index,
                                    kind: j.kind,
                                    psnr,
                                    ssim,
                                    vs_prod,
                                    size: (j.dec.width, j.dec.height),
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles.into_iter().flat_map(|h| h.join().expect("quality thread")).collect()
        });
        self.out.extend(results);
    }

    /// Scores everything and records the metrics into `r`.
    pub fn finish(mut self, r: &mut CaseResult) -> QualitySummary {
        self.flush();
        self.out.sort_by_key(|o| o.index);
        let coded: Vec<&JobOut> = self.out.iter().filter(|o| o.kind != JobKind::Stale).collect();
        let shown: Vec<&JobOut> = self.out.iter().filter(|o| o.kind != JobKind::Coded).collect();
        let agg =
            |v: &[&JobOut], f: fn(&JobOut) -> f64| Aggregate::of(&v.iter().map(|o| f(o)).collect::<Vec<_>>());
        let yuv = agg(&coded, |o| o.psnr.yuv);
        let ssim = agg(&coded, |o| o.ssim);
        let q = |v: f64| if v.is_finite() { v } else { f64::NAN };
        r.metric("psnr_yuv_db", single("dB", Better::Higher, Gate::Quality, q(yuv.mean)));
        r.metric("psnr_y_db", single("dB", Better::Higher, Gate::Quality, q(agg(&coded, |o| o.psnr.y).mean)));
        r.metric("psnr_u_db", single("dB", Better::Higher, Gate::Info, q(agg(&coded, |o| o.psnr.u).mean)));
        r.metric("psnr_v_db", single("dB", Better::Higher, Gate::Info, q(agg(&coded, |o| o.psnr.v).mean)));
        r.metric("psnr_yuv_min_db", single("dB", Better::Higher, Gate::Info, q(yuv.min)));
        r.metric("psnr_yuv_p5_db", single("dB", Better::Higher, Gate::Info, q(yuv.p5)));
        r.metric("ssim_y", single("1", Better::Higher, Gate::Quality, q(ssim.mean)));
        r.metric("ssim_y_min", single("1", Better::Higher, Gate::Info, q(ssim.min)));
        r.metric(
            "psnr_yuv_vs_production_db",
            single("dB", Better::Higher, Gate::Info, q(agg(&coded, |o| o.vs_prod).mean)),
        );
        r.param("quality_frames", coded.len());
        if let Some(o) = coded.first() {
            r.param("ssim_step", quality::ssim_step_for(o.size.0, o.size.1));
        }
        let summary = QualitySummary { psnr_yuv: q(yuv.mean), ssim_y: q(ssim.mean) };
        // Always recorded (equal to the coded figures when every frame was shown on
        // time): a hardware encoder that returns some frames late on one side only
        // must not make the metric exist on one side only ("missing").
        {
            let dy = agg(&shown, |o| o.psnr.yuv);
            let ds = agg(&shown, |o| o.ssim);
            r.metric("displayed_psnr_yuv_db", single("dB", Better::Higher, Gate::Quality, q(dy.mean)));
            r.metric("displayed_ssim_y", single("1", Better::Higher, Gate::Quality, q(ds.mean)));
            r.param("displayed_frames", shown.len());
            r.param("unshown_frames", self.unshown);
        }
        summary
    }
}

/// Coded quality of a stream (for the ladder point).
#[derive(Clone, Copy, Debug)]
pub struct QualitySummary {
    pub psnr_yuv: f64,
    pub ssim_y: f64,
}

/// I420 frame of packed planes from an NV12 buffer (`pitch` bytes per row, the
/// UV plane at `uv_offset`).
pub fn nv12_to_yuv(data: &[u8], w: usize, h: usize, pitch: usize, uv_offset: usize) -> YuvFrame {
    let (cw, ch) = (w / 2, h / 2);
    let mut y = Vec::with_capacity(w * h);
    for row in 0..h {
        y.extend_from_slice(&data[row * pitch..][..w]);
    }
    let (mut u, mut v) = (Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch));
    for row in 0..ch {
        let r = &data[uv_offset + row * pitch..][..cw * 2];
        for px in r.as_chunks::<2>().0 {
            u.push(px[0]);
            v.push(px[1]);
        }
    }
    YuvFrame { width: w as u32, height: h as u32, y, u, v, ..Default::default() }
}

/// PSNR (dB) of two BGRA images over the B, G, R channels (common region).
pub fn psnr_bgra(a: &[u8], a_stride: usize, b: &[u8], b_stride: usize, w: usize, h: usize) -> f64 {
    let mut sum = 0f64;
    for y in 0..h {
        let ra = &a[y * a_stride..][..w * 4];
        let rb = &b[y * b_stride..][..w * 4];
        for (pa, pb) in ra.as_chunks::<4>().0.iter().zip(rb.as_chunks::<4>().0) {
            for c in 0..3 {
                let d = pa[c] as f64 - pb[c] as f64;
                sum += d * d;
            }
        }
    }
    quality::psnr_db(sum / (w * h * 3).max(1) as f64)
}

/// Mean absolute difference of two luma planes (common region).
pub fn luma_diff(a: &[u8], a_stride: usize, b: &[u8], b_stride: usize, w: usize, h: usize) -> f64 {
    let mut sum = 0u64;
    for y in 0..h {
        let ra = &a[y * a_stride..][..w];
        let rb = &b[y * b_stride..][..w];
        sum += ra.iter().zip(rb).map(|(p, q)| p.abs_diff(*q) as u64).sum::<u64>();
    }
    sum as f64 / (w * h).max(1) as f64
}
