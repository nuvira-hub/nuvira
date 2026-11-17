//! Optional statistics log of a session (`NUVIRA_STATS`, docs/48-statistics-log.md).
//!
//! Off unless the environment asks for it: each side (agent, viewer) then keeps
//! atomic counters and fixed [`Histogram`]s, updated on its hot path without
//! allocating, and a [`Recorder`] thread at low priority writes one JSON line per
//! interval to `stats/<side>-<yyyymmdd-hhmmss>-<session>.jsonl`: a header, the
//! intervals, a summary. Files are rotated at [`ROTATE_BYTES`] and at most
//! [`KEEP_FILES`] are kept in the folder (oldest deleted).
//!
//! Nothing here sends anything: no protocol message, no setting, no UI. What is
//! written is numbers, the session id given by the server and a path type: no
//! addresses, file names, keystrokes or screen content.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{info, warn};

pub const SCHEMA: &str = "nuvira-stats/1";
/// `1` (or `on`, `true`, `yes`): files in `<data_dir>/stats`; another value: that folder;
/// unset, empty, `0`, `off`, `false`, `no`: off.
pub const ENV: &str = "NUVIRA_STATS";
/// Interval between two lines, in ms (default [`DEFAULT_INTERVAL`], at least [`MIN_INTERVAL`]).
pub const ENV_INTERVAL: &str = "NUVIRA_STATS_INTERVAL_MS";
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(1000);
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);
/// A file this large is closed and the session goes on in the next part.
pub const ROTATE_BYTES: u64 = 50 * 1024 * 1024;
/// Statistics files kept in the folder, all sessions and sides together.
pub const KEEP_FILES: usize = 20;

/// Where and how often statistics are written; `None` from [`Config::from_env`] when off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub dir: PathBuf,
    pub interval: Duration,
}

impl Config {
    /// Read once at the start of each session.
    pub fn from_env() -> Option<Config> {
        let stats = std::env::var_os(ENV);
        let interval = std::env::var(ENV_INTERVAL).ok();
        Self::parse(stats.as_deref(), interval.as_deref(), crate::identity::data_dir)
    }

    /// [`Config::from_env`] on given values (`data_dir` only called for `1`).
    pub fn parse(
        stats: Option<&OsStr>,
        interval_ms: Option<&str>,
        data_dir: impl FnOnce() -> PathBuf,
    ) -> Option<Config> {
        let stats = stats?;
        let text = stats.to_string_lossy();
        let dir = match text.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" | "false" | "no" => return None,
            "1" | "on" | "true" | "yes" => data_dir().join("stats"),
            _ => PathBuf::from(stats),
        };
        let interval = interval_ms
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(DEFAULT_INTERVAL, Duration::from_millis)
            .max(MIN_INTERVAL);
        Some(Config { dir, interval })
    }
}

/// Buckets of a [`Histogram`]: 0 is below 0.1 ms, 1..=62 are log-spaced from 0.1 ms to
/// 10 s (about 20% wide each), 63 is 10 s and more.
pub const BUCKETS: usize = 64;
const LOW_US: f64 = 100.0;
const HIGH_US: u64 = 10_000_000;
/// ln(10 s / 0.1 ms) / 62.
const LN_STEP: f64 = 0.185_692_346_209_197_24;

fn bucket_of(us: u64) -> usize {
    if us < LOW_US as u64 {
        0
    } else if us >= HIGH_US {
        BUCKETS - 1
    } else {
        (1 + ((us as f64 / LOW_US).ln() / LN_STEP) as usize).min(BUCKETS - 2)
    }
}

fn lower_us(bucket: usize) -> f64 {
    match bucket {
        0 => 0.0,
        b if b >= BUCKETS - 1 => HIGH_US as f64,
        b => LOW_US * ((b - 1) as f64 * LN_STEP).exp(),
    }
}

/// Durations of one interval, in fixed buckets: recording is two relaxed atomic
/// operations and never allocates.
pub struct Histogram {
    counts: [AtomicU32; BUCKETS],
    max_us: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self { counts: [const { AtomicU32::new(0) }; BUCKETS], max_us: AtomicU64::new(0) }
    }
}

impl Histogram {
    pub fn record_us(&self, us: u64) {
        self.counts[bucket_of(us)].fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    pub fn record(&self, d: Duration) {
        self.record_us(d.as_micros().min(u64::MAX as u128) as u64);
    }

    /// What was recorded since the previous call (the histogram starts again empty).
    pub fn take(&self) -> Snapshot {
        let mut s = Snapshot::default();
        for (n, c) in s.counts.iter_mut().zip(&self.counts) {
            *n = c.swap(0, Ordering::Relaxed) as u64;
        }
        s.max_us = self.max_us.swap(0, Ordering::Relaxed);
        s
    }
}

/// Counts taken from a [`Histogram`]; merged over a session for the summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub counts: [u64; BUCKETS],
    pub max_us: u64,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self { counts: [0; BUCKETS], max_us: 0 }
    }
}

impl Snapshot {
    pub fn count(&self) -> u64 {
        self.counts.iter().sum()
    }

