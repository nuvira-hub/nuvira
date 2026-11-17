//! B-MACRO-08b (WP4): input latency end to end, with real injection.
//!
//! An in-process agent with the real screen capture and real input injection
//! (`AgentConfig{synthetic: None, view_only: false}`) and the headless viewer. The
//! agent is `serve_reverse` on a 127.0.0.1 endpoint (the same `session()` as
//! `serve_direct`, whose policy keeps mouse and keyboard), so nothing listens on
//! 0.0.0.0 (no firewall prompt). The viewer sends `MouseMove` alternating x = 0.25 / 0.75 at
//! y = 0.5 (as the viewer's `--input-test`, `viewer/src/lib.rs` :1966) and measures
//! until the agent's `CursorPosition` control message reports the pointer there
//! (pixels of the picture, within max(4 px, 1% of the width)). The time covers the
//! input stream, the agent's `input_thread`, `InputInjector::inject`, the OS moving
//! the pointer, the capture seeing it and the control message back.
//!
//! The real mouse moves: only where allowed ([`input_runnable`]:
//! `NUVIRA_BENCH_INPUT=1` and `NUVIRA_BENCH_REAL_CAPTURE=1`, i.e. PC-LABORATORIO
//! through a scheduled task in the user's session; never on the NUC without the
//! user's OK). Only moves, no clicks, no keys (the lock screen of LAB takes digits
//! as a PIN). PC-LABORATORIO has a USB mouse jiggler: its stray movements show up as
//! `CursorPosition`s near neither target (counted as `stray_positions`) or move the
//! pointer away before the capture saw the target (counted as `missed`, not in the
//! latency figures).

use std::time::{Duration, Instant};

use anyhow::Result;
use nuvira_proto::v1::control_message::Msg;
use nuvira_proto::v1::input_event::Event;
use nuvira_proto::v1::{ControlMessage, InputEvent, MouseMove};

use crate::measure::{Better, Gate, Metric};
use crate::net::headless::{HeadlessViewer, ViewerOptions};
use crate::net::server_mode::{self, NetCase, events_for, ms, ms_events};
use crate::net::{AgentEntry, LoopbackAgent};
use crate::registry::{CaseResult, FULL_ONLY, Level, Registry, RunCtx};
use crate::report::Status;

/// Pause between two moves (the pointer settles, the capture reports it).
const SPACING: Duration = Duration::from_millis(150);
/// A move not seen back within this is `missed`.
const TIMEOUT: Duration = Duration::from_secs(1);

pub fn register(reg: &mut Registry) {
    reg.add_case(
        NetCase::new(
            "B-MACRO-08",
            "b/real-injection/mouse-move/primary-display",
            Level::Macro,
            FULL_ONLY,
            run,
        )
        .config_ids(&["WIN11", "WIN10"])
        .runnable(server_mode::input_runnable()),
    );
}

fn run(ctx: &mut RunCtx, r: &mut CaseResult) -> Result<()> {
    let moves = events_for(ctx.tier, 100, 200);
    let (agent_rt, viewer_rt) = server_mode::runtimes()?;
    let mut config = server_mode::real_capture_config(8000, 60);
    config.view_only = false;
    // `serve_reverse` on 127.0.0.1 (nothing listens on 0.0.0.0): the same `session()`
    // as `serve_direct`; its policy keeps the mouse and keyboard (`reverse_policy`).
    let agent = LoopbackAgent::start(&agent_rt, config, AgentEntry::Reverse, &ctx.tmp.join("agent"), true)?;
    let (addr, fp) = (agent.addr, agent.fingerprint);
    let opts = ViewerOptions { password: agent.password.clone(), ..Default::default() };
    let res = viewer_rt.block_on(async {
        let mut s = HeadlessViewer::connect_direct(addr, Some(fp), opts).await?;
        s.first_frame(Duration::from_secs(20)).await?;
        let (w, h) = (s.format.width as i32, s.format.height as i32);
        let tol = (w / 100).max(4);
        // Warm-up: a few moves not measured (injector and capture pointer state).
        let (mut lat, mut missed, mut stray) = (Vec::new(), 0u32, 0u32);
        for i in 0..moves + 5 {
            let x = if i % 2 == 0 { 0.25f32 } else { 0.75 };
            let (tx, ty) = ((x * w as f32) as i32, h / 2);
            // Drop what is queued (positions of the previous move, jiggles).
            while s.events().try_recv().is_ok() {}
            let sent = Instant::now();
            s.input().send(&InputEvent { event: Some(Event::MouseMove(MouseMove { x, y: 0.5 })) }).await?;
            let deadline = tokio::time::Instant::now() + TIMEOUT;
            let mut seen = None;
            while seen.is_none() {
                match tokio::time::timeout_at(deadline, s.events().recv()).await {
                    Ok(Some(ControlMessage { msg: Some(Msg::CursorPosition(p)) })) => {
                        let at = Instant::now();
                        if (p.x - tx).abs() <= tol && (p.y - ty).abs() <= tol {
                            seen = Some(at);
                        } else {
                            // Neither the previous target nor this one: the jiggler (or a person).
                            let prev_x =
                                if i % 2 == 0 { (0.75 * w as f32) as i32 } else { (0.25 * w as f32) as i32 };
                            if (p.x - prev_x).abs() > tol || (p.y - ty).abs() > tol {
                                stray += u32::from(i >= 5);
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => anyhow::bail!("session closed: {:?}", s.error()),
                    Err(_) => break,
                }
            }
            if i >= 5 {
                match seen {
                    Some(at) => lat.push(ms(at - sent)),
                    None => missed += 1,
                }
            }
            tokio::time::sleep(SPACING).await;
        }
        s.close().await;
        Ok::<_, anyhow::Error>((lat, missed, stray, w, h))
    });
    let _ = agent.finish(&agent_rt, Duration::from_secs(3));
    agent_rt.shutdown_timeout(Duration::from_secs(2));
    let (lat, missed, stray, w, h) = res?;
    r.param("moves", moves as u64);
    r.param("agent_entry", AgentEntry::Reverse.name());
    r.param("picture", format!("{w}x{h}"));
    r.param("spacing_ms", SPACING.as_millis() as u64);
    ms_events(r, "input_to_cursor_ms", Gate::MacroLatency, &lat);
    r.metric("missed", Metric::single("1", Better::Lower, Gate::Info, missed as f64));
    r.metric("stray_positions", Metric::single("1", Better::Lower, Gate::Info, stray as f64));
    if lat.is_empty() {
        anyhow::bail!("no move came back as CursorPosition ({missed} missed)");
    }
    if missed as usize > moves / 10 {
        r.status = Status::Noisy;
        r.note(format!("{missed} of {moves} moves not seen back within {TIMEOUT:?} (jiggler or capture)"));
    }
    Ok(())
}
