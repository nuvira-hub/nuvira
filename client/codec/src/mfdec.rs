//! H.264 decoding on the graphics card through Media Foundation: Windows' H.264
//! decoder with a D3D11 device decodes with DXVA (Intel, NVIDIA, AMD alike).
//!
//! Two ways out of the decoder:
//! - [`MfDecoder::native`], when the viewer draws with Direct3D 12 on the same
//!   adapter: the picture is copied on the GPU into an NV12 texture shared through
//!   an NT handle ([`SharedSurface`]), which the renderer opens on its own device
//!   and draws as it is. Nothing goes through RAM.
//! - [`MfDecoder::new`], for the other renderers: the NV12 texture is copied to a
//!   staging texture and read back into the I420 [`YuvFrame`].
//!
//! Low-latency mode is required: without it the decoder holds frames back to
//! reorder them, and the image would arrive several frames late.
//!
//! Not the default (`NUVIRA_DECODER=hardware` selects it). With the readback it
//! costs more than it saves: at 1080p (example `decbench`) 9.7 ms per frame on an
//! Intel UHD (i7-10710U) and 7.3 ms on an Intel HD Graphics 2500, against 4.5 and
//! 4.2 ms for OpenH264, the copy and map alone 6 ms. Drawn directly (NUC, Intel
//! UHD, 1080p 60 fps synthetic stream, 20 s, 2026-11-14): 6.4-7.6 s of viewer CPU
//! against 7.9-8.5 for OpenH264, decoding 2.8-3.2 ms against 3.8-4.1, the same
//! latency and frame rate, identical colours on screen. On a fast processor it
//! loses: PC-FEDERICO (Ryzen 7 5700X, RTX 4060 Ti, same test) 5.5-6.4 s against
//! 3.6-5.4 for OpenH264, decoding 3.7-5.2 ms against 1.3-2.7, latency 32-46 ms
//! against 27-31.

use anyhow::{Context, Result, anyhow, bail};
use tracing::debug;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use crate::{VideoDecoder, YuvFrame, gpu, mf};

/// A decoded picture shared with the renderer's Direct3D 12 device: an NV12
/// texture of the decoder's device, of the visible size, opened there through
/// its NT handle. The decoder writes into it only when nobody holds it.
pub struct SharedSurface {
    texture: ID3D11Texture2D,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    handle: windows::Win32::Foundation::HANDLE,
    pub width: u32,
    pub height: u32,
    /// The adapter of the decoder's device: the renderer opens the texture only
    /// on the same one.
    pub luid: (u32, i32),
}

// SAFETY: D3D11 objects of a multithread-protected device; the handle is a
// kernel handle, valid from any thread.
unsafe impl Send for SharedSurface {}
unsafe impl Sync for SharedSurface {}

impl SharedSurface {
    /// The NT handle of the texture (owned by this surface).
    pub fn handle(&self) -> windows::Win32::Foundation::HANDLE {
        self.handle
    }

    /// Its pixels into I420 planes (through a staging texture).
    pub fn read(&self, frame: &mut YuvFrame) -> Result<()> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }?;
        let staging = staging.ok_or_else(|| anyhow!("missing staging texture"))?;
        unsafe {
            self.context.CopyResource(&staging, &self.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).context("Map")?;
            let pitch = mapped.RowPitch as usize;
            let total = pitch * self.height as usize * 3 / 2;
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, total);
            nv12_to_i420(frame, self.width, self.height, data, pitch, pitch * self.height as usize);
            self.context.Unmap(&staging, 0);
        }
        Ok(())
    }
}

