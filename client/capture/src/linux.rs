//! Wayland capture: the screen shared through the remote desktop portal
//! (`nuvira-portal`), received as a PipeWire stream in shared memory.
//!
//! PipeWire objects belong to the thread of their loop: the stream runs in its own
//! thread and hands over copies of the images; the buffers come back to it to be
//! reused, so that a still screen allocates nothing.
//!
//! Locked screen: GNOME closes every remote desktop session when the screen
//! locks and allows none until it is unlocked (also through its own API). While
//! the session is locked the capture gives a dark placeholder once a second, and
//! [`screen_locked`] tells the agent, which tells the viewer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::VideoFormat;
use pw::spa::pod::Pod;
use tracing::{debug, info, warn};

use crate::{Capturer, Display, Frame};
use nuvira_portal::cursor;

/// `struct spa_meta_cursor` (spa/buffer/meta.h): the pointer as stream metadata.
#[repr(C)]
struct CursorMeta {
    id: u32,
    flags: u32,
    x: i32,
    y: i32,
    hot_x: i32,
    hot_y: i32,
    bitmap_offset: u32,
}

impl spa::buffer::meta::Metadata for CursorMeta {
    const META_TYPE: u32 = spa::sys::SPA_META_Cursor;
}

/// `struct spa_meta_bitmap`: the pointer's image, `offset` bytes after it.
#[repr(C)]
struct BitmapMeta {
    format: u32,
    width: u32,
    height: u32,
    stride: i32,
    offset: u32,
}

/// Room asked for the pointer's metadata: the two headers and a picture of
/// `side`x`side` RGBA.
const fn cursor_meta_size(side: u32) -> i32 {
    (std::mem::size_of::<CursorMeta>() + std::mem::size_of::<BitmapMeta>()) as i32 + (side * side * 4) as i32
}

/// File in the data folder with the portal's restore token.
const TOKEN_FILE: &str = "remote-desktop-token";

/// The person's session is locked and the system shares no screen meanwhile.
static LOCKED: AtomicBool = AtomicBool::new(false);

pub fn screen_locked() -> bool {
    LOCKED.load(Ordering::Relaxed)
}

/// logind's `LockedHint` of this user's graphical session (set by GNOME's lock screen).
fn session_locked() -> bool {
    let value = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("loginctl").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() }.to_string();
    let Some(session) = value(&["show-user", &uid, "-p", "Display", "--value"]).filter(|s| !s.is_empty()) else {
        return false;
    };
    value(&["show-session", &session, "-p", "LockedHint", "--value"]).as_deref() == Some("yes")
}

/// Dark grey of the placeholder (BGRA), as on macOS.
const PLACEHOLDER: [u8; 4] = [0x2a, 0x26, 0x24, 0xff];

fn portal_session(ask: bool) -> Result<Arc<nuvira_portal::Session>> {
    nuvira_portal::session(nuvira_transport::identity::data_dir().join(TOKEN_FILE), ask)
}

/// The shared screen, as the portal describes it. Without an open session there is
/// nothing to list: asking would show the permission dialog.
pub fn displays() -> Vec<Display> {
    nuvira_portal::current().map(|s| vec![display(&s.screen(), None)]).unwrap_or_default()
}

fn display(screen: &nuvira_portal::Screen, pixels: Option<(u32, u32)>) -> Display {
    let (width, height) = pixels.unwrap_or((screen.width, screen.height));
    Display { index: 0, name: "Screen".into(), left: 0, top: 0, width, height, primary: true }
}

/// Image from the stream thread: size, stride, bytes, and whether the bytes are RGB
/// (to be swapped to BGR).
struct Image {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
    rgb: bool,
    /// The picture left in the GPU's memory (`data` empty).
    dmabuf: Option<crate::dmabuf::DmaBuf>,
}