    pub fn merge(&mut self, other: &Snapshot) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.max_us = self.max_us.max(other.max_us);
    }

    /// Quantile `q` (0..=1) in ms, interpolated inside its bucket and never above the
    /// maximum; `None` without samples.
    pub fn quantile_ms(&self, q: f64) -> Option<f64> {
        let n = self.count();
        if n == 0 {
            return None;
        }
        let rank = (q.clamp(0.0, 1.0) * n as f64).max(0.5);
        let mut below = 0u64;
        for (b, &c) in self.counts.iter().enumerate() {
            if c == 0 {
                continue;
            }
            if (below + c) as f64 >= rank {
                let lower = lower_us(b);
                let upper = if b >= BUCKETS - 1 { self.max_us as f64 } else { lower_us(b + 1) };
                let us = lower + (rank - below as f64) / c as f64 * (upper - lower).max(0.0);
                return Some(us.min(self.max_us as f64) / 1000.0);
            }
            below += c;
        }
        Some(self.max_us as f64 / 1000.0)
    }

    pub fn max_ms(&self) -> Option<f64> {
        (self.count() > 0).then(|| self.max_us as f64 / 1000.0)
    }
}

/// Which values of a histogram a line shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stat {
    P50,
    P95,
    P99,
    Max,
}

/// One JSON object being written into a line (keys are plain ASCII, values escaped).
pub struct Obj<'a> {
    buf: &'a mut String,
    first: bool,
}

impl<'a> Obj<'a> {
    fn open(buf: &'a mut String) -> Self {
        buf.push('{');
        Self { buf, first: true }
    }

    fn key(&mut self, key: &str) {
        if !std::mem::replace(&mut self.first, false) {
            self.buf.push(',');
        }
        self.buf.push('"');
        self.buf.push_str(key);
        self.buf.push_str("\":");
    }

    pub fn u64(&mut self, key: &str, v: u64) -> &mut Self {
        self.key(key);
        let _ = write!(self.buf, "{v}");
        self
    }

    /// Up to three decimals, trailing zeros removed; not finite: `null`.
    pub fn f64(&mut self, key: &str, v: f64) -> &mut Self {
        self.key(key);
        push_f64(self.buf, v);
        self
    }

    pub fn opt_u64(&mut self, key: &str, v: Option<u64>) -> &mut Self {
        match v {
            Some(v) => self.u64(key, v),
            None => self.null(key),
        }
    }

    pub fn opt_f64(&mut self, key: &str, v: Option<f64>) -> &mut Self {
        match v {
            Some(v) => self.f64(key, v),
            None => self.null(key),
        }
    }

    pub fn str(&mut self, key: &str, v: &str) -> &mut Self {
        self.key(key);
        push_str(self.buf, v);
        self
    }

    pub fn opt_str(&mut self, key: &str, v: Option<&str>) -> &mut Self {
        match v {
            Some(v) => self.str(key, v),
            None => self.null(key),
        }
    }

    pub fn bool(&mut self, key: &str, v: bool) -> &mut Self {
        self.key(key);
        self.buf.push_str(if v { "true" } else { "false" });
        self
    }

    pub fn null(&mut self, key: &str) -> &mut Self {
        self.key(key);
        self.buf.push_str("null");
        self
    }

