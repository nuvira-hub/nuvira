//! Appunti condivisi durante la sessione (per ora solo testo).
//!
//! Viewer e agent controllano a intervalli se il proprio sistema ha testo nuovo
//! negli appunti e lo inviano all'altra parte, che lo scrive nei suoi. Il testo
//! ricevuto non viene rimandato indietro (niente ping-pong).
//!
//! Su Windows il cambio si riconosce dal numero di sequenza degli appunti, senza
//! leggerli; altrove si confronta il testo.

use anyhow::{Context, Result};
use tracing::debug;

/// Testo più lungo di così non viene condiviso (resta solo negli appunti locali).
pub const MAX_TEXT_BYTES: usize = 1 << 20;

pub struct Clipboard {
    inner: arboard::Clipboard,
    /// Ultimo stato visto: numero di sequenza (Windows) o testo.
    last_seq: u32,
    last_text: Option<String>,
}

impl Clipboard {
    /// Il testo già presente all'apertura non viene inviato: conta solo quello copiato dopo.
    pub fn new() -> Result<Self> {
        let mut inner = arboard::Clipboard::new().context("appunti non disponibili")?;
        let last_text = inner.get_text().ok();
        Ok(Self { inner, last_seq: sequence(), last_text })
    }

    /// Testo copiato localmente dall'ultima chiamata, se c'è.
    pub fn poll(&mut self) -> Option<String> {
        if cfg!(windows) {
            let seq = sequence();
            if seq == self.last_seq {
                return None;
            }
            self.last_seq = seq;
        }
        // Errori normali: appunti occupati da un altro programma, contenuto non testuale.
        let text = self.inner.get_text().ok()?;
        if self.last_text.as_ref() == Some(&text) {
            return None;
        }
        self.last_text = Some(text.clone());
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            debug!(bytes = text.len(), "testo degli appunti non condiviso");
            return None;
        }
        Some(text)
    }

    /// Scrive il testo ricevuto dall'altra parte, senza poi segnalarlo come nuovo.
    pub fn set(&mut self, text: String) -> Result<()> {
        self.inner.set_text(text.clone()).context("scrittura degli appunti")?;
        self.last_seq = sequence();
        self.last_text = Some(text);
        Ok(())
    }
}

/// Thread che controlla gli appunti ogni 400 ms. Il testo copiato localmente va a
/// `outgoing` (se restituisce `false` il thread termina); il testo inviato sul canale
/// restituito viene scritto negli appunti. Chiudendo il canale il thread termina.
pub fn spawn_sync(
    mut outgoing: impl FnMut(String) -> bool + Send + 'static,
) -> Result<std::sync::mpsc::Sender<String>> {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let (incoming_tx, incoming_rx) = channel::<String>();
    let (ready_tx, ready_rx) = channel();
    std::thread::Builder::new().name("appunti".into()).spawn(move || {
        // Gli appunti si aprono nel thread che li usa.
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
    ready_rx.recv().context("thread degli appunti terminato")??;
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
