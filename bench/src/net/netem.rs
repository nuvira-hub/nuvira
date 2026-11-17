//! Userspace UDP impairment proxy (plan 5.5, WP3): `nuvira-bench netem`.
//!
//! The proxy listens on `--listen` (127.0.0.1 only by default) and forwards every
//! client's datagrams to `--to` from a socket of its own per client, so both
//! directions of a QUIC connection go through it, each with its own simulated link
//! (as the relay's `impairer` does, `server/internal/relay/impair.go`).
//!
//! The specification has the syntax and the semantics of `NUVIRA_RELAY_IMPAIR`
//! (`ParseImpairment`, impair.go :33): `delay`, `jitter`, `loss`, `rate`, `queue`
//! (default 200ms), `stall`, `every` (default 3s when `stall` is given), in the
//! same order of decisions (loss, then the rate queue, then delay + jitter, stall,
//! no reordering). Proxy-only keys:
//! - `burst=B`: Gilbert-Elliott loss with mean burst length `B` packets and the
//!   same mean loss as `loss` (profile N5);
//! - `blackhole=D,at=T`: every packet of both directions is dropped for `D`,
//!   starting `T` after the proxy started (profiles NX-n); also on demand with the
//!   `blackhole <ms>` command (below);
//! - `seed=N`: seed of the loss/jitter generators (default: the bench seed), so A
//!   and B see the same pattern for the same packet sequence.
//!
//! Named profiles `N0`..`N5` and `NX-1`/`NX-3`/`NX-8`/`NX-12` ([`PROFILES`]) are
//! accepted wherever a spec is.
//!
//! Control on stdin, one command per line, answers on stdout:
//! `counts` -> one JSON line of [`Counts`]; `blackhole <ms>` -> `ok`; `quit` or
//! end of input (the parent died or closed the pipe) -> the final counts as a
//! JSON line, then exit. The first stdout line is `listening <addr>`.
//!
//! On Windows the proxy raises the timer resolution to 1 ms (`timeBeginPeriod`)
//! in its own process only (plan 5.5, risk R8): the measured processes keep
//! their own timer behaviour.
//!
//! [`Netem::spawn`] starts the proxy as a child process of this binary and returns
//! a [`NetemHandle`]; [`Proxy::start`] runs it in-process (tests).

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, UdpSocket};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::rng::Xoshiro256;

/// Named network profiles of plan 5.5 (`NX-n`: outage of n s at t = 10 s).
pub const PROFILES: &[(&str, &str)] = &[
    ("N0", ""),
    ("N1", "delay=3ms,jitter=2ms,loss=0.2%"),
    ("N2", "delay=20ms,jitter=5ms,loss=0.5%,rate=20mbit,queue=100ms"),
    ("N3", "delay=40ms,jitter=10ms,loss=1%,rate=3mbit,queue=150ms"),
    ("N4", "delay=50ms,jitter=20ms,loss=2%,rate=5mbit,queue=200ms,stall=300ms,every=3s"),
    ("N5", "delay=20ms,loss=5%,burst=4"),
    ("NX-1", "blackhole=1s,at=10s"),
    ("NX-3", "blackhole=3s,at=10s"),
    ("NX-8", "blackhole=8s,at=10s"),
    ("NX-12", "blackhole=12s,at=10s"),
];

/// The spec of a profile name, or the text itself when it is not a name.
pub fn profile_spec(name_or_spec: &str) -> &str {
    PROFILES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name_or_spec)).map_or(name_or_spec, |(_, s)| s)
}

/// A parsed impairment.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Spec {
    pub delay: Duration,
    pub jitter: Duration,
    /// 0..1
    pub loss: f64,
    /// bit/s, 0 = unlimited.
    pub rate_bps: f64,
    pub queue: Duration,
    pub stall: Duration,
    pub stall_every: Duration,
    /// Mean loss burst length in packets; <= 1 = independent losses.
    pub burst: f64,
    pub blackhole: Duration,
    pub blackhole_at: Duration,
    pub seed: u64,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            loss: 0.0,
            rate_bps: 0.0,
            queue: Duration::from_millis(200),
            stall: Duration::ZERO,
            stall_every: Duration::ZERO,
            burst: 0.0,
            blackhole: Duration::ZERO,
            blackhole_at: Duration::ZERO,
            seed: crate::rng::DEFAULT_SEED,
        }
    }
}

