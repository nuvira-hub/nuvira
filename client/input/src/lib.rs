//! Simulazione dell'input ricevuto dal viewer.
//!
//! - Windows: SendInput (`windows.rs`). Il desktop sicuro (UAC, login) richiede
//!   il servizio di sistema della fase 3.
//! - macOS: CGEvent; Linux: portale RemoteDesktop / XTest (fase 4).

use nexodesk_proto::v1::InputEvent;

pub mod keys;
#[cfg(windows)]
mod windows;

/// Area dello schermo trasmesso nel desktop virtuale, in pixel fisici.
#[derive(Debug, Clone, Copy)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

pub trait InputInjector: Send {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;

    /// Rilascia tasti e pulsanti ancora premuti (fine sessione, connessione persa).
    fn release_all(&mut self);

    /// Lo schermo trasmesso è cambiato: le coordinate del mouse si riferiscono a questo.
    fn set_screen(&mut self, screen: ScreenRect);
}

/// Apre l'iniezione di input sullo schermo indicato.
pub fn open(screen: ScreenRect) -> anyhow::Result<Box<dyn InputInjector>> {
    #[cfg(windows)]
    return Ok(Box::new(windows::SendInputInjector::new(screen)));
    #[cfg(not(windows))]
    {
        let _ = screen;
        anyhow::bail!("controllo remoto dell'input non ancora disponibile su questo sistema");
    }
}

/// Rende il processo consapevole dei DPI per monitor: coordinate in pixel fisici,
/// coerenti con quelle della cattura. Va chiamata all'avvio, prima di creare finestre.
pub fn set_dpi_awareness() {
    #[cfg(windows)]
    windows::set_dpi_awareness();
}
