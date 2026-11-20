//! Repetitions, statistics, CPU time and pacing (plan section 6.1).
//!
//! Micro cases: [`Sampler`] runs one warm-up rep (which also calibrates how many
//! iterations make a rep last at least [`MIN_REP`]), then `reps` measured reps with
//! the same iteration count, repeating a rep that the [`NoiseGuard`] finds noisy (at
//! most 3 times). Each [`Rep`] carries wall time, CPU time of the thread and of the
//! process, and the exact allocation counts of the measuring thread.
//!
//! Every metric of a result is a [`Metric`]: all values plus median, MAD, standard
//! deviation, min, max and p95 ([`Metric::from_values`]). Per-frame values of one rep
//! go to a [`Distribution`].

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize, Serializer};

use crate::alloc::{AllocCounts, ThreadAllocs};

/// Shortest measured rep of a micro case.
pub const MIN_REP: Duration = Duration::from_millis(200);

/// Retries of a noisy rep before it is kept and the case marked `noisy`.
pub const MAX_NOISY_RETRIES: u32 = 3;

/// Which direction is better for a metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Better {
    Lower,
    Higher,
}

/// Gate class of a metric (plan section 8.1 and 9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    Micro,
    MacroLatency,
    MacroCpu,
    MacroThroughput,
    Quality,
    Exact,
    Resource,
    SecurityBothWays,
    /// Never gates.
    Info,
}

/// Summary statistics of a set of values (non-finite values are ignored).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub n: usize,
    pub mean: f64,
    pub median: f64,
    /// Median absolute deviation (raw, not scaled by 1.4826).
    pub mad: f64,
    /// Sample standard deviation (n - 1); 0 with one value.
    pub stddev: f64,
    pub min: f64,
    pub max: f64,
    pub p95: f64,
}

impl Stats {
    /// None when there is no finite value.
    pub fn of(values: &[f64]) -> Option<Stats> {
        let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let mean = v.iter().sum::<f64>() / n as f64;
        let median = percentile_sorted(&v, 50.0);
        let mut dev: Vec<f64> = v.iter().map(|x| (x - median).abs()).collect();
        dev.sort_by(f64::total_cmp);
        let mad = percentile_sorted(&dev, 50.0);
        let stddev = if n > 1 {
            (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
        } else {
            0.0
        };
        Some(Stats {
            n,
            mean,
            median,
            mad,
            stddev,
            min: v[0],
            max: v[n - 1],
            p95: percentile_sorted(&v, 95.0),
        })
    }
}

/// Percentile `p` (0..=100) of sorted values, linear interpolation between closest
/// ranks (the "type 7" definition of R and NumPy's default). Empty input gives NaN.
pub fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    match sorted.len() {
        0 => f64::NAN,
        1 => sorted[0],
        n => {
            let rank = (p / 100.0).clamp(0.0, 1.0) * (n - 1) as f64;
            let lo = rank.floor() as usize;
            let hi = rank.ceil() as usize;
            sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
        }
    }
}

/// Percentile of unsorted values (non-finite ignored).
pub fn percentile(values: &[f64], p: f64) -> f64 {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_by(f64::total_cmp);
    percentile_sorted(&v, p)
}

/// Median of unsorted values (non-finite ignored).
pub fn median(values: &[f64]) -> f64 {
    percentile(values, 50.0)
}

/// Least-squares line through `(x, y)` points: `(slope, intercept)`; None with
/// fewer than two distinct x. Used for RSS growth (MB/h) in long sessions.
pub fn least_squares(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = points.len() as f64;
    if points.len() < 2 {
        return None;
    }
    let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let slope = sxy / sxx;
    Some((slope, my - slope * mx))
}

/// f64 written as JSON null when it is not finite (JSON has no inf/NaN).
fn ser_f64_vec<S: Serializer>(v: &[f64], s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = s.serialize_seq(Some(v.len()))?;
    for x in v {
        if x.is_finite() { seq.serialize_element(x)? } else { seq.serialize_element(&None::<f64>)? }
    }
    seq.end()
}

fn de_f64_vec<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
    let v: Vec<Option<f64>> = Deserialize::deserialize(d)?;
    Ok(v.into_iter().map(|x| x.unwrap_or(f64::NAN)).collect())
}

/// One metric of a result: unit, direction, gate, every value and its statistics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metric {
    pub unit: String,
    pub better: Better,
    pub gate: Gate,
    #[serde(serialize_with = "ser_f64_vec", deserialize_with = "de_f64_vec")]
    pub values: Vec<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mad: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stddev: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95: Option<f64>,
}

impl Metric {
    /// A metric with its statistics computed from `values` (one per rep or per run).
    pub fn from_values(unit: &str, better: Better, gate: Gate, values: Vec<f64>) -> Metric {
        let mut m = Metric {
            unit: unit.into(),
            better,
            gate,
            values,
            median: None,
            mad: None,
            stddev: None,
            min: None,
            max: None,
            p95: None,
        };
        m.recompute();
        m
    }

    /// One deterministic value (quality at fixed input, counts, checksums sizes).
    pub fn single(unit: &str, better: Better, gate: Gate, value: f64) -> Metric {
        Metric::from_values(unit, better, gate, vec![value])
    }