impl Spec {
    /// Parses a spec or a profile name; empty = no impairment.
    pub fn parse(text: &str) -> Result<Spec> {
        let text = profile_spec(text.trim());
        let mut s = Spec::default();
        if text.trim().is_empty() {
            return Ok(s);
        }
        for part in text.split(',') {
            let Some((key, value)) = part.trim().split_once('=') else {
                bail!("expected key=value: {part:?}");
            };
            let value = value.trim();
            let r: Result<()> = (|| {
                match key.trim() {
                    "delay" => s.delay = parse_duration(value)?,
                    "jitter" => s.jitter = parse_duration(value)?,
                    "queue" => s.queue = parse_duration(value)?,
                    "loss" => s.loss = value.trim_end_matches('%').parse::<f64>()? / 100.0,
                    "rate" => s.rate_bps = parse_rate(value)?,
                    "stall" => s.stall = parse_duration(value)?,
                    "every" => s.stall_every = parse_duration(value)?,
                    "burst" => s.burst = value.parse()?,
                    "blackhole" => s.blackhole = parse_duration(value)?,
                    "at" => s.blackhole_at = parse_duration(value)?,
                    "seed" => s.seed = value.parse()?,
                    _ => bail!("unknown key"),
                }
                Ok(())
            })();
            r.with_context(|| format!("{}: {value:?}", key.trim()))?;
        }
        if s.stall > Duration::ZERO && s.stall_every.is_zero() {
            s.stall_every = Duration::from_secs(3);
        }
        if !(0.0..=1.0).contains(&s.loss) {
            bail!("loss must be between 0% and 100%");
        }
        Ok(s)
    }

    pub fn is_none(&self) -> bool {
        *self == Spec { seed: self.seed, ..Spec::default() }
    }
}

/// Go's `time.ParseDuration` for the units used here: `ns`, `us`/`µs`, `ms`, `s`,
/// `m`, `h`, with decimals and sequences (`1m30s`).
pub fn parse_duration(text: &str) -> Result<Duration> {
    let t = text.trim();
    if t == "0" {
        return Ok(Duration::ZERO);
    }
    let mut total = 0f64;
    let mut rest = t;
    if rest.is_empty() {
        bail!("empty duration");
    }
    while !rest.is_empty() {
        let num_len = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        if num_len == 0 {
            bail!("invalid duration {t:?}");
        }
        let n: f64 = rest[..num_len].parse().with_context(|| format!("invalid duration {t:?}"))?;
        rest = &rest[num_len..];
        let unit_len = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            u => bail!("unknown unit {u:?} in duration {t:?}"),
        };
        total += n * scale;
        rest = &rest[unit_len..];
    }
    Ok(Duration::from_secs_f64(total))
}

/// `parseRate` of impair.go: `20mbit`, `500kbit`, `9600bit`, plain bit/s.
pub fn parse_rate(v: &str) -> Result<f64> {
    let (num, mult) = if let Some(n) = v.strip_suffix("mbit") {
        (n, 1e6)
    } else if let Some(n) = v.strip_suffix("kbit") {
        (n, 1e3)
    } else if let Some(n) = v.strip_suffix("bit") {
        (n, 1.0)
    } else {
        (v, 1.0)
    };
    Ok(num.parse::<f64>()? * mult)
}

// ---------------------------------------------------------------------------
// Counters

#[derive(Default)]
struct DirCounters {
    packets: AtomicU64,
    bytes: AtomicU64,
    sent: AtomicU64,
    dropped_loss: AtomicU64,
    dropped_queue: AtomicU64,
    dropped_blackhole: AtomicU64,
    dropped_overflow: AtomicU64,
    delayed: AtomicU64,
    /// Sum of the scheduled delay beyond arrival, microseconds.
    delay_us_total: AtomicU64,
    /// Sum of |actual send time - scheduled time|, microseconds (timer accuracy).
    late_us_total: AtomicU64,
}

/// Counts of one direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DirCounts {
    pub packets: u64,
    pub bytes: u64,
    pub sent: u64,
    pub dropped_loss: u64,
    pub dropped_queue: u64,
    pub dropped_blackhole: u64,
    /// Internal queue full (4096 packets, as impair.go).
    pub dropped_overflow: u64,
    /// Packets sent later than they arrived (> 0.5 ms).
    pub delayed: u64,
    /// Mean scheduled delay of the packets sent, ms.
    pub mean_delay_ms: f64,
    /// Mean error of the send time against the schedule, ms (timer accuracy).
    pub mean_timer_error_ms: f64,
}

