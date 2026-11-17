//! Network side of the bench (plan 7.2, section 11).
//!
//! - [`netem`] (WP3): userspace UDP impairment proxy (`nuvira-bench netem`).
//! - [`headless`] (WP3): the headless protocol client that plays the viewer.
//! - `server_mode` (WP4): local `nuvira-server` + server-mode connection.
//!
//! Shared helpers of this file (WP3, used by `macro_/` and `res/`):
//! - per-side CPU attribution of an in-process session: [`viewer_runtime`] builds
//!   the bench viewer's tokio runtime with its threads marked
//!   [`Side::Viewer`](crate::alloc::Side) for the allocator and their CPU time
//!   accumulated ([`viewer_thread_start`], [`viewer_checkpoint`]); [`CpuMark`]
//!   gives process / viewer / agent CPU and per-side allocations between two
//!   moments; [`exclude_cpu`] removes bench-only work (fixture generation);
//! - [`loopback_connect`]: a viewer QUIC connection with the production transport
//!   config bound to 127.0.0.1 (no firewall prompt), [`free_udp_port`];
//! - [`LoopbackAgent`]: an in-process agent on the synthetic source, either through
//!   `serve_reverse` on a 127.0.0.1 endpoint ([`AgentEntry::Reverse`], default) or
//!   `serve_direct` ([`AgentEntry::Direct`], binds 0.0.0.0);
//! - [`macro_durations`]: warm-up and measured time of a macro session per tier
//!   (`NUVIRA_BENCH_MACRO_SECS=warm,measure` overrides, e.g. `1,4` for a short check);
//! - `nuvira-bench agent-host` ([`cli_agent_host`]): the same agent in a process of
//!   its own (two-process variants).

pub mod headless;
pub mod netem;

#[cfg(bench_has_net_server_mode)]
pub mod server_mode;

use std::cell::Cell;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_agent::AgentConfig;
use nuvira_transport::{Connection, Fingerprint};

use crate::alloc::{AllocSnapshot, Side};
use crate::measure::{cpu_time_process, cpu_time_thread};
use crate::registry::Tier;

/// `nuvira-bench netem ...` (see [`netem`]).
pub fn cli_netem(args: &[String]) -> Result<i32> {
    netem::cli(args)
}

// ---------------------------------------------------------------------------
// CPU and allocation attribution per side

static VIEWER_CPU_NS: AtomicU64 = AtomicU64::new(0);
static EXCLUDED_CPU_NS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static LAST_CPU_NS: Cell<u64> = const { Cell::new(0) };
}

/// Marks the calling thread as the bench viewer's (allocator side and CPU account).
pub fn viewer_thread_start() {
    crate::alloc::set_side(Side::Viewer);
    LAST_CPU_NS.with(|c| c.set(cpu_time_thread().as_nanos() as u64));
}

/// Adds the CPU time the calling viewer thread used since its last checkpoint to
/// the viewer account. Called by the runtime's threads when they park and stop,
/// and by the bench's decoder thread after each frame.
pub fn viewer_checkpoint() {
    let now = cpu_time_thread().as_nanos() as u64;
    let last = LAST_CPU_NS.with(|c| c.replace(now));
    VIEWER_CPU_NS.fetch_add(now.saturating_sub(last), Relaxed);
}

/// CPU time spent on bench-only work (fixture generation) that must not count for
/// the agent side.
pub fn exclude_cpu(d: Duration) {
    EXCLUDED_CPU_NS.fetch_add(d.as_nanos() as u64, Relaxed);
}

/// Process CPU, viewer CPU, excluded CPU and allocations at one moment.
#[derive(Clone, Copy, Debug)]
pub struct CpuMark {
    pub at: Instant,
    process: Duration,
    viewer_ns: u64,
    excluded_ns: u64,
    allocs: AllocSnapshot,
}