    pub fn push(&mut self, value: f64) {
        self.values.push(value);
        self.recompute();
    }

    /// Recomputes the statistics after `values` changed.
    pub fn recompute(&mut self) {
        let s = Stats::of(&self.values);
        self.median = s.map(|s| s.median);
        self.mad = s.map(|s| s.mad);
        self.stddev = s.map(|s| s.stddev);
        self.min = s.map(|s| s.min);
        self.max = s.map(|s| s.max);
        self.p95 = s.map(|s| s.p95);
    }

    pub fn stats(&self) -> Option<Stats> {
        Stats::of(&self.values)
    }
}

/// Distribution of per-frame (or per-event) values: percentiles of one rep, or the
/// median across reps of each percentile ([`Distribution::median_of`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
    pub n: usize,
    pub mean: f64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Distribution {
    pub fn of(values: &[f64]) -> Distribution {
        let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return Distribution::default();
        }
        v.sort_by(f64::total_cmp);
        Distribution {
            n: v.len(),
            mean: v.iter().sum::<f64>() / v.len() as f64,
            min: v[0],
            p50: percentile_sorted(&v, 50.0),
            p95: percentile_sorted(&v, 95.0),
            p99: percentile_sorted(&v, 99.0),
            max: v[v.len() - 1],
        }
    }

    /// Field-wise median across reps.
    pub fn median_of(reps: &[Distribution]) -> Distribution {
        let f = |g: fn(&Distribution) -> f64| median(&reps.iter().map(g).collect::<Vec<_>>());
        Distribution {
            n: reps.iter().map(|d| d.n).sum(),
            mean: f(|d| d.mean),
            min: f(|d| d.min),
            p50: f(|d| d.p50),
            p95: f(|d| d.p95),
            p99: f(|d| d.p99),
            max: f(|d| d.max),
        }
    }
}

// ---------------------------------------------------------------------------
// CPU time

/// User + kernel CPU time of the whole process.
///
/// Fine-grained on every OS: Windows `QueryProcessCycleTime` converted with the
/// invariant TSC rate (as Chromium's `ThreadTicks`), `GetProcessTimes` only when the
/// TSC is not invariant (its 15.6 ms clock tick would quantize short reps); Unix
/// `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` (Linux, macOS 10.12+).
pub fn cpu_time_process() -> Duration {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::GetCurrentProcess;
        use windows::Win32::System::WindowsProgramming::QueryProcessCycleTime;
        if let Some(hz) = wincpu::tsc_hz() {
            let mut cycles = 0u64;
            // SAFETY: valid out pointer; the pseudo handle needs no closing.
            if unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut cycles) }.is_ok() {
                return wincpu::cycles(cycles, hz);
            }
        }
        cpu_time_process_ticks()
    }
    #[cfg(unix)]
    {
        clock(libc::CLOCK_PROCESS_CPUTIME_ID).unwrap_or_else(|| {
            // SAFETY: getrusage fills the struct.
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } == 0 {
                return timeval(ru.ru_utime) + timeval(ru.ru_stime);
            }
            Duration::ZERO
        })
    }
}

/// `GetProcessTimes` (clock-tick resolution, about 15.6 ms).
#[cfg(windows)]
pub fn cpu_time_process_ticks() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: valid out pointers; the pseudo handle needs no closing.
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.is_ok() {
        return filetime(k) + filetime(u);
    }
    Duration::ZERO
}

/// User + kernel CPU time of the calling thread (fine-grained, see
/// [`cpu_time_process`]: `QueryThreadCycleTime` / `CLOCK_THREAD_CPUTIME_ID`).
pub fn cpu_time_thread() -> Duration {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::GetCurrentThread;
        use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;
        if let Some(hz) = wincpu::tsc_hz() {
            let mut cycles = 0u64;
            // SAFETY: valid out pointer; the pseudo handle needs no closing.
            if unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) }.is_ok() {
                return wincpu::cycles(cycles, hz);
            }
        }
        cpu_time_thread_ticks()
    }
    #[cfg(unix)]
    {
        clock(libc::CLOCK_THREAD_CPUTIME_ID).unwrap_or_default()
    }
}

/// `GetThreadTimes` (clock-tick resolution, about 15.6 ms).
#[cfg(windows)]
pub fn cpu_time_thread_ticks() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: valid out pointers; the pseudo handle needs no closing.
    if unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) }.is_ok() {
        return filetime(k) + filetime(u);
    }
    Duration::ZERO
}

#[cfg(unix)]
fn clock(id: libc::clockid_t) -> Option<Duration> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime fills ts.
    (unsafe { libc::clock_gettime(id, &mut ts) } == 0)
        .then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

/// Resolution of [`cpu_time_thread`] / [`cpu_time_process`] on this machine.
pub fn cpu_resolution() -> Duration {
    #[cfg(windows)]
    {
        match wincpu::tsc_hz() {
            Some(hz) => Duration::from_nanos((1e9 / hz).ceil().max(1.0) as u64),
            None => Duration::from_micros(15_625),
        }
    }
    #[cfg(unix)]
    {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_getres fills ts.
        if unsafe { libc::clock_getres(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32).max(Duration::from_nanos(1));
        }
        Duration::from_millis(1)
    }
}

