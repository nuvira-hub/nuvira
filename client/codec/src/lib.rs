//! Video encoding and decoding.
//!
//! The rest of the client sees only the [`VideoEncoder`] and [`VideoDecoder`] traits:
//! - `mf`: H.264 with the graphics card's encoder through Media Foundation (Windows);
//! - `mfdec`: H.264 decoding on the graphics card through Media Foundation (Windows, opt-in);
//! - `vt`: H.264 with VideoToolbox (macOS);
//! - `openh264`: software H.264, available everywhere (fallback);
//! - `vaapi`: H.264 with VA-API, Intel and AMD graphics cards (Linux);
//! - `nvenc`: H.264 with NVENC, NVIDIA graphics cards (Linux; preferred to VA-API);
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
#[cfg(target_os = "linux")]
mod nvenc;
#[cfg(target_os = "linux")]
#[rustfmt::skip]
#[allow(dead_code)]
mod nvenc_layout;
mod openh264;
mod select;
#[cfg(target_os = "linux")]
mod vaapi;
#[cfg(target_os = "macos")]
mod vt;

#[cfg(windows)]
pub use crate::mf::{MfEncoder, hardware_encoders};
#[cfg(windows)]
pub use crate::mfdec::MfDecoder;
#[cfg(target_os = "linux")]
pub use crate::nvenc::NvencEncoder;
pub use crate::openh264::{OpenH264Decoder, OpenH264Encoder};
#[cfg(target_os = "linux")]
pub use crate::vaapi::VaapiEncoder;
#[cfg(target_os = "macos")]
pub use crate::vt::VtEncoder;

/// BGRA image to encode (as it comes from capture).
pub struct BgraImage<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: usize,
    pub data: &'a [u8],
}

#[derive(Default)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Kept as long-term reference in this slot (see [`EncoderSettings::ltr`]).
    pub ltr_mark: Option<u8>,
    /// Encoded referring only to the long-term reference in this slot.
    pub ltr_ref: Option<u8>,
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
    /// Long-term reference frames ([`LTR_SLOTS`]): after a loss the next frame can
    /// refer to one the viewer has instead of being a keyframe. Only encoders that
    /// support them use it (see [`VideoEncoder::use_ltr`]).
    pub ltr: bool,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self { bitrate_bps: 8_000_000, max_fps: 60, ltr: false }
    }
}

/// Long-term reference slots, with [`EncoderSettings::ltr`].
pub const LTR_SLOTS: u8 = 2;

/// A picture in a DMA-BUF (Linux), as the compositor shares it: one plane of a
/// 32-bit RGB format.
#[cfg(target_os = "linux")]
pub struct DmaBufImage {
    pub fd: std::os::fd::RawFd,
    pub width: u32,
    pub height: u32,
    /// DRM fourcc (XRGB8888, ARGB8888, XBGR8888, ABGR8888).
    pub drm_format: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
    pub size: u32,
}

pub trait VideoEncoder: Send {
    fn name(&self) -> &'static str;

    /// Encodes an image. `None` if the encoder decides to skip the frame.
    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> anyhow::Result<Option<EncodedFrame>>;

    /// Encodes a frame already on the GPU (only encoders with [`accepts_textures`](Self::accepts_textures)):
    /// `input` is the size of the texture, `output` the size to encode (smaller = scaled down).
    #[cfg(windows)]
    fn encode_texture(
        &mut self,
        _texture: &gpu::GpuTexture,
        _input: (u32, u32),
        _output: (u32, u32),
        _force_keyframe: bool,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        anyhow::bail!("this encoder does not accept GPU textures")
    }

    /// The encoder wants frames as GPU textures instead of in RAM.
    fn accepts_textures(&self) -> bool {
        false
    }

    /// Encodes a picture left in a DMA-BUF, reduced to `size`, with no copy through
    /// RAM (VA-API with video processing). `None` when it cannot: the caller reads
    /// the buffer into RAM and calls [`encode`](Self::encode).
    #[cfg(target_os = "linux")]
    fn encode_dmabuf(
        &mut self,
        _image: &DmaBufImage,
        _size: (u32, u32),
        _force_keyframe: bool,
    ) -> Option<anyhow::Result<Option<EncodedFrame>>> {
        None
    }

    /// Changes the bitrate during the session (adaptive bitrate).
    fn set_bitrate(&mut self, _bps: u32) {}

    /// The next frame is kept as long-term reference in `slot` (replacing the one
    /// there). `false` if the encoder has no long-term references.
    fn mark_ltr(&mut self, _slot: u8) -> bool {
        false
    }

    /// The next frame refers only to the long-term reference in `slot` (recovery
    /// after a loss, instead of a keyframe). `false` if the encoder cannot.
    fn use_ltr(&mut self, _slot: u8) -> bool {
        false
    }

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
        #[cfg(target_os = "macos")]
        EncoderPlan::Hardware | EncoderPlan::HardwareGpu => {
            let _ = gpu;
            Ok(Box::new(VtEncoder::new(settings, true)?))
        }
        // NVENC where there is an NVIDIA card with its driver, else VA-API;
        // `NUVIRA_HW_ENCODER=vaapi` or `nvenc` keeps to one.
        #[cfg(target_os = "linux")]
        EncoderPlan::Hardware | EncoderPlan::HardwareGpu => {
            let _ = gpu;
            let only = std::env::var("NUVIRA_HW_ENCODER").unwrap_or_default();
            let nvenc_error = if only == "vaapi" {
                None
            } else {
                match NvencEncoder::new(settings) {
                    Ok(enc) => {
                        tracing::info!("hardware encoder: NVENC on {}", enc.device_name());
                        return Ok(Box::new(enc));
                    }
                    Err(e) if only == "nvenc" => return Err(e),
                    Err(e) => Some(e),
                }
            };
            match VaapiEncoder::new(settings) {
                Ok(enc) => Ok(Box::new(enc)),
                Err(e) => match nvenc_error {
                    Some(nv) => Err(anyhow::anyhow!("NVENC: {nv:#}; VA-API: {e:#}")),
                    None => Err(e),
                },
            }
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
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
