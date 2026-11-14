//! macOS capture: ScreenCaptureKit (`SCStream`) with BGRA frames. macOS gives the
//! images only once the person allowed NuviraHub in System Settings → Privacy &
//! Security → Screen Recording.
//!
//! The pointer: ScreenCaptureKit leaves it out (`showsCursor` off) and its
//! position and shape are read from the system ([`crate::macos_pointer`]) and
//! published for the viewers that draw it themselves (`nuvira_pointer`). For the
//! others, a recording or a guest it is drawn into the picture here, keeping the
//! pixels it covers so that a move of the pointer alone needs no new picture.
//! When the system gives no shape, ScreenCaptureKit draws it in as before.
//!
//! The frames arrive on a queue of ScreenCaptureKit: the handler copies them
//! into a channel of one image (a newer one replaces what the capture did not
//! take yet).
//!
//! Privacy mode: the windows of the black-screen process are left out of the
//! stream ([`hide_process`]); the filter is changed by the capture itself,
//! between two frames.
//!
//! Locked screen: ScreenCaptureKit shows the lock screen like any other screen,
//! as long as the display is on. macOS turns the display off a few seconds after
//! the lock (also with `displaysleep 0`), and then ScreenCaptureKit lists no
//! display and stops the stream (error -3815). So during a capture the display
//! is kept on ([`DisplayAwake`]), and woken up while the screen is locked
//! (tested on macOS 27.2, Mac mini M4, 2026-11-09). When the stream stops all
//! the same, or no display can be found (a closed laptop lid), the capture gives
//! a dark placeholder image once a second, so the session goes on, and tries to
//! open the stream again. The viewer is told by the agent ([`screen_locked`]).
//! The lock screen takes no synthetic input: the password is typed through a
//! virtual HID keyboard (`nuvira_input::lockhid`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use core_graphics::display::CGDisplay;
use screencapturekit::cm::{CMSampleBufferExt, CMTime, SCFrameStatus};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use screencapturekit::stream::ErrorHandler;
use nuvira_pointer as cursor;
use tracing::{debug, info};

use crate::macos_pointer;
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

/// CoreGraphics ID of the screen at `index` in [`displays`].
pub fn display_id(index: usize) -> Result<u32> {
    let list = displays_of(&content()?);
    if list.is_empty() && screen_locked() && index == 0 {
        return Ok(CGDisplay::main().id);
    }
    list.into_iter().nth(index).map(|(d, _)| d.display_id()).context("the screen is gone")
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
    /// Set by ScreenCaptureKit when the stream stopped by itself.
    stopped: Arc<AtomicBool>,
}

/// Keeps the display on while a capture is open: with the display off (as macOS
/// does a few seconds after the lock) there is nothing to capture. An IOKit
/// power assertion, as `caffeinate -d`; [`DisplayAwake::wake`] declares user
/// activity, which also turns a display that is already off back on.
struct DisplayAwake {
    assertion: Option<u32>,
    activity: u32,
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        kind: core_foundation::string::CFStringRef,
        level: u32,
        name: core_foundation::string::CFStringRef,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionDeclareUserActivity(
        name: core_foundation::string::CFStringRef,
        user_type: u32,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

const ASSERTION_NAME: &str = "NuviraHub remote session";

impl DisplayAwake {
    fn new() -> Self {
        use core_foundation::base::TCFType;
        use core_foundation::string::CFString;
        let kind = CFString::from_static_string("PreventUserIdleDisplaySleep");
        let name = CFString::from_static_string(ASSERTION_NAME);
        let mut id = 0;
        // SAFETY: two valid strings and an out pointer; 255 = kIOPMAssertionLevelOn.
        let ret = unsafe {
            IOPMAssertionCreateWithName(kind.as_concrete_TypeRef(), 255, name.as_concrete_TypeRef(), &mut id)
        };
        if ret != 0 {
            info!(ret, "display sleep assertion refused");
        }
        Self { assertion: (ret == 0).then_some(id), activity: 0 }
    }

    /// Declares local user activity: the display turns on, if it was off.
    fn wake(&mut self) {
        use core_foundation::base::TCFType;
        use core_foundation::string::CFString;
        let name = CFString::from_static_string(ASSERTION_NAME);
        // SAFETY: a valid string and an out pointer; 0 = kIOPMUserActiveLocal.
        // The same id is passed again, so the system extends one assertion.
        let ret =
            unsafe { IOPMAssertionDeclareUserActivity(name.as_concrete_TypeRef(), 0, &mut self.activity) };
        if ret != 0 {
            info!(ret, "user activity not declared");
        }
    }
}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        // SAFETY: ids given by IOKit to this process.
        unsafe {
            if let Some(id) = self.assertion {
                IOPMAssertionRelease(id);
            }
            if self.activity != 0 {
                IOPMAssertionRelease(self.activity);
            }
        }
    }
}