/// The source of the CPU times ("tsc-cycles", "clock-ticks", "clock_gettime"),
/// recorded in results.
pub fn cpu_source() -> &'static str {
    #[cfg(windows)]
    {
        if wincpu::tsc_hz().is_some() { "tsc-cycles" } else { "clock-ticks" }
    }
    #[cfg(unix)]
    {
        "clock_gettime"
    }
}

/// A CPU-time metric is gated only when every rep measured at least this many
/// [`cpu_resolution`]s of CPU time; below that the clock's quantization is part
/// of the value and the metric is `info` ([`cpu_gate`]).
pub const CPU_MIN_RESOLUTIONS: u32 = 20;

/// `gate`, or `info` when a rep's CPU time is too short for the CPU clock.
pub fn cpu_gate(gate: Gate, rep_cpu: impl IntoIterator<Item = Duration>) -> Gate {
    let min = cpu_resolution() * CPU_MIN_RESOLUTIONS;
    if gate == Gate::Info || rep_cpu.into_iter().all(|c| c >= min) { gate } else { Gate::Info }
}

#[cfg(windows)]
mod wincpu {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    /// TSC ticks per second when the TSC is invariant (CPUID 0x80000007 EDX bit 8):
    /// `Query{Thread,Process}CycleTime` count at this rate. Measured once against
    /// `Instant` (QPC) over 50 ms. `NUVIRA_BENCH_CPU_CLOCK=ticks` forces the
    /// `Get*Times` fallback.
    pub fn tsc_hz() -> Option<f64> {
        static HZ: OnceLock<Option<f64>> = OnceLock::new();
        *HZ.get_or_init(|| {
            if std::env::var("NUVIRA_BENCH_CPU_CLOCK").as_deref() == Ok("ticks") {
                return None;
            }
            measure()
        })
    }

