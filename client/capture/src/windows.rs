//! Capture with DXGI Desktop Duplication.
//!
//! The GPU provides a texture only when the screen changes; it is copied into a
//! staging texture the CPU can read, or, in GPU mode, into textures that go
//! straight to the hardware encoder.
//!
//! The pointer is never in those textures: DXGI gives its position with every
//! frame and its shape when it changes. Both are published for the viewers that
//! draw it themselves (`nuvira_pointer`); for the others, a recording or a guest
//! it is drawn into the picture here: on the CPU path into the image in RAM (the
//! pixels it covers kept to take it out again), on the GPU path from a clean copy
//! of the desktop, the small piece under it read back, drawn on and written again.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use nuvira_pointer as cursor;
use tracing::{debug, info};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1,
    IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use crate::{Capturer, Display, Frame};

/// Rotating GPU textures: while the encoder reads one, capture writes another.
const GPU_POOL: usize = 3;

/// All the screens at once ([`crate::AllCapturer`]): where its picture starts in the
/// virtual desktop. Its screens publish the pointer in its coordinates and leave the
/// drawing to it.
static NESTED: Mutex<Option<(i32, i32)>> = Mutex::new(None);
/// The screen the pointer was last seen on: another screen saying it does not have
/// it does not hide it.
static OWNER: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Set by [`crate::AllCapturer`] while it lives.
pub(crate) fn set_nested(origin: Option<(i32, i32)>) {
    *NESTED.lock().unwrap_or_else(|e| e.into_inner()) = origin;
}

fn nested() -> Option<(i32, i32)> {
    *NESTED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The pointer must be in this capturer's pictures: a viewer that does not draw
/// it, a recording, a guest (not when the screen is part of all of them).
fn draw_wanted() -> bool {
    nested().is_none() && (!cursor::active() || cursor::composite())
}

struct Output {
    display: Display,
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
}

fn outputs() -> Result<Vec<Output>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut result = Vec::new();
    for a in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e).context("EnumAdapters1"),
        };
        for o in 0.. {
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(output) => output,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e).context("EnumOutputs"),
            };
            let desc = unsafe { output.GetDesc() }?;
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let r = desc.DesktopCoordinates;
            let name_len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
            let display = Display {
                index: result.len(),
                name: String::from_utf16_lossy(&desc.DeviceName[..name_len]),
                left: r.left,
                top: r.top,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
                primary: r.left == 0 && r.top == 0,
            };
            result.push(Output { display, adapter: adapter.clone(), output });
        }
    }
    Ok(result)
}

pub fn displays() -> Result<Vec<Display>> {
    Ok(outputs()?.into_iter().map(|o| o.display).collect())
}

pub struct DxgiCapturer {
    display: Display,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    /// GPU mode: the image stays in these (rotating) textures instead of going through RAM.
    gpu_output: bool,
    gpu_pool: Vec<ID3D11Texture2D>,
    gpu_next: usize,
    frame: Frame,
    /// The pointer as DXGI last gave it: shape (kept: DXGI gives it only when it
    /// changes) and the top-left of the shape on this screen, with its visibility.
    pointer_shape: Option<cursor::Shape>,
    pointer_at: Option<(i32, i32, bool)>,
    shape_buffer: Vec<u8>,
    /// CPU path: the pointer drawn into `frame.data`.
    drawn: cursor::Drawn,
    /// GPU path: the generation of the pointer drawn into the last texture, the
    /// desktop without it, and the piece under it read back.
    gpu_drawn: Option<u64>,
    clean: Option<ID3D11Texture2D>,
    piece: Option<ID3D11Texture2D>,
    piece_data: Vec<u8>,
}

