//! macOS capture: ScreenCaptureKit (`SCStream`) with BGRA frames and the cursor
//! drawn in. macOS gives the images only once the person allowed NuviraHub in
//! System Settings → Privacy & Security → Screen Recording.
//!
//! The frames arrive on a queue of ScreenCaptureKit: the handler copies them
//! into a channel of one image (a newer one replaces what the capture did not
//! take yet).
//!
//! Privacy mode: the windows of the black-screen process are left out of the
//! stream ([`hide_process`]); the filter is changed by the capture itself,
//! between two frames.
//!
//! Locked screen: macOS shows no capture program the lock screen (ScreenCaptureKit
//! lists no display and stops a running stream a few seconds after the lock;
//! the older CoreGraphics captures are black, also as root). While the screen is
//! locked the capture gives a dark placeholder image once a second, so the session
//! goes on, and opens the stream again once it is unlocked. The viewer is told by
//! the agent ([`screen_locked`]). The lock screen takes no synthetic input either
//! (tried on macOS 27: CGEventPost with every source and tap, CGPostKeyboardEvent,
//! as the user and as root, with the pre-login section): someone unlocks it at the
//! Mac, or the Mac is restarted and signed in at the sign-in screen.

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use core_graphics::display::CGDisplay;
use screencapturekit::cm::{CMSampleBufferExt, CMTime, SCFrameStatus};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use tracing::info;

use crate::{Capturer, Display, Frame};

/// Processes whose windows the capture leaves out, with a change counter.
struct Hidden {
    generation: u64,
    pids: Vec<i32>,
    /// Last generation a capture applied.
    applied: u64,
}

static HIDDEN: Mutex<Hidden> = Mutex::new(Hidden { generation: 0, pids: Vec::new(), applied: 0 });
static HIDDEN_APPLIED: Condvar = Condvar::new();
/// How long [`hide_process`] waits for the capture.
const HIDE_WAIT: Duration = Duration::from_millis(1500);

pub fn hide_process(pid: u32) {
    let Ok(mut hidden) = HIDDEN.lock() else { return };
    // Processes that ended are dropped: only the last black screens are alive.
    hidden.pids.retain(|&p| unsafe { libc::kill(p, 0) } == 0);
    hidden.pids.push(pid as i32);
    hidden.generation += 1;
    let wanted = hidden.generation;
    let (hidden, timeout) = HIDDEN_APPLIED
        .wait_timeout_while(hidden, HIDE_WAIT, |h| h.applied < wanted)
        .unwrap_or_else(|e| e.into_inner());
    if timeout.timed_out() {
        info!(applied = hidden.applied, wanted, "capture did not confirm the hidden windows in time");
    }
}

/// Filter of `display`, without the windows of the hidden processes.
fn filter_for(content: &SCShareableContent, display: &SCDisplay, pids: &[i32]) -> Result<SCContentFilter> {
    let apps: Vec<_> =
        content.applications().into_iter().filter(|a| pids.contains(&a.process_id())).collect();
    let builder = SCContentFilter::create().with_display(display);
    let builder = if apps.is_empty() {
        if !pids.is_empty() {
            info!(?pids, "hidden processes not among the applications of ScreenCaptureKit");
        }
        builder.with_excluding_windows(&[])
    } else {
        let apps: Vec<&_> = apps.iter().collect();
        builder.with_excluding_applications(&apps, &[])
    };
    Ok(builder.build()?)
}

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
    let list: Vec<Display> = displays_of(&content()?).into_iter().map(|(_, d)| d).collect();
    if list.is_empty() && screen_locked() {
        return Ok(vec![main_display()]);
    }
    Ok(list)
}

/// The main display as CoreGraphics sees it (also while the screen is locked).
fn main_display() -> Display {
    let main = CGDisplay::main();
    Display {
        index: 0,
        name: "Screen 1".into(),
        left: 0,
        top: 0,
        width: main.pixels_wide() as u32,
        height: main.pixels_high() as u32,
        primary: true,
    }
}

/// The screen of the person's session is locked (`CGSSessionScreenIsLocked`).
/// False outside a person's session, as at the sign-in screen.
pub fn screen_locked() -> bool {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    }
    // SAFETY: no arguments; the result is owned (+1) or NULL.
    let dict = unsafe { CGSessionCopyCurrentDictionary() };
    if dict.is_null() {
        return false;
    }
    // SAFETY: a dictionary just copied, released by the wrapper.
    let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_create_rule(dict) };
    dict.find(CFString::from_static_string("CGSSessionScreenIsLocked"))
        .and_then(|v| v.downcast::<CFBoolean>())
        .is_some_and(bool::from)
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

/// A running ScreenCaptureKit stream and its images.
struct Running {
    stream: SCStream,
    images: Receiver<Image>,
}

pub struct ScreenCaptureKitCapturer {
    display: Display,
    /// The display asked for (`None`: the primary one).
    index: Option<usize>,
    /// CoreGraphics id of the display, to build the filter again.
    display_id: u32,
    /// Generation of [`HIDDEN`] in the stream's filter.
    hidden_generation: u64,
    /// `None` while the screen is locked.
    running: Option<Running>,
    frame: Option<Frame>,
    /// Last look at the lock state, and the next placeholder or reopening.
    lock_checked: Instant,
    next_try: Instant,
}