/// The DMA-BUF modifiers the compositor is offered (empty: shared memory only),
/// and a generation that changes with them: a stream in progress negotiates again.
static DMABUF_MODIFIERS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
static DMABUF_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The DMA-BUF layouts (modifiers) the encoder imports, in order of preference,
/// for a VA-API encoder to take the picture with no copy through RAM: the agent
/// sets them from its encoder (empty: shared memory); a stream in progress
/// negotiates again when they change (an import that failed: linear only).
/// `NUVIRA_DMABUF=1` asks for DMA-BUFs whatever the encoder (linear if it said
/// nothing), `NUVIRA_DMABUF=0` never.
pub fn set_dmabuf_modifiers(modifiers: Vec<u64>) {
    let mut current = DMABUF_MODIFIERS.lock().expect("modifiers");
    if *current != modifiers {
        info!(?modifiers, "DMA-BUF modifiers offered to the compositor");
        *current = modifiers;
        DMABUF_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED` (PipeWire 0.3.75; Debian 12's
/// headers, which the packages are built with, do not have it yet).
const VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED: u32 = 1 << 3;

/// The modifiers to offer now (empty: no DMA-BUF). The pointer as metadata comes
/// only with shared memory (the agent asks for one or the other): it can then be
/// drawn into the picture when needed.
fn dmabuf_wanted() -> Vec<u64> {
    if cursor::wanted() {
        return Vec::new();
    }
    let modifiers = DMABUF_MODIFIERS.lock().expect("modifiers").clone();
    match std::env::var("NUVIRA_DMABUF").as_deref() {
        Ok("1") if modifiers.is_empty() => vec![crate::dmabuf::MODIFIER_LINEAR],
        Ok("0") => Vec::new(),
        _ => modifiers,
    }
}

/// The formats offered: DMA-BUF with `modifiers` first (if any), then shared memory.
fn offered_formats(modifiers: &[u64]) -> Result<Vec<Vec<u8>>> {
    let mut formats = Vec::new();
    if !modifiers.is_empty() {
        formats.push(format_pod(Some(Modifiers::Offer(modifiers)))?);
    }
    formats.push(format_pod(None)?);
    Ok(formats)
}

/// The modifier to settle on when the compositor leaves a choice: the first of
/// ours (in our order of preference) among those it offers.
fn choose_modifier(param: &Pod, ours: &[u64], fallback: u64) -> u64 {
    use pw::spa::pod::{ChoiceValue, Value, deserialize::PodDeserializer};
    use pw::spa::utils::ChoiceEnum;
    let Ok((_, Value::Object(obj))) = PodDeserializer::deserialize_any_from(param.as_bytes()) else {
        return fallback;
    };
    let key = spa::param::format::FormatProperties::VideoModifier.as_raw();
    let theirs: Vec<u64> = match obj.properties.iter().find(|p| p.key == key).map(|p| &p.value) {
        Some(Value::Long(m)) => vec![*m as u64],
        Some(Value::Choice(ChoiceValue::Long(choice))) => match &choice.1 {
            ChoiceEnum::Enum { default, alternatives } => {
                std::iter::once(*default).chain(alternatives.iter().copied()).map(|m| m as u64).collect()
            }
            ChoiceEnum::None(m) => vec![*m as u64],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    ours.iter().copied().find(|m| theirs.contains(m)).unwrap_or(fallback)
}

pub struct PipeWireCapturer {
    display: Display,
    stream: Option<Stream>,
    frame: Option<Frame>,
    /// When to try again to share the screen, after the system stopped it.
    retry_at: Instant,
    /// Last placeholder sent while the screen is locked.
    placeholder_at: Option<Instant>,
}

impl PipeWireCapturer {
    pub fn new() -> Result<Self> {
        let stream = Stream::open(true)?;
        Ok(Self {
            display: stream.display.clone(),
            stream: Some(stream),
            frame: None,
            retry_at: Instant::now(),
            placeholder_at: None,
        })
    }

    /// The system stopped the sharing (GNOME does it when the screen locks, and
    /// refuses new sessions until it is unlocked): it is asked again every two
    /// seconds, with the saved permission, and the images resume by themselves.
    fn reopen(&mut self, timeout: Duration) -> Result<bool> {
        if Instant::now() < self.retry_at {
            std::thread::sleep(timeout);
            return Ok(false);
        }
        match Stream::open(false) {
            Ok(stream) => {
                info!("screen sharing resumed");
                self.stream = Some(stream);
                Ok(true)
            }
            Err(e) => {
                self.retry_at = Instant::now() + Duration::from_secs(2);
                Err(e.context("screen sharing stopped on this computer (screen locked?)"))
            }
        }
    }
}

impl PipeWireCapturer {
    /// The dark image, once a second while the screen is locked.
    fn placeholder(&mut self) -> Option<&Frame> {
        if !LOCKED.load(Ordering::Relaxed) || self.placeholder_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return None;
        }
        self.placeholder_at = Some(Instant::now());
        let (width, height) = (self.display.width, self.display.height);
        self.frame = Some(Frame {
            width,
            height,
            stride: width as usize * 4,
            data: PLACEHOLDER.repeat(width as usize * height as usize),
            captured_us: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64),
            dmabuf: None,
        });
        self.frame.as_ref()
    }
}

impl Capturer for PipeWireCapturer {
    fn display(&self) -> &Display {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<&Frame>> {
        if self.stream.as_ref().is_none_or(|s| !s.session.is_open()) {
            if self.stream.take().is_some() {
                info!("the system stopped the screen sharing");
                // Asking at once, while the system is still closing the old
                // session, got no answer at all.
                self.retry_at = Instant::now() + Duration::from_secs(2);
            }
            match self.reopen(timeout) {
                Ok(true) => {
                    if LOCKED.swap(false, Ordering::Relaxed) {
                        info!("screen unlocked: capture resumed");
                    }
                }
                Ok(false) => return Ok(self.placeholder()),
                Err(e) => {
                    if !session_locked() {
                        LOCKED.store(false, Ordering::Relaxed);
                        return Err(e);
                    }
                    if !LOCKED.swap(true, Ordering::Relaxed) {
                        info!("screen locked: placeholder until it is unlocked");
                    }
                    return Ok(self.placeholder());
                }
            }
        }
        let Some(stream) = &self.stream else { return Ok(None) };
        let image = match stream.images.recv_timeout(timeout) {
            Ok(image) => image,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                self.stream = None;
                return Ok(None);
            }
        };
        let mut data = image.data;
        if image.rgb {
            for px in data.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
        }
        let frame = Frame {
            width: image.width,
            height: image.height,
            stride: image.stride,
            data,
            captured_us: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64),
            dmabuf: image.dmabuf.map(Arc::new),
        };
        if let Some(old) = self.frame.replace(frame) {
            let _ = stream.spare.send(old.data);
        }
        Ok(self.frame.as_ref())
    }

    fn last_frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }
}

/// One PipeWire stream of the shared screen, with its thread.
struct Stream {
    display: Display,
    images: Receiver<Image>,
    spare: Sender<Vec<u8>>,
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    session: Arc<nuvira_portal::Session>,
}

impl Stream {
    fn open(ask: bool) -> Result<Self> {
        let session = portal_session(ask)?;
        let screen = session.screen();
        let fd = session.pipewire()?;
        let (images_tx, images) = std::sync::mpsc::sync_channel(1);
        let (spare, spare_rx) = std::sync::mpsc::channel();
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("pipewire".into()).spawn(move || {
            if let Err(e) = stream_loop(fd, screen.node_id, images_tx, spare_rx, quit_rx, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        let mut stream =
            Self { display: display(&screen, None), images, spare, quit, thread: Some(thread), session };
        // The real size in pixels comes with the format (the portal gives the
        // logical one, smaller with a display scale above 100%).
        let size = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow!("no image format from PipeWire"))??;
        stream.display = display(&screen, Some(size));
        info!(?screen, width = size.0, height = size.1, "PipeWire capture");
        Ok(stream)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        debug!("stopping the PipeWire stream");
        if self.quit.send(()).is_err() {
            debug!("PipeWire loop already ended");
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        debug!("PipeWire stream stopped");
    }
}

struct State {
    format: spa::param::video::VideoInfoRaw,
    /// The negotiated format has a modifier: the buffers are DMA-BUFs.
    dmabuf: bool,
    images: SyncSender<Image>,
    spare: Receiver<Vec<u8>>,
    ready: Option<Sender<Result<(u32, u32)>>>,
    pool: Vec<Vec<u8>>,
    /// The modifiers offered, and their generation.
    modifiers: Vec<u64>,
    generation: u64,
    /// The pointer as metadata: the last picture (without it), to draw it again
    /// when only the pointer moves and it must be in the picture; the last
    /// bitmap, its serial, and the generation of the pointer drawn.
    last: Option<Image>,
    bitmap: Vec<u8>,
    serial: u32,
    drawn: u64,
}

fn stream_loop(
    fd: std::os::fd::OwnedFd,
    node_id: u32,
    images: SyncSender<Image>,
    spare: Receiver<Vec<u8>>,
    quit: pw::channel::Receiver<()>,
    ready: &Sender<Result<(u32, u32)>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(fd, None).context("PipeWire connection")?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "nuvira-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let generation = DMABUF_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
    let modifiers = dmabuf_wanted();
    let state = State {
        format: Default::default(),
        dmabuf: false,
        images,
        spare,
        ready: Some(ready.clone()),
        pool: Vec::new(),
        modifiers: modifiers.clone(),
        generation,
        last: None,
        bitmap: Vec::new(),
        serial: 0,
        drawn: u64::MAX,
    };
    let ml = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(move |_, _, old, new| {
            debug!("PipeWire stream: {old:?} -> {new:?}");
            // The shared screen went away (the person stopped sharing): ending the
            // loop closes the image channel, and the capture reports it.
            if matches!(new, pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected) {
                ml.quit();
            }
        })
        .param_changed(|stream, state, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Err(e) = state.format.parse(param) {
                warn!("PipeWire format: {e}");
                return;
            }
            let size = state.format.size();
            let flags = state.format.flags();
            state.dmabuf = flags.bits() & spa::sys::SPA_VIDEO_FLAG_MODIFIER != 0;
            debug!(
                format = ?state.format.format(),
                size.width,
                size.height,
                dmabuf = state.dmabuf,
                modifier = state.format.modifier(),
                "PipeWire format"
            );
            // The pointer as metadata: room for it in every buffer.
            if !state.dmabuf && cursor::active() {
                match cursor_meta_param() {
                    Ok(pod) => {
                        let mut params: Vec<&Pod> = Pod::from_bytes(&pod).into_iter().collect();
                        if let Err(e) = stream.update_params(&mut params) {
                            warn!("PipeWire pointer metadata: {e}");
                        }
                    }
                    Err(e) => warn!("PipeWire pointer metadata: {e:#}"),
                }
            }
            if state.dmabuf {
                // The compositor left the modifier open: the one we take (the first
                // of ours it can do) is sent back with the buffers we want.
                let fixate = (flags.bits() & VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED != 0)
                    .then(|| choose_modifier(param, &state.modifiers, state.format.modifier()));
                match dmabuf_params(fixate) {
                    Ok(pods) => {
                        let mut params: Vec<&Pod> = pods.iter().filter_map(|p| Pod::from_bytes(p)).collect();
                        if let Err(e) = stream.update_params(&mut params) {
                            warn!("PipeWire DMA-BUF buffers: {e}");
                        }
                    }
                    Err(e) => warn!("PipeWire DMA-BUF buffers: {e:#}"),
                }
                if fixate.is_some() {
                    return;
                }
                info!(modifier = state.format.modifier(), "PipeWire capture in DMA-BUF buffers");
            }
            if let Some(ready) = state.ready.take() {
                let _ = ready.send(Ok((size.width, size.height)));
            }
        })
        .process(|stream, state| {
            // The encoder's modifiers changed (an import that failed): negotiate again.
            let generation = DMABUF_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
            if generation != state.generation {
                state.generation = generation;
                let modifiers = dmabuf_wanted();
                if modifiers != state.modifiers {
                    info!(?modifiers, "PipeWire: formats offered again");
                    state.modifiers = modifiers;
                    match offered_formats(&state.modifiers) {
                        Ok(formats) => {
                            let mut params: Vec<&Pod> = formats.iter().filter_map(|f| Pod::from_bytes(f)).collect();
                            if let Err(e) = stream.update_params(&mut params) {
                                warn!("PipeWire formats: {e}");
                            }
                        }
                        Err(e) => warn!("PipeWire formats: {e:#}"),
                    }
                    return;
                }
            }
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            if cursor::active() {
                read_cursor(&buffer, state);
            }
            let rgb = match state.format.format() {
                VideoFormat::BGRx | VideoFormat::BGRA => false,
                VideoFormat::RGBx | VideoFormat::RGBA => true,
                _ => return,
            };
            let size = state.format.size();
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            if data.type_() == spa::buffer::DataType::DmaBuf {
                let chunk = data.chunk();
                if chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) || chunk.stride() <= 0 {
                    return;
                }
                let drm_format = match state.format.format() {
                    VideoFormat::BGRx => crate::dmabuf::DRM_FORMAT_XRGB8888,
                    VideoFormat::BGRA => crate::dmabuf::DRM_FORMAT_ARGB8888,
                    VideoFormat::RGBx => crate::dmabuf::DRM_FORMAT_XBGR8888,
                    _ => crate::dmabuf::DRM_FORMAT_ABGR8888,
                };
                // Our own copy of the descriptor: the buffer goes back to the
                // compositor when this callback returns.
                // SAFETY: fcntl on the buffer's descriptor, valid during the callback.
                let fd = unsafe { libc::fcntl(data.fd() as i32, libc::F_DUPFD_CLOEXEC, 0) };
                if fd < 0 {
                    return;
                }
                // SAFETY: a new descriptor of ours.
                let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
                let image = Image {
                    width: size.width,
                    height: size.height,
                    stride: chunk.stride() as usize,
                    data: Vec::new(),
                    rgb: false,
                    dmabuf: Some(crate::dmabuf::DmaBuf {
                        fd,
                        width: size.width,
                        height: size.height,
                        drm_format,
                        modifier: state.format.modifier(),
                        offset: chunk.offset(),
                        stride: chunk.stride() as u32,
                        size: data.as_raw().maxsize,
                    }),
                };
                let _ = state.images.try_send(image);
                return;
            }
            let chunk = data.chunk();
            let (offset, len, stride) = (chunk.offset() as usize, chunk.size() as usize, chunk.stride());
            // An empty chunk: nothing changed (or only the cursor, with metadata).
            if len == 0 || stride <= 0 {
                // The pointer moved and must be in the picture: the last picture again.
                if cursor::active() && cursor::composite() && state.drawn != cursor::generation()
                    && let Some(last) = &state.last
                {
                    let mut out = state.pool.pop().unwrap_or_default();
                    out.clear();
                    out.extend_from_slice(&last.data);
                    let image = Image { data: out, dmabuf: None, ..*last };
                    send_with_cursor(state, image);
                }
                return;
            }
            let stride = stride as usize;
            let Some(bytes) = data.data() else { return };
            let need = stride * size.height as usize;
            let Some(src) = bytes.get(offset..offset + need.min(len)) else { return };
            if src.len() < need {
                return;
            }
            while let Ok(buf) = state.spare.try_recv() {
                state.pool.push(buf);
            }
            let mut out = state.pool.pop().unwrap_or_default();
            out.clear();
            out.extend_from_slice(src);
            let image = Image { width: size.width, height: size.height, stride, data: out, rgb, dmabuf: None };
            if cursor::active() && cursor::composite() {
                // Kept without the pointer, to draw it again when only it moves.
                let keep = Image { data: image.data.clone(), dmabuf: None, ..image };
                state.last = Some(keep);
                send_with_cursor(state, image);
                return;
            }
            state.last = None;
            // The capture thread takes one image at a time: if it is behind, this
            // one is dropped (the next will be newer anyway).
            if let Err(std::sync::mpsc::TrySendError::Full(image)) = state.images.try_send(image) {
                state.pool.push(image.data);
            }
        })
        .register()?;

    // DMA-BUF first when wanted, shared memory otherwise: the compositor picks.
    let formats = offered_formats(&modifiers)?;
    let mut params: Vec<&Pod> = formats.iter().filter_map(|f| Pod::from_bytes(f)).collect();
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| ml.quit());
    mainloop.run();
    debug!("PipeWire loop ended");
    Ok(())
}

/// Raw video in 32-bit formats with no conversion on our side but the swap of R
/// and B. `None`: in shared memory (no modifier). `Some(fixed)`: in DMA-BUFs with
/// the linear modifier, offered as a choice to the compositor, or `fixed` when it
/// asks us to settle it.
/// The modifier of a DMA-BUF format: offered (a choice) or settled.
enum Modifiers<'a> {
    Offer(&'a [u64]),
    Fixed(u64),
}

fn format_pod(modifier: Option<Modifiers>) -> Result<Vec<u8>> {
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pw::spa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};
    let mut obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 240, denom: 1 }
        ),
    );
    match modifier {
        None => {}
        Some(Modifiers::Fixed(m)) => obj.properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(m as i64),
        }),
        Some(Modifiers::Offer(list)) => {
            // SPA_POD_PROP_FLAG_DONT_FIXATE (1 << 4): the compositor settles it.
            let dont_fixate = PropertyFlags::from_bits_retain(1 << 4);
            let list: Vec<i64> = list.iter().map(|&m| m as i64).collect();
            obj.properties.push(Property {
                key: FormatProperties::VideoModifier.as_raw(),
                flags: PropertyFlags::MANDATORY | dont_fixate,
                value: Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum { default: list[0], alternatives: list },
                ))),
            });
        }
    }
    let (cursor, _) =
        pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pw::spa::pod::Value::Object(obj))
            .map_err(|e| anyhow!("format: {e:?}"))?;
    Ok(cursor.into_inner())
}

/// The pointer's metadata read from a buffer and published for the agent.
fn read_cursor(buffer: &pw::buffer::Buffer, state: &mut State) {
    let Some(meta) = buffer.find_meta::<CursorMeta>() else { return };
    // id 0: no pointer on this screen (hidden, or on another one).
    if meta.id == 0 {
        cursor::publish(None, cursor::Position { visible: false, serial: state.serial, ..Default::default() });
        return;
    }
    let mut shape = None;
    if meta.bitmap_offset as usize >= std::mem::size_of::<CursorMeta>() {
        // SAFETY: the compositor wrote the bitmap header at bitmap_offset inside the
        // metadata it filled (room asked by cursor_meta_param), and its pixels at
        // offset after that header.
        unsafe {
            let base = (meta as *const CursorMeta).cast::<u8>();
            let bitmap = &*(base.add(meta.bitmap_offset as usize).cast::<BitmapMeta>());
            let (w, h) = (bitmap.width.min(256), bitmap.height.min(256));
            let stride = bitmap.stride.max(0) as usize;
            if w > 0 && h > 0 && stride >= w as usize * 4 && bitmap.offset as usize >= std::mem::size_of::<BitmapMeta>() {
                let pixels = std::slice::from_raw_parts(
                    (bitmap as *const BitmapMeta).cast::<u8>().add(bitmap.offset as usize),
                    stride * (h as usize - 1) + w as usize * 4,
                );
                let format = bitmap.format;
                let mut bgra = Vec::with_capacity((w * h * 4) as usize);
                for y in 0..h as usize {
                    for px in pixels[y * stride..][..w as usize * 4].chunks_exact(4) {
                        let [b, g, r, a] = if format == VideoFormat::RGBA.as_raw() {
                            [px[2], px[1], px[0], px[3]]
                        } else if format == VideoFormat::BGRA.as_raw() {
                            [px[0], px[1], px[2], px[3]]
                        } else if format == VideoFormat::ARGB.as_raw() {
                            [px[3], px[2], px[1], px[0]]
                        } else {
                            [px[1], px[2], px[3], px[0]] // ABGR
                        };
                        bgra.extend_from_slice(&[b, g, r, a]);
                    }
                }
                if bgra != state.bitmap {
                    state.serial = state.serial.wrapping_add(1).max(1);
                    state.bitmap.clone_from(&bgra);
                    shape = Some(cursor::Shape {
                        serial: state.serial,
                        width: w,
                        height: h,
                        hot_x: meta.hot_x.clamp(0, w as i32 - 1) as u32,
                        hot_y: meta.hot_y.clamp(0, h as i32 - 1) as u32,
                        bgra: Arc::new(bgra),
                    });
                }
            }
        }
    }
    cursor::publish(shape, cursor::Position { x: meta.x, y: meta.y, visible: state.serial != 0, serial: state.serial });
}

/// Sends a picture with the pointer drawn in (it must be in the picture).
fn send_with_cursor(state: &mut State, mut image: Image) {
    cursor::draw(&mut image.data, image.width, image.height, image.stride, image.rgb);
    state.drawn = cursor::generation();
    if let Err(std::sync::mpsc::TrySendError::Full(image)) = state.images.try_send(image) {
        state.pool.push(image.data);
    }
}

/// `SPA_PARAM_Meta` for the pointer: room for a picture of up to 256x256.
fn cursor_meta_param() -> Result<Vec<u8>> {
    use pw::spa::pod::{ChoiceValue, Property, Value};
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id};
    let meta = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: pw::spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_META_type, Value::Id(Id(spa::sys::SPA_META_Cursor))),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range { default: cursor_meta_size(64), min: cursor_meta_size(1), max: cursor_meta_size(256) },
                ))),
            ),
        ],
    };
    let (cursor, _) =
        pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(meta))
            .map_err(|e| anyhow!("meta: {e:?}"))?;
    Ok(cursor.into_inner())
}

/// After a DMA-BUF format: the buffers we take (DMA-BUFs), and the format with the
/// modifier settled when the compositor asked for it.
fn dmabuf_params(fixate: Option<u64>) -> Result<Vec<Vec<u8>>> {
    use pw::spa::pod::{Property, Value};
    let buffers = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property::new(
            pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Int(1 << pw::spa::sys::SPA_DATA_DmaBuf),
        )],
    };
    let (cursor, _) = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(buffers),
    )
    .map_err(|e| anyhow!("buffers: {e:?}"))?;
    let mut pods = vec![cursor.into_inner()];
    if let Some(m) = fixate {
        pods.push(format_pod(Some(Modifiers::Fixed(m)))?);
    }
    Ok(pods)
}

/// Opens the shared screen; `index` is ignored: the person chooses the screen in
/// the portal dialog.
pub fn open() -> Result<Box<dyn Capturer>> {
    Ok(Box::new(PipeWireCapturer::new()?))
}