// The D3D11 COM objects are used by one thread at a time.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    pub fn new(index: Option<usize>) -> Result<Self> {
        let mut all = outputs()?;
        let pos = match index {
            Some(i) => all.iter().position(|o| o.display.index == i),
            None => all.iter().position(|o| o.display.primary).or(Some(0)),
        }
        .filter(|&p| p < all.len())
        .ok_or_else(|| anyhow!("screen not found"))?;
        let Output { display, adapter, output } = all.swap_remove(pos);

        // VIDEO_SUPPORT is needed by the video processor that converts to NV12 for the
        // hardware encoder; if the driver does not offer it, capture still works.
        let adapter: IDXGIAdapter = adapter.cast()?;
        let create = |flags| {
            let (mut device, mut context) = (None, None);
            unsafe {
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            }
            .map(|()| (device, context))
        };
        let (device, context) = create(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT)
            .or_else(|_| create(D3D11_CREATE_DEVICE_BGRA_SUPPORT))
            .context("D3D11CreateDevice")?;
        let context: ID3D11DeviceContext = context.ok_or_else(|| anyhow!("missing D3D11 context"))?;
        // The device is shared with the encoder, which works on another thread.
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }

        let mut capturer = Self {
            device: device.ok_or_else(|| anyhow!("missing D3D11 device"))?,
            context,
            output: output.cast()?,
            duplication: None,
            staging: None,
            gpu_output: false,
            gpu_pool: Vec::new(),
            gpu_next: 0,
            frame: Frame { width: 0, height: 0, stride: 0, data: Vec::new(), captured_us: 0, texture: None },
            display,
            pointer_shape: None,
            pointer_at: None,
            shape_buffer: Vec::new(),
            drawn: cursor::Drawn::default(),
            gpu_drawn: None,
            clean: None,
            piece: None,
            piece_data: Vec::new(),
        };
        capturer.duplicate()?;
        Ok(capturer)
    }

    fn duplicate(&mut self) -> Result<()> {
        self.duplication = None;
        // After a switch to UAC or the lock screen, duplication must be created on the new desktop.
        if let Some(name) = crate::desktop::follow_input_desktop() {
            info!("capturing on desktop {name}");
        }
        let dup = unsafe { self.output.DuplicateOutput(&self.device) }
            .context("DuplicateOutput (protected screen or non-interactive session?)")?;
        let desc = unsafe { dup.GetDesc() };
        let (w, h) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        debug!(w, h, "desktop duplication started");

        let tex_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&tex_desc, None, Some(&mut staging)) }?;
        self.staging = staging;
        self.gpu_pool.clear();
        if self.gpu_output {
            let gpu_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                ..tex_desc
            };
            for _ in 0..GPU_POOL {
                let mut tex = None;
                unsafe { self.device.CreateTexture2D(&gpu_desc, None, Some(&mut tex)) }?;
                self.gpu_pool.extend(tex);
            }
        }
        self.frame.texture = None;
        self.drawn.forget();
        self.gpu_drawn = None;
        self.clean = None;
        self.frame.width = w;
        self.frame.height = h;
        // The previous image no longer matches the new size.
        self.frame.captured_us = 0;
        self.display.width = w;
        self.display.height = h;
        self.duplication = Some(dup);
        Ok(())
    }

    fn acquire(&mut self, timeout: Duration) -> Result<bool> {
        let dup = self.duplication.clone().expect("duplication initialised");
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        unsafe { dup.AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut resource) }?;
        let captured_us = nuvira_proto::now_us();

        // The pointer: its shape must be read before ReleaseFrame.
        self.follow_wanted();
        if info.LastMouseUpdateTime != 0 {
            if info.PointerShapeBufferSize > 0 {
                self.read_shape(&dup, info.PointerShapeBufferSize);
            }
            let p = info.PointerPosition;
            self.pointer_at = Some((p.Position.x, p.Position.y, p.Visible.as_bool()));
            self.publish_pointer();
        }
        let draw = draw_wanted();
        let generation = cursor::generation();

        // LastPresentTime == 0: only the pointer changed, the image is the same.
        let changed = info.LastPresentTime != 0;
        let target =
            if self.gpu_output { self.gpu_pool.get(self.gpu_next).cloned() } else { self.staging.clone() };
        // GPU path with the pointer drawn in: the desktop goes into the clean copy first.
        let into = if draw && self.gpu_output && target.is_some() {
            self.clean_texture().ok()
        } else {
            target.clone()
        };
        let copied = match (changed, resource, &into) {
            (true, Some(resource), Some(into)) => resource.cast::<ID3D11Texture2D>().map(|texture| {
                unsafe { self.context.CopyResource(into, &texture) };
            }),
            _ => Ok(()),
        };
        // ReleaseFrame must be called in any case, even if the copy failed.
        unsafe { dup.ReleaseFrame() }?;
        copied?;

        let drawn = if self.gpu_output { self.gpu_drawn } else { self.drawn.generation() };
        let redraw = if draw { drawn != Some(generation) } else { drawn.is_some() };
        // Nothing new, or no picture yet to draw the pointer on.
        if !changed && (!redraw || self.frame.captured_us == 0) {
            return Ok(false);
        }

        if self.gpu_output {
            let Some(target) = target else { return Ok(false) };
            if draw {
                // Turned on without a new picture: the last one has no pointer.
                if !changed && self.gpu_drawn.is_none() {
                    let Some(last) = self.frame.texture.clone() else { return Ok(false) };
                    let clean = self.clean_texture()?;
                    unsafe { self.context.CopyResource(&clean, &last) };
                }
                let clean = self.clean_texture()?;
                unsafe { self.context.CopyResource(&target, &clean) };
                self.draw_on_texture(&target)?;
                self.gpu_drawn = Some(generation);
            } else {
                if !changed {
                    // Taken out again: the clean copy without it.
                    let Some(clean) = self.clean.clone() else { return Ok(false) };
                    unsafe { self.context.CopyResource(&target, &clean) };
                }
                self.clean = None;
                self.gpu_drawn = None;
            }
            // The image stays on the GPU: no copy to RAM.
            self.frame.texture = Some(target);
            self.gpu_next = (self.gpu_next + 1) % self.gpu_pool.len().max(1);
            self.frame.data.clear();
            self.frame.stride = self.frame.width as usize * 4;
            self.frame.captured_us = captured_us;
            return Ok(true);
        }

        if changed {
            let staging = self.staging.as_ref().expect("staging initialised");
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe { self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;
            let stride = mapped.RowPitch as usize;
            let len = stride * self.frame.height as usize;
            // SAFETY: Map returns RowPitch * height readable bytes until Unmap.
            let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
            self.frame.data.clear();
            self.frame.data.extend_from_slice(src);
            unsafe { self.context.Unmap(staging, 0) };
            self.frame.stride = stride;
            // A new picture: what the pointer covered went with the old one.
            self.drawn.forget();
        } else {
            self.drawn.undo(&mut self.frame.data, self.frame.stride);
        }
        if draw {
            let (w, h, stride) = (self.frame.width, self.frame.height, self.frame.stride);
            self.drawn.draw(&mut self.frame.data, w, h, stride, false);
        }
        self.frame.captured_us = captured_us;
        Ok(true)
    }

    /// The viewers asked for the pointer apart (or no longer): DXGI can give it.
    /// The shared state is emptied on a change, so what DXGI gave is published again.
    fn follow_wanted(&mut self) {
        let wanted = cursor::wanted();
        if cursor::active() != wanted {
            cursor::set_active(wanted);
            self.publish_pointer();
        }
    }

    /// The new pointer shape, as BGRA.
    fn read_shape(&mut self, dup: &IDXGIOutputDuplication, size: u32) {
        self.shape_buffer.resize(size as usize, 0);
        let mut needed = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let read = unsafe {
            dup.GetFramePointerShape(size, self.shape_buffer.as_mut_ptr().cast(), &mut needed, &mut info)
        };
        if let Err(e) = read {
            debug!("GetFramePointerShape: {e}");
            return;
        }
        let buffer = &self.shape_buffer[..needed.min(size) as usize];
        let Some((width, height, bgra)) =
            cursor::from_windows(info.Type, info.Width, info.Height, info.Pitch, buffer)
        else {
            debug!(kind = info.Type, "pointer shape not understood");
            return;
        };
        let hot_x = (info.HotSpot.x.max(0) as u32).min(width - 1);
        let hot_y = (info.HotSpot.y.max(0) as u32).min(height - 1);
        if self.pointer_shape.is_none() {
            info!(width, height, kind = info.Type, "pointer shape from DXGI");
        } else {
            debug!(width, height, kind = info.Type, "new pointer shape");
        }
        let serial = cursor::serial_of(width, height, hot_x, hot_y, &bgra);
        self.pointer_shape =
            Some(cursor::Shape { serial, width, height, hot_x, hot_y, bgra: std::sync::Arc::new(bgra) });
    }

    /// Shape and position, in the coordinates of the picture (all the screens'
    /// when this one is part of them).
    fn publish_pointer(&self) {
        let Some((x, y, visible)) = self.pointer_at else { return };
        let index = self.display.index;
        if visible {
            OWNER.store(index, Ordering::Relaxed);
        } else if OWNER.load(Ordering::Relaxed) != index {
            // On another screen, which says where it is.
            return;
        }
        let origin = nested().unwrap_or((self.display.left, self.display.top));
        let shape = self.pointer_shape.as_ref();
        let (hot_x, hot_y, serial) = shape.map_or((0, 0, 0), |s| (s.hot_x as i32, s.hot_y as i32, s.serial));
        // Published again only when the shared state does not have it (a new
        // shape, or emptied).
        let fresh =
            shape.filter(|s| cursor::latest().0.as_ref().map(|p| p.serial) != Some(s.serial)).cloned();
        cursor::publish(
            fresh,
            cursor::Position {
                x: self.display.left + x + hot_x - origin.0,
                y: self.display.top + y + hot_y - origin.1,
                visible: visible && shape.is_some(),
                serial,
            },
        );
    }

    /// GPU path: the desktop without the pointer, while it is drawn in.
    fn clean_texture(&mut self) -> Result<ID3D11Texture2D> {
        if let Some(clean) = &self.clean {
            return Ok(clean.clone());
        }
        let first = self.gpu_pool.first().context("no GPU textures")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { first.GetDesc(&mut desc) };
        let mut clean = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut clean)) }?;
        let clean = clean.context("clean texture")?;
        self.clean = Some(clean.clone());
        Ok(clean)
    }

    /// GPU path: the piece of `target` under the pointer, read back, drawn on and
    /// written again (at most 256x256 pixels).
    fn draw_on_texture(&mut self, target: &ID3D11Texture2D) -> Result<()> {
        let Some((x, y, w, h)) = cursor::bounds(self.frame.width, self.frame.height) else { return Ok(()) };
        if self.piece.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: cursor::MAX_SIDE,
                Height: cursor::MAX_SIDE,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut piece = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut piece)) }?;
            self.piece = piece;
        }
        let piece = self.piece.clone().context("pointer texture")?;
        let (w, h) = (w.min(cursor::MAX_SIDE), h.min(cursor::MAX_SIDE));
        let area = D3D11_BOX { left: x, top: y, front: 0, right: x + w, bottom: y + h, back: 1 };
        unsafe { self.context.CopySubresourceRegion(&piece, 0, 0, 0, 0, target, 0, Some(&area)) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(&piece, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;
        let row = w as usize * 4;
        self.piece_data.clear();
        for r in 0..h as usize {
            // SAFETY: the mapped piece has RowPitch bytes for each of its 256 rows.
            let src = unsafe {
                std::slice::from_raw_parts((mapped.pData as *const u8).add(r * mapped.RowPitch as usize), row)
            };
            self.piece_data.extend_from_slice(src);
        }
        unsafe { self.context.Unmap(&piece, 0) };
        cursor::draw_region(&mut self.piece_data, x as i32, y as i32, w, h, row, false);
        unsafe {
            self.context.UpdateSubresource(
                target,
                0,
                Some(&area),
                self.piece_data.as_ptr().cast(),
                row as u32,
                0,
            );
        }
        Ok(())
    }
}

