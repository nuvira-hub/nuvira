//! Remote terminal: a bidirectional stream of the viewer for each terminal
//! (`TerminalMessage`), connected to a shell in a pseudo-terminal (`pty`).
//!
//! The stream starts with `StreamOpen.terminal` (size, read by the caller); then the keys go to the shell, its output
//! comes back as `data`, and `exit` closes it. If the viewer closes the stream the
//! shell is ended with everything it started.

use std::io::{Read, Write};

use anyhow::Result;
use nuvira_proto::v1::{TerminalExit, TerminalMessage, TerminalSize, terminal_message::Msg};
use nuvira_transport::{FramedRecv, FramedSend};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

/// Output read from the shell in pieces of at most this size.
const CHUNK: usize = 16 * 1024;

/// Serves a terminal stream. `refused` is the reason ("@key") when terminals are not
/// allowed: the viewer gets it in `exit` without a shell starting. When `stop` ends
/// (the person at the PC switched terminals off) the shell is ended.
pub async fn serve(
    mut tx: FramedSend,
    mut rx: FramedRecv,
    size: TerminalSize,
    refused: Option<&'static str>,
    stop: impl std::future::Future<Output = ()>,
) -> Result<()> {
    if let Some(reason) = refused {
        return exit(&mut tx, 0, reason).await;
    }
    let (cols, rows) = (size.cols.clamp(1, 1000) as u16, size.rows.clamp(1, 1000) as u16);
    let spawned = tokio::task::spawn_blocking(move || crate::pty::spawn(cols, rows)).await?;
    let (shell, mut reader) = match spawned {
        Ok(spawned) => spawned,
        Err(e) => {
            warn!("terminal not started: {e:#}");
            let reason = match e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@')).as_deref() {
                // Nobody signed in on the PC: no user to run the shell as.
                Some("@agent.no_user") => "@agent.terminal_no_user",
                _ => "@agent.terminal_failed",
            };
            return exit(&mut tx, 0, reason).await;
        }
    };
    info!(cols, rows, "terminal opened");
    let process = shell.process.clone();

    // Output: a thread reads the shell; a full channel slows it down (back-pressure).
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(16);
    std::thread::Builder::new().name("terminal-out".into()).spawn(move || {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    })?;
    // Keys: written by a thread of their own, so a busy shell does not hold up the stream.
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut writer = shell.writer;
    std::thread::Builder::new().name("terminal-in".into()).spawn(move || {
        for data in in_rx {
            if writer.write_all(&data).is_err() {
                break;
            }
        }
    })?;
    let (code_tx, mut code_rx) = oneshot::channel::<i32>();
    {
        let process = process.clone();
        std::thread::Builder::new().name("terminal-wait".into()).spawn(move || {
            let _ = code_tx.send(process.wait());
        })?;
    }

    tokio::pin!(stop);
    let result = async {
        loop {
            tokio::select! {
                () = &mut stop => {
                    info!("terminal ended: switched off by the person at the PC");
                    process.kill();
                    return exit(&mut tx, 0, "@agent.terminal_switched_off").await;
                }
                msg = rx.recv::<TerminalMessage>() => match msg {
                    Ok(Some(TerminalMessage { msg: Some(Msg::Data(data)) })) => {
                        let _ = in_tx.send(data);
                    }
                    Ok(Some(TerminalMessage { msg: Some(Msg::Resize(size)) })) => {
                        process.resize(size.cols.clamp(1, 1000) as u16, size.rows.clamp(1, 1000) as u16);
                    }
                    Ok(Some(_)) => {}
                    // The viewer closed the terminal.
                    Ok(None) | Err(_) => {
                        info!("terminal closed by the viewer");
                        process.kill();
                        return Ok(());
                    }
                },
                out = out_rx.recv() => match out {
                    Some(data) => {
                        let msg = TerminalMessage { msg: Some(Msg::Data(data)) };
                        tx.send(&msg).await?;
                    }
                    // The output is over: the shell ended.
                    None => {
                        let code = (&mut code_rx).await.unwrap_or(1);
                        info!(code, "terminal ended");
                        return exit(&mut tx, code, "").await;
                    }
                },
            }
        }
    }
    .await;
    // Whatever happened, no shell stays behind.
    process.kill();
    result
}

async fn exit(tx: &mut FramedSend, code: i32, error: &str) -> Result<()> {
    let msg = TerminalMessage { msg: Some(Msg::Exit(TerminalExit { code, error: error.into() })) };
    // Dropping the stream afterwards finishes it.
    tx.send(&msg).await
}
