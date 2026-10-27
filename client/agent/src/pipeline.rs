//! Capture and encoding on two threads.
//!
//! The capture thread puts every new image in a slot; the encoding thread always
//! takes the most recent one. If encoding is slower than capture the intermediate
//! images are skipped: fps depend on the slower stage and not on the sum of the
//! two, and no delay builds up.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use nexodesk_capture::{Capturer, Display};
use nexodesk_codec::{BgraImage, GpuDevice, VideoEncoder};
use nexodesk_proto::v1::VideoFrame;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Captured image waiting to be encoded.
struct Captured {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
    captured_us: u64,
    /// GPU mode: the image is in this texture and `data` is empty.
    #[cfg(windows)]
    texture: Option<nexodesk_codec::GpuTexture>,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Option<Captured>>,
    ready: Condvar,
    /// Already allocated buffers to reuse (avoids 8 MB of allocations per frame at 1080p).
    spare: Mutex<Vec<Vec<u8>>>,
    stop: AtomicBool,
    captured: AtomicU64,
    skipped: AtomicU64,
}

impl Shared {
    fn recycle(&self, buf: Vec<u8>) {
        let mut spare = self.spare.lock().expect("spare");
        if spare.len() < 3 {
            spare.push(buf);
        }
    }
}

pub struct Pipeline {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Pipeline {
    /// Starts the two threads. Blocks until capture and encoder are ready.
    /// With `gpu` capture leaves the images on the graphics card and the encoder gets
    /// its device (if the source does not support it, images go through RAM).
    pub fn start(
        open_capturer: impl FnOnce() -> Result<Box<dyn Capturer>>,
        open_encoder: impl FnOnce(Option<GpuDevice>) -> Result<Box<dyn VideoEncoder>> + Send + 'static,
        gpu: bool,
        max_fps: Arc<AtomicU32>,
        frames: mpsc::Sender<VideoFrame>,
        keyframe: Arc<AtomicBool>,
        bitrate: Arc<AtomicU32>,
    ) -> Result<(Display, Self)> {
        let shared = Arc::new(Shared::default());
        let mut pipeline = Self { shared: shared.clone(), threads: Vec::new() };

        let mut capturer = open_capturer()?;
        #[cfg(windows)]
        let device = if gpu { capturer.enable_gpu_output().map(GpuDevice) } else { None };
        #[cfg(not(windows))]
        let device: Option<GpuDevice> = None;
        if gpu && device.is_none() {
            info!("the source does not offer images on the GPU: the encoder gets them from RAM");
        }
        let screen = capturer.display().clone();

        // The encoder is created in the thread that will use it (Media Foundation initialises COM per thread).
        let (enc_ready_tx, enc_ready_rx) = std::sync::mpsc::channel();
        let s = shared.clone();
        pipeline.threads.push(std::thread::Builder::new().name("encoding".into()).spawn(move || {
            let encoder = match open_encoder(device) {
                Ok(e) => {
                    let _ = enc_ready_tx.send(Ok(e.name()));
                    e
                }
                Err(e) => {
                    let _ = enc_ready_tx.send(Err(e));
                    return;
                }
            };
            encode_loop(&s, encoder, max_fps, &frames, &keyframe, &bitrate);
        })?);

        let s = shared.clone();
        pipeline.threads.push(
            std::thread::Builder::new().name("capture".into()).spawn(move || capture_loop(&s, capturer))?,
        );

        // On error `pipeline` is dropped and stops the other thread.
        let encoder = enc_ready_rx.recv().map_err(|_| anyhow!("encoding thread ended"))??;
        info!("streaming {} {}x{} with {encoder}", screen.name, screen.width, screen.height);
        Ok((screen, pipeline))
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.ready.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn capture_loop(shared: &Shared, mut capturer: Box<dyn Capturer>) {
    let mut failing = false;
    while !shared.stop.load(Ordering::Relaxed) {
        let frame = match capturer.next_frame(Duration::from_millis(100)) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(e) => {
                // E.g. secure desktop without nexodesk-service: it is retried without closing
                // the session, reporting it only once.
                if !failing {
                    warn!("capture failed, retrying every half second: {e:#}");
                    failing = true;
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if std::mem::take(&mut failing) {
            info!("capture resumed");
        }
        let mut data = shared.spare.lock().expect("spare").pop().unwrap_or_default();
        data.clear();
        data.extend_from_slice(&frame.data);
        let captured = Captured {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            data,
            captured_us: frame.captured_us,
            #[cfg(windows)]
            texture: frame.texture.clone().map(nexodesk_codec::GpuTexture),
        };
        shared.captured.fetch_add(1, Ordering::Relaxed);
        let old = shared.slot.lock().expect("slot").replace(captured);
        if let Some(old) = old {
            shared.skipped.fetch_add(1, Ordering::Relaxed);
            shared.recycle(old.data);
        }
        shared.ready.notify_one();
    }
}

/// After the screen stops changing, the last image is encoded again this many times
/// (one every ~100 ms, while no new image arrives). Each pass spends the bitrate on
/// what the previous one left out, so text that came out blurry while things were
/// moving becomes sharp; once the image matches, the passes cost almost nothing.
/// Without this a still screen kept the quality of its first, hurried encoding
/// (first Internet test: unreadable text on a still screen).
const REFINE_FRAMES: u32 = 15;

fn encode_loop(
    shared: &Shared,
    mut encoder: Box<dyn VideoEncoder>,
    max_fps: Arc<AtomicU32>,
    frames: &mpsc::Sender<VideoFrame>,
    keyframe: &AtomicBool,
    bitrate: &AtomicU32,
) {
    let mut applied_bitrate = bitrate.load(Ordering::Relaxed);
    let mut last_encoded = Instant::now() - Duration::from_secs(1);
    // Last encoded image: used to make a keyframe again on a still screen.
    let mut last: Option<Captured> = None;
    let mut seq = 0u64;
    let mut stats = Stats::default();
    // Frames left to refine a still screen (see REFINE_FRAMES).
    let mut refine_left = 0u32;

    while !shared.stop.load(Ordering::Relaxed) {
        // Frames-per-second limit: meanwhile the slot is updated with the most recent image.
        let t_loop = Instant::now();
        let min_interval = Duration::from_secs(1) / max_fps.load(Ordering::Relaxed).max(1);
        let wait = min_interval.saturating_sub(last_encoded.elapsed());
        if !wait.is_zero() && !keyframe.load(Ordering::Relaxed) {
            std::thread::sleep(wait);
        }

        let slept = t_loop.elapsed();
        // The image is taken only when the encoder can accept it: that way it is the freshest.
        let t_ready = Instant::now();
        match encoder.wait_ready(Duration::from_millis(100)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                warn!("encoder not available: {e:#}");
                break;
            }
        }
        let waited_ready = t_ready.elapsed();
        let t_slot = Instant::now();
        let fresh = {
            let slot = shared.slot.lock().expect("slot");
            let (mut slot, _) = shared
                .ready
                .wait_timeout_while(slot, Duration::from_millis(100), |s| {
                    s.is_none() && !shared.stop.load(Ordering::Relaxed)
                })
                .expect("slot");
            slot.take()
        };
        let waited_slot = t_slot.elapsed();
        let force_key = keyframe.load(Ordering::Relaxed);
        let target = bitrate.load(Ordering::Relaxed);
        if target != applied_bitrate {
            encoder.set_bitrate(target);
            applied_bitrate = target;
        }
        let repeat = fresh.is_none();
        if let Some(fresh) = fresh {
            if let Some(old) = last.replace(fresh) {
                shared.recycle(old.data);
            }
            refine_left = REFINE_FRAMES;
        } else if force_key {
            // Still screen and keyframe requested: the last image is encoded again.
        } else if refine_left > 0 {
            refine_left -= 1;
        } else {
            continue;
        }
        let Some(image) = &last else { continue };

        let t = Instant::now();
        let result = match image_texture(image) {
            #[cfg(windows)]
            Some(texture) if encoder.accepts_textures() => {
                encoder.encode_texture(texture, image.width, image.height, force_key)
            }
            _ => {
                let bgra = BgraImage {
                    width: image.width,
                    height: image.height,
                    stride: image.stride,
                    data: &image.data,
                };
                encoder.encode(&bgra, force_key)
            }
        };
        let encoded = match result {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => {
                warn!("encoding failed: {e:#}");
                continue;
            }
        };
        // The pace is measured from the start of encoding, not from its end.
        last_encoded = t;
        if encoded.keyframe {
            keyframe.store(false, Ordering::Relaxed);
        }
        let encode_us = t.elapsed().as_micros() as u32;
        stats.add(encode_us, encoded.data.len());
        seq += 1;
        let msg = VideoFrame {
            seq,
            // A repeated image is still what is on screen now.
            capture_us: if repeat { nexodesk_proto::now_us() } else { image.captured_us },
            encode_us,
            encoded_us: nexodesk_proto::now_us(),
            keyframe: encoded.keyframe,
            data: encoded.data,
        };
        // Single-frame queue: if the network is slow capture is skipped instead of building up delay.
        let t_send = Instant::now();
        let sent = frames.blocking_send(msg);
        tracing::trace!(?slept, ?waited_ready, ?waited_slot, encode = ?t.elapsed().saturating_sub(t_send.elapsed()), send = ?t_send.elapsed(), "loop");
        if sent.is_err() {
            break;
        }
        stats.report(shared);
    }
}

#[cfg(windows)]
fn image_texture(image: &Captured) -> Option<&nexodesk_codec::GpuTexture> {
    image.texture.as_ref()
}

#[cfg(not(windows))]
fn image_texture(_image: &Captured) -> Option<&()> {
    None
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    bytes: usize,
    encode_us: u64,
    captured_before: u64,
    skipped_before: u64,
}

impl Stats {
    fn add(&mut self, encode_us: u32, bytes: usize) {
        self.since.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.bytes += bytes;
        self.encode_us += encode_us as u64;
    }

    fn report(&mut self, shared: &Shared) {
        let Some(since) = self.since else { return };
        let secs = since.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        let captured = shared.captured.load(Ordering::Relaxed);
        let skipped = shared.skipped.load(Ordering::Relaxed);
        info!(
            "{:.1} fps sent ({:.1} captured, {} skipped), encoding {:.1} ms/frame, {:.0} kbit/s",
            self.frames as f64 / secs,
            (captured - self.captured_before) as f64 / secs,
            skipped - self.skipped_before,
            self.encode_us as f64 / self.frames.max(1) as f64 / 1000.0,
            self.bytes as f64 * 8.0 / secs / 1000.0
        );
        *self = Stats { captured_before: captured, skipped_before: skipped, ..Default::default() };
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    #[test]
    fn produces_frames_and_stops() {
        let (tx, mut rx) = mpsc::channel(1);
        let keyframe = Arc::new(AtomicBool::new(true));
        let (display, pipeline) = Pipeline::start(
            || Ok(Box::new(nexodesk_capture::SyntheticCapturer::new(320, 180, 60)) as Box<dyn Capturer>),
            |gpu| {
                nexodesk_codec::create_encoder(nexodesk_codec::EncoderPlan::Software, Default::default(), gpu)
            },
            false,
            Arc::new(AtomicU32::new(60)),
            tx,
            keyframe,
            Arc::new(AtomicU32::new(1_000_000)),
        )
        .context("start")
        .unwrap();
        assert_eq!((display.width, display.height), (320, 180));
        let first = rx.blocking_recv().expect("first frame");
        assert!(first.keyframe);
        assert!(rx.blocking_recv().is_some());
        drop(pipeline);
    }
}