    pub fn obj(&mut self, key: &str, fill: impl FnOnce(&mut Obj<'_>)) -> &mut Self {
        self.key(key);
        let mut inner = Obj::open(self.buf);
        fill(&mut inner);
        inner.buf.push('}');
        self
    }

    /// An object (`null` without samples), or `null` when `v` is `None`.
    pub fn opt_obj<T>(&mut self, key: &str, v: Option<T>, fill: impl FnOnce(&mut Obj<'_>, T)) -> &mut Self {
        match v {
            Some(v) => self.obj(key, |o| fill(o, v)),
            None => self.null(key),
        }
    }

    /// `{"p50":..,"p95":..}` in ms, or `null` when there were no samples.
    pub fn hist(&mut self, key: &str, s: &Snapshot, stats: &[Stat]) -> &mut Self {
        if s.count() == 0 {
            return self.null(key);
        }
        self.obj(key, |o| {
            for stat in stats {
                match stat {
                    Stat::P50 => o.opt_f64("p50", s.quantile_ms(0.50)),
                    Stat::P95 => o.opt_f64("p95", s.quantile_ms(0.95)),
                    Stat::P99 => o.opt_f64("p99", s.quantile_ms(0.99)),
                    Stat::Max => o.opt_f64("max", s.max_ms()),
                };
            }
        })
    }
}

fn push_f64(buf: &mut String, v: f64) {
    if !v.is_finite() {
        buf.push_str("null");
        return;
    }
    let start = buf.len();
    let _ = write!(buf, "{v:.3}");
    if buf[start..].contains('.') {
        let trimmed = buf.trim_end_matches('0').trim_end_matches('.').len();
        buf.truncate(trimmed);
    }
    if &buf[start..] == "-0" {
        buf.truncate(start);
        buf.push('0');
    }
}

fn push_str(buf: &mut String, v: &str) {
    buf.push('"');
    for c in v.chars() {
        match c {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(buf, "\\u{:04x}", c as u32);
            }
            c => buf.push(c),
        }
    }
    buf.push('"');
}

/// What one side records: called by the [`Recorder`] thread only, never on the hot path.
pub trait Source: Send + 'static {
    /// Side-specific header fields (after `session` and `path`).
    fn header(&mut self, o: &mut Obj<'_>);
    /// The counters of the interval that lasted `dt` (since the previous call).
    fn interval(&mut self, o: &mut Obj<'_>, dt: Duration);
    /// The whole session, after the last interval.
    fn summary(&mut self, o: &mut Obj<'_>, seconds: f64);
}

/// The thread that writes the file of one session; stops (last interval, summary)
/// when finished or dropped.
pub struct Recorder {
    tx: Option<mpsc::Sender<Option<&'static str>>>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts writing for `side` ("agent", "viewer"). `session` is the id given by the
    /// server (letters, digits, `-` and `_` are kept), `path` how the sides are
    /// connected (`direct`, `relay`, `websocket`, `nested`).
    pub fn start(
        config: &Config,
        side: &'static str,
        session: &str,
        path: &str,
        source: Box<dyn Source>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let (config, session, path) = (config.clone(), sanitize(session), path.to_string());
        let thread = std::thread::Builder::new()
            .name("stats".into())
            .spawn(move || Writer::run(config, side, session, path, source, rx));
        match thread {
            Ok(t) => Self { tx: Some(tx), thread: Some(t) },
            Err(e) => {
                warn!("statistics not written: {e}");
                Self { tx: None, thread: None }
            }
        }
    }

    /// Ends the file with the reason the session ended (`None`: normally).
    pub fn finish(mut self, exit_reason: Option<&'static str>) {
        self.stop(exit_reason);
    }

    fn stop(&mut self, exit_reason: Option<&'static str>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(exit_reason);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop(None);
    }
}

/// Why a session ended, as a short word without any detail of the error (which could
/// hold an address): `None` when it ended without error.
pub fn exit_reason<T>(result: &anyhow::Result<T>) -> Option<&'static str> {
    let e = result.as_ref().err()?;
    Some(e.chain().find_map(quic_reason).unwrap_or("error"))
}

/// The QUIC cause of an error, also inside an `io::Error` (the framed streams).
fn quic_reason(cause: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    use quinn::{ConnectionError as C, ReadError as R, ReadExactError as RE, WriteError as W};
    let conn = |c: &C| match c {
        C::TimedOut => "timed_out",
        C::ApplicationClosed(_) => "closed_by_peer",
        C::ConnectionClosed(_) => "connection_closed",
        C::LocallyClosed => "closed_locally",
        C::Reset => "reset",
        C::VersionMismatch | C::TransportError(_) => "transport_error",
        C::CidsExhausted => "cids_exhausted",
    };
    let read = |r: &R| match r {
        R::ConnectionLost(c) => conn(c),
        R::Reset(_) => "stream_reset_by_peer",
        _ => "stream_error",
    };
    let write = |w: &W| match w {
        W::ConnectionLost(c) => conn(c),
        W::Stopped(_) => "stream_stopped_by_peer",
        _ => "stream_error",
    };
    if let Some(c) = cause.downcast_ref::<C>() {
        return Some(conn(c));
    }
    if let Some(r) = cause.downcast_ref::<R>() {
        return Some(read(r));
    }
    if let Some(w) = cause.downcast_ref::<W>() {
        return Some(write(w));
    }
    if let Some(RE::ReadError(r)) = cause.downcast_ref::<RE>() {
        return Some(read(r));
    }
    cause.downcast_ref::<std::io::Error>().and_then(|io| io.get_ref()).and_then(|inner| quic_reason(inner))
}

fn sanitize(session: &str) -> String {
    let s: String =
        session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(64).collect();
    if s.is_empty() { "local".into() } else { s }
}

/// A local id for sessions that have none from a server (LAN, switch sides, tests).
pub fn local_session_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("local{:x}{:04x}{n}", std::process::id(), nuvira_proto::now_us() & 0xffff)
}

/// `yyyymmdd-hhmmss` (UTC) of `unix_s`.
fn timestamp(unix_s: u64) -> String {
    let (days, secs) = ((unix_s / 86_400) as i64, unix_s % 86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Deletes the oldest statistics files so that `keep` remain with the one about to be created.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            (name.starts_with("agent-") || name.starts_with("viewer-")) && name.ends_with(".jsonl")
        })
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), e.path()))
        })
        .collect();
    files.sort();
    let excess = (files.len() + 1).saturating_sub(keep);
    for (_, path) in files.into_iter().take(excess) {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::debug!("old statistics file not deleted: {e}");
        }
    }
}

/// The open file of a session, rotated by size.
struct Output {
    dir: PathBuf,
    stem: String,
    part: u32,
    file: std::io::BufWriter<std::fs::File>,
    bytes: u64,
    rotate_bytes: u64,
    keep: usize,
}

