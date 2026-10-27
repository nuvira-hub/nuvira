//! Clipboard shared during the session (text only for now).
//!
//! Viewer and agent check at intervals whether their system has new text on the
//! clipboard and send it to the other side, which writes it to its own. Received
//! text is not sent back (no ping-pong).
//!
//! On Windows a change is detected from the clipboard sequence number, without
//! reading it; elsewhere the text is compared.

use anyhow::{Context, Result};
use tracing::debug;

/// Longer text is not shared (it stays only on the local clipboard).
pub const MAX_TEXT_BYTES: usize = 1 << 20;

pub struct Clipboard {
    inner: arboard::Clipboard,
    /// Last state seen: sequence number (Windows) or text.
    last_seq: u32,
    last_text: Option<String>,
}

impl Clipboard {
    /// Text already present when opened is not sent: only what is copied afterwards counts.
    pub fn new() -> Result<Self> {
        let mut inner = arboard::Clipboard::new().context("clipboard not available")?;
        let last_text = inner.get_text().ok();
        Ok(Self { inner, last_seq: sequence(), last_text })
    }

    /// Text copied locally since the last call, if any.
    pub fn poll(&mut self) -> Option<String> {
        if cfg!(windows) {
            let seq = sequence();
            if seq == self.last_seq {
                return None;
            }
            self.last_seq = seq;
        }
        // Normal errors: clipboard held by another program, non-text content.
        let text = self.inner.get_text().ok()?;
        if self.last_text.as_ref() == Some(&text) {
            return None;
        }
        self.last_text = Some(text.clone());
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            debug!(bytes = text.len(), "clipboard text not shared");
            return None;
        }
        Some(text)
    }

    /// Writes text received from the other side, without reporting it as new afterwards.
    pub fn set(&mut self, text: String) -> Result<()> {
        self.inner.set_text(text.clone()).context("writing the clipboard")?;
        self.last_seq = sequence();
        self.last_text = Some(text);
        Ok(())
    }
}

/// Thread that checks the clipboard every 400 ms. Text copied locally goes to
/// `outgoing` (if it returns `false` the thread ends); text sent on the returned
/// channel is written to the clipboard. Closing the channel ends the thread.
pub fn spawn_sync(
    mut outgoing: impl FnMut(String) -> bool + Send + 'static,
) -> Result<std::sync::mpsc::Sender<String>> {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let (incoming_tx, incoming_rx) = channel::<String>();
    let (ready_tx, ready_rx) = channel();
    std::thread::Builder::new().name("appunti".into()).spawn(move || {
        // The clipboard is opened in the thread that uses it.
        let mut clipboard = match Clipboard::new() {
            Ok(c) => {
                let _ = ready_tx.send(Ok(()));
                c
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        loop {
            match incoming_rx.recv_timeout(std::time::Duration::from_millis(400)) {
                Ok(text) => {
                    if let Err(e) = clipboard.set(text) {
                        debug!("{e:#}");
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(text) = clipboard.poll()
                        && !outgoing(text)
                    {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    })?;
    ready_rx.recv().context("clipboard thread ended")??;
    Ok(incoming_tx)
}

#[cfg(windows)]
fn sequence() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
fn sequence() -> u32 {
    0
}