impl Drop for SharedSurface {
    fn drop(&mut self) {
        // SAFETY: the handle made by CreateSharedHandle, closed once.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// The adapter LUID of a D3D11 device.
fn adapter_luid(device: &ID3D11Device) -> Result<(u32, i32)> {
    let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device.cast()?;
    let adapter = unsafe { dxgi.GetAdapter() }?;
    let desc = unsafe { adapter.GetDesc() }?;
    Ok((desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart))
}

pub struct MfDecoder {
    transform: IMFTransform,
    context: ID3D11DeviceContext,
    device: ID3D11Device,
    _manager: IMFDXGIDeviceManager,
    /// Readback texture, with the size of the decoder's textures.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Size of the decoder's images (aligned, e.g. 1920x1088) and visible part.
    coded: (u32, u32),
    visible: (u32, u32),
    provides_samples: bool,
    time: i64,
    frame: YuvFrame,
    /// Pictures handed to the renderer as shared textures ([`MfDecoder::native`]).
    native: bool,
    shared: Vec<std::sync::Arc<SharedSurface>>,
    /// Waits for the copy into the shared texture before handing it on.
    done: Option<ID3D11Query>,
}

// Used by one thread at a time; MF and D3D11 objects are MTA (the device is
// multithread protected, see `gpu::create_device`).
unsafe impl Send for MfDecoder {}

impl MfDecoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        mf::startup()?;
        let transform = create_transform()?;
        unsafe {
            let attrs = transform.GetAttributes()?;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                bail!("the H.264 decoder does not take a D3D11 device");
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let device = gpu::create_device()?;
        let context = unsafe { device.GetImmediateContext() }?;
        let manager = unsafe {
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(|| anyhow!("missing DXGI manager"))?;
            manager.ResetDevice(&device, token)?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("the decoder refuses the D3D11 device (no DXVA)")?;
            manager
        };
        let api: ICodecAPI = transform.cast().context("ICodecAPI")?;
        unsafe { api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(1u32)) }
            .context("the decoder has no low-latency mode")?;
        unsafe {
            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &inp, 0).context("H.264 input type")?;
        }
        let mut dec = Self {
            transform,
            context,
            device,
            _manager: manager,
            staging: None,
            coded: (0, 0),
            visible: (0, 0),
            provides_samples: false,
            time: 0,
            frame: YuvFrame::default(),
            native: false,
            shared: Vec::new(),
            done: None,
        };
        // Before the first SPS the decoder may not offer NV12 yet: it does at the stream change.
        let _ = dec.set_output_type();
        unsafe {
            dec.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            dec.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(dec)
    }

