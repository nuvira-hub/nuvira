//! B-MACRO-10 (WP4): file transfer through the agent's file manager.
//!
//! An in-process agent with the **real** screen capture (`synthetic: None`: the
//! synthetic source turns the file manager off, plan 4.2) on `serve_direct`
//! (`once = false`, so a cut session can be followed by a new one), and the
//! headless viewer, direct or through the netem proxy (N2, N3). The video keeps
//! flowing during the transfer, as in a real session.
//!
//! - download (agent -> viewer): `FsRequest{download}` on the control stream,
//!   `FsResponse`, then the agent's uni stream of `FileChunk`s (header with
//!   `transfer_id`, then data); written by the bench to `name.part`, renamed at the
//!   end, as `receive_download` does (`viewer/src/net.rs` :1532);
//! - upload (viewer -> agent): `FsRequest{upload_offset}` first (as the viewer),
//!   then a uni stream: header `FileChunk{name, size, dir, offset, transfer_id}` and
//!   256 KiB chunks (`send_upload`, `net.rs` :1484), until the agent's
//!   `FileReceived{transfer_id}`; the destination folder is always an explicit
//!   temporary folder (never "" = the real Downloads, plan 4.3);
//! - resume: the session is cut at 50% (connection closed), a new session asks
//!   where to resume (`FsUploadOffset` / the local `.part` length for downloads,
//!   `FsDownload.offset`) and finishes.
//!
//! Metrics: MB/s, time to first byte (download), CPU ms/MB per side, allocations per
//! MB per side, peak RSS of the process during the transfer (B-RES-03 compares it
//! across sizes); gate `exact`: SHA-256 of the received file = manifest, `.part`
//! gone. Runs only where the real capture is allowed ([`real_capture_runnable`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::control_message::Msg;
use nuvira_proto::v1::{ControlMessage, FileChunk, FsDownload, FsRequest, FsUploadOffset, fs_request};
use nuvira_transport::{FramedRecv, FramedSend};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::alloc::Side;
use crate::fixtures::{Manifest, files, hex};
use crate::measure::{Better, Gate, Metric};
use crate::net::headless::{HeadlessSession, HeadlessViewer, ViewerOptions};
use crate::net::netem::{Netem, NetemHandle};
use crate::net::server_mode::{self, NetCase, ms};
use crate::net::{AgentEntry, CpuMark, LoopbackAgent};
use crate::registry::{BOTH_TIERS, CaseResult, FULL_ONLY, Level, Registry, RunCtx, Tier};
use crate::report::Status;

const CHUNK: usize = 256 * 1024;
const MIB: f64 = (1u64 << 20) as f64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    Download,
    Upload,
}

impl Dir {
    fn name(self) -> &'static str {
        match self {
            Dir::Download => "download",
            Dir::Upload => "upload",
        }
    }
}

fn size_name(size: u64) -> String {
    match size {
        s if s >= files::GIB => format!("{}GiB", s / files::GIB),
        s if s >= files::MIB => format!("{}MiB", s / files::MIB),
        s => format!("{}KiB", s / files::KIB),
    }
}

pub fn register(reg: &mut Registry) {
    let (k, m, m100, g) = (files::KIB, files::MIB, 100 * files::MIB, files::GIB);
    for dir in [Dir::Download, Dir::Upload] {
        for (size, profile, tiers) in [
            (m, "N0", BOTH_TIERS),
            (m100, "N0", BOTH_TIERS),
            (k, "N0", FULL_ONLY),
            (g, "N0", FULL_ONLY),
            (m, "N2", FULL_ONLY),
            (m100, "N2", FULL_ONLY),
            (k, "N3", FULL_ONLY),
            (m, "N3", FULL_ONLY),
        ] {
            let name = format!("{}/{}/{profile}", dir.name(), size_name(size));
            reg.add_case(file_case(name, tiers, move |ctx, r| plain(ctx, r, dir, size, profile)));
        }
        let name = format!("resume/{}/{}/N0", dir.name(), size_name(m100));
        reg.add_case(file_case(name, FULL_ONLY, move |ctx, r| resume(ctx, r, dir, m100)));
    }
}

fn file_case(
    name: String,
    tiers: &'static [Tier],
    f: impl FnMut(&mut RunCtx, &mut CaseResult) -> Result<()> + Send + 'static,
) -> NetCase {
    NetCase::new("B-MACRO-10", name, Level::Macro, tiers, f)
        .config_ids(&["WIN11", "AV-SOFTWARE", "NET-DEGRADED"])
        .runnable(server_mode::real_capture_runnable())
}

