//! Composed pipeline (plan 4.2, 7.2): fixture content cannot reach the real agent
//! pipeline at the tag, so the bench chains the production pieces itself:
//!
//! fixture frame -> production encoder (`create_encoder`) -> `VideoFrame` ->
//! stream mode: `FramedSend` on a uni stream / datagram mode: `encode_to_vec` +
//! `fec::encode` + `send_datagram_wait` -> production QUIC config in loopback
//! (`server_endpoint`, viewer [`loopback_connect`]), optionally through the netem
//! proxy -> `FramedRecv` / `read_datagram` + `fec::Receiver` -> production decoder.
//!
//! Frames are paced on a real-time clock ([`Pacer`]); capture and decode
//! timestamps use one clock (`now_us`), so latency is exact. The encoder thread
//! and the agent runtime are the agent side; the viewer runtime and the decoder
//! thread are the viewer side (CPU and allocations attributed per side, fixture
//! generation excluded from the agent's CPU). Keyframe requests go from the
//! decoder to the encoder through a flag (no control channel: not what is measured).
//!
//! A generator that cannot produce a frame in 25% of the frame interval is
//! replaced by a pre-generated ring of 32 frames (S6: 8 per size), recorded as
//! `"source": "ring"` (plan 5.1).
//!
//! Quality: up to 40 frames of the measured window are sampled: **coded** quality
//! compares the decoded frame of that index with its source, **displayed**
//! quality compares the source with whatever was decoded last one frame interval
//! after its capture (skipped, late and lost frames count as stale), PSNR-YUV
//! 6:1:1 and SSIM-Y against the bench's own BT.601 reference (`quality.rs`).
//!
//! Cases: B-MACRO-01 (stream), B-MACRO-02 (datagrams + FEC, netem N0..N5),
//! B-MACRO-08a (input transport latency: 125 Hz `MouseMove` viewer -> agent on the
//! input stream while S5 video flows).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_codec::{EncoderPlan, EncoderSettings, YuvFrame};
use nuvira_proto::v1::{InputEvent, MouseMove, VideoFrame, input_event};
use nuvira_transport::{FramedRecv, FramedSend};

use crate::fixtures::{Frame, Scene, scenes::SceneGen};
use crate::measure::{Better, Distribution, Gate, Metric, Pacer, cpu_time_thread, median};
use crate::net::headless::FREEZE;
use crate::net::netem::{Netem, Spec, profile_spec};
use crate::net::{CpuMark, SideUsage, agent_runtime, bitrate_kbps_for, loopback_connect, viewer_runtime};
use crate::quality::{Reference, psnr, ssim_y};
use crate::registry::{BOTH_TIERS, Case, CaseResult, FULL_ONLY, Level, Registry, RunCtx, Status, Tier};
use crate::resources::ProcSample;

/// FEC parity of the agent at the start of a session (`FEC_START`, agent lib.rs
/// :54) and its keyframe rule (twice, at least `FEC_MAX` :56).
const FEC_RATIO: f64 = 0.2;
const FEC_MAX: f64 = 0.5;
/// Frames of the ring when the generator is too slow.
const RING: u32 = 32;
/// S6 sequence: four sizes of 60 frames (plan 5.1), repeated.
const RESIZE_CYCLE: u32 = 240;
const MAX_SAMPLES: usize = 40;
/// Input test rate and count (plan B-MACRO-08a).
const INPUT_HZ: u32 = 125;
const INPUT_EVENTS: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Stream,
    Datagrams,
}

pub struct ComposedCase {
    pub id: &'static str,
    pub scene: Scene,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hardware: bool,
    pub mode: Mode,
    /// Netem profile (name of `netem::PROFILES`), None = direct loopback.
    pub profile: Option<&'static str>,
    pub input_test: bool,
    pub tiers: &'static [Tier],
}

pub fn register(reg: &mut Registry) {
    let sizes = [(1280u32, 720u32, 60u32), (1920, 1080, 60), (2560, 1440, 60), (3840, 2160, 30)];
    for hardware in [false, true] {
        for scene in [Scene::Terminal, Scene::Scroll, Scene::Video, Scene::Drag, Scene::Resize] {
            for (w, h, fps) in sizes {
                // Full tier: every scene at 1080p, the other sizes on S5 only (time
                // budget, plan section 13).
                if (w, h) != (1920, 1080) && scene != Scene::Drag {
                    continue;
                }
                let quick = matches!(scene, Scene::Video | Scene::Drag) && (w, h) == (1920, 1080);
                reg.add_case(ComposedCase {
                    id: "B-MACRO-01",
                    scene,
                    width: w,
                    height: h,
                    fps,
                    hardware,
                    mode: Mode::Stream,
                    profile: None,
                    input_test: false,
                    tiers: if quick { BOTH_TIERS } else { FULL_ONLY },
                });
            }
        }
    }
    for scene in [Scene::Video, Scene::Drag] {
        for p in ["N0", "N1", "N2", "N3", "N4", "N5"] {
            reg.add_case(ComposedCase {
                id: "B-MACRO-02",
                scene,
                width: 1920,
                height: 1080,
                fps: 60,
                hardware: false,
                mode: Mode::Datagrams,
                profile: Some(p),
                input_test: false,
                tiers: if matches!(p, "N0" | "N3") { BOTH_TIERS } else { FULL_ONLY },
            });
        }
    }
    reg.add_case(ComposedCase {
        id: "B-MACRO-08",
        scene: Scene::Drag,
        width: 1920,
        height: 1080,
        fps: 60,
        hardware: false,
        mode: Mode::Stream,
        profile: None,
        input_test: true,
        tiers: FULL_ONLY,
    });
}

