//! macOS capture: ScreenCaptureKit (`SCStream`) with BGRA frames and the cursor
//! drawn in. macOS gives the images only once the person allowed NuviraHub in
//! System Settings → Privacy & Security → Screen Recording.
//!
//! The frames arrive on a queue of ScreenCaptureKit: the handler copies them
//! into a channel of one image (a newer one replaces what the capture did not
//! take yet).

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use core_graphics::display::CGDisplay;
use screencapturekit::cm::{CMSampleBufferExt, CMTime};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use tracing::info;

use crate::{Capturer, Display, Frame};

/// Pixels per point of a display (2 on a Retina screen).
fn scale(display_id: u32) -> f64 {
    let display = CGDisplay::new(display_id);
    (display.pixels_wide() as f64 / display.bounds().size.width.max(1.0)).max(1.0)
}

fn displays_of(content: &SCShareableContent) -> Vec<(SCDisplay, Display)> {
    let main = CGDisplay::main().id;
    content
        .displays()
        .into_iter()
        .enumerate()
        .map(|(index, d)| {
            let s = scale(d.display_id());
            let frame = d.frame();
            let info = Display {
                index,
                name: format!("Screen {}", index + 1),
                left: (frame.origin.x * s) as i32,
                top: (frame.origin.y * s) as i32,
                width: (f64::from(d.width()) * s) as u32,
                height: (f64::from(d.height()) * s) as u32,
                primary: d.display_id() == main,
            };
            (d, info)
        })
        .collect()
}

/// Asking ScreenCaptureKit for the displays fails when the permission is missing.
fn content() -> Result<SCShareableContent> {
    SCShareableContent::get().map_err(|e| anyhow!("{e:?}")).context("@agent.screen_permission_mac")
}

pub fn displays() -> Result<Vec<Display>> {
    Ok(displays_of(&content()?).into_iter().map(|(_, d)| d).collect())
}

struct Image {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
}

struct Handler {
    images: Mutex<SyncSender<Image>>,
}

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output_type: SCStreamOutputType) {
        if !matches!(output_type, SCStreamOutputType::Screen) {
            return;
        }
        // Idle frames: nothing changed on the screen.
        if !matches!(sample.frame_status(), Some(SCFrameStatus::Complete)) {
            return;
        }
        let Some(buffer) = sample.pixel_buffer() else { return };
        let Ok(guard) = buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else { return };
        let Some(bytes) = (unsafe { guard.as_slice() }) else { return };
        let (width, height, stride) = (guard.width() as u32, guard.height() as u32, guard.bytes_per_row());
        let need = stride * height as usize;
        let Some(bytes) = bytes.get(..need) else { return };
        let image = Image { width, height, stride, data: bytes.to_vec() };
        if let Ok(images) = self.images.lock()
            && let Err(TrySendError::Full(_)) = images.try_send(image)
        {
            // The capture is behind: this image is dropped, the next is newer.
        }
    }
}

pub struct ScreenCaptureKitCapturer {
    display: Display,
    stream: SCStream,
    images: Receiver<Image>,
    frame: Option<Frame>,
}

impl ScreenCaptureKitCapturer {
    pub fn new(index: Option<usize>) -> Result<Self> {
        let content = content()?;
        let mut list = displays_of(&content);
        let pick = match index {
            Some(i) => list.iter().position(|(_, d)| d.index == i),
            None => list.iter().position(|(_, d)| d.primary).or(Some(0)),
        }
        .filter(|&i| i < list.len())
        .context("screen not found")?;
        let (sc_display, display) = list.swap_remove(pick);
        let filter = SCContentFilter::create().with_display(&sc_display).with_excluding_windows(&[]).build()?;
        let config = SCStreamConfiguration::new()
            .with_width(display.width)
            .with_height(display.height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(true)
            .with_queue_depth(3)
            .with_minimum_frame_interval(&CMTime { value: 1, timescale: 60, flags: 1, epoch: 0 });
        let (tx, images) = std::sync::mpsc::sync_channel(1);
        let mut stream = SCStream::new(&filter, &config)?;
        stream.add_output_handler(Handler { images: Mutex::new(tx) }, SCStreamOutputType::Screen)?;
        stream.start_capture().map_err(|e| anyhow!("{e:?}")).context("@agent.screen_permission_mac")?;
        info!(name = display.name, width = display.width, height = display.height, "ScreenCaptureKit capture");
        Ok(Self { display, stream, images, frame: None })
    }
}

impl Capturer for ScreenCaptureKitCapturer {
    fn display(&self) -> &Display {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<&Frame>> {
        let image = match self.images.recv_timeout(timeout) {
            Ok(image) => image,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!("the screen stream ended"),
        };
        self.frame = Some(Frame {
            width: image.width,
            height: image.height,
            stride: image.stride,
            data: image.data,
            captured_us: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64),
        });
        Ok(self.frame.as_ref())
    }

    fn last_frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }
}

impl Drop for ScreenCaptureKitCapturer {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}

pub fn open(index: Option<usize>) -> Result<Box<dyn Capturer>> {
    Ok(Box::new(ScreenCaptureKitCapturer::new(index)?))
}