/// One open session (and the proxy in front of the agent, if any).
pub(crate) struct Rig {
    pub agent_rt: tokio::runtime::Runtime,
    pub agent: LoopbackAgent,
    pub proxy: Option<NetemHandle>,
    next_id: u32,
}

impl Rig {
    /// Starts the agent; the viewer side runs on the caller's `viewer_runtime`.
    pub fn start(dir: &Path, profile: &str) -> Result<Rig> {
        let agent_rt = crate::net::agent_runtime(0)?;
        let config = server_mode::real_capture_config(8000, 30);
        let agent = LoopbackAgent::start(&agent_rt, config, AgentEntry::Direct, dir, false)?;
        let proxy = match profile {
            "N0" | "" => None,
            p => Some(Netem::spawn("127.0.0.1:0".parse()?, agent.addr, p)?),
        };
        Ok(Rig { agent_rt, agent, proxy, next_id: 1 })
    }

    /// Opens a session and waits for its first frame (capture works, video flows).
    pub async fn session(&self) -> Result<HeadlessSession> {
        let addr = self.proxy.as_ref().map_or(self.agent.addr, |p| p.addr);
        let s = HeadlessViewer::connect_direct(addr, Some(self.agent.fingerprint), ViewerOptions::default())
            .await?;
        s.first_frame(Duration::from_secs(20)).await.context("no video from the real capture")?;
        if !s.agent.file_manager {
            bail!("the agent offers no file manager (Hello.file_manager false)");
        }
        Ok(s)
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    pub fn stop(self) {
        let Rig { agent_rt, agent, proxy, .. } = self;
        drop(proxy);
        drop(agent);
        agent_rt.shutdown_timeout(Duration::from_secs(2));
    }
}

/// Waits for the control message `pred` picks.
async fn wait_event<T>(
    s: &mut HeadlessSession,
    timeout: Duration,
    mut pred: impl FnMut(&Msg) -> Option<T>,
) -> Result<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, s.events().recv()).await {
            Err(_) => bail!("no answer from the agent in {timeout:?}"),
            Ok(None) => bail!("session closed: {:?}", s.error()),
            Ok(Some(ControlMessage { msg: Some(m) })) => {
                if let Some(v) = pred(&m) {
                    return Ok(v);
                }
            }
            Ok(Some(_)) => {}
        }
    }
}

/// What one transfer measured.
#[derive(Clone, Debug, Default)]
pub(crate) struct Outcome {
    pub bytes: u64,
    pub secs: f64,
    pub ttfb_ms: Option<f64>,
    pub viewer_cpu_ms: f64,
    pub agent_cpu_ms: f64,
    pub agent_allocs: u64,
    pub viewer_allocs: u64,
    pub agent_alloc_bytes: u64,
    pub viewer_alloc_bytes: u64,
    pub peak_rss: u64,
    pub rss_before: u64,
    /// Where the received file is.
    pub dest: PathBuf,
}

/// Stops the transfer at this many bytes (resume tests): the connection is closed.
#[derive(Clone, Copy)]
enum Stop {
    Never,
    At(u64),
}

/// Download `src` into `dest_dir` from `offset` (a `.part` there continued).
async fn download(
    s: &mut HeadlessSession,
    id: u32,
    src: &Path,
    dest_dir: &Path,
    offset: u64,
    stop: Stop,
) -> Result<(u64, Option<Instant>, PathBuf)> {
    let name = src.file_name().context("file name")?.to_string_lossy().into_owned();
    let part = dest_dir.join(format!("{name}.part"));
    let dest = dest_dir.join(&name);
    let req = FsRequest {
        id,
        op: Some(fs_request::Op::Download(FsDownload { path: src.display().to_string(), offset })),
    };
    s.control().send_msg(Msg::FsRequest(req)).await?;
    let conn = s.conn.clone();
    // The agent's answer and its stream come in any order.
    let mut rx = loop {
        let stream = tokio::time::timeout(Duration::from_secs(20), conn.accept_uni())
            .await
            .context("no download stream in 20 s")??;
        let mut rx = FramedRecv::new(stream);
        match rx.recv::<FileChunk>().await? {
            Some(first) if first.transfer_id == id => {
                if first.offset != offset {
                    bail!("the agent resumes from {} instead of {offset}", first.offset);
                }
                break rx;
            }
            // Another stream of the agent (a clipboard image): not ours.
            _ => continue,
        }
    };
    let answer = wait_event(s, Duration::from_secs(10), |m| match m {
        Msg::FsResponse(r) if r.id == id => Some(r.error.clone()),
        _ => None,
    })
    .await?;
    if !answer.is_empty() {
        bail!("download refused: {answer}");
    }
    let mut file = if offset == 0 {
        tokio::fs::File::create(&part).await?
    } else {
        tokio::fs::OpenOptions::new().append(true).open(&part).await?
    };
    let mut got = 0u64;
    let mut first_byte = None;
    while let Some(chunk) = rx.recv::<FileChunk>().await? {
        if first_byte.is_none() && !chunk.data.is_empty() {
            first_byte = Some(Instant::now());
        }
        file.write_all(&chunk.data).await?;
        got += chunk.data.len() as u64;
        if let Stop::At(limit) = stop
            && offset + got >= limit
        {
            file.flush().await?;
            return Ok((got, first_byte, part));
        }
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&part, &dest).await?;
    Ok((got, first_byte, dest))
}

