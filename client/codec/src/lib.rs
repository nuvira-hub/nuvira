//! Compressione e decompressione video.
//!
//! Il resto del client vede solo i trait [`VideoEncoder`] e [`VideoDecoder`]:
//! - `mf`: H.264 con l'encoder della scheda video tramite Media Foundation (Windows);
//! - `openh264`: H.264 software, disponibile ovunque (ripiego);
//! - VideoToolbox (macOS), VAAPI (Linux) e la decodifica hardware: prossimi passi.
//!
//! Formato intermedio: YUV 4:2:0 BT.601 a gamma limitata (I420 per OpenH264,
//! NV12 per gli encoder hardware), vedi [`convert`].

pub mod convert;
#[cfg(windows)]
pub mod gpu;
#[cfg(windows)]
mod mf;
mod openh264;
mod select;

#[cfg(windows)]
pub use crate::mf::{MfEncoder, hardware_encoders};
pub use crate::openh264::{OpenH264Decoder, OpenH264Encoder};

/// Immagine BGRA da comprimere (come esce dalla cattura).
pub struct BgraImage<'a> {
    pub width: u32,
    pub height: u32,
    /// Byte per riga.
    pub stride: usize,
    pub data: &'a [u8],
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Immagine decompressa in I420 con piani compatti (stride = larghezza del piano).
#[derive(Default)]
pub struct YuvFrame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: u32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self { bitrate_bps: 8_000_000, max_fps: 60 }
    }
}

pub trait VideoEncoder: Send {
    fn name(&self) -> &'static str;

    /// Comprime un'immagine. `None` se l'encoder decide di saltare il frame.
    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> anyhow::Result<Option<EncodedFrame>>;

    /// Comprime un frame che è già sulla GPU (solo encoder con [`accepts_textures`](Self::accepts_textures)).
    #[cfg(windows)]
    fn encode_texture(
        &mut self,
        _texture: &gpu::GpuTexture,
        _width: u32,
        _height: u32,
        _force_keyframe: bool,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        anyhow::bail!("questo encoder non accetta texture GPU")
    }

    /// L'encoder vuole i frame come texture GPU invece che in RAM.
    fn accepts_textures(&self) -> bool {
        false
    }

    /// Attende (al massimo `timeout`) che l'encoder possa ricevere subito un frame.
    /// Chi chiama può così scegliere l'immagine più recente solo quando serve,
    /// invece di farla invecchiare dentro `encode`. `true` se è pronto.
    fn wait_ready(&mut self, _timeout: std::time::Duration) -> anyhow::Result<bool> {
        Ok(true)
    }
}

pub trait VideoDecoder: Send {
    fn name(&self) -> &'static str;

    /// Decomprime un frame. `None` se il decoder non ha ancora un'immagine da mostrare.
    fn decode(&mut self, data: &[u8]) -> anyhow::Result<Option<&YuvFrame>>;
}

/// Quale encoder usare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderChoice {
    /// Il più veloce tra hardware e software, provati all'avvio.
    #[default]
    Auto,
    Hardware,
    Software,
}

/// Come comprimere, deciso una volta per processo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPlan {
    /// OpenH264 sulla CPU.
    Software,
    /// Encoder della scheda video, immagine passata dalla RAM.
    Hardware,
    /// Encoder della scheda video, immagine che resta sulla GPU dalla cattura in poi.
    HardwareGpu,
}

/// Device grafico condiviso tra cattura ed encoder (solo Windows).
#[cfg(not(windows))]
#[derive(Clone)]
pub struct GpuDevice(());
#[cfg(windows)]
pub use gpu::{GpuDevice, GpuTexture};

/// Traduce la scelta dell'utente in un piano. Con `Auto` prova gli encoder disponibili
/// (circa due secondi, una volta sola: conviene chiamarla all'avvio).
pub fn plan_encoder(choice: EncoderChoice, settings: EncoderSettings) -> EncoderPlan {
    match choice {
        EncoderChoice::Auto => select::auto_plan(settings),
        EncoderChoice::Hardware if cfg!(windows) => EncoderPlan::HardwareGpu,
        EncoderChoice::Hardware => EncoderPlan::Hardware,
        EncoderChoice::Software => EncoderPlan::Software,
    }
}

/// Crea l'encoder del piano. Per `HardwareGpu` serve il device della cattura;
/// senza, si ripiega sull'ingresso dalla RAM.
pub fn create_encoder(
    plan: EncoderPlan,
    settings: EncoderSettings,
    gpu: Option<GpuDevice>,
) -> anyhow::Result<Box<dyn VideoEncoder>> {
    match plan {
        #[cfg(windows)]
        EncoderPlan::Hardware | EncoderPlan::HardwareGpu => {
            let enc = match (plan, gpu) {
                (EncoderPlan::HardwareGpu, Some(device)) => MfEncoder::with_device(settings, device)?,
                _ => MfEncoder::new(settings)?,
            };
            tracing::info!("encoder hardware: {}", enc.device_name());
            Ok(Box::new(enc))
        }
        #[cfg(not(windows))]
        EncoderPlan::Hardware | EncoderPlan::HardwareGpu => {
            let _ = gpu;
            anyhow::bail!("encoder hardware non ancora disponibile su questo sistema")
        }
        EncoderPlan::Software => Ok(Box::new(OpenH264Encoder::new(settings)?)),
    }
}

/// Il miglior decoder H.264 disponibile su questo sistema.
pub fn best_decoder() -> anyhow::Result<Box<dyn VideoDecoder>> {
    Ok(Box::new(OpenH264Decoder::new()?))
}