    /// Picks NV12 among the output types and reads the image size.
    fn set_output_type(&mut self) -> Result<()> {
        let mut i = 0;
        let chosen = loop {
            let t = unsafe { self.transform.GetOutputAvailableType(0, i) }.context("no NV12 output")?;
            if unsafe { t.GetGUID(&MF_MT_SUBTYPE) }? == MFVideoFormat_NV12 {
                break t;
            }
            i += 1;
        };
        unsafe { self.transform.SetOutputType(0, &chosen, 0) }.context("NV12 output type")?;
        let size = unsafe { chosen.GetUINT64(&MF_MT_FRAME_SIZE) }?;
        self.coded = ((size >> 32) as u32, size as u32);
        self.visible = self.coded;
        let mut area = MFVideoArea::default();
        let ok = unsafe {
            chosen.GetBlob(
                &MF_MT_MINIMUM_DISPLAY_APERTURE,
                std::slice::from_raw_parts_mut(
                    (&mut area as *mut MFVideoArea).cast::<u8>(),
                    size_of::<MFVideoArea>(),
                ),
                None,
            )
        };
        if ok.is_ok() && area.Area.cx > 0 && area.Area.cy > 0 {
            self.visible = (area.Area.cx as u32, area.Area.cy as u32);
        }
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }?;
        self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        debug!(coded = ?self.coded, visible = ?self.visible, "hardware decoder output");
        Ok(())
    }

    /// Takes one decoded image out of the decoder, if there is one.
    fn pull(&mut self) -> Result<bool> {
        let sample = if self.provides_samples {
            None
        } else {
            let (w, h) = self.coded;
            unsafe {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(w * h * 3 / 2)?)?;
                Some(sample)
            }
        };
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0;
        let result =
            unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
        // SAFETY: the references returned in the structure are released only once.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { std::mem::ManuallyDrop::take(&mut buffer.pEvents) });
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.set_output_type()?;
                return self.pull();
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
            Err(e) => return Err(e).context("ProcessOutput"),
        }
        let Some(sample) = sample else { return Ok(false) };
        let media = unsafe { sample.GetBufferByIndex(0) }?;
        match media.cast::<IMFDXGIBuffer>() {
            Ok(dxgi) => self.read_texture(&dxgi)?,
            Err(_) => self.read_memory(&media)?,
        }
        Ok(true)
    }

    /// Pictures stay on the GPU, in textures shared with a Direct3D 12 renderer
    /// on the same adapter ([`YuvFrame::shared`]).
    pub fn native() -> Result<Self> {
        let mut decoder = Self::new()?;
        decoder.native = true;
        Ok(decoder)
    }

    /// A shared texture of the visible size nobody else holds (at most a few).
    fn free_shared(&mut self) -> Result<std::sync::Arc<SharedSurface>> {
        let (w, h) = self.visible;
        self.shared.retain(|s| (s.width, s.height) == (w, h));
        if let Some(s) = self.shared.iter().find(|s| std::sync::Arc::strong_count(s) == 1) {
            return Ok(s.clone());
        }
        if self.shared.len() >= 6 {
            bail!("all the shared textures still in use");
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        let mut texture = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("shared NV12 texture")?;
        let texture = texture.ok_or_else(|| anyhow!("missing shared texture"))?;
        let resource: windows::Win32::Graphics::Dxgi::IDXGIResource1 = texture.cast()?;
        let handle = unsafe {
            resource.CreateSharedHandle(
                None,
                windows::Win32::Graphics::Dxgi::DXGI_SHARED_RESOURCE_READ.0
                    | windows::Win32::Graphics::Dxgi::DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            )
        }
        .context("shared handle")?;
        let surface = std::sync::Arc::new(SharedSurface {
            texture,
            device: self.device.clone(),
            context: self.context.clone(),
            handle,
            width: w,
            height: h,
            luid: adapter_luid(&self.device)?,
        });
        debug!(w, h, count = self.shared.len() + 1, "shared NV12 texture");
        self.shared.push(surface.clone());
        Ok(surface)
    }

    /// Decoded image on the GPU, copied (on the GPU) into a shared texture.
    fn share_texture(&mut self, texture: &ID3D11Texture2D, index: u32) -> Result<()> {
        let shared = self.free_shared()?;
        let (w, h) = self.visible;
        let area = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        unsafe {
            self.context.CopySubresourceRegion(&shared.texture, 0, 0, 0, 0, texture, index, Some(&area))
        };
        // The copy done before the other device reads it.
        if self.done.is_none() {
            let desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
            let mut query = None;
            unsafe { self.device.CreateQuery(&desc, Some(&mut query)) }?;
            self.done = query;
        }
        if let Some(query) = &self.done {
            unsafe {
                self.context.End(query);
                self.context.Flush();
                let mut finished = windows::core::BOOL(0);
                let start = std::time::Instant::now();
                while self
                    .context
                    .GetData(
                        query,
                        Some((&raw mut finished).cast()),
                        std::mem::size_of::<windows::core::BOOL>() as u32,
                        0,
                    )
                    .is_err()
                    || !finished.as_bool()
                {
                    if start.elapsed() > std::time::Duration::from_millis(100) {
                        break;
                    }
                    std::thread::yield_now();
                }
            }
        }
        self.frame.width = w;
        self.frame.height = h;
        self.frame.y.clear();
        self.frame.u.clear();
        self.frame.v.clear();
        self.frame.shared = Some(shared);
        Ok(())
    }

    /// Decoded image on the GPU: copied to the staging texture and read.
    fn read_texture(&mut self, buffer: &IMFDXGIBuffer) -> Result<()> {
        let mut raw = std::ptr::null_mut();
        unsafe { buffer.GetResource(&ID3D11Texture2D::IID, &mut raw) }?;
        // SAFETY: GetResource returned a reference to an ID3D11Texture2D.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let index = unsafe { buffer.GetSubresourceIndex() }?;
        if self.native {
            return self.share_texture(&texture, index);
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if self.staging.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (desc.Width, desc.Height)) {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            unsafe { self.device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }?;
            let staging = staging.ok_or_else(|| anyhow!("missing staging texture"))?;
            self.staging = Some((staging, desc.Width, desc.Height));
        }
        let (staging, _, tex_height) = self.staging.as_ref().expect("staging texture");
        unsafe {
            self.context.CopySubresourceRegion(staging, 0, 0, 0, 0, &texture, index, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).context("Map")?;
            let pitch = mapped.RowPitch as usize;
            let total = pitch * *tex_height as usize * 3 / 2;
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, total);
            let uv_offset = pitch * *tex_height as usize;
            let (w, h) = self.visible;
            nv12_to_i420(&mut self.frame, w, h, data, pitch, uv_offset);
            self.context.Unmap(staging, 0);
        }
        Ok(())
    }

    /// Decoded image in RAM (decoder without DXVA for this stream).
    fn read_memory(&mut self, media: &IMFMediaBuffer) -> Result<()> {
        let (w, h) = self.visible;
        let coded_height = self.coded.1 as usize;
        if let Ok(b2) = media.cast::<IMF2DBuffer>() {
            let mut ptr = std::ptr::null_mut();
            let mut pitch = 0i32;
            unsafe { b2.Lock2D(&mut ptr, &mut pitch) }?;
            let pitch = pitch.unsigned_abs() as usize;
            let data = unsafe { std::slice::from_raw_parts(ptr, pitch * coded_height * 3 / 2) };
            nv12_to_i420(&mut self.frame, w, h, data, pitch, pitch * coded_height);
            unsafe { b2.Unlock2D() }?;
        } else {
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            unsafe { media.Lock(&mut ptr, None, Some(&mut len)) }?;
            let pitch = self.coded.0 as usize;
            let data = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            nv12_to_i420(&mut self.frame, w, h, data, pitch, pitch * coded_height);
            unsafe { media.Unlock() }?;
        }
        Ok(())
    }
}