/// Upload `src` into `dest_dir` on the agent, asking first where to resume.
async fn upload(
    s: &mut HeadlessSession,
    id: u32,
    src: &Path,
    dest_dir: &Path,
    stop: Stop,
) -> Result<(u64, u64, Option<PathBuf>)> {
    let name = src.file_name().context("file name")?.to_string_lossy().into_owned();
    let dir = dest_dir.display().to_string();
    let req = FsRequest {
        id,
        op: Some(fs_request::Op::UploadOffset(FsUploadOffset { dir: dir.clone(), name: name.clone() })),
    };
    s.control().send_msg(Msg::FsRequest(req)).await?;
    let offset = wait_event(s, Duration::from_secs(10), |m| match m {
        Msg::FsResponse(r) if r.id == id => Some((r.error.clone(), r.offset)),
        _ => None,
    })
    .await
    .and_then(|(e, o)| if e.is_empty() { Ok(o) } else { bail!("upload offset refused: {e}") })?;
    let mut file = tokio::fs::File::open(src).await?;
    let total = file.metadata().await?.len();
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut tx = FramedSend::new(s.conn.open_uni().await?);
    tx.send(&FileChunk {
        name: name.clone(),
        size: total,
        dir,
        offset,
        transfer_id: id,
        ..Default::default()
    })
    .await?;
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        tx.send(&FileChunk { data: buf[..n].to_vec(), ..Default::default() }).await?;
        sent += n as u64;
        if let Stop::At(limit) = stop
            && offset + sent >= limit
        {
            return Ok((offset, sent, None));
        }
    }
    tx.finish()?;
    let received = wait_event(s, Duration::from_secs(600), |m| match m {
        Msg::FileReceived(f) if f.transfer_id == id => Some(f.clone()),
        _ => None,
    })
    .await?;
    if !received.ok {
        bail!("the agent did not keep the file: {}", received.message);
    }
    Ok((offset, sent, Some(PathBuf::from(received.path))))
}

/// One full transfer of `src` with measurements.
pub(crate) async fn measured(
    rig: &mut Rig,
    s: &mut HeadlessSession,
    dir: Dir,
    src: &Path,
    dest_dir: &Path,
) -> Result<Outcome> {
    std::fs::create_dir_all(dest_dir)?;
    let id = rig.id();
    let rss_before = crate::resources::ProcSample::of_self().rss_bytes;
    let sampler = crate::resources::Sampler::start(std::process::id(), Duration::from_millis(100));
    let mark = CpuMark::now();
    let t0 = Instant::now();
    let (bytes, first, dest) = match dir {
        Dir::Download => {
            let (got, first, dest) = download(s, id, src, dest_dir, 0, Stop::Never).await?;
            (got, first, dest)
        }
        Dir::Upload => {
            let (_, sent, dest) = upload(s, id, src, dest_dir, Stop::Never).await?;
            (sent, None, dest.context("no destination path")?)
        }
    };
    let secs = t0.elapsed().as_secs_f64();
    let usage = CpuMark::now().since(&mark);
    let samples = sampler.finish();
    let peak_rss = samples.iter().map(|x| x.rss_bytes).max().unwrap_or(rss_before);
    Ok(Outcome {
        bytes,
        secs,
        ttfb_ms: first.map(|f| ms(f - t0)),
        viewer_cpu_ms: usage.viewer_cpu_ms,
        agent_cpu_ms: usage.agent_cpu_ms,
        agent_allocs: usage.allocs.side(Side::Agent).allocs,
        viewer_allocs: usage.allocs.side(Side::Viewer).allocs,
        agent_alloc_bytes: usage.allocs.side(Side::Agent).bytes,
        viewer_alloc_bytes: usage.allocs.side(Side::Viewer).bytes,
        peak_rss,
        rss_before,
        dest,
    })
}