/// Usage between two [`CpuMark`]s.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct SideUsage {
    pub wall_s: f64,
    pub process_cpu_ms: f64,
    pub viewer_cpu_ms: f64,
    /// Process minus viewer minus excluded: the agent side (production threads,
    /// agent runtime, the bench's encoder thread in the composed pipeline).
    pub agent_cpu_ms: f64,
    pub excluded_cpu_ms: f64,
    pub allocs: AllocSnapshot,
}

impl CpuMark {
    pub fn now() -> CpuMark {
        CpuMark {
            at: Instant::now(),
            process: cpu_time_process(),
            viewer_ns: VIEWER_CPU_NS.load(Relaxed),
            excluded_ns: EXCLUDED_CPU_NS.load(Relaxed),
            allocs: AllocSnapshot::take(),
        }
    }

    /// Usage from `earlier` to `self`.
    pub fn since(&self, earlier: &CpuMark) -> SideUsage {
        let process = self.process.saturating_sub(earlier.process).as_secs_f64() * 1e3;
        let viewer = self.viewer_ns.saturating_sub(earlier.viewer_ns) as f64 / 1e6;
        let excluded = self.excluded_ns.saturating_sub(earlier.excluded_ns) as f64 / 1e6;
        SideUsage {
            wall_s: self.at.duration_since(earlier.at).as_secs_f64(),
            process_cpu_ms: process,
            viewer_cpu_ms: viewer,
            agent_cpu_ms: (process - viewer - excluded).max(0.0),
            excluded_cpu_ms: excluded,
            allocs: self.allocs.since(&earlier.allocs),
        }
    }
}

/// Multi-thread runtime for the bench viewer side: threads marked
/// [`Side::Viewer`] and their CPU time accounted.
pub fn viewer_runtime(workers: usize) -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers.max(1))
        .thread_name("bench-viewer")
        .enable_all()
        .on_thread_start(viewer_thread_start)
        .on_thread_park(viewer_checkpoint)
        .on_thread_stop(viewer_checkpoint)
        .build()?)
}

/// Multi-thread runtime for the agent side (default side, as the shipped agent's
/// `Runtime::new()`; `workers = 0` keeps tokio's default of one per core).
pub fn agent_runtime(workers: usize) -> Result<tokio::runtime::Runtime> {
    let mut b = tokio::runtime::Builder::new_multi_thread();
    if workers > 0 {
        b.worker_threads(workers);
    }
    Ok(b.thread_name("bench-agent").enable_all().build()?)
}

// ---------------------------------------------------------------------------
// Durations, ports, bitrates

/// Warm-up (discarded) and measured time of one macro session: 5 s + 20 s
/// (`quick`), 5 s + 60 s (`full`); `NUVIRA_BENCH_MACRO_SECS=warm,measure`
/// overrides both (recorded in the run's env and in each result's params).
pub fn macro_durations(tier: Tier) -> (Duration, Duration) {
    if let Ok(v) = std::env::var("NUVIRA_BENCH_MACRO_SECS")
        && let Some((w, m)) = v.split_once(',')
        && let (Ok(w), Ok(m)) = (w.trim().parse::<f64>(), m.trim().parse::<f64>())
    {
        return (Duration::from_secs_f64(w.max(0.0)), Duration::from_secs_f64(m.max(0.5)));
    }
    match tier {
        Tier::Quick => (Duration::from_secs(5), Duration::from_secs(20)),
        // 20 s measured in both tiers (plan section 13: 60 s made the full tier
        // about 2.3 h of macro sessions alone); the full tier differs in its matrix.
        Tier::Full => (Duration::from_secs(5), Duration::from_secs(20)),
    }
}

/// The agent's default 8 Mbit/s at 1080p (`cli.rs`), scaled by pixel count.
pub fn bitrate_kbps_for(w: u32, h: u32) -> u32 {
    ((8000u64 * w as u64 * h as u64) / (1920 * 1080)).clamp(500, 100_000) as u32
}