    #[cfg(target_arch = "x86_64")]
    fn measure() -> Option<f64> {
        use std::arch::x86_64::{__cpuid, _rdtsc};
        // CPUID is safe on x86_64; RDTSC below exists on every x86_64 CPU.
        let invariant = __cpuid(0x8000_0000).eax >= 0x8000_0007 && __cpuid(0x8000_0007).edx & (1 << 8) != 0;
        if !invariant {
            return None;
        }
        // SAFETY: as above.
        let (t0, c0) = (Instant::now(), unsafe { _rdtsc() });
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: as above.
        let dc = unsafe { _rdtsc() }.wrapping_sub(c0);
        let hz = dc as f64 / t0.elapsed().as_secs_f64();
        (hz.is_finite() && hz > 1e8).then_some(hz)
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn measure() -> Option<f64> {
        None
    }

    pub fn cycles(c: u64, hz: f64) -> Duration {
        Duration::from_secs_f64(c as f64 / hz)
    }

    /// Time all logical processors spent in their idle threads
    /// (`QueryIdleProcessorCycleTime`, TSC cycles; one processor group).
    pub fn idle_time() -> Option<Duration> {
        use windows::Win32::System::WindowsProgramming::QueryIdleProcessorCycleTime;
        let hz = tsc_hz()?;
        let mut len = 0u32;
        // SAFETY: a size query (no buffer), then a buffer of that size.
        let _ = unsafe { QueryIdleProcessorCycleTime(&mut len, None) };
        let mut buf = vec![0u64; (len as usize / 8).max(1)];
        let mut len = (buf.len() * 8) as u32;
        unsafe { QueryIdleProcessorCycleTime(&mut len, Some(buf.as_mut_ptr())) }.ok()?;
        Some(cycles(buf[..len as usize / 8].iter().sum(), hz))
    }
}

/// Idle time of every logical processor (Windows with an invariant TSC; precise,
/// unlike the 15.6 ms-sampled `% Idle Time` counter). None elsewhere.
pub fn idle_time() -> Option<Duration> {
    #[cfg(windows)]
    {
        wincpu::idle_time()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
pub(crate) fn filetime(t: windows::Win32::Foundation::FILETIME) -> Duration {
    let ticks = ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Duration::from_nanos(ticks.saturating_mul(100))
}

#[cfg(unix)]
fn timeval(t: libc::timeval) -> Duration {
    Duration::new(t.tv_sec as u64, (t.tv_usec as u32).saturating_mul(1000))
}

// ---------------------------------------------------------------------------
// Background noise

/// Detects reps during which other processes used the CPU (plan 6.1): whole-machine
/// CPU minus this process's, in cores. Threshold from `NUVIRA_BENCH_NOISE_CORES`
/// (default 0.10 of one core; `off` disables the guard).
///
/// macOS (Apple silicon): only the performance cores are watched, read directly from
/// the processors' tick counters ([`maccpu`]); see [`NoiseGuard::end`] for why.
pub struct NoiseGuard {
    sys: Option<sysinfo::System>,
    threshold: f64,
    /// Start of the observed interval.
    start: Option<GuardStart>,
}

struct GuardStart {
    at: Instant,
    /// This process's CPU time.
    own: Duration,
    /// All processors' idle time (Windows).
    idle: Option<Duration>,
    /// Busy and total ticks per processor (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    ticks: Option<Vec<(u64, u64)>>,
}

impl NoiseGuard {
    pub fn new() -> Self {
        let env = std::env::var("NUVIRA_BENCH_NOISE_CORES").ok();
        if env.as_deref() == Some("off") {
            return Self { sys: None, threshold: f64::INFINITY, start: None };
        }
        let threshold = env.and_then(|v| v.parse().ok()).unwrap_or(0.10);
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_usage();
        Self { sys: Some(sys), threshold, start: None }
    }

    /// Start of an observed interval.
    pub fn begin(&mut self) {
        if let Some(sys) = &mut self.sys {
            #[cfg(target_os = "macos")]
            let ticks = maccpu::ticks();
            #[cfg(not(target_os = "macos"))]
            let ticks = None;
            if ticks.is_none() {
                sys.refresh_cpu_usage();
            }
            self.start =
                Some(GuardStart { at: Instant::now(), own: cpu_time_process(), idle: idle_time(), ticks });
        }
    }

    /// End of the interval: cores used by other processes, if measured.
    pub fn end(&mut self) -> Option<f64> {
        let sys = self.sys.as_mut()?;
        let st = self.start.take()?;
        let wall = st.at.elapsed().as_secs_f64();
        // Shorter intervals than this give too coarse a reading (Windows counters
        // advance in 15.6 ms ticks, macOS's processor ticks every 10 ms).
        if wall < 0.05 {
            return None;
        }
        let own = (cpu_time_process() - st.own).as_secs_f64() / wall;
        // macOS: the processors' own tick counters, read at both ends of the rep.
        // sysinfo is not used there: it ignores a CPU refresh less than 200 ms after
        // the previous one (its MINIMUM_CPU_UPDATE_INTERVAL) and returns the old
        // value, so the reading of a rep could cover another interval (the previous
        // rep, or the gap between reps) while `own` covered this rep.
        // On Apple silicon only the performance cluster is counted: macOS keeps
        // background work (Spotlight, media analysis, the Neural Engine compiler:
        // several cores on a freshly installed Mac; 0.1-0.2 core of daemons at any
        // time) on the efficiency cores, where it does not compete with the bench's
        // default-priority threads, which run on the performance cores. Counted over
        // all cores it put nearly every micro rep above the threshold (Phase 1 Mac
        // baseline: medians 0.14-0.47 core on a machine left alone). A process that
        // needs more cores than the performance cluster has spills over to the
        // efficiency cores: then the whole machine counts again.
        #[cfg(target_os = "macos")]
        if let (Some(t0), Some(t1)) = (&st.ticks, maccpu::ticks())
            && let Some(other) = maccpu::other_cores(t0, &t1, own)
        {
            return Some(other);
        }
        sys.refresh_cpu_usage();
        let n = sys.cpus().len() as f64;
        let all = match (st.idle, idle_time()) {
            // Windows: busy = processors - idle, from the idle threads' cycle times
            // (precise). `% Idle Time` (sysinfo) is sampled at the 15.6 ms clock tick
            // per processor: on a 200 ms rep that is +-0.08 core per processor and
            // marked nearly every rep of the NUC as noisy.
            (Some(a), Some(b)) => n - b.saturating_sub(a).as_secs_f64() / wall,
            _ => sys.global_cpu_usage() as f64 / 100.0 * n,
        };
        Some((all - own).max(0.0))
    }

    pub fn is_noisy(&self, other_cores: Option<f64>) -> bool {
        other_cores.is_some_and(|c| c > self.threshold)
    }
}

/// macOS processor load from `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`: busy
/// (user + system + nice) and total ticks per logical processor, counted at the
/// statistics clock (100 Hz).
#[cfg(target_os = "macos")]
pub mod maccpu {
    use std::ops::Range;

    /// Busy and total ticks per logical processor, in logical processor order.
    pub fn ticks() -> Option<Vec<(u64, u64)>> {
        let mut n: libc::natural_t = 0;
        let mut info: libc::processor_info_array_t = std::ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        // SAFETY: out pointers; on success `info` is a kernel-allocated array of
        // `count` integers (CPU_STATE_MAX per processor), freed below.
        #[allow(deprecated)]
        let r = unsafe {
            libc::host_processor_info(
                libc::mach_host_self(),
                libc::PROCESSOR_CPU_LOAD_INFO,
                &mut n,
                &mut info,
                &mut count,
            )
        };
        if r != libc::KERN_SUCCESS || info.is_null() {
            return None;
        }
        let per = libc::CPU_STATE_MAX as usize;
        // SAFETY: `count` integers at `info`.
        let all = unsafe { std::slice::from_raw_parts(info, count as usize) };
        let out = (count as usize >= n as usize * per && n > 0).then(|| {
            all.chunks_exact(per)
                .take(n as usize)
                .map(|c| {
                    let t = |i: libc::c_int| c[i as usize] as u32 as u64;
                    let busy = t(libc::CPU_STATE_USER) + t(libc::CPU_STATE_SYSTEM) + t(libc::CPU_STATE_NICE);
                    (busy, busy + t(libc::CPU_STATE_IDLE))
                })
                .collect()
        });
        // SAFETY: the array was allocated by the kernel in this task.
        #[allow(deprecated)]
        unsafe {
            libc::vm_deallocate(
                libc::mach_task_self(),
                info as libc::vm_address_t,
                count as libc::vm_size_t * std::mem::size_of::<libc::integer_t>() as libc::vm_size_t,
            );
        }
        out
    }

    /// Logical processors of the performance cluster on Apple silicon: the last
    /// `hw.perflevel0.logicalcpu` of them (the efficiency cores are numbered first:
    /// `ioreg -p IODeviceTree -n cpus` gives cluster-type "E" for cpu0..3 on an M1).
    /// None on a Mac with one kind of core (Intel).
    pub fn performance_cores(total: usize) -> Option<Range<usize>> {
        let levels = sysctl_u32(c"hw.nperflevels")?;
        let p = sysctl_u32(c"hw.perflevel0.logicalcpu")? as usize;
        (levels >= 2 && p > 0 && p < total).then(|| total - p..total)
    }

    fn sysctl_u32(name: &std::ffi::CStr) -> Option<u32> {
        let mut v: u32 = 0;
        let mut len = std::mem::size_of::<u32>();
        // SAFETY: a u32 out value and its length.
        let r = unsafe {
            libc::sysctlbyname(name.as_ptr(), (&mut v as *mut u32).cast(), &mut len, std::ptr::null_mut(), 0)
        };
        (r == 0 && len == std::mem::size_of::<u32>()).then_some(v)
    }

    /// Cores used by other processes between two readings, given this process's
    /// own use (cores) over the same interval: on Apple silicon the performance
    /// cluster only, unless `own` does not fit in it (then all processors).
    pub fn other_cores(t0: &[(u64, u64)], t1: &[(u64, u64)], own: f64) -> Option<f64> {
        other_cores_in(t0, t1, own, performance_cores(t0.len()))
    }

    pub(crate) fn other_cores_in(
        t0: &[(u64, u64)],
        t1: &[(u64, u64)],
        own: f64,
        perf: Option<Range<usize>>,
    ) -> Option<f64> {
        if t0.len() != t1.len() || t0.is_empty() {
            return None;
        }
        let busy = |r: Range<usize>| -> f64 {
            r.map(|i| {
                let (b, t) = (t1[i].0.saturating_sub(t0[i].0), t1[i].1.saturating_sub(t0[i].1));
                if t == 0 { 0.0 } else { b as f64 / t as f64 }
            })
            .sum()
        };
        let range = match perf {
            // Half a core of margin: the process's threads are not pinned and some of
            // `own` may run on the efficiency cores.
            Some(p) if own <= p.len() as f64 - 0.5 => p,
            _ => 0..t0.len(),
        };
        Some((busy(range) - own).max(0.0))
    }
}

impl Default for NoiseGuard {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Reps

/// One measured rep of a micro case.
#[derive(Clone, Copy, Debug)]
pub struct Rep {
    pub wall: Duration,
    pub iterations: u64,
    pub cpu_thread: Duration,
    pub cpu_process: Duration,
    /// Exact allocations of the measuring thread during the rep.
    pub allocs: AllocCounts,
    /// Cores used by other processes during the rep (None: not measured).
    pub other_cores: Option<f64>,
    pub noisy: bool,
}

impl Rep {
    pub fn ns_per_iter(&self) -> f64 {
        self.wall.as_nanos() as f64 / self.iterations.max(1) as f64
    }
    pub fn ms_per_iter(&self) -> f64 {
        self.ns_per_iter() / 1e6
    }
    pub fn cpu_ms_per_iter(&self) -> f64 {
        self.cpu_thread.as_secs_f64() * 1e3 / self.iterations.max(1) as f64
    }
    pub fn allocs_per_iter(&self) -> f64 {
        self.allocs.allocs as f64 / self.iterations.max(1) as f64
    }
    pub fn bytes_per_iter(&self) -> f64 {
        self.allocs.bytes as f64 / self.iterations.max(1) as f64
    }
}

/// All reps of a case and whether noise retries were exhausted.
#[derive(Clone, Debug, Default)]
pub struct Reps {
    pub reps: Vec<Rep>,
    pub iterations: u64,
    pub warmup_reps: u32,
    /// Retries done because of noise.
    pub retries: u32,
    /// Some rep stayed noisy after [`MAX_NOISY_RETRIES`]: the case status is `noisy`.
    pub noisy: bool,
}

impl Reps {
    /// A metric from one value per rep.
    pub fn metric(&self, unit: &str, better: Better, gate: Gate, f: impl Fn(&Rep) -> f64) -> Metric {
        Metric::from_values(unit, better, gate, self.reps.iter().map(f).collect())
    }
    pub fn ns_per_iter(&self, gate: Gate) -> Metric {
        self.metric("ns", Better::Lower, gate, Rep::ns_per_iter)
    }
    pub fn ms_per_iter(&self, gate: Gate) -> Metric {
        self.metric("ms", Better::Lower, gate, Rep::ms_per_iter)
    }
    /// Thread CPU per iteration; `info` when a rep is too short for the CPU clock
    /// ([`cpu_gate`]).
    pub fn cpu_ms_per_iter(&self, gate: Gate) -> Metric {
        let gate = cpu_gate(gate, self.reps.iter().map(|r| r.cpu_thread));
        self.metric("ms", Better::Lower, gate, Rep::cpu_ms_per_iter)
    }
    /// Process CPU per iteration, gated as [`Reps::cpu_ms_per_iter`].
    pub fn cpu_process_ms_per_iter(&self, gate: Gate) -> Metric {
        let gate = cpu_gate(gate, self.reps.iter().map(|r| r.cpu_process));
        self.metric("ms", Better::Lower, gate, |x| {
            x.cpu_process.as_secs_f64() * 1e3 / x.iterations.max(1) as f64
        })
    }
    pub fn allocs_per_iter(&self, gate: Gate) -> Metric {
        self.metric("1", Better::Lower, gate, Rep::allocs_per_iter)
    }
    pub fn bytes_per_iter(&self, gate: Gate) -> Metric {
        self.metric("B", Better::Lower, gate, Rep::bytes_per_iter)
    }
}

/// Runs micro reps: calibration + warm-up, then `reps` reps with a fixed iteration
/// count (written to the result so A and B do the same work).
pub struct Sampler {
    pub warmup: u32,
    pub reps: u32,
    pub min_rep: Duration,
    /// Fixed iterations per rep; None = calibrated on the first rep.
    pub iterations: Option<u64>,
    pub guard: NoiseGuard,
}

impl Sampler {
    /// From the run context (reps, warm-up) with [`MIN_REP`].
    pub fn new(ctx: &crate::registry::RunCtx) -> Self {
        Self {
            warmup: ctx.warmup,
            reps: ctx.reps,
            min_rep: MIN_REP,
            iterations: None,
            guard: NoiseGuard::new(),
        }
    }

    /// Same, with a fixed number of iterations per rep (no calibration).
    pub fn with_iterations(mut self, n: u64) -> Self {
        self.iterations = Some(n.max(1));
        self
    }

    /// Iterations giving at least `min_rep`, found by doubling (the calls count as
    /// warm-up; their cost is not recorded).
    pub fn calibrate<F: FnMut()>(&mut self, f: &mut F) -> u64 {
        if let Some(n) = self.iterations {
            return n;
        }
        let target = self.min_rep.max(Duration::from_millis(1));
        let mut n = 1u64;
        loop {
            let t = Instant::now();
            for _ in 0..n {
                f();
            }
            let e = t.elapsed();
            if e >= target / 4 || n >= 1 << 40 {
                let per = e.as_secs_f64() / n as f64;
                let need = (target.as_secs_f64() * 1.05 / per.max(1e-12)).ceil() as u64;
                let it = need.max(1);
                self.iterations = Some(it);
                return it;
            }
            n *= 2;
        }
    }

    /// One rep of the calibrated (or fixed) iteration count.
    pub fn rep<F: FnMut()>(&mut self, mut f: F) -> Rep {
        let n = self.calibrate(&mut f);
        self.guard.begin();
        let a0 = ThreadAllocs::take();
        let p0 = cpu_time_process();
        let c0 = cpu_time_thread();
        let t0 = Instant::now();
        for _ in 0..n {
            f();
        }
        let wall = t0.elapsed();
        let cpu_thread = cpu_time_thread() - c0;
        let cpu_process = cpu_time_process() - p0;
        let allocs = ThreadAllocs::take().since(&a0);
        let other_cores = self.guard.end();
        let noisy = self.guard.is_noisy(other_cores);
        Rep { wall, iterations: n, cpu_thread, cpu_process, allocs, other_cores, noisy }
    }

    /// Warm-up reps (discarded) then the measured reps; a noisy rep is repeated up
    /// to [`MAX_NOISY_RETRIES`] times in total for the case.
    pub fn run<F: FnMut()>(&mut self, mut f: F) -> Reps {
        let fixed = self.iterations.is_some();
        self.calibrate(&mut f);
        let warmup = self.warmup.max(u32::from(!fixed));
        for k in 0..warmup {
            let r = self.rep(&mut f);
            // The first warm-up rep corrects a short calibration (the count is then fixed).
            if k == 0 && !fixed && r.wall < self.min_rep {
                let scale = self.min_rep.as_secs_f64() / r.wall.as_secs_f64().max(1e-9) * 1.05;
                self.iterations = Some((r.iterations as f64 * scale).ceil() as u64);
            }
        }
        let mut out = Reps { warmup_reps: warmup, ..Default::default() };
        while out.reps.len() < self.reps as usize {
            let r = self.rep(&mut f);
            if r.noisy && out.retries < MAX_NOISY_RETRIES {
                out.retries += 1;
                continue;
            }
            out.noisy |= r.noisy;
            out.reps.push(r);
        }
        out.iterations = self.iterations.unwrap_or(1);
        out
    }
}

// ---------------------------------------------------------------------------
// Pacing

/// Real-time pacing at a fixed rate with deadline-based spin-then-sleep (the 15.6 ms
/// default timer of Windows makes plain sleeps too coarse, risk R8).
pub struct Pacer {
    interval: Duration,
    next: Instant,
}

impl Pacer {
    pub fn new(fps: u32) -> Self {
        Self::with_interval(Duration::from_secs(1) / fps.max(1))
    }

    pub fn with_interval(interval: Duration) -> Self {
        Self { interval, next: Instant::now() }
    }

    /// Waits until the next deadline and returns it. A deadline already missed is not
    /// made up (the schedule restarts from now), as a capturer does.
    pub fn wait_next(&mut self) -> Instant {
        let deadline = self.next;
        sleep_until(deadline);
        self.next += self.interval;
        let now = Instant::now();
        if self.next < now {
            self.next = now + self.interval;
        }
        deadline
    }
}

/// Sleeps until `deadline`: coarse sleep until ~2 ms before, then spins.
pub fn sleep_until(deadline: Instant) {
    const SPIN: Duration = Duration::from_millis(2);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let left = deadline - now;
        if left > SPIN {
            std::thread::sleep(left - SPIN);
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_known_values() {
        let s = Stats::of(&[1.0, 2.0, 3.0, 4.0, 100.0]).unwrap();
        assert_eq!(s.n, 5);
        assert_eq!(s.median, 3.0);
        assert_eq!(s.mean, 22.0);
        // |x - 3| = 2 1 0 1 97 -> median 1.
        assert_eq!(s.mad, 1.0);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 100.0);
        // Type-7 p95: rank 0.95 * 4 = 3.8 -> 4 + 0.8 * 96.
        assert!((s.p95 - 80.8).abs() < 1e-9);
        // Sample stddev: sum of squares of deviations = 441+400+361+324+6084 = 7610.
        assert!((s.stddev - (7610.0f64 / 4.0).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn stats_even_count_and_nonfinite() {
        let s = Stats::of(&[4.0, 1.0, f64::NAN, 3.0, 2.0, f64::INFINITY]).unwrap();
        assert_eq!(s.n, 4);
        assert_eq!(s.median, 2.5);
        assert_eq!(s.stddev, (5.0f64 / 3.0).sqrt());
        assert!(Stats::of(&[]).is_none());
        assert!(Stats::of(&[f64::NAN]).is_none());
        let one = Stats::of(&[7.0]).unwrap();
        assert_eq!((one.median, one.mad, one.stddev, one.p95), (7.0, 0.0, 0.0, 7.0));
    }

    #[test]
    fn percentiles_type7() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert!((percentile(&v, 50.0) - 50.5).abs() < 1e-12);
        assert!((percentile(&v, 99.0) - 99.01).abs() < 1e-9);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 100.0), 100.0);
        let d = Distribution::of(&v);
        assert_eq!((d.n, d.min, d.max), (100, 1.0, 100.0));
        assert!((d.p95 - 95.05).abs() < 1e-9);
    }

    #[test]
    fn least_squares_line() {
        let pts: Vec<(f64, f64)> = (0..10).map(|i| (i as f64, 3.0 * i as f64 + 2.0)).collect();
        let (m, q) = least_squares(&pts).unwrap();
        assert!((m - 3.0).abs() < 1e-12 && (q - 2.0).abs() < 1e-12);
        assert!(least_squares(&[(1.0, 1.0), (1.0, 2.0)]).is_none());
    }

    #[test]
    fn metric_serialises_nonfinite_as_null() {
        let m = Metric::from_values("dB", Better::Higher, Gate::Quality, vec![40.0, f64::INFINITY]);
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["values"], serde_json::json!([40.0, null]));
        assert_eq!(j["median"], 40.0);
        assert_eq!(j["gate"], "quality");
        assert_eq!(j["better"], "higher");
        let back: Metric = serde_json::from_value(j).unwrap();
        assert!(back.values[1].is_nan());
    }

    #[test]
    fn sampler_calibrates_and_fixes_iterations() {
        let ctx = crate::registry::RunCtx::for_tests();
        let mut s = Sampler::new(&ctx);
        s.min_rep = Duration::from_millis(5);
        s.guard = NoiseGuard { sys: None, threshold: f64::INFINITY, start: None };
        let mut x = 0u64;
        let reps = s.run(|| {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
        });
        assert_eq!(reps.reps.len(), ctx.reps as usize);
        assert!(reps.iterations > 1);
        assert!(reps.reps.iter().all(|r| r.iterations == reps.iterations));
        // Half the target: the iteration count is fixed after calibration, so a rep
        // can run faster when the CPU clock rises or the machine gets quieter.
        assert!(reps.reps.iter().all(|r| r.wall >= Duration::from_micros(2500)), "{reps:?}");
        assert!(reps.reps.iter().all(|r| r.allocs.allocs == 0));
    }

    #[test]
    fn noise_guard_reads_other_cpu() {
        let mut g = NoiseGuard { threshold: 0.10, ..NoiseGuard::new() };
        if g.sys.is_none() {
            g = NoiseGuard { sys: Some(sysinfo::System::new()), threshold: 0.10, start: None };
        }
        g.begin();
        std::thread::sleep(Duration::from_millis(120));
        let other = g.end();
        assert!(other.is_some_and(|c| c >= 0.0 && c.is_finite()), "{other:?}");
    }

    /// macOS: background load on the efficiency cores is not "other" while the
    /// process fits in the performance cluster; it is once the process spills over.
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_guard_counts_the_performance_cluster() {
        use super::maccpu::other_cores_in;
        // 4 E (0..4) + 4 P (4..8), 20 ticks: E cores fully busy, P cores: one at
        // 100 % (our thread), one at 25 %.
        let t0 = vec![(0, 0); 8];
        let t1 = vec![(20, 20), (20, 20), (20, 20), (20, 20), (20, 20), (5, 20), (0, 20), (0, 20)];
        let p = Some(4..8);
        let other = other_cores_in(&t0, &t1, 1.0, p.clone()).unwrap();
        assert!((other - 0.25).abs() < 1e-9, "{other}");
        // A process using 4 cores: all processors (5.25 busy - 4 own).
        let other = other_cores_in(&t0, &t1, 4.0, p).unwrap();
        assert!((other - 1.25).abs() < 1e-9, "{other}");
        // One kind of core: all processors.
        let other = other_cores_in(&t0, &t1, 1.0, None).unwrap();
        assert!((other - 4.25).abs() < 1e-9, "{other}");
        assert!(other_cores_in(&t0, &t1[..4], 1.0, None).is_none());
        // The real counters move and the cluster is found on Apple silicon.
        let a = super::maccpu::ticks().expect("host_processor_info");
        assert_eq!(a.len(), std::thread::available_parallelism().unwrap().get());
        if cfg!(target_arch = "aarch64") {
            assert!(super::maccpu::performance_cores(a.len()).is_some());
        }
    }

    #[test]
    fn cpu_time_is_fine_grained() {
        // A 2 ms busy loop reads 1-4 ms of thread CPU: a 15.6 ms clock tick would
        // read 0 or 15.6 ms.
        let spin = |d: Duration| {
            let start = Instant::now();
            let mut x = 1u64;
            while start.elapsed() < d {
                x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
        };
        spin(Duration::from_millis(5));
        let mut ok = 0;
        for _ in 0..5 {
            let (t0, p0) = (cpu_time_thread(), cpu_time_process());
            spin(Duration::from_millis(2));
            let (t, p) = (cpu_time_thread() - t0, cpu_time_process() - p0);
            // Preemption only makes it smaller; other threads only make p larger.
            if t >= Duration::from_millis(1)
                && t <= Duration::from_millis(4)
                && p + Duration::from_micros(50) >= t
            {
                ok += 1;
            }
        }
        assert!(ok >= 3, "{} ({:?})", cpu_source(), cpu_resolution());
        assert!(cpu_resolution() < Duration::from_millis(1), "{}", cpu_source());
    }

    /// `cargo test probe_cpu_clocks -- --ignored --nocapture`: CPU clock vs wall
    /// time on one busy thread, and what the noise guard sees meanwhile.
    #[test]
    #[ignore]
    fn probe_cpu_clocks() {
        let mut g = NoiseGuard { threshold: 0.0, ..NoiseGuard::new() };
        g.begin();
        let (t0, p0, w0) = (cpu_time_thread(), cpu_time_process(), Instant::now());
        let mut x = 1u64;
        while w0.elapsed() < Duration::from_millis(1000) {
            x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        let wall = w0.elapsed().as_secs_f64();
        let own = (cpu_time_process() - p0).as_secs_f64();
        println!(
            "source {} res {:?}: thread {:.3} process {own:.3} of wall {wall:.3}; other cores {:?}",
            cpu_source(),
            cpu_resolution(),
            (cpu_time_thread() - t0).as_secs_f64(),
            g.end()
        );
        #[cfg(windows)]
        {
            let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
            let (i0, w0) = (idle_time(), Instant::now());
            let p0 = cpu_time_process();
            std::thread::sleep(Duration::from_millis(1000));
            let wall = w0.elapsed().as_secs_f64();
            let own = (cpu_time_process() - p0).as_secs_f64() / wall;
            if let (Some(a), Some(b)) = (i0, idle_time()) {
                let busy = n - (b - a).as_secs_f64() / wall;
                println!(
                    "idle-cycle method, sleeping: busy {busy:.3} cores, own {own:.3}, other {:.3}",
                    busy - own
                );
            }
        }
    }

    #[test]
    fn cpu_metrics_too_short_for_the_clock_are_info() {
        let r = cpu_resolution() * CPU_MIN_RESOLUTIONS;
        assert_eq!(cpu_gate(Gate::Micro, [r, r * 2]), Gate::Micro);
        assert_eq!(cpu_gate(Gate::Micro, [r, r / 2]), Gate::Info);
        assert_eq!(cpu_gate(Gate::Info, [r * 10]), Gate::Info);
    }

    #[test]
    fn cpu_time_advances() {
        let t0 = cpu_time_thread();
        let p0 = cpu_time_process();
        let start = Instant::now();
        let mut x = 1u64;
        while start.elapsed() < Duration::from_millis(60) {
            x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        assert!(cpu_time_thread() > t0);
        assert!(cpu_time_process() > p0);
    }

    #[test]
    fn pacer_keeps_the_rate() {
        let mut p = Pacer::new(200);
        let t0 = Instant::now();
        for _ in 0..20 {
            p.wait_next();
        }
        let e = t0.elapsed();
        // 19 intervals of 5 ms after the first (immediate) deadline.
        assert!(e >= Duration::from_millis(94), "{e:?}");
    }
}