/// Checks the received file against the manifest (gate `exact`).
pub(crate) fn check(r: &mut CaseResult, size: u64, dest: &Path) -> Result<bool> {
    let got = hex(&files::sha256_file(dest)?);
    let manifest = Manifest::embedded()?;
    let expected = manifest
        .blob_sha256(&files::file_name(size, false))
        .map(String::from)
        .unwrap_or_else(|| hex(&files::digest(size, false)));
    let part_gone = !PathBuf::from(format!("{}.part", dest.display())).exists();
    let ok = got == expected && part_gone;
    r.checksum("received", got);
    r.checksum("expected", expected);
    r.metric("sha256_match", Metric::single("1", Better::Higher, Gate::Exact, f64::from(u8::from(ok))));
    if !part_gone {
        r.note(format!("{}.part left behind", dest.display()));
    }
    Ok(ok)
}

fn record(r: &mut CaseResult, outs: &[Outcome]) {
    let v = |f: &dyn Fn(&Outcome) -> f64| outs.iter().map(f).collect::<Vec<_>>();
    let mb = |o: &Outcome| o.bytes as f64 / MIB;
    r.metric(
        "mb_s",
        Metric::from_values("MB/s", Better::Higher, Gate::MacroThroughput, v(&|o| mb(o) / o.secs)),
    );
    r.metric("seconds", Metric::from_values("s", Better::Lower, Gate::Info, v(&|o| o.secs)));
    let ttfb: Vec<f64> = outs.iter().filter_map(|o| o.ttfb_ms).collect();
    if !ttfb.is_empty() {
        r.metric("ttfb_ms", Metric::from_values("ms", Better::Lower, Gate::MacroLatency, ttfb));
    }
    let per_mb = |x: f64, o: &Outcome| x / mb(o).max(1e-6);
    // Small files: per-MB figures are dominated by the fixed costs; kept as info.
    let gate = if outs.first().is_some_and(|o| o.bytes >= files::MIB) { Gate::MacroCpu } else { Gate::Info };
    r.metric(
        "cpu_ms_mb_viewer",
        Metric::from_values("ms", Better::Lower, gate, v(&|o| per_mb(o.viewer_cpu_ms, o))),
    );
    r.metric(
        "cpu_ms_mb_agent",
        Metric::from_values("ms", Better::Lower, gate, v(&|o| per_mb(o.agent_cpu_ms, o))),
    );
    r.metric(
        "allocs_mb_agent",
        Metric::from_values("1", Better::Lower, Gate::Info, v(&|o| per_mb(o.agent_allocs as f64, o))),
    );
    r.metric(
        "allocs_mb_viewer",
        Metric::from_values("1", Better::Lower, Gate::Info, v(&|o| per_mb(o.viewer_allocs as f64, o))),
    );
    r.metric(
        "alloc_bytes_mb_agent",
        Metric::from_values("B", Better::Lower, Gate::Info, v(&|o| per_mb(o.agent_alloc_bytes as f64, o))),
    );
    r.metric(
        "alloc_bytes_mb_viewer",
        Metric::from_values("B", Better::Lower, Gate::Info, v(&|o| per_mb(o.viewer_alloc_bytes as f64, o))),
    );
    r.metric(
        "peak_rss_mb",
        Metric::from_values("MiB", Better::Lower, Gate::Resource, v(&|o| o.peak_rss as f64 / MIB)),
    );
    r.metric(
        "rss_growth_mb",
        Metric::from_values(
            "MiB",
            Better::Lower,
            Gate::Resource,
            v(&|o| o.peak_rss.saturating_sub(o.rss_before) as f64 / MIB),
        ),
    );
}

/// Transfers per run: 5 for small files, 1 from 100 MiB.
fn transfers(size: u64) -> usize {
    if size <= files::MIB { 5 } else { 1 }
}