/// A UDP port free on 127.0.0.1 now (bind and release).
pub fn free_udp_port() -> Result<u16> {
    Ok(std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port())
}

// ---------------------------------------------------------------------------
// Viewer QUIC connection on 127.0.0.1

/// Pins the agent's certificate by SHA-256 like `transport::PinnedCert`.
#[derive(Debug)]
struct Pinned {
    expected: Option<Fingerprint>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use sha2::Digest;
        let fp: Fingerprint = sha2::Sha256::digest(end_entity).into();
        match self.expected {
            Some(e) if e != fp => Err(rustls::Error::General("certificate fingerprint differs".into())),
            _ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// ALPN of agent sessions (wire constant, `transport/src/lib.rs` :55 at the tag).
pub const SESSION_ALPN: &[u8] = b"nuvira/0";

/// What `nuvira_transport::connect` does (TLS 1.3, ring, pinned certificate, ALPN,
/// the production `transport_config()` taken from `server_config()`), but with the
/// client endpoint bound to 127.0.0.1 instead of 0.0.0.0, so the bench never
/// listens on a public address (Windows firewall prompt, plan risk R4). The
/// endpoint is driven by the current runtime.
pub async fn loopback_connect(addr: SocketAddr, expected: Option<Fingerprint>) -> Result<Connection> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { expected, provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![SESSION_ALPN.to_vec()];
    let mut config =
        quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(tls)?));
    // The production transport configuration (BBR, keep-alive, idle timeout, send
    // window, GSO off): `transport_config()` is crate-private, `server_config()`
    // carries it.
    let (server, _) = nuvira_transport::server_config()?;
    config.transport_config(server.transport.clone());
    let bind: SocketAddr = if addr.is_ipv6() { "[::1]:0" } else { "127.0.0.1:0" }.parse()?;
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(config);
    let conn =
        endpoint.connect(addr, "nuvira-agent")?.await.with_context(|| format!("connecting to {addr}"))?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// In-process agent

/// How the in-process agent accepts the bench viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentEntry {
    /// `nuvira_agent::serve_reverse` on a `server_endpoint(127.0.0.1:0)`: the same
    /// `session()` as `serve_direct`, one session, the viewer gives a password
    /// (one extra control round trip before the video starts), the switch-sides
    /// policy (files, clipboard, audio... off: already off with the synthetic
    /// source). Nothing listens on a public address. Default.
    Reverse,
    /// `nuvira_agent::serve_direct(config, port, once = true, fingerprint_file)` as
    /// in the plan: binds 0.0.0.0 (on Windows the firewall asks once per exe path;
    /// loopback traffic works either way).
    Direct,
}

impl AgentEntry {
    /// `NUVIRA_BENCH_AGENT_ENTRY=direct|reverse` (default reverse).
    pub fn from_env() -> AgentEntry {
        match std::env::var("NUVIRA_BENCH_AGENT_ENTRY").as_deref() {
            Ok("direct") => AgentEntry::Direct,
            _ => AgentEntry::Reverse,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AgentEntry::Reverse => "serve_reverse",
            AgentEntry::Direct => "serve_direct",
        }
    }
}

/// Session password of the [`AgentEntry::Reverse`] agent.
pub const REVERSE_PASSWORD: &str = "bench-loopback-4711";

