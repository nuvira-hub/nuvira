//! Resource benchmarks (plan 7.3, WP4): `nuvira-bench run res`.
//!
//! - [`long_session`]: B-RES-01, long session (leaks), in-process and with the agent
//!   binary in a process of its own;
//! - [`churn`]: B-RES-02, 200 sequential sessions on one agent, direct and through
//!   a local server;
//! - here: B-RES-03 (file transfer memory, from B-MACRO-10's harness) and B-RES-04
//!   (allocations per frame per side of a real session; the micro figures of the
//!   same summary are the `allocs_frame` metrics of the B-MICRO results, compared by
//!   `compare`).
//!
//! Every case runs in a child process of its own ([`NetCase`]). B-RES-05 (server
//! soak) is a Go test (WP5).

#[cfg(bench_has_res_churn)]
pub mod churn;
#[cfg(bench_has_res_long_session)]
pub mod long_session;

use std::time::Duration;

use anyhow::Result;

use crate::alloc::Side;
use crate::measure::{Better, Gate, Metric, least_squares};
use crate::net::headless::{HeadlessViewer, ViewerOptions};
use crate::net::server_mode::{self, NetCase};
use crate::net::{AgentEntry, LoopbackAgent, macro_durations};
use crate::registry::{BOTH_TIERS, CaseResult, Level, Registry, RunCtx};
use crate::resources::ProcSample;

pub fn register(reg: &mut Registry) {
    #[cfg(bench_has_res_long_session)]
    long_session::register(reg);
    #[cfg(bench_has_res_churn)]
    churn::register(reg);
    #[cfg(bench_has_macro__files)]
    reg.add_case(
        NetCase::new("B-RES-03", "file-memory/download+upload", Level::Res, BOTH_TIERS, file_memory)
            .config_ids(&["WIN11", "AV-SOFTWARE"])
            .runnable(server_mode::real_capture_runnable()),
    );
    reg.add_case(NetCase::new(
        "B-RES-04",
        "allocs-per-frame/session/synthetic-1920x1080@60",
        Level::Res,
        BOTH_TIERS,
        allocs_per_frame,
    ));
}

const MIB: f64 = (1u64 << 20) as f64;

/// Differences `post - pre` of a process (`prefix` + name), gate `resource`.
pub(crate) fn record_delta(r: &mut CaseResult, prefix: &str, pre: &ProcSample, post: &ProcSample) {
    let m = |unit: &str, v: f64| Metric::single(unit, Better::Lower, Gate::Resource, v);
    let n = |x: &str| format!("{prefix}{x}");
    r.metric(&n("rss_delta_mb"), m("MiB", (post.rss_bytes as f64 - pre.rss_bytes as f64) / MIB));
    if let (Some(a), Some(b)) = (pre.private_bytes, post.private_bytes) {
        r.metric(&n("private_delta_mb"), m("MiB", (b as f64 - a as f64) / MIB));
    }
    let d = |a: Option<u32>, b: Option<u32>| a.zip(b).map(|(a, b)| b as f64 - a as f64);
    if let Some(v) = d(pre.threads, post.threads) {
        r.metric(&n("threads_delta"), m("1", v));
    }
    if let Some(v) = d(pre.open_objects(), post.open_objects()) {
        r.metric(&n(if pre.handles.is_some() { "handles_delta" } else { "fds_delta" }), m("1", v));
    }
    if let Some(v) = d(pre.sockets, post.sockets) {
        r.metric(&n("sockets_delta"), m("1", v));
    }
    r.extra.insert(format!("{prefix}pre"), serde_json::to_value(pre).unwrap_or_default());
    r.extra.insert(format!("{prefix}post"), serde_json::to_value(post).unwrap_or_default());
}

/// RSS slope (MiB per hour) of the samples taken in `[from_ms, to_ms]` (Unix ms).
pub(crate) fn rss_slope_mb_h(samples: &[ProcSample], from_ms: u64, to_ms: u64) -> Option<f64> {
    let pts: Vec<(f64, f64)> = samples
        .iter()
        .filter(|s| s.unix_ms >= from_ms && s.unix_ms <= to_ms)
        .map(|s| ((s.unix_ms - from_ms) as f64 / 3_600_000.0, s.rss_bytes as f64 / MIB))
        .collect();
    least_squares(&pts).map(|(slope, _)| slope)
}

/// The samples as a compact series in the result (`series_<name>`: [s, MiB, threads, handles|fds]).
pub(crate) fn record_series(r: &mut CaseResult, name: &str, samples: &[ProcSample]) {
    let t0 = samples.first().map_or(0, |s| s.unix_ms);
    let rows: Vec<serde_json::Value> = samples
        .iter()
        .map(|s| {
            serde_json::json!([
                (s.unix_ms - t0) as f64 / 1000.0,
                (s.rss_bytes as f64 / MIB * 100.0).round() / 100.0,
                s.threads,
                s.open_objects(),
                s.sockets
            ])
        })
        .collect();
    r.extra.insert(format!("series_{name}"), serde_json::Value::Array(rows));
}