fn plain(ctx: &mut RunCtx, r: &mut CaseResult, dir: Dir, size: u64, profile: &str) -> Result<()> {
    let src_dir = ctx.tmp.join("src");
    let (src, _) = files::make(size, &src_dir)?;
    r.param("size_bytes", size);
    r.param("network_profile", profile);
    r.param("direction", dir.name());
    let mut rig = Rig::start(&ctx.tmp.join("agent"), profile)?;
    let n = transfers(size);
    r.param("transfers", n as u64);
    let tmp = ctx.tmp.clone();
    let viewer_rt = crate::net::viewer_runtime(2)?;
    let res = viewer_rt.block_on(async {
        let mut s = rig.session().await?;
        // A moment of video first: the transfer competes with a running session.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut outs = Vec::new();
        let mut all_ok = true;
        for i in 0..n {
            let dest = tmp.join(format!("dest-{i}"));
            let o = measured(&mut rig, &mut s, dir, &src, &dest).await?;
            all_ok &= check(r, size, &o.dest)?;
            std::fs::remove_file(&o.dest).ok();
            outs.push(o);
        }
        if let Some(e) = s.error() {
            r.note(format!("session error: {e}"));
        }
        s.close().await;
        Ok::<_, anyhow::Error>((outs, all_ok))
    });
    viewer_rt.shutdown_timeout(Duration::from_secs(2));
    if let Some(p) = &rig.proxy
        && let Ok(c) = p.counts()
    {
        r.param("netem_dropped", c.dropped());
    }
    rig.stop();
    let (outs, ok) = res?;
    record(r, &outs);
    if !ok {
        r.status = Status::Failed;
        r.note("received file differs from the manifest (exact gate)");
    }
    Ok(())
}

fn resume(ctx: &mut RunCtx, r: &mut CaseResult, dir: Dir, size: u64) -> Result<()> {
    let (src, _) = files::make(size, &ctx.tmp.join("src"))?;
    r.param("size_bytes", size);
    r.param("direction", dir.name());
    r.param("cut_at", size / 2);
    let mut rig = Rig::start(&ctx.tmp.join("agent"), "N0")?;
    let dest_dir = ctx.tmp.join("dest");
    std::fs::create_dir_all(&dest_dir)?;
    let viewer_rt = crate::net::viewer_runtime(2)?;
    let res = viewer_rt.block_on(async {
        let mut s = rig.session().await?;
        let id = rig.id();
        // First half, then the connection is cut.
        let before = match dir {
            Dir::Download => download(&mut s, id, &src, &dest_dir, 0, Stop::At(size / 2)).await?.0,
            Dir::Upload => upload(&mut s, id, &src, &dest_dir, Stop::At(size / 2)).await?.1,
        };
        s.conn.close(1u32.into(), b"cut");
        drop(s);
        // The agent notices and frees itself; a new session asks where to resume.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let t = Instant::now();
        let mut s = rig.session().await?;
        let reconnect = t.elapsed();
        let id = rig.id();
        let (resumed_from, dest) = match dir {
            Dir::Download => {
                let name = src.file_name().context("name")?.to_string_lossy().into_owned();
                let have = std::fs::metadata(dest_dir.join(format!("{name}.part")))?.len();
                let (_, _, dest) = download(&mut s, id, &src, &dest_dir, have, Stop::Never).await?;
                (have, dest)
            }
            Dir::Upload => {
                let (offset, _, dest) = upload(&mut s, id, &src, &dest_dir, Stop::Never).await?;
                (offset, dest.context("no destination")?)
            }
        };
        let finish = t.elapsed();
        s.close().await;
        Ok::<_, anyhow::Error>((before, resumed_from, dest, reconnect, finish))
    });
    viewer_rt.shutdown_timeout(Duration::from_secs(2));
    rig.stop();
    let (before, resumed_from, dest, reconnect, finish) = res?;
    r.param("sent_before_cut", before);
    r.param("resumed_from", resumed_from);
    r.metric("reconnect_ms", Metric::single("ms", Better::Lower, Gate::MacroLatency, ms(reconnect)));
    r.metric("resume_to_done_ms", Metric::single("ms", Better::Lower, Gate::MacroLatency, ms(finish)));
    r.metric(
        "resumed_fraction",
        Metric::single("1", Better::Higher, Gate::Info, resumed_from as f64 / size.max(1) as f64),
    );
    let ok = check(r, size, &dest)?;
    if resumed_from == 0 {
        r.note("resumed from 0: nothing was kept from the first half");
    }
    if !ok {
        r.status = Status::Failed;
    }
    Ok(())
}
