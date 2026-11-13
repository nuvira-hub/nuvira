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

/// `NUVIRA_DMABUF=1`: the compositor is asked for DMA-BUF buffers too (linear
/// ones), for a VA-API encoder to import with no copy through RAM. Off by default
/// until tried on real graphics cards.
fn dmabuf_wanted() -> bool {
    std::env::var("NUVIRA_DMABUF").as_deref() == Ok("1")
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
    let state = State {
        format: Default::default(),
        dmabuf: false,
        images,
        spare,
        ready: Some(ready.clone()),
        pool: Vec::new(),
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
            if state.dmabuf {
                // The compositor left the modifier open: the one we take (linear,
                // the only one offered) is sent back with the buffers we want.
                let fixate = flags.bits() & spa::sys::SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED != 0;
                match dmabuf_params(fixate) {
                    Ok(pods) => {
                        let mut params: Vec<&Pod> = pods.iter().filter_map(|p| Pod::from_bytes(p)).collect();
                        if let Err(e) = stream.update_params(&mut params) {
                            warn!("PipeWire DMA-BUF buffers: {e}");
                        }
                    }
                    Err(e) => warn!("PipeWire DMA-BUF buffers: {e:#}"),
                }
                if fixate {
                    return;
                }
                info!(modifier = state.format.modifier(), "PipeWire capture in DMA-BUF buffers");
            }
            if let Some(ready) = state.ready.take() {
                let _ = ready.send(Ok((size.width, size.height)));
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
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
            // The capture thread takes one image at a time: if it is behind, this
            // one is dropped (the next will be newer anyway).
            if let Err(std::sync::mpsc::TrySendError::Full(image)) = state.images.try_send(image) {
                state.pool.push(image.data);
            }
        })
        .register()?;

    // DMA-BUF first when wanted, shared memory otherwise: the compositor picks.
    let mut formats = Vec::new();
    if dmabuf_wanted() {
        formats.push(format_pod(Some(false))?);
    }
    formats.push(format_pod(None)?);
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
fn format_pod(modifier: Option<bool>) -> Result<Vec<u8>> {
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pw::spa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};
    let linear = crate::dmabuf::MODIFIER_LINEAR as i64;
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
    if let Some(fixed) = modifier {
        // SPA_POD_PROP_FLAG_DONT_FIXATE (1 << 4): the compositor settles it.
        let dont_fixate = PropertyFlags::from_bits_retain(1 << 4);
        obj.properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: if fixed { PropertyFlags::MANDATORY } else { PropertyFlags::MANDATORY | dont_fixate },
            value: if fixed {
                Value::Long(linear)
            } else {
                Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum { default: linear, alternatives: vec![linear] },
                )))
            },
        });
    }
    let (cursor, _) =
        pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pw::spa::pod::Value::Object(obj))
            .map_err(|e| anyhow!("format: {e:?}"))?;
    Ok(cursor.into_inner())
}

/// After a DMA-BUF format: the buffers we take (DMA-BUFs), and the format with the
/// modifier settled when the compositor asked for it.
fn dmabuf_params(fixate: bool) -> Result<Vec<Vec<u8>>> {
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
    if fixate {
        pods.push(format_pod(Some(true))?);
    }
    Ok(pods)
}

/// Opens the shared screen; `index` is ignored: the person chooses the screen in
/// the portal dialog.
pub fn open() -> Result<Box<dyn Capturer>> {
    Ok(Box::new(PipeWireCapturer::new()?))
}