/// B-RES-03: peak RSS growth of a small and a large transfer (download and upload)
/// in one run; it must not depend on the file size (within 32 MiB).
#[cfg(bench_has_macro__files)]
fn file_memory(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    use crate::fixtures::files;
    use crate::macro_::files::{Dir, Rig, check, measured};
    let big = match ctx.tier {
        crate::registry::Tier::Quick => 100 * files::MIB,
        crate::registry::Tier::Full => files::GIB,
    };
    let sizes = [files::MIB, big];
    r.param("sizes", serde_json::json!(sizes));
    let src_dir = ctx.tmp.join("src");
    let srcs: Vec<_> = sizes.iter().map(|&s| files::make(s, &src_dir).map(|x| x.0)).collect::<Result<_>>()?;
    let mut rig = Rig::start(&ctx.tmp.join("agent"), "N0")?;
    let viewer_rt = crate::net::viewer_runtime(2)?;
    let tmp = ctx.tmp.clone();
    let res = viewer_rt.block_on(async {
        let mut s = rig.session().await?;
        let mut growth = Vec::new();
        let mut ok = true;
        for dir in [Dir::Download, Dir::Upload] {
            let mut g = Vec::new();
            for (i, src) in srcs.iter().enumerate() {
                let dest = tmp.join(format!("dest-{}-{i}", if dir == Dir::Download { "dl" } else { "ul" }));
                let o = measured(&mut rig, &mut s, dir, src, &dest).await?;
                ok &= check(r, sizes[i], &o.dest)?;
                std::fs::remove_file(&o.dest).ok();
                g.push(o.peak_rss.saturating_sub(o.rss_before) as f64 / MIB);
            }
            growth.push((dir, g));
        }
        s.close().await;
        Ok::<_, anyhow::Error>((growth, ok))
    });
    viewer_rt.shutdown_timeout(Duration::from_secs(2));
    rig.stop();
    let (growth, ok) = res?;
    for (dir, g) in growth {
        let name = if dir == Dir::Download { "download" } else { "upload" };
        let m = |v: f64| Metric::single("MiB", Better::Lower, Gate::Resource, v);
        r.metric(&format!("{name}_rss_growth_small_mb"), m(g[0]));
        r.metric(&format!("{name}_rss_growth_large_mb"), m(g[1]));
        r.metric(&format!("{name}_rss_growth_diff_mb"), m(g[1] - g[0]));
        if g[1] - g[0] > 32.0 {
            r.note(format!(
                "{name}: peak RSS grows with the file size ({:.1} MiB more for the large file)",
                g[1] - g[0]
            ));
        }
    }
    if !ok {
        r.status = crate::report::Status::Failed;
    }
    Ok(())
}

/// B-RES-04: allocations and bytes allocated per decoded frame, agent and viewer
/// side, of a real in-process session (B-MACRO-03 harness: synthetic 1080p60,
/// software encoder, fixed bitrate, headless viewer on its own runtime).
fn allocs_per_frame(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let config = crate::net::synthetic_config(1920, 1080, 60, false, true);
    let entry = AgentEntry::from_env();
    let agent = LoopbackAgent::start(&agent_rt, config, entry, &ctx.tmp.join("agent"), true)?;
    let (addr, fp, password) = (agent.addr, agent.fingerprint, agent.password.clone());
    let (warm, measured) = macro_durations(ctx.tier);
    let stats = viewer_rt.block_on(async {
        let mut s =
            HeadlessViewer::connect_direct(addr, Some(fp), ViewerOptions { password, ..Default::default() })
                .await?;
        s.first_frame(Duration::from_secs(20)).await?;
        tokio::time::sleep(warm).await;
        let st = s.run(measured).await;
        s.close().await;
        st
    });
    let _ = agent.finish(&agent_rt, Duration::from_secs(3));
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    let stats = stats?;
    anyhow::ensure!(stats.frames > 0, "no frame decoded in the window");
    let f = stats.frames as f64;
    let a = stats.usage.allocs;
    let m = |unit: &str, v: f64| Metric::single(unit, Better::Lower, Gate::Micro, v);
    r.metric("allocs_frame_agent", m("1", a.side(Side::Agent).allocs as f64 / f));
    r.metric("allocs_frame_viewer", m("1", a.side(Side::Viewer).allocs as f64 / f));
    r.metric("alloc_bytes_frame_agent", m("B", a.side(Side::Agent).bytes as f64 / f));
    r.metric("alloc_bytes_frame_viewer", m("B", a.side(Side::Viewer).bytes as f64 / f));
    // Copy proxy: bytes allocated per frame over the frame's raw size (BGRA).
    let raw = 1920.0 * 1080.0 * 4.0;
    r.metric(
        "alloc_bytes_per_raw_frame_agent",
        Metric::single("1", Better::Lower, Gate::Info, a.side(Side::Agent).bytes as f64 / f / raw),
    );
    r.metric(
        "alloc_bytes_per_raw_frame_viewer",
        Metric::single("1", Better::Lower, Gate::Info, a.side(Side::Viewer).bytes as f64 / f / raw),
    );
    r.param("frames", stats.frames);
    r.param("agent_entry", entry.name());
    r.param("window_s", stats.window_s);
    Ok(())
}