impl DirCounters {
    fn snapshot(&self) -> DirCounts {
        let sent = self.sent.load(Relaxed);
        let per = |v: u64| if sent == 0 { 0.0 } else { v as f64 / sent as f64 / 1000.0 };
        DirCounts {
            packets: self.packets.load(Relaxed),
            bytes: self.bytes.load(Relaxed),
            sent,
            dropped_loss: self.dropped_loss.load(Relaxed),
            dropped_queue: self.dropped_queue.load(Relaxed),
            dropped_blackhole: self.dropped_blackhole.load(Relaxed),
            dropped_overflow: self.dropped_overflow.load(Relaxed),
            delayed: self.delayed.load(Relaxed),
            mean_delay_ms: per(self.delay_us_total.load(Relaxed)),
            mean_timer_error_ms: per(self.late_us_total.load(Relaxed)),
        }
    }
}

/// What the proxy saw: `up` = client -> target, `down` = target -> client.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    pub spec: String,
    pub clients: u64,
    pub up: DirCounts,
    pub down: DirCounts,
    /// Seconds since the proxy started.
    pub uptime_s: f64,
}

impl Counts {
    /// Every packet lost in the proxy, both directions.
    pub fn dropped(&self) -> u64 {
        [self.up, self.down]
            .iter()
            .map(|d| d.dropped_loss + d.dropped_queue + d.dropped_blackhole + d.dropped_overflow)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// One simulated link (one direction of one client)

struct LinkState {
    rng: Xoshiro256,
    /// Gilbert-Elliott: in the bad state.
    bad: bool,
    next_free: Instant,
    last_at: Instant,
    next_stall: Option<Instant>,
    stall_until: Instant,
}

struct Link {
    spec: Arc<Spec>,
    state: Mutex<LinkState>,
    queue: Mutex<VecDeque<(Instant, Instant, Vec<u8>)>>,
    ready: Condvar,
    counters: Arc<DirCounters>,
}

const MAX_QUEUED: usize = 4096;

impl Link {
    fn new(spec: Arc<Spec>, seed: u64, counters: Arc<DirCounters>) -> Link {
        let now = Instant::now();
        Link {
            spec,
            state: Mutex::new(LinkState {
                rng: Xoshiro256::new(seed),
                bad: false,
                next_free: now,
                last_at: now,
                next_stall: None,
                stall_until: now,
            }),
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            counters,
        }
    }

    /// A packet arrived: dropped, or queued for its delivery time.
    fn push(&self, data: &[u8], blackholed: bool) {
        let c = &self.counters;
        c.packets.fetch_add(1, Relaxed);
        c.bytes.fetch_add(data.len() as u64, Relaxed);
        if blackholed {
            c.dropped_blackhole.fetch_add(1, Relaxed);
            return;
        }
        let sp = &*self.spec;
        let now = Instant::now();
        let at = {
            let mut st = self.state.lock().expect("link");
            // Loss first (as impair.go): independent, or Gilbert-Elliott bursts with
            // the same mean loss: P(good->bad) = p / (B (1 - p)), P(bad->good) = 1 / B.
            let lost = if sp.loss <= 0.0 {
                false
            } else if sp.burst > 1.0 && sp.loss < 1.0 {
                let to_bad = sp.loss / (sp.burst * (1.0 - sp.loss));
                let to_good = 1.0 / sp.burst;
                let flip = st.rng.next_f64();
                st.bad = if st.bad { flip >= to_good } else { flip < to_bad };
                st.bad
            } else {
                st.rng.next_f64() < sp.loss
            };
            if lost {
                c.dropped_loss.fetch_add(1, Relaxed);
                return;
            }
            // Limited bandwidth: the packet leaves when the link has finished the
            // previous ones; beyond the queue it is dropped, as a router would.
            let start = st.next_free.max(now);
            if sp.rate_bps > 0.0 {
                if start.duration_since(now) > sp.queue {
                    c.dropped_queue.fetch_add(1, Relaxed);
                    return;
                }
                st.next_free = start + Duration::from_secs_f64(data.len() as f64 * 8.0 / sp.rate_bps);
            }
            let mut at = start + sp.delay;
            if !sp.jitter.is_zero() {
                let j = sp.jitter.as_nanos() as i64;
                let off = st.rng.range(0, 2 * j) - j;
                at = if off >= 0 {
                    at + Duration::from_nanos(off as u64)
                } else {
                    at.checked_sub(Duration::from_nanos((-off) as u64)).unwrap_or(at)
                };
            }
            if !sp.stall.is_zero() {
                let next = *st.next_stall.get_or_insert(now + sp.stall_every);
                if now >= next {
                    st.stall_until = now + sp.stall;
                    // Irregular interval: from half to one and a half times the average.
                    let every = sp.stall_every.as_nanos() as u64;
                    let r = st.rng.below(every.max(1));
                    st.next_stall = Some(now + sp.stall_every / 2 + Duration::from_nanos(r));
                }
                if at < st.stall_until + sp.delay {
                    at = st.stall_until + sp.delay;
                }
            }
            // The delay varies but the order stays the sending order.
            if at < st.last_at {
                at = st.last_at;
            }
            st.last_at = at;
            at
        };
        let mut q = self.queue.lock().expect("queue");
        if q.len() >= MAX_QUEUED {
            c.dropped_overflow.fetch_add(1, Relaxed);
            return;
        }
        q.push_back((now, at, data.to_vec()));
        drop(q);
        self.ready.notify_one();
    }

    /// Sends queued packets at their time until `stop`.
    fn run(&self, stop: &AtomicBool, send: impl Fn(&[u8])) {
        let c = &self.counters;
        let mut q = self.queue.lock().expect("queue");
        while !stop.load(Relaxed) {
            let Some(&(arrived, at, _)) = q.front() else {
                q = self.ready.wait_timeout(q, Duration::from_millis(100)).expect("queue").0;
                continue;
            };
            let now = Instant::now();
            if at > now {
                q = self.ready.wait_timeout(q, at - now).expect("queue").0;
                continue;
            }
            let (_, _, data) = q.pop_front().expect("front");
            drop(q);
            send(&data);
            let sent = Instant::now();
            c.sent.fetch_add(1, Relaxed);
            let delay = at.saturating_duration_since(arrived);
            if delay > Duration::from_micros(500) {
                c.delayed.fetch_add(1, Relaxed);
            }
            c.delay_us_total.fetch_add(delay.as_micros() as u64, Relaxed);
            c.late_us_total.fetch_add(sent.saturating_duration_since(at).as_micros() as u64, Relaxed);
            q = self.queue.lock().expect("queue");
        }
    }
}

// ---------------------------------------------------------------------------
// The proxy

struct Shared {
    spec: Arc<Spec>,
    spec_text: String,
    to: SocketAddr,
    stop: AtomicBool,
    started: Instant,
    /// On-demand outage end, microseconds after `started` (0 = none).
    blackhole_until_us: AtomicU64,
    up: Arc<DirCounters>,
    down: Arc<DirCounters>,
    clients: AtomicU64,
}

impl Shared {
    fn blackholed(&self) -> bool {
        let t = self.started.elapsed();
        let s = &self.spec;
        let scheduled = !s.blackhole.is_zero() && t >= s.blackhole_at && t < s.blackhole_at + s.blackhole;
        scheduled || (t.as_micros() as u64) < self.blackhole_until_us.load(Relaxed)
    }

    fn counts(&self) -> Counts {
        Counts {
            spec: self.spec_text.clone(),
            clients: self.clients.load(Relaxed),
            up: self.up.snapshot(),
            down: self.down.snapshot(),
            uptime_s: self.started.elapsed().as_secs_f64(),
        }
    }
}

/// A running proxy (threads in this process).
pub struct Proxy {
    shared: Arc<Shared>,
    pub addr: SocketAddr,
    threads: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

/// Read timeout of the sockets: how often the threads check for `stop`.
const POLL: Duration = Duration::from_millis(100);

impl Proxy {
    /// Binds `listen` and forwards to `to` with `spec` (a spec or a profile name).
    pub fn start(listen: SocketAddr, to: SocketAddr, spec: &str) -> Result<Proxy> {
        let parsed = Spec::parse(spec)?;
        let sock = UdpSocket::bind(listen).with_context(|| format!("netem: binding {listen}"))?;
        sock.set_read_timeout(Some(POLL))?;
        let addr = sock.local_addr()?;
        let shared = Arc::new(Shared {
            spec: Arc::new(parsed),
            spec_text: profile_spec(spec).to_string(),
            to,
            stop: AtomicBool::new(false),
            started: Instant::now(),
            blackhole_until_us: AtomicU64::new(0),
            up: Arc::default(),
            down: Arc::default(),
            clients: AtomicU64::new(0),
        });
        let threads = Arc::new(Mutex::new(Vec::new()));
        let sock = Arc::new(sock);
        let (sh, th) = (shared.clone(), threads.clone());
        let t = std::thread::Builder::new()
            .name("netem-listen".into())
            .spawn(move || listen_loop(sock, sh, th))?;
        threads.lock().expect("threads").push(t);
        Ok(Proxy { shared, addr, threads })
    }

    pub fn counts(&self) -> Counts {
        self.shared.counts()
    }

    /// Drops everything, both directions, for `d` from now.
    pub fn blackhole(&self, d: Duration) {
        let until = self.shared.started.elapsed() + d;
        self.shared.blackhole_until_us.store(until.as_micros() as u64, Relaxed);
    }

    /// Stops every thread and returns the final counts.
    pub fn stop(self) -> Counts {
        self.shared.stop.store(true, Relaxed);
        loop {
            let t = self.threads.lock().expect("threads").pop();
            match t {
                Some(t) => {
                    let _ = t.join();
                }
                None => break,
            }
        }
        self.shared.counts()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
    }
}

fn listen_loop(sock: Arc<UdpSocket>, sh: Arc<Shared>, threads: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>) {
    let mut clients: HashMap<SocketAddr, Arc<Link>> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    let mut n_links = 0u64;
    while !sh.stop.load(Relaxed) {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                continue;
            }
            // Windows reports an ICMP "port unreachable" of an earlier send as a
            // receive error (WSAECONNRESET): not fatal for the listener.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => {
                eprintln!("netem: receive error: {e}");
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        let up = match clients.get(&from) {
            Some(l) => l.clone(),
            None => match new_client(&sock, &sh, from, n_links, &threads) {
                Ok(l) => {
                    n_links += 2;
                    sh.clients.fetch_add(1, Relaxed);
                    clients.insert(from, l.clone());
                    l
                }
                Err(e) => {
                    eprintln!("netem: client {from}: {e:#}");
                    continue;
                }
            },
        };
        up.push(&buf[..n], sh.blackholed());
    }
}

/// A new client: its upstream socket (connected to the target), the two links and
/// their threads. Returns the client -> target link.
fn new_client(
    listen: &Arc<UdpSocket>,
    sh: &Arc<Shared>,
    client: SocketAddr,
    index: u64,
    threads: &Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
) -> Result<Arc<Link>> {
    let local: SocketAddr = if sh.to.is_ipv6() { "[::1]:0" } else { "127.0.0.1:0" }.parse()?;
    let upstream = Arc::new(UdpSocket::bind(local)?);
    upstream.connect(sh.to)?;
    upstream.set_read_timeout(Some(POLL))?;
    let seed = |k: u64| crate::rng::mix64(sh.spec.seed ^ (index + k).wrapping_mul(0x9e37_79b9));
    let up = Arc::new(Link::new(sh.spec.clone(), seed(0), sh.up.clone()));
    let down = Arc::new(Link::new(sh.spec.clone(), seed(1), sh.down.clone()));
    let mut t = threads.lock().expect("threads");
    {
        let (l, s, u) = (up.clone(), sh.clone(), upstream.clone());
        t.push(std::thread::Builder::new().name("netem-up".into()).spawn(move || {
            l.run(&s.stop, |d| {
                let _ = u.send(d);
            })
        })?);
    }
    {
        let (l, s, sock) = (down.clone(), sh.clone(), listen.clone());
        t.push(std::thread::Builder::new().name("netem-down".into()).spawn(move || {
            l.run(&s.stop, |d| {
                let _ = sock.send_to(d, client);
            })
        })?);
    }
    {
        let (l, s, u) = (down, sh.clone(), upstream);
        t.push(std::thread::Builder::new().name("netem-upstream".into()).spawn(move || {
            let mut buf = vec![0u8; 65536];
            while !s.stop.load(Relaxed) {
                match u.recv(&mut buf) {
                    Ok(n) => l.push(&buf[..n], s.blackholed()),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::ConnectionReset
                        ) => {}
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        })?);
    }
    Ok(up)
}

// ---------------------------------------------------------------------------
// Subcommand

/// Raises the Windows timer resolution for this process while alive.
struct TimerResolution;

impl TimerResolution {
    fn raise() -> TimerResolution {
        #[cfg(windows)]
        // SAFETY: plain call; paired with timeEndPeriod in Drop.
        unsafe {
            windows::Win32::Media::timeBeginPeriod(1);
        }
        TimerResolution
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: pairs the timeBeginPeriod(1) above.
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

/// `nuvira-bench netem --listen 127.0.0.1:P --to 127.0.0.1:Q (--profile N3 | --spec S)
///  [--seed N] [--duration SECS]`
pub fn cli(args: &[String]) -> Result<i32> {
    let mut listen: SocketAddr = "127.0.0.1:0".parse()?;
    let mut to: Option<SocketAddr> = None;
    let mut spec = String::new();
    let mut seed: Option<u64> = None;
    let mut duration: Option<f64> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--listen" => listen = val()?.parse().context("--listen")?,
            "--to" => to = Some(val()?.parse().context("--to")?),
            "--profile" | "--spec" => spec = val()?.clone(),
            "--seed" => seed = Some(val()?.parse().context("--seed")?),
            "--duration" => duration = Some(val()?.parse().context("--duration")?),
            "-h" | "--help" => {
                eprintln!(
                    "usage: nuvira-bench netem --listen 127.0.0.1:P --to 127.0.0.1:Q --profile N0..N5|NX-n|SPEC \
                     [--seed N] [--duration SECS]\nstdin: counts | blackhole <ms> | quit"
                );
                return Ok(0);
            }
            other => bail!("netem: unknown option {other}"),
        }
    }
    let to = to.context("netem: --to ADDR is required")?;
    let mut spec_text = profile_spec(&spec).to_string();
    if let Some(s) = seed {
        spec_text = if spec_text.is_empty() { format!("seed={s}") } else { format!("{spec_text},seed={s}") };
    }
    let _timer = TimerResolution::raise();
    let proxy = Proxy::start(listen, to, &spec_text)?;
    eprintln!("netem: {} -> {to}: {:?}", proxy.addr, Spec::parse(&spec_text)?);
    let mut out = std::io::stdout().lock();
    writeln!(out, "listening {}", proxy.addr)?;
    out.flush()?;
    drop(out);

    // Commands on stdin; a duration (or the end of stdin) stops the proxy.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new().name("netem-stdin".into()).spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send("quit".into());
    })?;
    let deadline = duration.map(|d| Instant::now() + Duration::from_secs_f64(d));
    loop {
        let cmd = match deadline {
            Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(c) => c,
                Err(_) => "quit".into(),
            },
            None => rx.recv().unwrap_or_else(|_| "quit".into()),
        };
        let mut words = cmd.split_whitespace();
        let mut out = std::io::stdout().lock();
        match words.next() {
            Some("counts") => writeln!(out, "{}", serde_json::to_string(&proxy.counts())?)?,
            Some("blackhole") => {
                let ms: u64 = words.next().unwrap_or("0").parse().context("blackhole <ms>")?;
                proxy.blackhole(Duration::from_millis(ms));
                writeln!(out, "ok")?;
            }
            Some("quit") => {
                drop(out);
                let counts = proxy.stop();
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{}", serde_json::to_string(&counts)?);
                let _ = out.flush();
                eprintln!(
                    "netem: up {} packets ({} dropped), down {} packets ({} dropped), timer error {:.3}/{:.3} ms",
                    counts.up.packets,
                    counts.up.packets - counts.up.sent,
                    counts.down.packets,
                    counts.down.packets - counts.down.sent,
                    counts.up.mean_timer_error_ms,
                    counts.down.mean_timer_error_ms
                );
                return Ok(0);
            }
            Some(other) => writeln!(out, "error unknown command {other}")?,
            None => continue,
        }
        out.flush()?;
    }
}