impl VideoDecoder for MfDecoder {
    fn name(&self) -> &'static str {
        "mediafoundation"
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<&YuvFrame>> {
        let sample = unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.time)?;
            sample
        };
        self.time += 1;
        let mut got = false;
        if let Err(e) = unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            if e.code() != MF_E_NOTACCEPTING {
                return Err(e).context("ProcessInput");
            }
            // Images still inside: taken out, then the frame goes in.
            while self.pull()? {
                got = true;
            }
            unsafe { self.transform.ProcessInput(0, &sample, 0) }.context("ProcessInput")?;
        }
        // Everything ready is taken out: the last image is the one to show.
        while self.pull()? {
            got = true;
        }
        Ok(got.then_some(&self.frame))
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// Windows' H.264 decoder (a synchronous MFT that uses DXVA when given a device).
fn create_transform() -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut list,
            &mut count,
        )?;
    }
    if list.is_null() {
        bail!("no H.264 decoder");
    }
    // SAFETY: MFTEnumEx returns an array of `count` pointers allocated with CoTaskMemAlloc.
    let items = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
    let first = items.iter_mut().find_map(|a| a.take());
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    let activate = first.ok_or_else(|| anyhow!("no H.264 decoder"))?;
    unsafe { activate.ActivateObject() }.context("ActivateObject")
}

/// NV12 (`pitch` bytes per row, UV plane at `uv_offset`) to packed I420 of `w`x`h`.
fn nv12_to_i420(frame: &mut YuvFrame, w: u32, h: u32, data: &[u8], pitch: usize, uv_offset: usize) {
    let (w, h) = (w as usize, h as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    frame.width = w as u32;
    frame.height = h as u32;
    frame.y.resize(w * h, 0);
    frame.u.resize(cw * ch, 0);
    frame.v.resize(cw * ch, 0);
    for row in 0..h {
        frame.y[row * w..][..w].copy_from_slice(&data[row * pitch..][..w]);
    }
    for row in 0..ch {
        let src = &data[uv_offset + row * pitch..][..cw * 2];
        let (u, v) = (&mut frame.u[row * cw..][..cw], &mut frame.v[row * cw..][..cw]);
        for (i, pair) in src.as_chunks::<2>().0.iter().enumerate() {
            u[i] = pair[0];
            v[i] = pair[1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_planes() {
        // 4x2 image, pitch 6: Y rows, then one UV row (U,V,U,V).
        let data = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0, 10, 20, 11, 21, 0, 0];
        let mut f = YuvFrame::default();
        nv12_to_i420(&mut f, 4, 2, &data, 6, 12);
        assert_eq!(f.y, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(f.u, [10, 11]);
        assert_eq!(f.v, [20, 21]);
    }
}
