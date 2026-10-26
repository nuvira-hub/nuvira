//! Cattura dello schermo.
//!
//! Ogni sistema ha la propria implementazione dietro il trait [`Capturer`]:
//! - Windows: DXGI Desktop Duplication (`windows.rs`);
//! - macOS: ScreenCaptureKit (fase 4);
//! - Linux: PipeWire / X11 (fase 4).

use std::time::Duration;

mod synthetic;
#[cfg(windows)]
mod windows;

pub use synthetic::SyntheticCapturer;

/// Uno schermo collegato al sistema.
#[derive(Debug, Clone)]
pub struct Display {
    pub index: usize,
    pub name: String,
    /// Posizione nel desktop virtuale, in pixel fisici.
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Immagine catturata in formato BGRA 8 bit.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Byte per riga (può essere maggiore di `width * 4`).
    pub stride: usize,
    pub data: Vec<u8>,
    /// Orologio di sistema in microsecondi al momento della cattura.
    pub captured_us: u64,
}

pub trait Capturer: Send {
    fn display(&self) -> &Display;

    /// Attende un'immagine nuova fino a `timeout`. `None` se lo schermo non è cambiato.
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<&Frame>>;

    /// Ultima immagine catturata (per ricomprimerla quando serve un keyframe a schermo fermo).
    fn last_frame(&self) -> Option<&Frame>;
}

/// Elenco degli schermi disponibili.
pub fn displays() -> anyhow::Result<Vec<Display>> {
    #[cfg(windows)]
    return windows::displays();
    #[cfg(not(windows))]
    anyhow::bail!("cattura schermo non ancora disponibile su questo sistema");
}

/// Apre la cattura dello schermo indicato (quello principale se `None`).
pub fn open(index: Option<usize>) -> anyhow::Result<Box<dyn Capturer>> {
    #[cfg(windows)]
    return Ok(Box::new(windows::DxgiCapturer::new(index)?));
    #[cfg(not(windows))]
    {
        let _ = index;
        anyhow::bail!("cattura schermo non ancora disponibile su questo sistema");
    }
}