impl Case for ComposedCase {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> String {
        let enc = if self.hardware { "hw" } else { "sw" };
        let mut n = format!("{enc}/{}/{}x{}@{}", self.scene.id(), self.width, self.height, self.fps);
        if self.mode == Mode::Datagrams {
            n.push_str("/datagrams");
        }
        if let Some(p) = self.profile {
            n.push('/');
            n.push_str(p);
        }
        if self.input_test {
            n = format!("a/{n}/input{INPUT_HZ}hz");
        }
        n
    }

    fn level(&self) -> Level {
        Level::Macro
    }

    fn tiers(&self) -> &'static [Tier] {
        self.tiers
    }

    fn config_ids(&self) -> &'static [&'static str] {
        if self.profile.is_some_and(|p| p != "N0") { &["NET-DEGRADED"] } else { &[] }
    }

    fn runnable(&self) -> Result<(), String> {
        Ok(())
    }

    fn isolated(&self) -> bool {
        true
    }

    fn run(&mut self, ctx: &mut RunCtx) -> Result<CaseResult> {
        crate::net::isolate_data_dir(&ctx.tmp.join("data"))?;
        // No adaptive bitrate in the composed pipeline: on a rate-limited profile
        // the fixed rate is capped at 70% of the link, or every frame would be lost
        // in the proxy's queue (the real session's ABR is B-MACRO-04).
        let mut bitrate_kbps = bitrate_kbps_for(self.width, self.height);
        if let Some(p) = self.profile {
            let rate = Spec::parse(p)?.rate_bps;
            if rate > 0.0 {
                bitrate_kbps = bitrate_kbps.min((rate * 0.7 / 1000.0) as u32);
            }
        }
        let settings =
            EncoderSettings { bitrate_bps: bitrate_kbps * 1000, max_fps: self.fps, ..Default::default() };
        let plan = if self.hardware { EncoderPlan::Hardware } else { EncoderPlan::Software };
        match nuvira_codec::create_encoder(plan, settings, None) {
            Ok(e) => drop(e),
            Err(e) if self.hardware => {
                return Ok(CaseResult::not_runnable(self, format!("no hardware encoder: {e:#}")));
            }
            Err(e) => return Err(e.context("OpenH264 encoder")),
        }
        let (warm, measure) = crate::net::macro_durations(ctx.tier);
        let t_src = Instant::now();
        let mut source = Source::new(self.scene, self.width, self.height, self.fps);
        let source_setup_s = t_src.elapsed().as_secs_f64();
        let mut runs = Vec::new();
        for _ in 0..ctx.reps.max(1) {
            let (out, back) = one_run(self, &settings, plan, source, warm, measure, ctx.rng_seed)?;
            source = back;
            runs.push(out);
        }
        let mut r = CaseResult::new(self);
        r.param("scene", self.scene.id())
            .param("width", self.width)
            .param("height", self.height)
            .param("fps", self.fps)
            .param("encoder", runs[0].encoder.clone())
            .param("mode", if self.mode == Mode::Stream { "stream" } else { "datagrams" })
            .param("bitrate_kbps", bitrate_kbps)
            .param("warmup_s", warm.as_secs_f64())
            .param("measure_s", measure.as_secs_f64())
            .param("sessions", runs.len())
            .param("source_setup_s", source_setup_s);
        if let Some(p) = self.profile {
            r.param("profile", p).param("netem_spec", profile_spec(p));
        }
        if self.scene == Scene::Resize {
            // The sizes really encoded (the big one capped as the agent does, VID-3).
            let sizes = crate::fixtures::scenes::resize_sizes(self.width, self.height);
            r.param("s6_sizes", sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect::<Vec<_>>().join(","));
        }
        r.extra.insert("source".into(), (if source.ring.is_some() { "ring" } else { "generator" }).into());
        fill_metrics(&mut r, &runs, self);
        Ok(r)
    }
}

// ---------------------------------------------------------------------------
// Source

/// The fixture source: the generator, or a ring of pre-generated frames.
struct Source {
    generator: SceneGen,
    scene: Scene,
    ring: Option<Vec<Frame>>,
}

impl Source {
    fn new(scene: Scene, w: u32, h: u32, fps: u32) -> Source {
        let n = if scene == Scene::Resize { RESIZE_CYCLE } else { crate::fixtures::FRAMES_FULL };
        let mut generator = scene.generator(w, h, n);
        let ring = (!crate::fixtures::fast_enough(scene, w, h, fps)).then(|| {
            let indices: Vec<u32> = if scene == Scene::Resize {
                (0..4).flat_map(|s| (0..RING / 4).map(move |k| s * (RESIZE_CYCLE / 4) + k)).collect()
            } else {
                (0..RING).collect()
            };
            indices.into_iter().map(|i| generator.frame(i)).collect()
        });
        Source { generator, scene, ring }
    }