impl Output {
    fn open(dir: &Path, stem: String, rotate_bytes: u64, keep: usize) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = Self::create(dir, &stem, 1, keep)?;
        Ok(Self { dir: dir.to_path_buf(), stem, part: 1, file, bytes: 0, rotate_bytes, keep })
    }

    fn name(stem: &str, part: u32) -> String {
        match part {
            1 => format!("{stem}.jsonl"),
            n => format!("{stem}.{n}.jsonl"),
        }
    }

    fn create(
        dir: &Path,
        stem: &str,
        part: u32,
        keep: usize,
    ) -> std::io::Result<std::io::BufWriter<std::fs::File>> {
        prune(dir, keep);
        let path = dir.join(Self::name(stem, part));
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        info!("statistics to {}", path.display());
        Ok(std::io::BufWriter::new(file))
    }

    /// Writes one line; a full file is closed first and the next part starts with `header`.
    fn line(&mut self, line: &str, header: impl FnOnce(u32) -> String) -> std::io::Result<()> {
        if self.bytes > 0 && self.bytes + line.len() as u64 + 1 > self.rotate_bytes {
            self.file.flush()?;
            self.part += 1;
            self.file = Self::create(&self.dir, &self.stem, self.part, self.keep)?;
            self.bytes = 0;
            let h = header(self.part);
            self.write(&h)?;
        }
        self.write(line)
    }

    fn write(&mut self, line: &str) -> std::io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        // Readable while the session runs; one write per interval.
        self.file.flush()?;
        self.bytes += line.len() as u64 + 1;
        Ok(())
    }
}

struct Writer {
    config: Config,
    side: &'static str,
    session: String,
    path: String,
    source: Box<dyn Source>,
    /// The side's header fields, written once (also at the top of every part).
    header_fields: Option<String>,
    process: ProcessSampler,
    rotate_bytes: u64,
    keep: usize,
}

impl Writer {
    fn run(
        config: Config,
        side: &'static str,
        session: String,
        path: String,
        source: Box<dyn Source>,
        rx: mpsc::Receiver<Option<&'static str>>,
    ) {
        lower_priority();
        let mut w = Writer {
            config,
            side,
            session,
            path,
            source,
            header_fields: None,
            process: ProcessSampler::default(),
            rotate_bytes: ROTATE_BYTES,
            keep: KEEP_FILES,
        };
        w.go(&rx, nuvira_proto::now_us() / 1_000_000);
    }

    fn go(&mut self, rx: &mpsc::Receiver<Option<&'static str>>, unix_s: u64) {
        let stem = format!("{}-{}-{}", self.side, timestamp(unix_s), self.session);
        let mut out = match Output::open(&self.config.dir, stem, self.rotate_bytes, self.keep) {
            Ok(o) => o,
            Err(e) => {
                warn!("statistics not written to {}: {e}", self.config.dir.display());
                let _ = rx.recv();
                return;
            }
        };
        let started = Instant::now();
        let first = self.process.sample();
        let mut last_proc = first;
        let mut rss_max = first.rss_bytes;
        let mut last = started;
        let mut next = started + self.config.interval;
        let mut line = String::with_capacity(2048);
        let mut failed = false;
        let mut write = |w: &mut Writer, out: &mut Output, line: &str| {
            if failed {
                return;
            }
            let header = |part| w.header(part);
            if let Err(e) = out.line(line, header) {
                warn!("statistics: writing failed, stopped: {e}");
                failed = true;
            }
        };
        let reason = loop {
            match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(reason) => break reason,
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if self.header_fields.is_none() {
                let h = self.header(1);
                write(self, &mut out, &h);
            }
            let now = Instant::now();
            self.interval(&mut line, now, started, now - last, &mut last_proc, &mut rss_max);
            write(self, &mut out, &line);
            last = now;
            next += self.config.interval;
            if next < now {
                next = now + self.config.interval;
            }
        };
        if self.header_fields.is_none() {
            let h = self.header(1);
            write(self, &mut out, &h);
        }
        let now = Instant::now();
        // The part of an interval since the last line.
        if now - last >= Duration::from_millis(50) {
            self.interval(&mut line, now, started, now - last, &mut last_proc, &mut rss_max);
            write(self, &mut out, &line);
        }
        line.clear();
        let mut o = Obj::open(&mut line);
        o.str("type", "summary").f64("seconds", (now - started).as_secs_f64());
        self.source.summary(&mut o, (now - started).as_secs_f64());
        o.opt_f64("cpu_ms", last_proc.cpu_ms.zip(first.cpu_ms).map(|(a, b)| (a - b).max(0.0)))
            .opt_f64("rss_mb_max", rss_max.map(mb))
            .opt_str("exit_reason", reason);
        line.push('}');
        write(self, &mut out, &line);
    }

    fn header(&mut self, part: u32) -> String {
        let fields = self.header_fields.get_or_insert_with(|| {
            let mut s = String::new();
            let mut o = Obj::open(&mut s);
            self.source.header(&mut o);
            s.push('}');
            s
        });
        let mut line = String::with_capacity(512);
        let mut o = Obj::open(&mut line);
        o.str("type", "header")
            .str("schema", SCHEMA)
            .str("side", self.side)
            .str("app_version", env!("CARGO_PKG_VERSION"))
            .str("os", std::env::consts::OS)
            .str("session", &self.session)
            .str("path", &self.path);
        // The side's fields, without their braces.
        let inner = &fields[1..fields.len() - 1];
        if !inner.is_empty() {
            line.push(',');
            line.push_str(inner);
        }
        let mut o = Obj { buf: &mut line, first: false };
        o.u64("interval_ms", self.config.interval.as_millis() as u64).u64("part", part as u64);
        line.push('}');
        line
    }

