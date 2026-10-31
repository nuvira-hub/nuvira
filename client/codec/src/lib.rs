//! Video encoding and decoding.
//!
//! The rest of the client sees only the [`VideoEncoder`] and [`VideoDecoder`] traits:
//! - `mf`: H.264 with the graphics card's encoder through Media Foundation (Windows);
//! - `mfdec`: H.264 decoding on the graphics card through Media Foundation (Windows, opt-in);
//! - `openh264`: software H.264, available everywhere (fallback);
//! - VideoToolbox (macOS), VAAPI (Linux): next steps.
//!
//! Intermediate format: YUV 4:2:0 BT.601 limited range (I420 for OpenH264,
//! NV12 for hardware encoders), see [`convert`].

pub mod convert;
#[cfg(windows)]
pub mod gpu;
#[cfg(windows)]
mod mf;
#[cfg(windows)]
mod mfdec;
mod openh264;
mod select;

#[cfg(windows)]
pub use crate::mf::{MfEncoder, hardware_encoders};
#[cfg(windows)]
pub use crate::mfdec::MfDecoder;
pub use crate::openh264::{OpenH264Decoder, OpenH264Encoder};

/// BGRA image to encode (as it comes from capture).
pub struct BgraImage<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: usize,
    pub data: &'a [u8],
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Decoded I420 image with packed planes (stride = plane width).
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

    /// Encodes an image. `None` if the encoder decides to skip the frame.
    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> anyhow::Result<Option<EncodedFrame>>;

    /// Encodes a frame already on the GPU (only encoders with [`accepts_textures`](Self::accepts_textures)).
    #[cfg(windows)]
    fn encode_texture(
        &mut self,
        _texture: &gpu::GpuTexture,
        _width: u32,
        _height: u32,
        _force_keyframe: bool,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        anyhow::bail!("this encoder does not accept GPU textures")
    }

    /// The encoder wants frames as GPU textures instead of in RAM.
    fn accepts_textures(&self) -> bool {
        false
    }

    /// Changes the bitrate during the session (adaptive bitrate).
    fn set_bitrate(&mut self, _bps: u32) {}

    /// Waits (at most `timeout`) until the encoder can take a frame right away.
    /// The caller can then pick the most recent image only when needed, instead
    /// of letting it age inside `encode`. `true` if it is ready.
    fn wait_ready(&mut self, _timeout: std::time::Duration) -> anyhow::Result<bool> {
        Ok(true)
    }
}

pub trait VideoDecoder: Send {
    fn name(&self) -> &'static str;

    /// Decodes a frame. `None` if the decoder has no image to show yet.
    fn decode(&mut self, data: &[u8]) -> anyhow::Result<Option<&YuvFrame>>;
}

/// Which encoder to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderChoice {
    /// The fastest of hardware and software, tried at start-up.
    #[default]
    Auto,
    Hardware,
    Software,
}

/// How to encode, decided once per process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPlan {
    /// OpenH264 on the CPU.
    Software,
    /// Graphics card encoder, image passed through RAM.
    Hardware,
    /// Graphics card encoder, image staying on the GPU from capture onwards.
    HardwareGpu,
}

/// Graphics device shared between capture and encoder (Windows only).
#[cfg(not(windows))]
#[derive(Clone)]
pub struct GpuDevice(());
#[cfg(windows)]
pub use gpu::{GpuDevice, GpuTexture};

/// Turns the user's choice into a plan. With `Auto` it tries the available encoders
/// (about two seconds, only once: better called at start-up).
pub fn plan_encoder(choice: EncoderChoice, settings: EncoderSettings) -> EncoderPlan {
    match choice {
        EncoderChoice::Auto => select::auto_plan(settings),
        EncoderChoice::Hardware if cfg!(windows) => EncoderPlan::HardwareGpu,
        EncoderChoice::Hardware => EncoderPlan::Hardware,
        EncoderChoice::Software => EncoderPlan::Software,
    }
}

/// Creates the plan's encoder. `HardwareGpu` needs the capture device; without
/// it, input from RAM is used instead.
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
            tracing::info!("hardware encoder: {}", enc.device_name());
            Ok(Box::new(enc))
        }
        #[cfg(not(windows))]
        EncoderPlan::Hardware | EncoderPlan::HardwareGpu => {
            let _ = gpu;
            anyhow::bail!("hardware encoder not available on this system yet")
        }
        EncoderPlan::Software => Ok(Box::new(OpenH264Encoder::new(settings)?)),
    }
}

/// H.264 decoder for this system: OpenH264, or the graphics card's with
/// `NUVIRA_DECODER=hardware` (slower for now, see the `mfdec` module).
pub fn best_decoder() -> anyhow::Result<Box<dyn VideoDecoder>> {
    #[cfg(windows)]
    if std::env::var("NUVIRA_DECODER").as_deref() == Ok("hardware") {
        match MfDecoder::new() {
            Ok(d) => return Ok(Box::new(d)),
            Err(e) => tracing::info!("hardware decoder not available ({e:#}): OpenH264"),
        }
    }
    software_decoder()
}

/// OpenH264, also the fallback when the hardware decoder fails during a session.
pub fn software_decoder() -> anyhow::Result<Box<dyn VideoDecoder>> {
    Ok(Box::new(OpenH264Decoder::new()?))
}
