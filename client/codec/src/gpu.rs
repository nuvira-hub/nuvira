//! Immagini che restano sulla GPU (Windows, Direct3D 11).
//!
//! La cattura DXGI produce texture BGRA; il processore video della scheda
//! (D3D11 Video Processor) le converte in NV12 e l'encoder hardware le legge
//! direttamente, senza passare dalla RAM.

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

/// Device Direct3D 11 condiviso tra cattura ed encoder.
#[derive(Clone)]
pub struct GpuDevice(pub ID3D11Device);

/// Texture BGRA di un frame catturato.
#[derive(Clone)]
pub struct GpuTexture(pub ID3D11Texture2D);

// Il device è free-threaded e il contesto immediato è protetto (SetMultithreadProtected):
// i riferimenti possono passare da un thread all'altro.
unsafe impl Send for GpuDevice {}
unsafe impl Sync for GpuDevice {}
unsafe impl Send for GpuTexture {}
unsafe impl Sync for GpuTexture {}

/// Texture NV12 di uscita a rotazione: l'encoder ne tiene qualcuna finché non ha finito.
const NV12_POOL: usize = 4;

/// Spazio colore del processore video: bit 4-5 = gamma nominale (1 = 16-235),
/// bit 2 = matrice (0 = BT.601), bit 1 = gamma RGB (0 = completa 0-255).
const COLOR_RGB_FULL: u32 = 0;
const COLOR_YUV_BT601_LIMITED: u32 = 1 << 4;

/// Conversione BGRA → NV12 sulla GPU.
pub struct Nv12Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    /// Viste di input per le texture della cattura (sempre le stesse, a rotazione).
    input_views: Vec<(usize, ID3D11VideoProcessorInputView)>,
}

impl Nv12Converter {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let video_device: ID3D11VideoDevice =
            device.cast().context("la scheda non offre il processore video D3D11")?;
        let context = unsafe { device.GetImmediateContext() }?;
        let video_context: ID3D11VideoContext = context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }?;
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: COLOR_RGB_FULL },
            );
            video_context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: COLOR_YUV_BT601_LIMITED },
            );
            // Niente "miglioramenti" automatici del driver: vogliamo una conversione fedele.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }

        let mut outputs = Vec::with_capacity(NV12_POOL);
        for _ in 0..NV12_POOL {
            let texture = create_nv12(device, width, height)?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view = None;
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &view_desc,
                    Some(&mut view),
                )
            }?;
            outputs.push((texture, view.ok_or_else(|| anyhow!("vista di uscita assente"))?));
        }
        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            outputs,
            next: 0,
            input_views: Vec::new(),
        })
    }

    fn input_view(&mut self, texture: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = texture.as_raw() as usize;
        if let Some((_, view)) = self.input_views.iter().find(|(k, _)| *k == key) {
            return Ok(view.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(texture, &self.enumerator, &desc, Some(&mut view))
        }?;
        let view = view.ok_or_else(|| anyhow!("vista di ingresso assente"))?;
        // Le texture della cattura sono poche; se cambiano (nuova risoluzione) si riparte.
        if self.input_views.len() >= 8 {
            self.input_views.clear();
        }
        self.input_views.push((key, view.clone()));
        Ok(view)
    }

    /// Converte `bgra` nella prossima texture NV12 libera e la restituisce.
    pub fn convert(&mut self, bgra: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let input = self.input_view(bgra)?;
        let (texture, output) = self.outputs[self.next].clone();
        self.next = (self.next + 1) % self.outputs.len();
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input)),
            ..Default::default()
        };
        let result = unsafe {
            self.video_context.VideoProcessorBlt(&self.processor, &output, 0, std::slice::from_ref(&stream))
        };
        // SAFETY: il riferimento alla vista va rilasciato una volta sola.
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("VideoProcessorBlt")?;
        Ok(texture)
    }
}

fn create_nv12(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // Non tutti i driver accettano il flag VIDEO_ENCODER: senza, l'encoder copia internamente.
    if unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.is_err() {
        desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.context("texture NV12")?;
    }
    texture.ok_or_else(|| anyhow!("texture NV12 assente"))
}

/// Device D3D11 sulla scheda predefinita, pronto per processore video e condivisione tra thread.
/// Usato dalla prova degli encoder, quando la cattura non è ancora aperta.
pub fn create_device() -> Result<ID3D11Device> {
    let (mut device, mut context) = (None, None);
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;
    let context: ID3D11DeviceContext = context.ok_or_else(|| anyhow!("context D3D11 assente"))?;
    if let Ok(mt) = context.cast::<ID3D11Multithread>() {
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    device.ok_or_else(|| anyhow!("device D3D11 assente"))
}

/// Texture BGRA con il contenuto indicato (righe compatte, `width * 4` byte).
pub fn upload_bgra(device: &ID3D11Device, width: u32, height: u32, data: &[u8]) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init =
        D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr().cast(), SysMemPitch: width * 4, SysMemSlicePitch: 0 };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }?;
    texture.ok_or_else(|| anyhow!("texture BGRA assente"))
}