// ---------------------------------------------------------------------------
// Child-process handle

/// Starts the proxy as a separate process (plan 5.5).
pub struct Netem;

impl Netem {
    /// `nuvira-bench netem --listen <listen> --to <to> --profile <profile>` as a
    /// child of this binary; returns once it listens. `profile` is a name of
    /// [`PROFILES`] or a spec.
    pub fn spawn(listen: SocketAddr, to: SocketAddr, profile: &str) -> Result<NetemHandle> {
        Self::spawn_with(&std::env::current_exe()?, listen, to, profile)
    }

    /// Same with an explicit `nuvira-bench` binary.
    pub fn spawn_with(
        exe: &std::path::Path,
        listen: SocketAddr,
        to: SocketAddr,
        profile: &str,
    ) -> Result<NetemHandle> {
        let spec = profile_spec(profile).to_string();
        Spec::parse(&spec)?;
        let mut child = Command::new(exe)
            .args(["netem", "--listen", &listen.to_string(), "--to", &to.to_string(), "--spec", &spec])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting {} netem", exe.display()))?;
        let stdin = child.stdin.take().context("netem stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().context("netem stdout")?);
        let mut line = String::new();
        stdout.read_line(&mut line)?;
        let Some(addr) = line.trim().strip_prefix("listening ") else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("netem did not start: {:?}", line.trim());
        };
        let addr: SocketAddr = addr.parse()?;
        Ok(NetemHandle {
            io: Mutex::new(Some((child, stdin, stdout))),
            addr,
            spec,
            final_counts: Mutex::new(None),
        })
    }
}

