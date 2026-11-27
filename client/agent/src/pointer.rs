//! The remote pointer apart from the picture (`Hello.cursor`; Linux, Windows, macOS):
//! what to ask the capture for, when it must be drawn into the picture after all,
//! and the messages for a viewer that draws it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nuvira_pointer as cursor;
use nuvira_proto::v1::control_message::Msg;

static RECORDING: AtomicBool = AtomicBool::new(false);
static PLAIN_GUESTS: AtomicUsize = AtomicUsize::new(0);

/// The capture can give it apart. Linux: also with the DMA-BUF capture (the
/// stream goes to shared memory while the pointer must be in the picture). Windows:
/// DXGI gives it apart (the GDI capture draws it in and says so). macOS: the
/// system gives its shape (otherwise ScreenCaptureKit draws it in and the
/// capture says so).
pub fn possible(plan: nuvira_codec::EncoderPlan) -> bool {
    let _ = plan;
    cfg!(any(windows, target_os = "linux", target_os = "macos"))
}

pub fn set_wanted(on: bool) {
    cursor::set_wanted(on);
}

/// At the end of the session: back to the pointer in the picture.
pub struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        set_wanted(false);
        set_recording(false);
    }
}

fn update() {
    cursor::set_composite(RECORDING.load(Ordering::Relaxed) || PLAIN_GUESTS.load(Ordering::Relaxed) > 0);
}

/// The viewer records the session: the pointer goes into the picture.
pub fn set_recording(on: bool) {
    RECORDING.store(on, Ordering::Relaxed);
    update();
}

/// A guest that cannot draw the pointer joins (or leaves): it needs it in the
/// picture.
pub fn plain_guest(joined: bool) {
    if joined {
        PLAIN_GUESTS.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = PLAIN_GUESTS.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
    update();
}

/// How many shapes a viewer with `Hello.cursor_cache` keeps at least.
const KEPT: usize = 32;

/// The pointer's messages for one viewer, when they change.
#[derive(Default)]
pub struct Sender {
    generation: Option<u64>,
    serial: u32,
    shown: bool,
    /// The viewer keeps the shapes it got (`Hello.cursor_cache`): their serials,
    /// the oldest first, and they go as PNG.
    cache: bool,
    sent: std::collections::VecDeque<u32>,
}

impl Sender {
    pub fn new(cache: bool) -> Self {
        Self { cache, ..Self::default() }
    }

    pub fn poll(&mut self) -> Vec<Msg> {
        let generation = cursor::generation();
        if self.generation == Some(generation) {
            return Vec::new();
        }
        self.generation = Some(generation);
        let mut out = Vec::new();
        // In the picture (a recording, a guest, a capture without it): the
        // viewer hides its own.
        if !cursor::active() || cursor::composite() {
            if self.shown {
                self.shown = false;
                out.push(Msg::CursorPosition(nuvira_proto::v1::CursorPosition::default()));
            }
            return out;
        }
        let (shape, pos) = cursor::latest();
        if let Some(s) = shape
            && s.serial != self.serial
        {
            self.serial = s.serial;
            // A shape the viewer keeps is named by the position alone.
            if !(self.cache && self.sent.contains(&s.serial)) {
                let mut msg = nuvira_proto::v1::CursorShape {
                    serial: s.serial,
                    width: s.width,
                    height: s.height,
                    hot_x: s.hot_x,
                    hot_y: s.hot_y,
                    ..Default::default()
                };
                let png = if self.cache { cursor::png_of(&s) } else { Vec::new() };
                if png.is_empty() {
                    msg.bgra = s.bgra.as_ref().clone();
                } else {
                    msg.png = png;
                }
                out.push(Msg::CursorShape(msg));
                if self.cache {
                    self.sent.push_back(s.serial);
                    if self.sent.len() > KEPT {
                        self.sent.pop_front();
                    }
                }
            }
        }
        self.shown = pos.visible;
        out.push(Msg::CursorPosition(nuvira_proto::v1::CursorPosition {
            x: pos.x,
            y: pos.y,
            visible: pos.visible,
            serial: pos.serial,
        }));
        out
    }
}