pub struct ScreenCaptureKitCapturer {
    display: Display,
    /// The display asked for (`None`: the primary one).
    index: Option<usize>,
    /// CoreGraphics id of the display, to build the filter again.
    display_id: u32,
    /// Generation of [`HIDDEN`] in the stream's filter.
    hidden_generation: u64,
    /// `None` while there is no stream (display off or not found).
    running: Option<Running>,
    frame: Option<Frame>,
    /// Last look at the lock state, and the next placeholder.
    lock_checked: Instant,
    next_try: Instant,
    /// Next attempt to open the stream again (separate from the placeholder's
    /// clock, which moved it forward every second).
    reopen_at: Instant,
    awake: DisplayAwake,
    /// Lock state at the last look, and the last user activity declared.
    locked: bool,
    woken_at: Option<Instant>,
    /// The pointer is left out of the stream and handled here (the system gives
    /// its shape).
    own_pointer: bool,
    /// The display in global points (left, top, width, height) and its pixels
    /// per point.
    points: (f64, f64, f64, f64),
    scale: f64,
    pointer_shape: Option<cursor::Shape>,
    shape_checked: Option<Instant>,
    /// Pixels of the last published position.
    pointer_at: Option<(i32, i32, bool)>,
    /// The pointer drawn into `frame.data`.
    drawn: cursor::Drawn,
}

/// How often the lock state is read, and the placeholder sent.
const LOCK_CHECK: Duration = Duration::from_millis(500);
const PLACEHOLDER_EVERY: Duration = Duration::from_secs(1);
/// How often user activity is declared while the screen is locked (the lock
/// screen turns the display off a few seconds after the last activity).
const WAKE_EVERY: Duration = Duration::from_secs(10);
/// Dark grey of the placeholder (BGRA).
const PLACEHOLDER: [u8; 4] = [0x2a, 0x26, 0x24, 0xff];
/// How often the pointer's position is read while waiting for a picture, and
/// its shape (the system says nothing when either changes).
const POINTER_POLL: Duration = Duration::from_millis(8);
const SHAPE_POLL: Duration = Duration::from_millis(50);

/// The pointer must be in the picture: a viewer that does not draw it, a
/// recording, a guest.
fn draw_wanted() -> bool {
    !cursor::active() || cursor::composite()
}

impl ScreenCaptureKitCapturer {
    pub fn new(index: Option<usize>) -> Result<Self> {
        let now = Instant::now();
        let own_pointer = macos_pointer::shape(1.0).is_some();
        if own_pointer {
            info!("pointer read from the system, apart from the picture");
        } else {
            info!("pointer shape not given by the system: drawn in by ScreenCaptureKit");
            if cursor::active() {
                cursor::set_active(false);
            }
        }
        let mut capturer = Self {
            display: main_display(),
            index,
            display_id: CGDisplay::main().id,
            hidden_generation: 0,
            running: None,
            frame: None,
            lock_checked: now,
            next_try: now,
            reopen_at: now,
            awake: DisplayAwake::new(),
            locked: screen_locked(),
            woken_at: None,
            own_pointer,
            points: (0.0, 0.0, 1.0, 1.0),
            scale: 1.0,
            pointer_shape: None,
            shape_checked: None,
            pointer_at: None,
            drawn: cursor::Drawn::default(),
        };
        if capturer.locked {
            // The display may already be off: turned on, then the stream opens
            // within a second (placeholder until then).
            info!("screen locked: waking the display");
            capturer.wake();
            capturer.reopen_at = now + Duration::from_millis(500);
            return Ok(capturer);
        }
        capturer.start()?;
        Ok(capturer)
    }

    fn wake(&mut self) {
        self.awake.wake();
        self.woken_at = Some(Instant::now());
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
            .with_shows_cursor(!self.own_pointer)
            .with_queue_depth(3)
            .with_minimum_frame_interval(&CMTime { value: 1, timescale: 60, flags: 1, epoch: 0 });
        let (tx, images) = std::sync::mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let on_error = ErrorHandler::new(move |error| {
            info!("ScreenCaptureKit stream stopped: {error}");
            flag.store(true, Ordering::Relaxed);
        });
        let mut stream = SCStream::new_with_delegate(&filter, &config, on_error)?;
        stream.add_output_handler(Handler { images: Mutex::new(tx) }, SCStreamOutputType::Screen)?;
        stream.start_capture().map_err(|e| anyhow!("{e:?}")).context("@agent.screen_permission_mac")?;
        info!(name = screen.name, width = screen.width, height = screen.height, "ScreenCaptureKit capture");
        self.display_id = sc_display.display_id();
        let frame = sc_display.frame();
        self.points = (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height);
        self.scale = scale(self.display_id);
        // A new stream: the pointer is published and drawn again.
        self.pointer_shape = None;
        self.pointer_at = None;
        self.drawn.forget();
        self.display = screen;
        self.hidden_generation = hidden_generation;
        self.running = Some(Running { stream, images, stopped });
        Ok(())
    }