    /// Generator index of frame `i` of the session.
    fn logical(&self, i: u32) -> u32 {
        match (self.scene, self.ring.is_some()) {
            (Scene::Resize, false) => i % RESIZE_CYCLE,
            (Scene::Resize, true) => {
                let j = i % RESIZE_CYCLE;
                let seg = RESIZE_CYCLE / 4;
                (j / seg) * seg + (j % seg) % (RING / 4)
            }
            (_, true) => i % RING,
            (_, false) => i,
        }
    }

    fn ring_index(&self, i: u32) -> usize {
        let l = self.logical(i);
        if self.scene == Scene::Resize {
            let seg = RESIZE_CYCLE / 4;
            ((l / seg) * (RING / 4) + l % seg) as usize
        } else {
            l as usize
        }
    }

    /// Frame `i`: from the ring, or rendered into `buf`.
    fn frame<'a>(&'a mut self, i: u32, buf: &'a mut Frame) -> &'a Frame {
        if self.ring.is_some() {
            let k = self.ring_index(i);
            if let Some(ring) = &self.ring {
                return &ring[k];
            }
        }
        let l = self.logical(i);
        self.generator.render(l, buf);
        buf
    }
}

// ---------------------------------------------------------------------------
// One run

#[derive(Clone, Copy, Default)]
struct SrcRec {
    produced: Option<Instant>,
    capture_us: u64,
    encoded: bool,
    bytes: u32,
    encode_us: u32,
    late: bool,
}

#[derive(Clone, Copy)]
struct DecRec {
    /// Source index + 1 (not the wire seq).
    seq: u64,
    decoded_us: u64,
    decode_us: u32,
}

struct SampleSet {
    coded: Vec<(u32, YuvFrame)>,
    displayed: Vec<(u32, Option<YuvFrame>)>,
}

struct RunOut {
    encoder: String,
    src: Vec<SrcRec>,
    dec: Vec<DecRec>,
    window: (u32, u32),
    usage: SideUsage,
    measure_s: f64,
    rss_bytes: u64,
    threads: Option<u32>,
    psnr_coded: Vec<f64>,
    ssim_coded: Vec<f64>,
    psnr_displayed: Vec<f64>,
    ssim_displayed: Vec<f64>,
    coded_missing: usize,
    input_latency_ms: Vec<f64>,
    input_sent: u32,
    shards: u64,
    shards_lost: u64,
    frames_lost: u64,
    frames_rebuilt: u64,
    keyframe_requests: u64,
    netem: Option<serde_json::Value>,
}

/// Viewer-side shared state of a run.
#[derive(Default)]
struct ViewerSide {
    /// Source index of each frame sent (seq - 1 -> index).
    index_of: Mutex<Vec<u32>>,
    dec: Mutex<Vec<DecRec>>,
    samples: Mutex<Option<SampleSet>>,
    shards: AtomicU64,
    shards_lost: AtomicU64,
    frames_lost: AtomicU64,
    frames_rebuilt: AtomicU64,
    keyframe_requests: AtomicU64,
}