    fn interval(
        &mut self,
        line: &mut String,
        now: Instant,
        started: Instant,
        dt: Duration,
        last_proc: &mut Process,
        rss_max: &mut Option<u64>,
    ) {
        line.clear();
        let mut o = Obj::open(line);
        o.str("type", "interval").u64("t_ms", (now - started).as_millis() as u64).str("side", self.side);
        self.source.interval(&mut o, dt);
        let p = self.process.sample();
        o.opt_f64("cpu_ms", p.cpu_ms.zip(last_proc.cpu_ms).map(|(a, b)| (a - b).max(0.0)))
            .opt_f64("rss_mb", p.rss_bytes.map(mb))
            .opt_u64("threads", p.threads.map(u64::from));
        line.push('}');
        if let Some(rss) = p.rss_bytes {
            *rss_max = Some(rss_max.unwrap_or(0).max(rss));
        }
        *last_proc = p;
    }
}

fn mb(bytes: u64) -> f64 {
    (bytes as f64 / 1_048_576.0 * 10.0).round() / 10.0
}

/// The writer thread gives way to the session's threads.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain calls on this thread (on Linux a thread id is a valid "process" here).
    unsafe {
        let tid = libc::gettid() as libc::id_t;
        let now = libc::getpriority(libc::PRIO_PROCESS, tid);
        libc::setpriority(libc::PRIO_PROCESS, tid, now.max(10));
    }
    #[cfg(windows)]
    // SAFETY: pseudo handle of this thread.
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }
}

/// CPU time, resident memory and threads of this process (`None` when the OS does not say).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Process {
    pub cpu_ms: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub threads: Option<u32>,
}

#[derive(Default)]
struct ProcessSampler {
    /// Linux: `/proc/self/stat`, read into the same buffer every time (no allocation
    /// once it has grown).
    #[cfg(target_os = "linux")]
    stat: String,
    /// Windows: buffer of the system's process list, kept between samples.
    #[cfg(windows)]
    buf: Vec<u64>,
    /// Windows: the last thread count and when it was read (see [`WINDOWS_THREADS_EVERY`]).
    #[cfg(windows)]
    threads: Option<(Instant, Option<u32>)>,
}

/// Reading the thread count costs several ms on Windows (the whole system's process
/// list: 7.6 ms on an i7-10710U with ~300 processes), so it is read this often and the
/// last value is repeated in between.
#[cfg(windows)]
const WINDOWS_THREADS_EVERY: Duration = Duration::from_secs(30);

impl ProcessSampler {
    #[cfg(unix)]
    fn sample(&mut self) -> Process {
        // SAFETY: zeroed rusage filled by the call.
        let cpu_ms = unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            (libc::getrusage(libc::RUSAGE_SELF, &mut ru) == 0).then(|| {
                let ms = |t: libc::timeval| t.tv_sec as f64 * 1e3 + t.tv_usec as f64 / 1e3;
                ms(ru.ru_utime) + ms(ru.ru_stime)
            })
        };
        #[cfg(target_os = "linux")]
        let (rss_bytes, threads) = rss_and_threads(&mut self.stat);
        #[cfg(not(target_os = "linux"))]
        let (rss_bytes, threads) = rss_and_threads();
        Process { cpu_ms, rss_bytes, threads }
    }

    #[cfg(windows)]
    fn sample(&mut self) -> Process {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let mut p = Process::default();
        // SAFETY: pseudo handle of this process and valid out pointers.
        unsafe {
            let h = GetCurrentProcess();
            let (mut c, mut e, mut k, mut u) = Default::default();
            if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok() {
                let ms = |t: FILETIME| {
                    (((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64) as f64 / 10_000.0
                };
                p.cpu_ms = Some(ms(k) + ms(u));
            }
            let mut mc = PROCESS_MEMORY_COUNTERS {
                cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            if GetProcessMemoryInfo(h, &mut mc, mc.cb).is_ok() {
                p.rss_bytes = Some(mc.WorkingSetSize as u64);
            }
        }
        p.threads = match self.threads {
            Some((at, n)) if at.elapsed() < WINDOWS_THREADS_EVERY => n,
            _ => {
                let n = self.read_threads();
                self.threads = Some((Instant::now(), n));
                n
            }
        };
        p
    }

    /// Thread count from `NtQuerySystemInformation(SystemProcessInformation)` (Windows has
    /// no call for one process), in a buffer kept between samples.
    #[cfg(windows)]
    fn read_threads(&mut self) -> Option<u32> {
        use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
        use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;
        const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
        let pid = std::process::id() as usize;
        if self.buf.is_empty() {
            self.buf = vec![0u64; (512 << 10) / 8];
        }
        for _ in 0..4 {
            let size = (self.buf.len() * 8) as u32;
            let mut needed = 0u32;
            // SAFETY: a buffer of `size` bytes, 8-aligned; the entries are walked by their
            // NextEntryOffset inside it.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    self.buf.as_mut_ptr().cast(),
                    size,
                    &mut needed,
                )
            };
            if status.0 == STATUS_INFO_LENGTH_MISMATCH {
                self.buf = vec![0u64; (needed.max(size) as usize + (128 << 10)) / 8];
                continue;
            }
            if status.0 < 0 {
                return None;
            }
            let base = self.buf.as_ptr() as *const u8;
            let mut off = 0usize;
            loop {
                // SAFETY: `off` stays inside the part of the buffer the call filled.
                let e = unsafe { &*(base.add(off) as *const SYSTEM_PROCESS_INFORMATION) };
                if e.UniqueProcessId.0 as usize == pid {
                    return Some(e.NumberOfThreads);
                }
                if e.NextEntryOffset == 0 {
                    return None;
                }
                off += e.NextEntryOffset as usize;
            }
        }
        None
    }

    #[cfg(not(any(unix, windows)))]
    fn sample(&mut self) -> Process {
        Process::default()
    }
}

