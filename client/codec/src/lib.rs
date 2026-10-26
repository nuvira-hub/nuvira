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
mod mf;
mod openh264;

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
}

pub trait VideoDecoder: Send {
    fn name(&self) -> &'static str;

    /// Decomprime un frame. `None` se il decoder non ha ancora un'immagine da mostrare.
    fn decode(&mut self, data: &[u8]) -> anyhow::Result<Option<&YuvFrame>>;
}

/// Quale encoder usare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderChoice {
    /// Hardware se disponibile, altrimenti software.
    #[default]
    Auto,
    Hardware,
    Software,
}

/// L'encoder H.264 scelto; con `Auto` quello hardware se c'è, altrimenti OpenH264.
pub fn create_encoder(
    choice: EncoderChoice,
    settings: EncoderSettings,
) -> anyhow::Result<Box<dyn VideoEncoder>> {
    #[cfg(windows)]
    if choice != EncoderChoice::Software {
        match MfEncoder::new(settings) {
            Ok(enc) => {
                tracing::info!("encoder hardware: {}", enc.device_name());
                return Ok(Box::new(enc));
            }
            Err(e) if choice == EncoderChoice::Hardware => return Err(e),
            Err(e) => tracing::info!("encoder hardware non disponibile ({e:#}), uso OpenH264"),
        }
    }
    #[cfg(not(windows))]
    if choice == EncoderChoice::Hardware {
        anyhow::bail!("encoder hardware non ancora disponibile su questo sistema");
    }
    Ok(Box::new(OpenH264Encoder::new(settings)?))
}

/// Il miglior decoder H.264 disponibile su questo sistema.
pub fn best_decoder() -> anyhow::Result<Box<dyn VideoDecoder>> {
    Ok(Box::new(OpenH264Decoder::new()?))
}
