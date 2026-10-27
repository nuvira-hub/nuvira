//! Screen capture.
//!
//! Each system has its own implementation behind the [`Capturer`] trait:
//! - Windows: DXGI Desktop Duplication (`windows.rs`);
//! - macOS: ScreenCaptureKit (phase 4);
//! - Linux: PipeWire / X11 (phase 4).

use std::time::Duration;

mod all;
#[cfg(windows)]
pub mod desktop;
mod synthetic;
#[cfg(windows)]
mod windows;

pub use all::{ALL_DISPLAYS, AllCapturer};
pub use synthetic::SyntheticCapturer;

/// A screen connected to the system.
#[derive(Debug, Clone)]
pub struct Display {
    pub index: usize,
    pub name: String,
    /// Position in the virtual desktop, in physical pixels.
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Captured image in 8-bit BGRA format.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (may be larger than `width * 4`).
    pub stride: usize,
    pub data: Vec<u8>,
    /// System clock in microseconds at capture time.
    pub captured_us: u64,
    /// GPU mode (Windows): the image is in this BGRA texture and `data` is empty.
    #[cfg(windows)]
    pub texture: Option<::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
}

pub trait Capturer: Send {
    fn display(&self) -> &Display;

    /// Waits up to `timeout` for a new image. `None` if the screen did not change.
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<&Frame>>;

    /// Last captured image (to encode it again when a keyframe is needed on a still screen).
    fn last_frame(&self) -> Option<&Frame>;

    /// Switches to GPU mode: frames stay in Direct3D 11 textures on this device,
    /// to be shared with the hardware encoder. `None` if the source does not support it.
    #[cfg(windows)]
    fn enable_gpu_output(&mut self) -> Option<::windows::Win32::Graphics::Direct3D11::ID3D11Device> {
        None
    }
}

/// List of the available screens.
pub fn displays() -> anyhow::Result<Vec<Display>> {
    #[cfg(windows)]
    return windows::displays();
    #[cfg(not(windows))]
    anyhow::bail!("screen capture is not available on this system yet");
}

/// Opens capture of the given screen (the primary one if `None`, all of them with
/// [`ALL_DISPLAYS`]).
pub fn open(index: Option<usize>) -> anyhow::Result<Box<dyn Capturer>> {
    #[cfg(windows)]
    {
        if index == Some(ALL_DISPLAYS) {
            let screens = displays()?;
            let open = |i| -> anyhow::Result<Box<dyn Capturer>> {
                Ok(Box::new(windows::DxgiCapturer::new(Some(i))?))
            };
            return Ok(Box::new(AllCapturer::new(open, &screens)?));
        }
        Ok(Box::new(windows::DxgiCapturer::new(index)?))
    }
    #[cfg(not(windows))]
    {
        let _ = index;
        anyhow::bail!("screen capture is not available on this system yet");
    }
}