type NetemIo = (Child, ChildStdin, BufReader<ChildStdout>);

/// The proxy process. Dropping it stops the process.
pub struct NetemHandle {
    io: Mutex<Option<NetemIo>>,
    /// Where clients connect.
    pub addr: SocketAddr,
    /// The spec in force (a profile name resolved).
    pub spec: String,
    final_counts: Mutex<Option<Counts>>,
}

impl NetemHandle {
    fn command(&self, cmd: &str) -> Result<String> {
        let mut g = self.io.lock().expect("netem");
        let (_, stdin, stdout) = g.as_mut().context("netem stopped")?;
        writeln!(stdin, "{cmd}")?;
        stdin.flush()?;
        let mut line = String::new();
        stdout.read_line(&mut line)?;
        Ok(line.trim().to_string())
    }

    /// Counts so far.
    pub fn counts(&self) -> Result<Counts> {
        if let Some(c) = self.final_counts.lock().expect("counts").clone() {
            return Ok(c);
        }
        Ok(serde_json::from_str(&self.command("counts")?)?)
    }

    /// Drops every packet, both directions, for `d` from now.
    pub fn blackhole(&self, d: Duration) -> Result<()> {
        let r = self.command(&format!("blackhole {}", d.as_millis()))?;
        if r != "ok" {
            bail!("netem: {r}");
        }
        Ok(())
    }