impl Capturer for DxgiCapturer {
    fn display(&self) -> &Display {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<&Frame>> {
        if self.duplication.is_none() {
            self.duplicate()?;
        }
        // In GPU mode the device is shared with the encoder and AcquireNextFrame holds
        // its lock for the whole wait: on a still screen conversion and encoding were
        // blocked for up to 100 ms. It is polled without waiting, at short intervals.
        let deadline = std::time::Instant::now() + timeout;
        let wait = if self.gpu_output { Duration::ZERO } else { timeout };
        let result = loop {
            let result = self.acquire(wait);
            let timed_out = matches!(&result, Err(e) if e.downcast_ref::<windows::core::Error>()
                .is_some_and(|e| e.code() == DXGI_ERROR_WAIT_TIMEOUT));
            if timed_out && self.gpu_output && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            break result;
        };
        match result {
            Ok(true) => Ok(Some(&self.frame)),
            Ok(false) => Ok(None),
            Err(e) => match e.downcast_ref::<windows::core::Error>().map(|e| e.code()) {
                Some(code) if code == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
                // Resolution change, secure desktop (UAC), lock screen: it is recreated.
                Some(code) if code == DXGI_ERROR_ACCESS_LOST => {
                    // Normal at every desktop switch: the switch is logged in duplicate().
                    debug!("duplication access lost, recreating it");
                    self.duplication = None;
                    Ok(None)
                }
                _ => Err(e),
            },
        }
    }

    fn last_frame(&self) -> Option<&Frame> {
        (self.frame.captured_us != 0).then_some(&self.frame)
    }

    fn enable_gpu_output(&mut self) -> Option<ID3D11Device> {
        if !self.gpu_output {
            self.gpu_output = true;
            // Duplication is recreated to allocate the GPU textures.
            self.duplication = None;
        }
        Some(self.device.clone())
    }
}