/// An agent running on a runtime of this process.
pub struct LoopbackAgent {
    pub addr: SocketAddr,
    pub fingerprint: Fingerprint,
    /// Password the viewer must give (Reverse), None for Direct.
    pub password: Option<String>,
    pub entry: AgentEntry,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl LoopbackAgent {
    /// Starts the agent on `rt`. `dir` gets the fingerprint file (Direct).
    /// `once = false` (Direct only) keeps accepting sessions.
    pub fn start(
        rt: &tokio::runtime::Runtime,
        config: AgentConfig,
        entry: AgentEntry,
        dir: &Path,
        once: bool,
    ) -> Result<LoopbackAgent> {
        match entry {
            AgentEntry::Reverse => {
                if !once {
                    bail!("serve_reverse serves one session only");
                }
                let _guard = rt.enter();
                let (endpoint, fingerprint) = nuvira_transport::server_endpoint(([127, 0, 0, 1], 0).into())?;
                let addr = endpoint.local_addr()?;
                let reverse = nuvira_agent::Reverse { password: REVERSE_PASSWORD.into() };
                let task = rt.spawn(nuvira_agent::serve_reverse(endpoint, config, reverse));
                Ok(LoopbackAgent {
                    addr,
                    fingerprint,
                    password: Some(REVERSE_PASSWORD.into()),
                    entry,
                    task: Some(task),
                })
            }
            AgentEntry::Direct => {
                std::fs::create_dir_all(dir)?;
                let fp_file: PathBuf = dir.join("agent-fingerprint.txt");
                let _ = std::fs::remove_file(&fp_file);
                let port = free_udp_port()?;
                let task = rt.spawn(nuvira_agent::serve_direct(config, port, once, Some(fp_file.clone())));
                let deadline = Instant::now() + Duration::from_secs(20);
                let fingerprint = loop {
                    if let Ok(t) = std::fs::read_to_string(&fp_file)
                        && t.trim().len() == 64
                    {
                        break nuvira_transport::parse_fingerprint(t.trim())?;
                    }
                    if task.is_finished() {
                        let r = rt.block_on(task);
                        bail!(
                            "serve_direct ended at start: {:?}",
                            r.map(|r| r.map_err(|e| format!("{e:#}")))
                        );
                    }
                    if Instant::now() > deadline {
                        task.abort();
                        bail!("serve_direct wrote no fingerprint in 20 s");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                Ok(LoopbackAgent {
                    addr: SocketAddr::from(([127, 0, 0, 1], port)),
                    fingerprint,
                    password: None,
                    entry,
                    task: Some(task),
                })
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Waits up to `timeout` for the agent to end (after the viewer closed), then
    /// aborts it. Returns its own result, or an error if it had to be aborted.
    pub fn finish(mut self, rt: &tokio::runtime::Runtime, timeout: Duration) -> Result<()> {
        let Some(task) = self.task.take() else { return Ok(()) };
        rt.block_on(async move {
            let abort = task.abort_handle();
            match tokio::time::timeout(timeout, task).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) if e.is_cancelled() => Ok(()),
                Ok(Err(e)) => Err(anyhow::anyhow!("agent task panicked: {e}")),
                Err(_) => {
                    abort.abort();
                    Err(anyhow::anyhow!("agent still running {timeout:?} after the viewer closed: aborted"))
                }
            }
        })
    }
}

impl Drop for LoopbackAgent {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// Handshake facts of a session, recorded in results.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct HandshakeInfo {
    pub times: headless::HandshakeTimes,
    pub agent_os: String,
}

/// An `AgentConfig` on the synthetic source.
pub fn synthetic_config(w: u32, h: u32, fps: u32, hardware: bool, fixed_bitrate: bool) -> AgentConfig {
    use nuvira_codec::{EncoderChoice, EncoderSettings};
    let bitrate_kbps = bitrate_kbps_for(w, h);
    let settings = EncoderSettings { bitrate_bps: bitrate_kbps * 1000, max_fps: fps, ..Default::default() };
    let choice = if hardware { EncoderChoice::Hardware } else { EncoderChoice::Software };
    AgentConfig {
        display: None,
        synthetic: Some((w, h, fps)),
        bitrate_kbps,
        fps,
        plan: nuvira_codec::plan_encoder(choice, settings),
        fixed_bitrate,
        view_only: true,
    }
}

/// Points `NUVIRA_DATA_DIR` at `dir` (the agent's `recover_screens` and identity
/// paths) and forces the file key store. Only in a case's own child process,
/// before its runtimes start.
pub fn isolate_data_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    // SAFETY: called at the start of a case in its own child process (the case is
    // `isolated()`), before the case starts any thread; the runner's threads do not
    // read or write the environment.
    unsafe {
        std::env::set_var("NUVIRA_DATA_DIR", dir);
        std::env::set_var("NUVIRA_KEY_STORE", "file");
    }
    Ok(())
}

/// `nuvira-bench agent-host`: an in-process agent on the synthetic source in a
/// process of its own.
///
/// ```text
/// nuvira-bench agent-host [--synthetic 1920x1080@60] [--encoder software|hardware]
///     [--bitrate KBPS] [--fixed-bitrate] [--entry reverse|direct] [--sessions N]
///     [--data-dir DIR]
/// ```
///
/// Prints `agent <addr> <fingerprint hex> <password or ->` on stdout once it
/// listens, then serves one session (`--sessions 1`, default) or, with `--sessions 0`
/// (direct only), sessions until stdin closes; it also exits when stdin closes.
pub fn cli_agent_host(args: &[String]) -> Result<i32> {
    let mut synthetic = (1920u32, 1080u32, 60u32);
    let mut hardware = false;
    let mut bitrate: Option<u32> = None;
    let mut fixed = false;
    let mut entry = AgentEntry::from_env();
    let mut sessions = 1u32;
    let mut data_dir: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--synthetic" => synthetic = nuvira_agent::parse_synthetic(val()?)?,
            "--encoder" => {
                hardware = match val()?.as_str() {
                    "software" => false,
                    "hardware" => true,
                    v => bail!("--encoder software|hardware, not {v}"),
                }
            }
            "--bitrate" => bitrate = Some(val()?.parse().context("--bitrate")?),
            "--fixed-bitrate" => fixed = true,
            "--entry" => {
                entry = match val()?.as_str() {
                    "reverse" => AgentEntry::Reverse,
                    "direct" => AgentEntry::Direct,
                    v => bail!("--entry reverse|direct, not {v}"),
                }
            }
            "--sessions" => sessions = val()?.parse().context("--sessions")?,
            "--data-dir" => data_dir = Some(PathBuf::from(val()?)),
            other => bail!("agent-host: unknown option {other}"),
        }
    }
    let dir = data_dir
        .unwrap_or_else(|| std::env::temp_dir().join(format!("nuvira-bench-agent-{}", std::process::id())));
    isolate_data_dir(&dir)?;
    let (w, h, fps) = synthetic;
    let mut config = synthetic_config(w, h, fps, hardware, fixed);
    if let Some(b) = bitrate {
        config.bitrate_kbps = b;
    }
    let rt = agent_runtime(0)?;
    let agents = if entry == AgentEntry::Reverse { 1 } else { sessions };
    let agent = LoopbackAgent::start(&rt, config, entry, &dir, agents == 1)?;
    {
        let mut out = std::io::stdout().lock();
        writeln!(
            out,
            "agent {} {} {}",
            agent.addr,
            nuvira_transport::fingerprint_hex(&agent.fingerprint),
            agent.password.as_deref().unwrap_or("-")
        )?;
        out.flush()?;
    }
    // End of stdin = the parent is gone: stop.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut sink = String::new();
        while std::io::stdin().read_line(&mut sink).map(|n| n > 0).unwrap_or(false) {
            sink.clear();
        }
        let _ = tx.send(());
    });
    loop {
        if agent.is_finished() {
            let r = agent.finish(&rt, Duration::from_secs(1));
            rt.shutdown_timeout(Duration::from_secs(2));
            return match r {
                Ok(()) => Ok(0),
                Err(e) => {
                    eprintln!("agent-host: {e:#}");
                    Ok(1)
                }
            };
        }
        if rx.recv_timeout(Duration::from_millis(100)).is_ok() {
            drop(agent);
            rt.shutdown_timeout(Duration::from_secs(2));
            return Ok(0);
        }
    }
}