#[allow(clippy::too_many_arguments)]
fn one_run(
    case: &ComposedCase,
    settings: &EncoderSettings,
    plan: EncoderPlan,
    source: Source,
    warm: Duration,
    measure: Duration,
    seed: u64,
) -> Result<(RunOut, Source)> {
    let fps = case.fps;
    let warm_frames = (warm.as_secs_f64() * fps as f64).round() as u32;
    let measure_frames = ((measure.as_secs_f64() * fps as f64).round() as u32).max(1);
    let total = warm_frames + measure_frames;
    let every = (measure_frames / MAX_SAMPLES as u32).max(1);
    let sampled = move |i: u32| i >= warm_frames && (i - warm_frames).is_multiple_of(every);

    let agent_rt = agent_runtime(0)?;
    let viewer_rt = viewer_runtime(2)?;
    let (endpoint, fp) = {
        let _g = agent_rt.enter();
        nuvira_transport::server_endpoint(([127, 0, 0, 1], 0).into())?
    };
    let agent_addr = endpoint.local_addr()?;
    let netem = match case.profile {
        Some(p) => Some(Netem::spawn("127.0.0.1:0".parse()?, agent_addr, p)?),
        None => None,
    };
    let target: SocketAddr = netem.as_ref().map_or(agent_addr, |n| n.addr);
    let accept = agent_rt.spawn(async move {
        let inc = endpoint.accept().await.context("no connection")?;
        let conn = inc.await?;
        anyhow::Ok((endpoint, conn))
    });
    let viewer_conn = viewer_rt.block_on(loopback_connect(target, Some(fp)))?;
    let (endpoint, agent_conn) = agent_rt.block_on(accept)??;

    let want_key = Arc::new(AtomicBool::new(false));
    let vside = Arc::new(ViewerSide::default());
    *vside.samples.lock().expect("samples") = Some(SampleSet { coded: Vec::new(), displayed: Vec::new() });
    // Sample deadlines (index, when to look at what is displayed), encoder -> decoder.
    let deadlines: Arc<Mutex<VecDeque<(u32, Instant)>>> = Arc::default();

    // Agent side: frames from the encoder thread to the network.
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel::<VideoFrame>(1);
    let mode = case.mode;
    let sender = {
        let conn = agent_conn.clone();
        agent_rt.spawn(async move {
            match mode {
                Mode::Stream => {
                    let mut tx = FramedSend::new(conn.open_uni().await?);
                    while let Some(f) = frames_rx.recv().await {
                        tx.send(&f).await?;
                    }
                    let _ = tx.finish();
                }
                Mode::Datagrams => {
                    while let Some(f) = frames_rx.recv().await {
                        let max = conn.max_datagram_size().context("datagrams not available")?;
                        let bytes = prost::Message::encode_to_vec(&f);
                        let ratio = if f.keyframe { (FEC_RATIO * 2.0).max(FEC_MAX) } else { FEC_RATIO };
                        for p in nuvira_transport::fec::encode(f.seq, &bytes, max, ratio)? {
                            conn.send_datagram_wait(p.into()).await?;
                        }
                    }
                }
            }
            anyhow::Ok(())
        })
    };

    // Input test: agent side receives, viewer side sends (paced on a std thread).
    let input_sent_at: Arc<Mutex<Vec<Option<Instant>>>> = Arc::new(Mutex::new(Vec::new()));
    let input_latency: Arc<Mutex<Vec<f64>>> = Arc::default();
    let input_events = if case.input_test {
        (INPUT_EVENTS as f64).min(measure.as_secs_f64() * INPUT_HZ as f64).floor() as u32
    } else {
        0
    };
    let input_rx_task = case.input_test.then(|| {
        let conn = agent_conn.clone();
        let (sent, lat) = (input_sent_at.clone(), input_latency.clone());
        agent_rt.spawn(async move {
            let mut rx = FramedRecv::new(conn.accept_uni().await?);
            while let Some(ev) = rx.recv::<InputEvent>().await? {
                let now = Instant::now();
                if let Some(input_event::Event::MouseMove(m)) = ev.event
                    && let Some(Some(t)) = sent.lock().expect("sent").get(m.x as usize).copied()
                {
                    lat.lock().expect("lat").push(now.duration_since(t).as_secs_f64() * 1e3);
                }
            }
            anyhow::Ok(())
        })
    });

    // Viewer side: network -> decoder thread.
    let (dec_tx, dec_rx) = std::sync::mpsc::sync_channel::<(VideoFrame, u64)>(4);
    let receiver = {
        let conn = viewer_conn.clone();
        let (vside, want_key) = (vside.clone(), want_key.clone());
        viewer_rt.spawn(async move {
            match mode {
                Mode::Stream => {
                    let mut rx = FramedRecv::new(conn.accept_uni().await?);
                    while let Some(f) = rx.recv::<VideoFrame>().await? {
                        if dec_tx.send((f, nuvira_proto::now_us())).is_err() {
                            break;
                        }
                    }
                }
                Mode::Datagrams => receive_datagrams(&conn, &dec_tx, &want_key, &vside).await?,
            }
            anyhow::Ok(())
        })
    };
    let decoder = {
        let (vside, want_key, deadlines) = (vside.clone(), want_key.clone(), deadlines.clone());
        std::thread::Builder::new()
            .name("bench-decoding".into())
            .spawn(move || decode_thread(dec_rx, vside, want_key, deadlines, sampled))?
    };
    let input_tx_task = case.input_test.then(|| {
        let conn = viewer_conn.clone();
        let sent = input_sent_at.clone();
        *sent.lock().expect("sent") = vec![None; input_events as usize];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let pacer = std::thread::spawn(move || {
            crate::net::viewer_thread_start();
            std::thread::sleep(warm);
            let mut p = Pacer::new(INPUT_HZ);
            for k in 0..input_events {
                p.wait_next();
                sent.lock().expect("sent")[k as usize] = Some(Instant::now());
                if tx.send(k).is_err() {
                    break;
                }
            }
            crate::net::viewer_checkpoint();
        });
        let task = viewer_rt.spawn(async move {
            let mut tx = FramedSend::new(conn.open_uni().await?);
            while let Some(k) = rx.recv().await {
                let ev = InputEvent {
                    event: Some(input_event::Event::MouseMove(MouseMove { x: k as f32, y: 0.5 })),
                };
                tx.send(&ev).await?;
            }
            let _ = tx.finish();
            anyhow::Ok(())
        });
        (pacer, task)
    });

    // Encoder thread (agent side).
    let marks: Arc<Mutex<(Option<CpuMark>, Option<CpuMark>)>> = Arc::default();
    let encoder_thread = {
        let (settings, want_key, deadlines, marks) =
            (*settings, want_key.clone(), deadlines.clone(), marks.clone());
        let index_of = vside.clone();
        std::thread::Builder::new().name("bench-encoding".into()).spawn(move || {
            encode_thread(
                source,
                plan,
                settings,
                frames_tx,
                want_key,
                deadlines,
                marks,
                (warm_frames, total),
                fps,
                sampled,
                &index_of,
            )
        })?
    };
    let (src, source, encoder_name, enc_err) =
        encoder_thread.join().map_err(|_| anyhow::anyhow!("encoder thread panicked"))?;
    let rss = ProcSample::of_self();
    // Let the last frames arrive and be decoded.
    let drain_until = Instant::now() + Duration::from_secs(3);
    let send_result = agent_rt.block_on(async {
        match tokio::time::timeout(Duration::from_secs(3), sender).await {
            Ok(r) => r.map_err(anyhow::Error::from).and_then(|r| r),
            Err(_) => Err(anyhow::anyhow!("sender still busy 3 s after the last frame")),
        }
    });
    while Instant::now() < drain_until {
        let last_decoded = vside.dec.lock().expect("dec").last().map(|d| d.seq).unwrap_or(0);
        let last_sent = src.iter().rposition(|s| s.encoded).map_or(0, |i| i as u64 + 1);
        if last_decoded >= last_sent
            || mode == Mode::Datagrams && Instant::now() + Duration::from_secs(2) > drain_until
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let input_rx_result = input_rx_task.map(|t| {
        agent_rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), t)
                .await
                .ok()
                .map(|r| r.map_err(anyhow::Error::from).and_then(|r| r))
        })
    });
    if let Some((pacer, task)) = input_tx_task {
        let _ = pacer.join();
        let _ = viewer_rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), task).await });
    }
    viewer_conn.close(0u32.into(), b"done");
    agent_conn.close(0u32.into(), b"done");
    let recv_result =
        viewer_rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), receiver).await });
    let _ = decoder.join();
    agent_rt
        .block_on(async { tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await.ok() });
    viewer_rt.shutdown_timeout(Duration::from_secs(2));
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    let netem_counts = netem.map(|n| n.stop()).transpose()?;

    if let Some(e) = enc_err {
        bail!("encoder: {e}");
    }
    if let Err(e) = send_result {
        bail!("sending: {e:#}");
    }
    if let Ok(Ok(Err(e))) = recv_result
        && mode == Mode::Stream
    {
        bail!("receiving: {e:#}");
    }
    if let Some(Some(Err(e))) = input_rx_result {
        bail!("input stream: {e:#}");
    }

    let (start, end) = {
        let m = marks.lock().expect("marks");
        (m.0.context("no start mark")?, m.1.context("no end mark")?)
    };
    let usage = end.since(&start);
    let dec = std::mem::take(&mut *vside.dec.lock().expect("dec"));
    let samples = vside.samples.lock().expect("samples").take().context("samples")?;

    // Quality (after the run, nothing else running).
    let mut source = source;
    let mut out = RunOut {
        encoder: encoder_name,
        src,
        dec,
        window: (warm_frames, total),
        usage,
        measure_s: usage.wall_s,
        rss_bytes: rss.rss_bytes,
        threads: rss.threads,
        psnr_coded: Vec::new(),
        ssim_coded: Vec::new(),
        psnr_displayed: Vec::new(),
        ssim_displayed: Vec::new(),
        coded_missing: 0,
        input_latency_ms: std::mem::take(&mut *input_latency.lock().expect("lat")),
        input_sent: input_events,
        shards: vside.shards.load(Relaxed),
        shards_lost: vside.shards_lost.load(Relaxed),
        frames_lost: vside.frames_lost.load(Relaxed),
        frames_rebuilt: vside.frames_rebuilt.load(Relaxed),
        keyframe_requests: vside.keyframe_requests.load(Relaxed),
        netem: netem_counts.map(|c| serde_json::to_value(c).unwrap_or_default()),
    };
    let _ = seed;
    let mut buf = Frame::default();
    let mut sampled_idx: Vec<u32> = (warm_frames..total).filter(|&i| sampled(i)).collect();
    sampled_idx.truncate(MAX_SAMPLES);
    for i in sampled_idx {
        let src_frame = source.frame(i, &mut buf);
        let reference = Reference::from_bgra(&src_frame.image());
        match samples.coded.iter().find(|(k, _)| *k == i) {
            Some((_, f)) => {
                out.psnr_coded.push(psnr(f, &reference).yuv);
                out.ssim_coded.push(ssim_y(f, &reference));
            }
            None => out.coded_missing += 1,
        }
        if let Some((_, shown)) = samples.displayed.iter().find(|(k, _)| *k == i) {
            match shown {
                // A picture of another size (S6 around a change) scores on the
                // common region, as decoded frames are compared.
                Some(f) => {
                    out.psnr_displayed.push(psnr(f, &reference).yuv);
                    out.ssim_displayed.push(ssim_y(f, &reference));
                }
                None => {
                    out.psnr_displayed.push(0.0);
                    out.ssim_displayed.push(0.0);
                }
            }
        }
    }
    Ok((out, source))
}