/// Linux: `/proc/self/stat` fields 20 (threads) and 24 (resident pages).
#[cfg(target_os = "linux")]
fn rss_and_threads(stat: &mut String) -> (Option<u64>, Option<u32>) {
    use std::io::Read as _;
    stat.clear();
    let read = std::fs::File::open("/proc/self/stat").and_then(|mut f| f.read_to_string(stat));
    if read.is_err() {
        return (None, None);
    }
    // After the command name, which may hold spaces: field 3 onwards.
    let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { return (None, None) };
    let mut fields = rest.split_whitespace();
    let threads = fields.nth(17).and_then(|v| v.parse().ok());
    // SAFETY: plain call.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let rss = fields.nth(3).and_then(|v| v.parse::<u64>().ok()).map(|p| p * page);
    (rss, threads)
}

/// macOS: `proc_pidinfo(PROC_PIDTASKINFO)`.
#[cfg(target_os = "macos")]
fn rss_and_threads() -> (Option<u64>, Option<u32>) {
    // SAFETY: zeroed struct of the size given to the call.
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size);
        if n != size {
            return (None, None);
        }
        (Some(info.pti_resident_size), u32::try_from(info.pti_threadnum).ok())
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn rss_and_threads() -> (Option<u64>, Option<u32>) {
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        n: u64,
    }

    impl Source for Fake {
        fn header(&mut self, o: &mut Obj<'_>) {
            o.str("encoder", "test \"enc\"").null("plan").u64("width", 64);
        }

        fn interval(&mut self, o: &mut Obj<'_>, _dt: Duration) {
            self.n += 1;
            o.u64("n", self.n).null("fec").obj("quic", |q| {
                q.f64("rtt_ms", 1.25);
            });
        }

        fn summary(&mut self, o: &mut Obj<'_>, _seconds: f64) {
            o.u64("frames", self.n);
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nuvira-stats-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn lines(dir: &Path) -> Vec<(String, Vec<String>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        files
            .into_iter()
            .map(|p| {
                let text = std::fs::read_to_string(&p).unwrap();
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    text.lines().map(String::from).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn env_parsing() {
        let data = || PathBuf::from("/data");
        let parse = |s: Option<&str>, i: Option<&str>| Config::parse(s.map(OsStr::new), i, data);
        assert_eq!(parse(None, None), None);
        for off in ["", "0", "off", "False", " no "] {
            assert_eq!(parse(Some(off), None), None, "{off:?}");
        }
        let on = parse(Some("1"), None).unwrap();
        assert_eq!(on, Config { dir: PathBuf::from("/data").join("stats"), interval: DEFAULT_INTERVAL });
        assert_eq!(parse(Some("on"), None).unwrap().dir, PathBuf::from("/data").join("stats"));
        assert_eq!(parse(Some("/tmp/st"), None).unwrap().dir, PathBuf::from("/tmp/st"));
        assert_eq!(parse(Some("1"), Some("500")).unwrap().interval, Duration::from_millis(500));
        assert_eq!(parse(Some("1"), Some("10")).unwrap().interval, MIN_INTERVAL);
        assert_eq!(parse(Some("1"), Some("x")).unwrap().interval, DEFAULT_INTERVAL);
        // The data folder is looked up only when it is used.
        assert!(Config::parse(Some(OsStr::new("/x")), None, || panic!("not needed")).is_some());
    }

    #[test]
    fn buckets_cover_the_range() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(99), 0);
        assert_eq!(bucket_of(100), 1);
        assert_eq!(bucket_of(9_999_999), 62);
        assert_eq!(bucket_of(10_000_000), 63);
        assert_eq!(bucket_of(u64::MAX), 63);
        assert!((lower_us(62 + 1) - 10_000_000.0).abs() < 1e-3);
        for us in [150u64, 1_000, 31_000, 250_000, 4_000_000] {
            let b = bucket_of(us);
            assert!(lower_us(b) <= us as f64 && (us as f64) < lower_us(b + 1), "{us} in {b}");
        }
        // Monotonic.
        let mut prev = 0;
        for us in (0..12_000_000u64).step_by(997) {
            let b = bucket_of(us);
            assert!(b >= prev);
            prev = b;
        }
    }

    #[test]
    fn histogram_percentiles() {
        let h = Histogram::default();
        assert_eq!(h.take().quantile_ms(0.5), None);
        // 1..=1000 ms, one each.
        for ms in 1..=1000u64 {
            h.record_us(ms * 1000);
        }
        let s = h.take();
        assert_eq!(s.count(), 1000);
        let within = |q: f64, want: f64| {
            let got = s.quantile_ms(q).unwrap();
            assert!((got - want).abs() / want < 0.05, "q{q}: {got} vs {want}");
        };
        within(0.5, 500.0);
        within(0.95, 950.0);
        within(0.99, 990.0);
        assert_eq!(s.max_ms(), Some(1000.0));
        // Taken: empty again.
        assert_eq!(h.take().count(), 0);
        // One value: never above it.
        h.record(Duration::from_micros(31_400));
        let one = h.take();
        assert!(one.quantile_ms(0.5).unwrap() <= 31.4 && one.quantile_ms(0.5).unwrap() > 26.0);
        assert_eq!(one.quantile_ms(0.99), Some(31.4));
        // Beyond 10 s: the maximum bounds the last bucket.
        h.record_us(20_000_000);
        let far = h.take();
        assert!((10_000.0..=20_000.0).contains(&far.quantile_ms(0.5).unwrap()));
        assert_eq!(far.quantile_ms(1.0), Some(20_000.0));
        // Merge.
        let mut a = s;
        a.merge(&one);
        assert_eq!(a.count(), 1001);
        assert_eq!(a.max_us, 1_000_000);
    }

    #[test]
    fn json_values() {
        let mut s = String::new();
        let mut o = Obj::open(&mut s);
        o.f64("a", 2.0)
            .f64("b", 2.125)
            .f64("c", f64::NAN)
            .f64("d", -0.0001)
            .opt_u64("e", None)
            .str("f", "x\"\\\n\u{1}");
        let h = Histogram::default();
        o.hist("g", &h.take(), &[Stat::P50]);
        h.record_us(2_000);
        o.hist("h", &h.take(), &[Stat::P50, Stat::Max]).opt_obj("i", None::<u8>, |_, _| {});
        s.push('}');
        assert_eq!(
            s,
            r#"{"a":2,"b":2.125,"c":null,"d":0,"e":null,"f":"x\"\\\n\u0001","g":null,"h":{"p50":2,"max":2},"i":null}"#
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0), "19700101-000000");
        assert_eq!(timestamp(951_782_400), "20000229-000000");
        assert_eq!(timestamp(1_794_960_000 + 3_723), "20261118-010203");
    }

    #[test]
    fn session_ids() {
        assert_eq!(sanitize("ab-12_Z"), "ab-12_Z");
        assert_eq!(sanitize("../../x y"), "xy");
        assert_eq!(sanitize(""), "local");
        assert_eq!(sanitize(&"a".repeat(100)).len(), 64);
        assert_ne!(local_session_id(), local_session_id());
    }

    #[test]
    fn exit_reasons() {
        assert_eq!(exit_reason(&anyhow::Ok(())), None);
        assert_eq!(exit_reason::<()>(&Err(anyhow::anyhow!("at 192.168.1.2"))), Some("error"));
        let e: anyhow::Error = quinn::ConnectionError::TimedOut.into();
        assert_eq!(exit_reason::<()>(&Err(e.context("video"))), Some("timed_out"));
        let e: anyhow::Error = quinn::WriteError::ConnectionLost(quinn::ConnectionError::Reset).into();
        assert_eq!(exit_reason::<()>(&Err(e)), Some("reset"));
        // The viewer closing its window: the agent's video stream is stopped.
        let e = anyhow::Error::from(quinn::WriteError::Stopped(0u32.into())).context("sending video");
        assert_eq!(exit_reason::<()>(&Err(e)), Some("stream_stopped_by_peer"));
        // Inside an io::Error, as the framed streams give it.
        let io = std::io::Error::other(quinn::ReadError::Reset(0u32.into()));
        assert_eq!(exit_reason::<()>(&Err(anyhow::Error::from(io))), Some("stream_reset_by_peer"));
    }

    #[test]
    fn writes_header_intervals_and_summary() {
        let dir = temp_dir("write");
        let config = Config { dir: dir.clone(), interval: MIN_INTERVAL };
        let rec = Recorder::start(&config, "agent", "s/1", "direct", Box::new(Fake { n: 0 }));
        std::thread::sleep(Duration::from_millis(600));
        rec.finish(Some("timed_out"));
        let files = lines(&dir);
        assert_eq!(files.len(), 1);
        let (name, lines) = &files[0];
        assert!(name.starts_with("agent-") && name.ends_with("-s1.jsonl"), "{name}");
        assert!(lines.len() >= 4, "{lines:?}");
        let header = &lines[0];
        assert!(
            header
                .starts_with(r#"{"type":"header","schema":"nuvira-stats/1","side":"agent","app_version":""#)
        );
        assert!(header.contains(r#""session":"s1","path":"direct","encoder":"test \"enc\"","plan":null,"width":64,"interval_ms":250,"part":1}"#), "{header}");
        assert!(lines[1].starts_with(r#"{"type":"interval","t_ms":"#), "{}", lines[1]);
        assert!(
            lines[1].contains(r#""side":"agent","n":1,"fec":null,"quic":{"rtt_ms":1.25},"cpu_ms":"#),
            "{}",
            lines[1]
        );
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        assert!(lines[1].contains(r#""rss_mb":"#) && !lines[1].contains(r#""threads":null"#), "{}", lines[1]);
        let summary = lines.last().unwrap();
        assert!(summary.starts_with(r#"{"type":"summary","seconds":"#), "{summary}");
        assert!(summary.ends_with(r#","exit_reason":"timed_out"}"#), "{summary}");
        let n = lines.len() - 2;
        assert!(summary.contains(&format!(r#""frames":{n},"#)), "{summary}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropped_recorder_ends_the_file() {
        let dir = temp_dir("drop");
        let config = Config { dir: dir.clone(), interval: Duration::from_secs(60) };
        drop(Recorder::start(&config, "viewer", "x", "relay", Box::new(Fake { n: 0 })));
        let files = lines(&dir);
        let lines = &files[0].1;
        // Header, then the summary (the session lasted less than 50 ms: no interval).
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains(r#""side":"viewer""#));
        assert!(lines[1].ends_with(r#""exit_reason":null}"#), "{}", lines[1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_and_retention() {
        let dir = temp_dir("rotate");
        std::fs::create_dir_all(&dir).unwrap();
        // Older files of other sessions, and one that is not ours (kept).
        for i in 0..25 {
            std::fs::write(dir.join(format!("viewer-2026010{}-0000{i:02}-old.jsonl", i % 10)), "x\n")
                .unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "keep").unwrap();
        let (tx, rx) = mpsc::channel();
        let mut w = Writer {
            config: Config { dir: dir.clone(), interval: MIN_INTERVAL },
            side: "agent",
            session: "rot".into(),
            path: "direct".into(),
            source: Box::new(Fake { n: 0 }),
            header_fields: None,
            process: ProcessSampler::default(),
            // A few lines per part.
            rotate_bytes: 700,
            keep: 5,
        };
        let t = std::thread::spawn(move || {
            w.go(&rx, 1_794_960_000);
            w
        });
        std::thread::sleep(Duration::from_millis(250 * 9 + 100));
        tx.send(None).unwrap();
        t.join().unwrap();
        let files = lines(&dir);
        let ours: Vec<_> = files.iter().filter(|(n, _)| n.ends_with(".jsonl")).collect();
        assert_eq!(ours.len(), 5, "{:?}", files.iter().map(|f| &f.0).collect::<Vec<_>>());
        assert!(files.iter().any(|(n, _)| n == "notes.txt"));
        let part_of = |name: &str| -> Option<u32> {
            let part = name.strip_prefix("agent-20261118-000000-rot")?;
            Some(if part == ".jsonl" { 1 } else { part[1..part.len() - 6].parse().unwrap() })
        };
        let mut parts: Vec<_> = ours.iter().filter_map(|f| Some((part_of(&f.0)?, f))).collect();
        parts.sort_by_key(|p| p.0);
        assert!(parts.len() >= 3, "{:?}", ours.iter().map(|f| &f.0).collect::<Vec<_>>());
        for (n, (name, lines)) in &parts {
            // Every part starts with the header, with its number.
            assert!(lines[0].starts_with(r#"{"type":"header""#), "{name}");
            assert!(lines[0].ends_with(&format!(r#""part":{n}}}"#)), "{name}: {}", lines[0]);
            let size: usize = lines.iter().map(|l| l.len() + 1).sum();
            assert!(size <= 700 || lines.len() == 1, "{name}: {size} bytes");
        }
        let last = parts.last().unwrap().1;
        assert!(last.1.last().unwrap().starts_with(r#"{"type":"summary""#));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn process_sample() {
        let mut p = ProcessSampler::default();
        let a = p.sample();
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        {
            assert!(a.cpu_ms.is_some() && a.rss_bytes.unwrap() > 1 << 20 && a.threads.unwrap() >= 1, "{a:?}");
            let t = Instant::now();
            for _ in 0..10 {
                p.sample();
            }
            // Cheap enough to run once per interval.
            assert!(t.elapsed() < Duration::from_millis(200), "{:?}", t.elapsed());
        }
        let _ = a;
    }

    /// Cost of one process sample on this machine:
    /// `cargo test --release -p nuvira-transport sample_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn sample_cost() {
        let mut p = ProcessSampler::default();
        p.sample();
        let t = Instant::now();
        for _ in 0..100 {
            p.sample();
        }
        println!("one process sample: {:.1} us", t.elapsed().as_secs_f64() * 1e4);
        let h = Histogram::default();
        let t = Instant::now();
        for i in 0..1_000_000u64 {
            h.record_us(std::hint::black_box(i % 50_000));
        }
        println!("one histogram record: {:.1} ns", t.elapsed().as_secs_f64() * 1e3);
        std::hint::black_box(h.take());
    }
}
