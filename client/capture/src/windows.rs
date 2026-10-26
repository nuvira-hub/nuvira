//! Cattura con DXGI Desktop Duplication.
//!
//! La GPU fornisce una texture solo quando lo schermo cambia; la copiamo in una
//! texture di staging leggibile dalla CPU. In una fase successiva la texture
//! resterà sulla GPU e andrà direttamente all'encoder hardware.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, warn};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use crate::{Capturer, Display, Frame};

/// Texture GPU a rotazione: mentre l'encoder legge l'una, la cattura scrive l'altra.
const GPU_POOL: usize = 3;

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
    /// Modalità GPU: l'immagine resta in queste texture (a rotazione) invece di passare in RAM.
    gpu_output: bool,
    gpu_pool: Vec<ID3D11Texture2D>,
    gpu_next: usize,
    frame: Frame,
}

// Gli oggetti COM di D3D11 sono usati da un solo thread alla volta.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    pub fn new(index: Option<usize>) -> Result<Self> {
        let mut all = outputs()?;
        let pos = match index {
            Some(i) => all.iter().position(|o| o.display.index == i),
            None => all.iter().position(|o| o.display.primary).or(Some(0)),
        }
        .filter(|&p| p < all.len())
        .ok_or_else(|| anyhow!("schermo non trovato"))?;
        let Output { display, adapter, output } = all.swap_remove(pos);

        // VIDEO_SUPPORT serve al processore video che converte in NV12 per l'encoder
        // hardware; se il driver non lo offre la cattura funziona comunque.
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
        let context: ID3D11DeviceContext = context.ok_or_else(|| anyhow!("context D3D11 assente"))?;
        // Il device viene condiviso con l'encoder, che lavora su un altro thread.
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }

        let mut capturer = Self {
            device: device.ok_or_else(|| anyhow!("device D3D11 assente"))?,
            context,
            output: output.cast()?,
            duplication: None,
            staging: None,
            gpu_output: false,
            gpu_pool: Vec::new(),
            gpu_next: 0,
            frame: Frame { width: 0, height: 0, stride: 0, data: Vec::new(), captured_us: 0, texture: None },
            display,
        };
        capturer.duplicate()?;
        Ok(capturer)
    }

    fn duplicate(&mut self) -> Result<()> {
        self.duplication = None;
        let dup = unsafe { self.output.DuplicateOutput(&self.device) }
            .context("DuplicateOutput (schermo protetto o sessione non interattiva?)")?;
        let desc = unsafe { dup.GetDesc() };
        let (w, h) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        debug!(w, h, "desktop duplication avviata");

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
        self.frame.width = w;
        self.frame.height = h;
        // L'immagine precedente non corrisponde più alle nuove dimensioni.
        self.frame.captured_us = 0;
        self.display.width = w;
        self.display.height = h;
        self.duplication = Some(dup);
        Ok(())
    }

    fn acquire(&mut self, timeout: Duration) -> Result<bool> {
        let dup = self.duplication.as_ref().expect("duplication inizializzata");
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        unsafe { dup.AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut resource) }?;
        let captured_us = nexodesk_proto::now_us();

        // LastPresentTime == 0: è cambiato solo il puntatore, l'immagine è la stessa.
        let changed = info.LastPresentTime != 0;
        let target =
            if self.gpu_output { self.gpu_pool.get(self.gpu_next).cloned() } else { self.staging.clone() };
        let copied = match (changed, resource, &target) {
            (true, Some(resource), Some(target)) => resource.cast::<ID3D11Texture2D>().map(|texture| {
                unsafe { self.context.CopyResource(target, &texture) };
            }),
            _ => Ok(()),
        };
        // ReleaseFrame va chiamato in ogni caso, anche se la copia è fallita.
        unsafe { dup.ReleaseFrame() }?;
        copied?;
        if !changed {
            return Ok(false);
        }

        if self.gpu_output {
            // L'immagine resta sulla GPU: nessuna copia in RAM.
            self.frame.texture = target;
            self.gpu_next = (self.gpu_next + 1) % self.gpu_pool.len().max(1);
            self.frame.data.clear();
            self.frame.stride = self.frame.width as usize * 4;
            self.frame.captured_us = captured_us;
            return Ok(true);
        }

        let staging = self.staging.as_ref().expect("staging inizializzata");
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;
        let stride = mapped.RowPitch as usize;
        let len = stride * self.frame.height as usize;
        // SAFETY: Map restituisce RowPitch * altezza byte leggibili fino a Unmap.
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
        self.frame.data.clear();
        self.frame.data.extend_from_slice(src);
        unsafe { self.context.Unmap(staging, 0) };
        self.frame.stride = stride;
        self.frame.captured_us = captured_us;
        Ok(true)
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
        // In modalità GPU il device è condiviso con l'encoder e AcquireNextFrame tiene il
        // suo lock per tutta l'attesa: con lo schermo fermo conversione e compressione
        // restavano bloccate fino a 100 ms. Si interroga senza attendere, a brevi intervalli.
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
                // Cambio risoluzione, desktop sicuro (UAC), blocco schermo: si ricrea.
                Some(code) if code == DXGI_ERROR_ACCESS_LOST => {
                    warn!("accesso alla duplicazione perso, la ricreo");
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
            // Si ricrea la duplicazione per allocare le texture GPU.
            self.duplication = None;
        }
        Some(self.device.clone())
    }
}