type EncOut = (Vec<SrcRec>, Source, String, Option<String>);

#[allow(clippy::too_many_arguments)]
fn encode_thread(
    mut source: Source,
    plan: EncoderPlan,
    settings: EncoderSettings,
    frames: tokio::sync::mpsc::Sender<VideoFrame>,
    want_key: Arc<AtomicBool>,
    deadlines: Arc<Mutex<VecDeque<(u32, Instant)>>>,
    marks: Arc<Mutex<(Option<CpuMark>, Option<CpuMark>)>>,
    (warm_frames, total): (u32, u32),
    fps: u32,
    sampled: impl Fn(u32) -> bool,
    index_of: &ViewerSide,
) -> EncOut {
    let mut src = vec![SrcRec::default(); total as usize];
    let mut encoder = match nuvira_codec::create_encoder(plan, settings, None) {
        Ok(e) => e,
        Err(e) => return (src, source, String::new(), Some(format!("{e:#}"))),
    };
    let name = encoder.name().to_string();
    let interval = Duration::from_secs(1) / fps;
    let mut pacer = Pacer::new(fps);
    let mut buf = Frame::default();
    let mut error = None;
    for i in 0..total {
        let deadline = pacer.wait_next();
        if i == warm_frames {
            marks.lock().expect("marks").0 = Some(CpuMark::now());
        }
        let c0 = cpu_time_thread();
        let frame = source.frame(i, &mut buf);
        crate::net::exclude_cpu(cpu_time_thread().saturating_sub(c0));
        let produced = Instant::now();
        let capture_us = nuvira_proto::now_us();
        if sampled(i) {
            deadlines.lock().expect("deadlines").push_back((i, produced + interval));
        }
        let force = i == 0 || want_key.swap(false, Relaxed);
        let t = Instant::now();
        let encoded = match encoder.encode(&frame.image(), force) {
            Ok(e) => e,
            Err(e) => {
                error = Some(format!("frame {i}: {e:#}"));
                break;
            }
        };
        let encode_us = t.elapsed().as_micros() as u32;
        let rec = &mut src[i as usize];
        *rec = SrcRec {
            produced: Some(produced),
            capture_us,
            encoded: encoded.is_some(),
            bytes: encoded.as_ref().map_or(0, |e| e.data.len() as u32),
            encode_us,
            late: produced.duration_since(deadline) > interval,
        };
        if let Some(e) = encoded {
            // Frames sent are numbered without gaps (as the agent does): a skipped
            // frame is not a lost one. The table gives the source index back.
            let seq = {
                let mut t = index_of.index_of.lock().expect("index");
                t.push(i);
                t.len() as u64
            };
            let vf = VideoFrame {
                seq,
                capture_us,
                encode_us,
                keyframe: e.keyframe,
                data: e.data,
                encoded_us: nuvira_proto::now_us(),
            };
            if frames.blocking_send(vf).is_err() {
                error = Some("network side gone".into());
                break;
            }
        }
    }
    marks.lock().expect("marks").1 = Some(CpuMark::now());
    drop(frames);
    (src, source, name, error)
}