/// How often the lock state is read, and the placeholder sent.
const LOCK_CHECK: Duration = Duration::from_millis(500);
const PLACEHOLDER_EVERY: Duration = Duration::from_secs(1);
/// Dark grey of the placeholder (BGRA).
const PLACEHOLDER: [u8; 4] = [0x2a, 0x26, 0x24, 0xff];

impl ScreenCaptureKitCapturer {
    pub fn new(index: Option<usize>) -> Result<Self> {
        let now = Instant::now();
        let mut capturer = Self {
            display: main_display(),
            index,
            display_id: CGDisplay::main().id,
            hidden_generation: 0,
            running: None,
            frame: None,
            lock_checked: now,
            next_try: now,
        };
        if screen_locked() {
            info!("screen locked: placeholder until it is unlocked");
            return Ok(capturer);
        }
        capturer.start()?;
        Ok(capturer)
    }

    fn start(&mut self) -> Result<()> {
        let content = content()?;
        let mut list = displays_of(&content);
        let pick = match self.index {
            Some(i) => list.iter().position(|(_, d)| d.index == i),
            None => list.iter().position(|(_, d)| d.primary).or(Some(0)),
        }
        .filter(|&i| i < list.len())
        .context("screen not found")?;
        let (sc_display, screen) = list.swap_remove(pick);
        let (hidden_generation, pids) =
            HIDDEN.lock().map(|h| (h.generation, h.pids.clone())).unwrap_or_default();
        let filter = filter_for(&content, &sc_display, &pids)?;
        let config = SCStreamConfiguration::new()
            .with_width(screen.width)
            .with_height(screen.height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(true)
            .with_queue_depth(3)
            .with_minimum_frame_interval(&CMTime { value: 1, timescale: 60, flags: 1, epoch: 0 });
        let (tx, images) = std::sync::mpsc::sync_channel(1);
        let mut stream = SCStream::new(&filter, &config)?;
        stream.add_output_handler(Handler { images: Mutex::new(tx) }, SCStreamOutputType::Screen)?;
        stream.start_capture().map_err(|e| anyhow!("{e:?}")).context("@agent.screen_permission_mac")?;
        info!(name = screen.name, width = screen.width, height = screen.height, "ScreenCaptureKit capture");
        self.display_id = sc_display.display_id();
        self.display = screen;
        self.hidden_generation = hidden_generation;
        self.running = Some(Running { stream, images });
        Ok(())
    }

    /// Follows the lock state: the stream is closed when the screen locks and
    /// opened again (once a second) after it is unlocked.
    fn follow_lock(&mut self) {
        if self.lock_checked.elapsed() < LOCK_CHECK {
            return;
        }
        self.lock_checked = Instant::now();
        let locked = screen_locked();
        if locked && let Some(running) = self.running.take() {
            info!("screen locked: placeholder until it is unlocked");
            let _ = running.stream.stop_capture();
        } else if !locked && self.running.is_none() && Instant::now() >= self.next_try {
            match self.start() {
                Ok(()) => info!("screen unlocked: capture resumed"),
                Err(e) => {
                    info!("capture not resumed yet: {e:#}");
                    self.next_try = Instant::now() + PLACEHOLDER_EVERY;
                }
            }
        }
    }

    /// The dark image sent while the screen is locked, once a second.
    fn placeholder(&mut self, timeout: Duration) -> Option<&Frame> {
        let now = Instant::now();
        if now < self.next_try {
            std::thread::sleep(timeout.min(self.next_try - now));
            return None;
        }
        self.next_try = now + PLACEHOLDER_EVERY;
        let (width, height) = (self.display.width, self.display.height);
        self.frame = Some(Frame {
            width,
            height,
            stride: width as usize * 4,
            data: PLACEHOLDER.repeat(width as usize * height as usize),
            captured_us: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64),
        });
        self.frame.as_ref()
    }

    /// Leaves out the windows of the processes hidden since the last frame.
    fn apply_hidden(&mut self) {
        let Some(running) = &self.running else { return };
        let Some((generation, pids)) = HIDDEN
            .lock()
            .ok()
            .filter(|h| h.generation != self.hidden_generation)
            .map(|h| (h.generation, h.pids.clone()))
        else {
            return;
        };
        let result = content().and_then(|content| {
            let display = content
                .displays()
                .into_iter()
                .find(|d| d.display_id() == self.display_id)
                .context("screen not found")?;
            let filter = filter_for(&content, &display, &pids)?;
            running.stream.update_content_filter(&filter).map_err(|e| anyhow!("{e:?}"))
        });
        match result {
            Ok(()) => info!(?pids, "capture filter updated"),
            Err(e) => info!("capture filter not updated: {e:#}"),
        }
        self.hidden_generation = generation;
        if let Ok(mut hidden) = HIDDEN.lock() {
            hidden.applied = hidden.applied.max(generation);
            HIDDEN_APPLIED.notify_all();
        }
    }
}

impl Capturer for ScreenCaptureKitCapturer {
    fn display(&self) -> &Display {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<&Frame>> {
        self.follow_lock();
        self.apply_hidden();
        let image = match &self.running {
            None => return Ok(self.placeholder(timeout)),
            Some(running) => match running.images.recv_timeout(timeout) {
                Ok(image) => image,
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => bail!("the screen stream ended"),
            },
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
        if let Some(running) = &self.running {
            let _ = running.stream.stop_capture();
        }
    }
}

pub fn open(index: Option<usize>) -> Result<Box<dyn Capturer>> {
    Ok(Box::new(ScreenCaptureKitCapturer::new(index)?))
}
