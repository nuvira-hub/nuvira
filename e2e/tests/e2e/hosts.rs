//! Child roles of the test binary, started by cases that kill a side abruptly:
//!
//! - `--e2e-agent-host ARGS`: the bench's `agent-host` (an in-process agent on the
//!   synthetic source, `nuvira_bench::net::cli_agent_host`): prints
//!   `agent <addr> <fingerprint> <password>`;
//! - `--e2e-viewer-host direct ADDR FP PASSWORD`: a headless viewer connected
//!   directly (127.0.0.1-bound) to an agent;
//! - `--e2e-viewer-host server ADDR FP WS DEVICE_ID PASSWORD DIR [udp|websocket]`:
//!   a headless viewer through a local server (identity in DIR).
//!
//! A viewer host prints `frames <n>` once it decoded frames, then `alive` every
//! 500 ms, until it is killed or its session fails (`failed <reason>`, exit 1).

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nuvira_e2e::rig;
use nuvira_transport::signal::TransportMode;

pub fn dispatch(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("--e2e-agent-host") => Some(match nuvira_bench::net::cli_agent_host(&args[1..]) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("agent-host: {e:#}");
                1
            }
        }),
        Some("--e2e-viewer-host") => Some(match viewer_host(&args[1..]) {
            Ok(()) => 0,
            Err(e) => {
                println!("failed {e:#}");
                1
            }
        }),
        _ => None,
    }
}

fn viewer_host(args: &[String]) -> Result<()> {
    let rt = nuvira_bench::net::viewer_runtime(2)?;
    rt.block_on(async {
        let s = match args.first().map(String::as_str) {
            Some("direct") => {
                let [_, addr, fp, password] = args else { bail!("direct ADDR FP PASSWORD") };
                let fp = nuvira_transport::parse_fingerprint(fp)?;
                rig::HeadlessViewer::connect_direct(addr.parse()?, Some(fp), rig::opts(password)).await?
            }
            Some("server") => {
                let [_, addr, fp, ws, id, password, dir, rest @ ..] = args else {
                    bail!("server ADDR FP WS DEVICE_ID PASSWORD DIR [udp|websocket]")
                };
                let srv = rig::Srv {
                    addr: addr.parse()?,
                    fingerprint: nuvira_transport::parse_fingerprint(fp)?,
                    ws: ws.clone(),
                };
                let mode = match rest.first().map(String::as_str) {
                    Some("websocket") => TransportMode::WebSocket,
                    _ => TransportMode::Udp,
                };
                let identity = rig::identity(std::path::Path::new(dir))?;
                let (conn, _) = rig::connect_via(&srv, &identity, id, mode).await?;
                rig::HeadlessViewer::from_connection(conn, rig::opts(password)).await?
            }
            _ => bail!("direct|server ..."),
        };
        let n = rig::frames(&s, 10, Duration::from_secs(20)).await.context("frames")?;
        println!("frames {n}");
        std::io::stdout().flush()?;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Some(e) = s.error() {
                bail!("{e}");
            }
            println!("alive");
            std::io::stdout().flush()?;
        }
    })
}