fn decode_thread(
    frames: std::sync::mpsc::Receiver<(VideoFrame, u64)>,
    vside: Arc<ViewerSide>,
    want_key: Arc<AtomicBool>,
    deadlines: Arc<Mutex<VecDeque<(u32, Instant)>>>,
    sampled: impl Fn(u32) -> bool,
) {
    crate::net::viewer_thread_start();
    let Ok(mut decoder) = nuvira_codec::software_decoder() else { return };
    let mut last: Option<YuvFrame> = None;
    let mut waiting_key = false;
    let mut asked: Option<Instant> = None;
    loop {
        // Displayed-quality deadlines that have passed: what is shown now.
        let next = {
            let mut d = deadlines.lock().expect("deadlines");
            let now = Instant::now();
            while let Some(&(i, at)) = d.front() {
                if at > now {
                    break;
                }
                d.pop_front();
                if let Some(s) = vside.samples.lock().expect("samples").as_mut() {
                    s.displayed.push((i, last.clone()));
                }
            }
            d.front().map(|&(_, at)| at)
        };
        let timeout =
            next.map_or(Duration::from_millis(50), |at| at.saturating_duration_since(Instant::now()));
        let (frame, received_us) = match frames.recv_timeout(timeout) {
            Ok(f) => f,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let _ = received_us;
        if waiting_key && !frame.keyframe {
            continue;
        }
        let t = Instant::now();
        match decoder.decode(&frame.data) {
            Ok(Some(yuv)) => {
                waiting_key = false;
                let decode_us = t.elapsed().as_micros() as u32;
                // The viewer copies every decoded frame (`DecodedFrame { yuv: yuv.clone() }`).
                let copy = yuv.with_planes().into_owned();
                let idx =
                    vside.index_of.lock().expect("index").get((frame.seq - 1) as usize).copied().unwrap_or(0);
                if sampled(idx)
                    && let Some(s) = vside.samples.lock().expect("samples").as_mut()
                    && s.coded.len() < MAX_SAMPLES
                {
                    s.coded.push((idx, copy.clone()));
                }
                last = Some(copy);
                vside.dec.lock().expect("dec").push(DecRec {
                    seq: idx as u64 + 1,
                    decoded_us: nuvira_proto::now_us(),
                    decode_us,
                });
            }
            Ok(None) => {}
            Err(_) => {
                waiting_key = true;
                if asked.is_none_or(|a| a.elapsed() > Duration::from_secs(1)) {
                    asked = Some(Instant::now());
                    want_key.store(true, Relaxed);
                    vside.keyframe_requests.fetch_add(1, Relaxed);
                }
            }
        }
        crate::net::viewer_checkpoint();
    }
    crate::net::viewer_checkpoint();
}

async fn receive_datagrams(
    conn: &quinn::Connection,
    frames: &std::sync::mpsc::SyncSender<(VideoFrame, u64)>,
    want_key: &AtomicBool,
    vside: &ViewerSide,
) -> Result<()> {
    use prost::Message;
    let mut rx = nuvira_transport::fec::Receiver::default();
    let mut seen = crate::net::headless::ShardSeen::default();
    let mut last = 0u64;
    let mut waiting_key = false;
    let mut asked: Option<Instant> = None;
    loop {
        let packet = match conn.read_datagram().await {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };
        seen.push(&packet);
        let pushed = rx.push(&packet);
        let (shards, lost) = rx.take_loss();
        vside.shards.fetch_add(shards, Relaxed);
        vside.shards_lost.fetch_add(lost, Relaxed);
        let Ok(Some((seq, bytes))) = pushed else { continue };
        if seen.completed(seq) {
            vside.frames_rebuilt.fetch_add(1, Relaxed);
        }
        let frame = VideoFrame::decode(bytes.as_slice())?;
        if seq != last + 1 {
            vside.frames_lost.fetch_add(seq - last - 1, Relaxed);
            waiting_key = true;
        }
        last = seq;
        if waiting_key {
            if frame.keyframe {
                waiting_key = false;
                asked = None;
            } else {
                if asked.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                    asked = Some(Instant::now());
                    want_key.store(true, Relaxed);
                    vside.keyframe_requests.fetch_add(1, Relaxed);
                }
                continue;
            }
        }
        if frames.send((frame, nuvira_proto::now_us())).is_err() {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Result

fn fill_metrics(r: &mut CaseResult, runs: &[RunOut], case: &ComposedCase) {
    let mut per = |name: &str, unit: &str, better: Better, gate: Gate, f: &dyn Fn(&RunOut) -> f64| {
        r.metric(name, Metric::from_values(unit, better, gate, runs.iter().map(f).collect()));
    };
    let lat = |o: &RunOut| -> Vec<f64> {
        let (a, b) = o.window;
        o.dec
            .iter()
            .filter(|d| (a as u64..b as u64).contains(&(d.seq - 1)))
            .map(|d| (d.decoded_us as f64 - o.src[(d.seq - 1) as usize].capture_us as f64) / 1000.0)
            .collect()
    };
    fn in_window(o: &RunOut) -> Vec<&SrcRec> {
        let (a, b) = o.window;
        o.src[a as usize..b as usize].iter().filter(|s| s.produced.is_some()).collect()
    }
    let dists: Vec<Distribution> = runs.iter().map(|o| Distribution::of(&lat(o))).collect();
    per("latency_p50_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| Distribution::of(&lat(o)).p50);
    per("latency_p95_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| Distribution::of(&lat(o)).p95);
    per("latency_max_ms", "ms", Better::Lower, Gate::Info, &|o| Distribution::of(&lat(o)).max);
    per("fps", "1/s", Better::Higher, Gate::MacroThroughput, &|o| {
        lat(o).len() as f64 / o.measure_s.max(1e-3)
    });
    per("kbit_s", "kbit/s", Better::Lower, Gate::Info, &|o| {
        in_window(o).iter().map(|s| s.bytes as f64).sum::<f64>() * 8.0 / o.measure_s.max(1e-3) / 1000.0
    });
    per("bytes_frame", "B", Better::Lower, Gate::Info, &|o| {
        let w = in_window(o);
        let enc: Vec<f64> = w.iter().filter(|s| s.encoded).map(|s| s.bytes as f64).collect();
        enc.iter().sum::<f64>() / enc.len().max(1) as f64
    });
    per("encode_ms_p50", "ms", Better::Lower, Gate::MacroLatency, &|o| {
        median(
            &in_window(o)
                .iter()
                .filter(|s| s.encoded)
                .map(|s| s.encode_us as f64 / 1000.0)
                .collect::<Vec<_>>(),
        )
    });
    per("decode_ms_p50", "ms", Better::Lower, Gate::MacroLatency, &|o| {
        let (a, b) = o.window;
        median(
            &o.dec
                .iter()
                .filter(|d| (a as u64..b as u64).contains(&(d.seq - 1)))
                .map(|d| d.decode_us as f64 / 1000.0)
                .collect::<Vec<_>>(),
        )
    });
    per("skipped", "1", Better::Lower, Gate::Info, &|o| {
        in_window(o).iter().filter(|s| !s.encoded).count() as f64
    });
    per("late_frames", "1", Better::Lower, Gate::Info, &|o| {
        in_window(o).iter().filter(|s| s.late).count() as f64
    });
    per("not_decoded", "1", Better::Lower, Gate::Info, &|o| {
        (in_window(o).iter().filter(|s| s.encoded).count() as f64 - lat(o).len() as f64).max(0.0)
    });
    let frames_of = |o: &RunOut| in_window(o).len().max(1) as f64;
    per("cpu_ms_frame_agent", "ms", Better::Lower, Gate::MacroCpu, &|o| o.usage.agent_cpu_ms / frames_of(o));
    per("cpu_ms_frame_viewer", "ms", Better::Lower, Gate::MacroCpu, &|o| {
        o.usage.viewer_cpu_ms / frames_of(o)
    });
    per("allocs_frame_agent", "1", Better::Lower, Gate::Micro, &|o| {
        o.usage.allocs.agent.allocs as f64 / frames_of(o)
    });
    per("allocs_frame_viewer", "1", Better::Lower, Gate::Micro, &|o| {
        o.usage.allocs.viewer.allocs as f64 / frames_of(o)
    });
    per("alloc_bytes_frame_agent", "B", Better::Lower, Gate::Micro, &|o| {
        o.usage.allocs.agent.bytes as f64 / frames_of(o)
    });
    per("alloc_bytes_frame_viewer", "B", Better::Lower, Gate::Micro, &|o| {
        o.usage.allocs.viewer.bytes as f64 / frames_of(o)
    });
    per("rss_mb", "MB", Better::Lower, Gate::Resource, &|o| o.rss_bytes as f64 / 1048576.0);
    per("threads", "1", Better::Lower, Gate::Info, &|o| o.threads.unwrap_or(0) as f64);
    let mean = |v: &[f64]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().map(|&x| crate::quality::cap(x)).sum::<f64>() / v.len() as f64
        }
    };
    // Through an impaired link (netem profile other than N0) the decoded pictures depend
    // on which packets the seeded-but-timing-dependent proxy drops and on the rate
    // control's reaction: the quality is not deterministic (two builds of the same code
    // differed by 0.22 dB on MSI-UBUNTU, and most runs scored no sampled frame at all),
    // so it informs there; it gates on the clean link (plan section 13.4).
    let qgate = if case.profile.is_some_and(|p| p != "N0") { Gate::Info } else { Gate::Quality };
    per("psnr_yuv_coded_db", "dB", Better::Higher, qgate, &|o| mean(&o.psnr_coded));
    per("ssim_y_coded", "1", Better::Higher, qgate, &|o| mean(&o.ssim_coded));
    per("psnr_yuv_displayed_db", "dB", Better::Higher, qgate, &|o| mean(&o.psnr_displayed));
    per("ssim_y_displayed", "1", Better::Higher, qgate, &|o| mean(&o.ssim_displayed));
    per("quality_coded_missing", "1", Better::Lower, Gate::Info, &|o| o.coded_missing as f64);
    if case.mode == Mode::Datagrams {
        per("shards_lost_pct", "%", Better::Lower, Gate::Info, &|o| {
            o.shards_lost as f64 * 100.0 / o.shards.max(1) as f64
        });
        per("frames_lost", "1", Better::Lower, Gate::Info, &|o| o.frames_lost as f64);
        per("frames_rebuilt", "1", Better::Higher, Gate::Info, &|o| o.frames_rebuilt as f64);
        per("keyframe_requests", "1", Better::Lower, Gate::Info, &|o| o.keyframe_requests as f64);
        per("freezes", "1", Better::Lower, Gate::Info, &|o| freezes(o).0);
        per("freeze_ms", "ms", Better::Lower, Gate::Info, &|o| freezes(o).1);
    }
    if case.input_test {
        per("input_latency_p50_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| {
            Distribution::of(&o.input_latency_ms).p50
        });
        per("input_latency_p95_ms", "ms", Better::Lower, Gate::MacroLatency, &|o| {
            Distribution::of(&o.input_latency_ms).p95
        });
        per("input_latency_max_ms", "ms", Better::Lower, Gate::Info, &|o| {
            Distribution::of(&o.input_latency_ms).max
        });
        per("input_events_lost", "1", Better::Lower, Gate::Info, &|o| {
            o.input_sent as f64 - o.input_latency_ms.len() as f64
        });
        let d: Vec<Distribution> = runs.iter().map(|o| Distribution::of(&o.input_latency_ms)).collect();
        r.distribution("input_latency_ms", Distribution::median_of(&d));
    }
    r.distribution("latency_ms", Distribution::median_of(&dists));
    if let Some(n) = runs.iter().find_map(|o| o.netem.clone()) {
        r.extra.insert("netem".into(), n);
    }
    // Every frame of the window lost or unreadable: not a valid measurement.
    // Every frame of the window lost: a broken run on a clean link; on a degraded
    // profile it is the measured outcome (fps 0), noted.
    if runs.iter().any(|o| lat(o).is_empty()) {
        if case.profile.is_none_or(|p| p == "N0") {
            r.status = Status::Failed;
        }
        r.note("no frame of the measured window was decoded");
    }
    let usage: Vec<serde_json::Value> =
        runs.iter().map(|o| serde_json::to_value(o.usage).unwrap_or_default()).collect();
    r.extra.insert("usage".into(), usage.into());
}

/// Gaps longer than [`FREEZE`] between decoded frames of the window: (count, total ms).
fn freezes(o: &RunOut) -> (f64, f64) {
    let (a, b) = o.window;
    let mut t: Vec<u64> =
        o.dec.iter().filter(|d| (a as u64..b as u64).contains(&(d.seq - 1))).map(|d| d.decoded_us).collect();
    t.sort_unstable();
    let mut n = 0.0;
    let mut total = 0.0;
    for w in t.windows(2) {
        let g = w[1].saturating_sub(w[0]);
        if g > FREEZE.as_micros() as u64 {
            n += 1.0;
            total += g as f64 / 1000.0;
        }
    }
    (n, total)
}