    /// Follows the lock state and the stream: while locked the display is kept
    /// on; a stream that stopped by itself (display off) is opened again, once a
    /// second, with the placeholder in between.
    fn follow_lock(&mut self) {
        if self.lock_checked.elapsed() < LOCK_CHECK {
            return;
        }
        self.lock_checked = Instant::now();
        let locked = screen_locked();
        if locked != self.locked {
            self.locked = locked;
            info!(locked, "screen lock state");
            if locked {
                // Woken at once: the lock screen turns the display off soon.
                self.woken_at = None;
            }
        }
        if locked && self.woken_at.is_none_or(|t| t.elapsed() >= WAKE_EVERY) {
            self.wake();
        }
        if self.running.as_ref().is_some_and(|r| r.stopped.load(Ordering::Relaxed)) {
            self.running = None;
            info!(locked, "capture stream lost: placeholder until it opens again");
            if locked {
                self.wake();
            }
            self.reopen_at = Instant::now() + Duration::from_millis(500);
        }
        if self.running.is_none() && Instant::now() >= self.reopen_at {
            match self.start() {
                Ok(()) => info!(locked, "capture resumed"),
                Err(e) => {
                    info!("capture not resumed yet: {e:#}");
                    self.reopen_at = Instant::now() + PLACEHOLDER_EVERY;
                }
            }
        }
    }

    /// The dark image sent while there is no stream, once a second.
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

impl ScreenCaptureKitCapturer {
    /// The viewers asked for the pointer apart (or no longer). The shared state is
    /// emptied on a change, so the pointer is published again.
    fn follow_wanted(&mut self) {
        let wanted = cursor::wanted();
        if cursor::active() != wanted {
            cursor::set_active(wanted);
            self.pointer_at = None;
        }
    }

    /// Reads the pointer and publishes what changed, in pixels of this display.
    fn poll_pointer(&mut self) {
        if self.shape_checked.is_none_or(|t| t.elapsed() >= SHAPE_POLL) {
            self.shape_checked = Some(Instant::now());
            if let Some((width, height, hot_x, hot_y, bgra)) = macos_pointer::shape(self.scale) {
                let serial = cursor::serial_of(width, height, hot_x, hot_y, &bgra);
                if self.pointer_shape.as_ref().is_none_or(|s| s.serial != serial) {
                    if self.pointer_shape.is_none() {
                        info!(width, height, "pointer shape from the system");
                    } else {
                        debug!(width, height, serial, "new pointer shape");
                    }
                    self.pointer_shape =
                        Some(cursor::Shape { serial, width, height, hot_x, hot_y, bgra: Arc::new(bgra) });
                    // Published with the position below.
                    self.pointer_at = None;
                }
            }
        }
        let Some(shape) = &self.pointer_shape else { return };
        let Some((x, y)) = macos_pointer::location() else { return };
        let (left, top, width, height) = self.points;
        let inside = x >= left && y >= top && x < left + width && y < top + height;
        let at = (((x - left) * self.scale) as i32, ((y - top) * self.scale) as i32, inside);
        if self.pointer_at == Some(at) {
            return;
        }
        self.pointer_at = Some(at);
        // Published again only when the shared state does not have it (a new
        // shape, or emptied).
        let fresh = (cursor::latest().0.as_ref().map(|p| p.serial) != Some(shape.serial)).then(|| shape.clone());
        cursor::publish(fresh, cursor::Position { x: at.0, y: at.1, visible: inside, serial: shape.serial });
    }
}

impl Capturer for ScreenCaptureKitCapturer {
    fn display(&self) -> &Display {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<&Frame>> {
        self.follow_lock();
        self.apply_hidden();
        if self.running.is_none() {
            return Ok(self.placeholder(timeout));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            // With the pointer handled here, it is read between the pictures.
            let wait = if self.own_pointer { left.min(POINTER_POLL) } else { left };
            let received = match self.running.as_ref().map(|r| r.images.recv_timeout(wait)) {
                Some(Ok(image)) => Some(image),
                Some(Err(RecvTimeoutError::Timeout)) | None => None,
                Some(Err(RecvTimeoutError::Disconnected)) => bail!("the screen stream ended"),
            };
            if self.own_pointer {
                self.follow_wanted();
                self.poll_pointer();
            }
            let now_us = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64);
            if let Some(image) = received {
                let mut frame = Frame {
                    width: image.width,
                    height: image.height,
                    stride: image.stride,
                    data: image.data,
                    captured_us: now_us,
                };
                // A new picture: what the pointer covered went with the old one.
                self.drawn.forget();
                if self.own_pointer && draw_wanted() {
                    let (w, h, stride) = (frame.width, frame.height, frame.stride);
                    self.drawn.draw(&mut frame.data, w, h, stride, false);
                }
                self.frame = Some(frame);
                return Ok(self.frame.as_ref());
            }
            if self.own_pointer
                && let Some(frame) = self.frame.as_mut()
            {
                let draw = draw_wanted();
                let drawn = self.drawn.generation();
                let redraw = if draw { drawn != Some(cursor::generation()) } else { drawn.is_some() };
                if redraw {
                    // Only the pointer moved (or must come in or out): the same
                    // picture with it drawn again.
                    self.drawn.undo(&mut frame.data, frame.stride);
                    if draw {
                        let (w, h, stride) = (frame.width, frame.height, frame.stride);
                        self.drawn.draw(&mut frame.data, w, h, stride, false);
                    }
                    frame.captured_us = now_us;
                    return Ok(self.frame.as_ref());
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
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