    /// Pid of the proxy process.
    pub fn pid(&self) -> Option<u32> {
        self.io.lock().expect("netem").as_ref().map(|(c, _, _)| c.id())
    }

    /// Stops the process and returns its final counts.
    pub fn stop(&self) -> Result<Counts> {
        let taken = self.io.lock().expect("netem").take();
        let Some((mut child, mut stdin, mut stdout)) = taken else {
            return self.final_counts.lock().expect("counts").clone().context("netem stopped");
        };
        let _ = writeln!(stdin, "quit");
        drop(stdin);
        let mut line = String::new();
        let read = stdout.read_line(&mut line);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = child.kill();
            let _ = child.wait();
        }
        read?;
        let counts: Counts = serde_json::from_str(line.trim()).context("netem final counts")?;
        *self.final_counts.lock().expect("counts") = Some(counts.clone());
        Ok(counts)
    }
}

impl Drop for NetemHandle {
    fn drop(&mut self) {
        if let Some((mut child, stdin, _)) = self.io.lock().expect("netem").take() {
            drop(stdin);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relay_syntax_and_profiles() {
        let s = Spec::parse("delay=40ms,jitter=10ms,loss=1%,rate=3mbit,queue=150ms").unwrap();
        assert_eq!(s.delay, Duration::from_millis(40));
        assert_eq!(s.jitter, Duration::from_millis(10));
        assert!((s.loss - 0.01).abs() < 1e-12);
        assert_eq!(s.rate_bps, 3e6);
        assert_eq!(s.queue, Duration::from_millis(150));
        assert_eq!(Spec::parse("N3").unwrap(), s);
        let n4 = Spec::parse("N4").unwrap();
        assert_eq!((n4.stall, n4.stall_every), (Duration::from_millis(300), Duration::from_secs(3)));
        let st = Spec::parse("delay=20ms,stall=300ms").unwrap();
        assert_eq!(st.stall_every, Duration::from_secs(3));
        assert_eq!(st.queue, Duration::from_millis(200));
        assert!(Spec::parse("").unwrap().is_none());
        assert!(Spec::parse("N0").unwrap().is_none());
        assert!(Spec::parse("bogus=1").is_err());
        assert!(Spec::parse("delay").is_err());
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1.5ms").unwrap(), Duration::from_micros(1500));
        assert_eq!(parse_rate("500kbit").unwrap(), 5e5);
        let nx = Spec::parse("NX-3").unwrap();
        assert_eq!((nx.blackhole, nx.blackhole_at), (Duration::from_secs(3), Duration::from_secs(10)));
    }

    /// Sends `n` datagrams through a proxy to an echo-less sink; returns the counts
    /// and what arrived (index, one-way delay).
    fn through(spec: &str, n: u32, gap: Duration) -> (Counts, Vec<(u32, Duration)>) {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        sink.set_read_timeout(Some(Duration::from_millis(400))).unwrap();
        let proxy = Proxy::start("127.0.0.1:0".parse().unwrap(), sink.local_addr().unwrap(), spec).unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let t0 = Instant::now();
        // The sink reads while the client sends (its socket buffer is small).
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut buf = [0u8; 2048];
            while let Ok(k) = sink.recv(&mut buf) {
                let i = u32::from_le_bytes(buf[..4].try_into().unwrap());
                assert_eq!(k, 1200);
                got.push((i, t0.elapsed()));
            }
            got
        });
        let mut sent_at = Vec::new();
        for i in 0..n {
            sent_at.push(t0.elapsed());
            let mut p = i.to_le_bytes().to_vec();
            p.resize(1200, 0);
            client.send_to(&p, proxy.addr).unwrap();
            // Zero gap: bursts of 16; MAX: one burst.
            if gap == Duration::MAX {
            } else if !gap.is_zero() {
                std::thread::sleep(gap);
            } else if i % 16 == 15 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let got = reader
            .join()
            .unwrap()
            .into_iter()
            .map(|(i, at)| (i, at.saturating_sub(sent_at[i as usize])))
            .collect();
        (proxy.stop(), got)
    }

    #[test]
    fn clean_link_forwards_everything_in_order() {
        let (c, got) = through("", 200, Duration::ZERO);
        // The OS may drop loopback datagrams before the proxy reads them when the
        // machine is loaded (small socket buffers): what the proxy got, it forwards.
        assert!(c.up.packets >= 20, "{:?}", c.up);
        assert_eq!(got.len() as u64, c.up.packets);
        assert!(got.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(c.dropped(), 0);
    }

    #[test]
    fn loss_and_delay_match_the_spec() {
        let (c, got) = through("delay=30ms,jitter=5ms,loss=10%,seed=7", 1000, Duration::ZERO);
        // Fractions of what reached the proxy (see clean_link_forwards_everything_in_order).
        assert!(c.up.packets >= 500, "{:?}", c.up);
        let lost = c.up.dropped_loss as f64 / c.up.packets as f64;
        assert!((0.06..0.14).contains(&lost), "loss {lost}");
        assert_eq!(got.len() as u64, c.up.packets - c.up.dropped_loss);
        // Order is kept even with jitter.
        assert!(got.windows(2).all(|w| w[0].0 < w[1].0));
        let min = got.iter().map(|g| g.1).min().unwrap();
        assert!(min >= Duration::from_millis(24), "min delay {min:?}");
        assert!(c.up.mean_delay_ms > 25.0 && c.up.mean_delay_ms < 40.0, "{:?}", c.up);
        // Same seed, same pattern.
        let (c2, _) = through("delay=30ms,jitter=5ms,loss=10%,seed=7", 1000, Duration::ZERO);
        if c.up.packets == 1000 && c2.up.packets == 1000 {
            assert_eq!(c.up.dropped_loss, c2.up.dropped_loss);
        }
    }

    #[test]
    fn rate_limit_drops_beyond_the_queue() {
        // 1 Mbit/s, 50 ms of queue: about 5 packets of 1200 bytes fit; a burst of
        // 100 loses most of them.
        let (c, got) = through("rate=1mbit,queue=50ms", 100, Duration::MAX);
        // (the proxy's own socket buffer may drop part of a burst before it.)
        assert!(c.up.dropped_queue * 4 > c.up.packets * 3, "{:?}", c.up);
        assert!(got.len() >= 4 && got.len() < 15, "{}", got.len());
    }

    #[test]
    fn bursts_have_the_mean_loss() {
        let (c, _) = through("loss=5%,burst=4,seed=3", 4000, Duration::ZERO);
        let lost = c.up.dropped_loss as f64 / 4000.0;
        assert!((0.02..0.09).contains(&lost), "loss {lost}");
    }

    #[test]
    fn blackhole_on_demand() {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        let proxy = Proxy::start("127.0.0.1:0".parse().unwrap(), sink.local_addr().unwrap(), "").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        proxy.blackhole(Duration::from_millis(200));
        for _ in 0..10 {
            client.send_to(b"x", proxy.addr).unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        client.send_to(b"y", proxy.addr).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let c = proxy.stop();
        assert_eq!(c.up.dropped_blackhole, 10);
        assert_eq!(c.up.sent, 1);
    }
}
