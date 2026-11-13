//! Capture and encoding on two threads.
//!
//! The capture thread puts every new image in a slot; the encoding thread always
//! takes the most recent one. If encoding is slower than capture the intermediate
//! images are skipped: fps depend on the slower stage and not on the sum of the
//! two, and no delay builds up.
//!
//! When the viewer draws the image in a smaller area than the screen, the image is
//! encoded already reduced to that area (see [`encoded_size`]): on the GPU by the
//! colour conversion itself, on the CPU by averaging pixels.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use nuvira_capture::{Capturer, Display};
use nuvira_codec::{BgraImage, GpuDevice, VideoEncoder};
use nuvira_proto::v1::VideoFrame;
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
    texture: Option<nuvira_codec::GpuTexture>,
    /// The image is in this DMA-BUF and `data` is empty (Linux, `NUVIRA_DMABUF=1`).
    #[cfg(target_os = "linux")]
    dmabuf: Option<std::sync::Arc<nuvira_capture::dmabuf::DmaBuf>>,
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

/// Values the session changes while the pipeline runs.
#[derive(Clone)]
pub struct Controls {
    pub max_fps: Arc<AtomicU32>,
    /// Next frame must be a keyframe (set by the session, cleared when one is sent).
    pub keyframe: Arc<AtomicBool>,
    pub bitrate: Arc<AtomicU32>,
    /// Viewer's drawing area, see [`pack_view`].
    pub view: Arc<AtomicU64>,
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
        frames: mpsc::Sender<VideoFrame>,
        controls: Controls,
    ) -> Result<(Display, Self)> {
        let shared = Arc::new(Shared::default());
        let mut pipeline = Self { shared: shared.clone(), threads: Vec::new() };

        #[cfg_attr(not(windows), allow(unused_mut))]
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
            #[cfg(target_os = "macos")]
            precise_timers();
            encode_loop(&s, encoder, &frames, &controls);
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
                // E.g. secure desktop without nuvira-service: it is retried without closing
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
            texture: frame.texture.clone().map(nuvira_codec::GpuTexture),
            #[cfg(target_os = "linux")]
            dmabuf: frame.dmabuf.clone(),
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

/// A new viewer area is applied once it has stayed the same this long: while a window
/// is being resized the encoder is not restarted at every step.
const VIEW_SETTLE: Duration = Duration::from_millis(300);

/// Viewer area packed in an `AtomicU64` (width in the high half, 0 = full size).
pub fn pack_view(width: u32, height: u32) -> u64 {
    ((width as u64) << 32) | height as u64
}

/// Size at which a `width`x`height` screen is encoded for a viewer area `view`
/// (packed, 0 = full size): reduced to fit the area, aspect kept, even sizes;
/// never larger than the screen.
pub fn encoded_size(width: u32, height: u32, view: u64) -> (u32, u32) {
    let (vw, vh) = ((view >> 32) as u32, view as u32);
    if vw == 0 || vh == 0 || (vw >= width && vh >= height) {
        return (width, height);
    }
    let scale = (vw as f64 / width as f64).min(vh as f64 / height as f64);
    let fit = |n: u32| (((n as f64 * scale).round() as u32) & !1).max(64).min(n);
    (fit(width), fit(height))
}

/// macOS gives an ordinary thread's timers a leeway of about half the time asked
/// (Mac mini M4, macOS 27: 12 ms of sleep lasted 17), so the pause between frames
/// became 17.8 ms instead of 12 and sessions stayed at 47 fps with any encoder.
/// A time-constraint thread (Apple's policy for audio and video) sleeps exactly;
/// one that exceeds its computation is only demoted by the kernel.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc's mach items point to the mach2 crate; these few are enough here.
fn precise_timers() {
    let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: plain calls on this thread with a filled policy of the right count.
    unsafe {
        libc::mach_timebase_info(&mut timebase);
        if timebase.numer == 0 {
            return;
        }
        let ticks = |ms: f64| (ms * 1e6 * timebase.denom as f64 / timebase.numer as f64) as u32;
        let mut policy = libc::thread_time_constraint_policy {
            period: ticks(1000.0 / 60.0),
            computation: ticks(8.0),
            constraint: ticks(1000.0 / 60.0),
            preemptible: 1,
        };
        let thread = libc::mach_thread_self();
        let result = libc::thread_policy_set(
            thread,
            libc::THREAD_TIME_CONSTRAINT_POLICY as libc::thread_policy_flavor_t,
            (&raw mut policy).cast(),
            libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        );
        if result != 0 {
            warn!("encoding thread: time-constraint policy refused ({result}), the frame pace may be slower");
        }
    }
}

fn encode_loop(
    shared: &Shared,
    mut encoder: Box<dyn VideoEncoder>,
    frames: &mpsc::Sender<VideoFrame>,
    controls: &Controls,
) {
    let Controls { max_fps, keyframe, bitrate, view } = controls;
    let mut applied_bitrate = bitrate.load(Ordering::Relaxed);
    let mut applied_view = view.load(Ordering::Relaxed);
    let mut pending_view: Option<(u64, Instant)> = None;
    let mut encoded_at = (0, 0);
    // Reduced image for the encoders that take it from RAM.
    let mut scaled = Vec::new();
    // A DMA-BUF picture read into RAM, when the encoder does not import it.
    #[cfg(target_os = "linux")]
    let (mut dmabuf_ram, mut dmabuf_warned) = (Vec::new(), false);
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
        let target = bitrate.load(Ordering::Relaxed);
        if target != applied_bitrate {
            encoder.set_bitrate(target);
            applied_bitrate = target;
        }
        let requested = view.load(Ordering::Relaxed);
        if requested == applied_view {
            pending_view = None;
        } else {
            match pending_view {
                Some((v, since)) if v == requested && since.elapsed() >= VIEW_SETTLE => {
                    applied_view = requested;
                    pending_view = None;
                    // Re-encoded at the new size even if the screen is still.
                    keyframe.store(true, Ordering::Relaxed);
                }
                Some((v, _)) if v == requested => {}
                _ => pending_view = Some((requested, Instant::now())),
            }
        }
        let force_key = keyframe.load(Ordering::Relaxed);
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
        let size = encoded_size(image.width, image.height, applied_view);
        if size != encoded_at {
            info!("encoding {}x{} at {}x{}", image.width, image.height, size.0, size.1);
            encoded_at = size;
        }

        let t = Instant::now();
        // A DMA-BUF picture: imported by the encoder, else read into RAM first.
        #[allow(unused_mut)]
        let mut imported: Option<anyhow::Result<Option<nuvira_codec::EncodedFrame>>> = None;
        #[cfg(target_os = "linux")]
        let mut read: Option<Captured> = None;
        #[cfg(target_os = "linux")]
        if let Some(buf) = &image.dmabuf {
            imported = encoder.encode_dmabuf(&dmabuf_image(buf), size, force_key);
            if imported.is_none() {
                let mut data = std::mem::take(&mut dmabuf_ram);
                if let Err(e) = buf.read(&mut data) {
                    if !std::mem::replace(&mut dmabuf_warned, true) {
                        warn!("DMA-BUF picture not readable: {e:#}");
                    }
                    continue;
                }
                read = Some(Captured {
                    width: buf.width,
                    height: buf.height,
                    stride: buf.width as usize * 4,
                    data,
                    captured_us: image.captured_us,
                    dmabuf: None,
                });
            }
        }
        #[cfg(target_os = "linux")]
        let image = read.as_ref().unwrap_or(image);
        let result = if let Some(result) = imported {
            result
        } else {
            match image_texture(image) {
            #[cfg(windows)]
            Some(texture) if encoder.accepts_textures() => {
                encoder.encode_texture(texture, (image.width, image.height), size, force_key)
            }
            _ => {
                let full = BgraImage {
                    width: image.width,
                    height: image.height,
                    stride: image.stride,
                    data: &image.data,
                };
                if size == (image.width, image.height) {
                    encoder.encode(&full, force_key)
                } else {
                    nuvira_codec::convert::scale_bgra(&full, size.0 as usize, size.1 as usize, &mut scaled);
                    let (w, h) = size;
                    encoder.encode(
                        &BgraImage { width: w, height: h, stride: w as usize * 4, data: &scaled },
                        force_key,
                    )
                }
            }
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
            tracing::debug!("keyframe {} kB (requested: {force_key})", encoded.data.len() / 1024);
        }
        let encode_us = t.elapsed().as_micros() as u32;
        stats.add(encode_us, encoded.data.len());
        seq += 1;
        let msg = VideoFrame {
            seq,
            // A repeated image is still what is on screen now.
            capture_us: if repeat { nuvira_proto::now_us() } else { image.captured_us },
            encode_us,
            encoded_us: nuvira_proto::now_us(),
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
fn image_texture(image: &Captured) -> Option<&nuvira_codec::GpuTexture> {
    image.texture.as_ref()
}

/// The codec's description of a captured DMA-BUF.
#[cfg(target_os = "linux")]
fn dmabuf_image(buf: &nuvira_capture::dmabuf::DmaBuf) -> nuvira_codec::DmaBufImage {
    use std::os::fd::AsRawFd;
    nuvira_codec::DmaBufImage {
        fd: buf.fd.as_raw_fd(),
        width: buf.width,
        height: buf.height,
        drm_format: buf.drm_format,
        modifier: buf.modifier,
        offset: buf.offset,
        stride: buf.stride,
        size: buf.size,
    }
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
    fn sizes() {
        assert_eq!(encoded_size(1920, 1080, 0), (1920, 1080));
        assert_eq!(encoded_size(1920, 1080, pack_view(2560, 1440)), (1920, 1080));
        assert_eq!(encoded_size(1920, 1080, pack_view(1280, 720)), (1280, 720));
        // Narrow window: the width decides, the aspect stays.
        assert_eq!(encoded_size(1920, 1080, pack_view(960, 1000)), (960, 540));
        assert_eq!(encoded_size(3840, 2160, pack_view(1500, 900)), (1500, 844));
        // Odd sizes become even, and never below 64.
        assert_eq!(encoded_size(1920, 1080, pack_view(1001, 2000)), (1000, 562));
        assert_eq!(encoded_size(1920, 1080, pack_view(10, 10)), (64, 64));
    }

    #[test]
    fn produces_frames_and_stops() {
        let (tx, mut rx) = mpsc::channel(1);
        let keyframe = Arc::new(AtomicBool::new(true));
        let view = Arc::new(AtomicU64::new(0));
        let (display, pipeline) = Pipeline::start(
            || Ok(Box::new(nuvira_capture::SyntheticCapturer::new(320, 180, 60)) as Box<dyn Capturer>),
            |gpu| nuvira_codec::create_encoder(nuvira_codec::EncoderPlan::Software, Default::default(), gpu),
            false,
            tx,
            Controls {
                max_fps: Arc::new(AtomicU32::new(60)),
                keyframe,
                bitrate: Arc::new(AtomicU32::new(1_000_000)),
                view: view.clone(),
            },
        )
        .context("start")
        .unwrap();
        assert_eq!((display.width, display.height), (320, 180));
        let first = rx.blocking_recv().expect("first frame");
        assert!(first.keyframe);
        assert!(rx.blocking_recv().is_some());

        // A smaller viewer area: after it settles a keyframe comes at the new size.
        view.store(pack_view(160, 90), Ordering::Relaxed);
        let mut decoder = nuvira_codec::OpenH264Decoder::new().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let size = loop {
            assert!(Instant::now() < deadline, "no frame at the new size");
            let frame = rx.blocking_recv().expect("frame");
            // Only keyframes: the stream is followed from the middle.
            if !frame.keyframe {
                continue;
            }
            if let Some(yuv) = nuvira_codec::VideoDecoder::decode(&mut decoder, &frame.data).unwrap()
                && (yuv.width, yuv.height) != (320, 180)
            {
                break (yuv.width, yuv.height);
            }
        };
        assert_eq!(size, (160, 90));
        drop(pipeline);
    }
}
