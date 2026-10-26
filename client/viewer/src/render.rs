//! Visualizzazione dei frame I420 con wgpu.
//!
//! I tre piani Y, U, V vengono caricati come texture R8 e convertiti in RGB
//! dallo shader (BT.601, gamma limitata). L'immagine è adattata alla finestra
//! mantenendo le proporzioni.

use std::sync::Arc;

use anyhow::{Context, Result};
use nexo_codec::YuvFrame;
use winit::window::Window;

const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    // Triangolo che copre tutto il viewport.
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(tex_y, samp, in.uv).r - 16.0 / 255.0) * 1.164383;
    let u = textureSample(tex_u, samp, in.uv).r - 0.5;
    let v = textureSample(tex_v, samp, in.uv).r - 0.5;
    let rgb = vec3<f32>(
        y + 1.596027 * v,
        y - 0.391762 * u - 0.812968 * v,
        y + 2.017232 * u,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// Rettangolo occupato dall'immagine nella finestra, in pixel fisici.
#[derive(Debug, Clone, Copy, Default)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    /// Posizione normalizzata 0..1 nell'immagine, se il punto cade al suo interno.
    pub fn normalize(&self, px: f64, py: f64) -> Option<(f32, f32)> {
        if self.width <= 0.0 || self.height <= 0.0 {
            return None;
        }
        let x = (px as f32 - self.x) / self.width;
        let y = (py as f32 - self.y) / self.height;
        ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some((x, y))
    }
}

/// API grafica nativa di ogni sistema, con OpenGL come ripiego dove esiste.
/// Su Windows Vulkan è escluso: alcuni driver Intel vanno in crash già all'enumerazione.
/// `WGPU_BACKEND` (es. `vulkan`, `gl`) permette comunque di forzarne uno.
fn native_backends() -> wgpu::Backends {
    if cfg!(windows) {
        wgpu::Backends::DX12 | wgpu::Backends::GL
    } else if cfg!(target_os = "macos") {
        wgpu::Backends::METAL
    } else {
        wgpu::Backends::VULKAN | wgpu::Backends::GL
    }
}

struct Planes {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    planes: Option<Planes>,
    pub adapter_info: String,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        if std::env::var_os("WGPU_BACKEND").is_none() {
            desc.backends = native_backends();
        }
        let instance = wgpu::Instance::new(desc);
        let surface = instance.create_surface(window).context("superficie wgpu")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("nessuna scheda video compatibile")?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .context("dispositivo wgpu")?;

        let caps = surface.get_capabilities(&adapter);
        // Lo shader produce valori già in gamma sRGB: serve un formato non-sRGB,
        // altrimenti la GPU li convertirebbe una seconda volta (immagine slavata).
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        // Mailbox: nessuno strappo e nessuna coda di frame vecchi. Altrimenti Fifo (sempre presente).
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("configurazione superficie")?;
        config.format = format;
        config.present_mode = present_mode;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Ok(Self { surface, device, queue, config, pipeline, layout, sampler, planes: None, adapter_info })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    fn create_planes(&self, width: u32, height: u32) -> Planes {
        let plane = |label, w, h| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let (y, u, v) = (plane("y", width, height), plane("u", cw, ch), plane("v", cw, ch));
        let view = |t: &wgpu::Texture| t.create_view(&Default::default());
        let (vy, vu, vv) = (view(&y), view(&u), view(&v));
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&vy) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&vu) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&vv) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        Planes { width, height, y, u, v, bind_group }
    }

    /// Carica un frame decompresso sulla GPU.
    pub fn upload(&mut self, frame: &YuvFrame) {
        let fits = self.planes.as_ref().is_some_and(|p| p.width == frame.width && p.height == frame.height);
        if !fits {
            self.planes = Some(self.create_planes(frame.width, frame.height));
        }
        let planes = self.planes.as_ref().expect("piani creati");
        let (cw, ch) = (frame.width.div_ceil(2), frame.height.div_ceil(2));
        for (texture, data, w, h) in [
            (&planes.y, &frame.y, frame.width, frame.height),
            (&planes.u, &frame.u, cw, ch),
            (&planes.v, &frame.v, cw, ch),
        ] {
            self.queue.write_texture(
                texture.as_image_copy(),
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
    }

    /// Dove finisce l'immagine nella finestra (proporzioni mantenute, centrata).
    pub fn viewport(&self) -> Viewport {
        let Some(p) = &self.planes else { return Viewport::default() };
        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        let scale = (sw / p.width as f32).min(sh / p.height as f32);
        let (w, h) = (p.width as f32 * scale, p.height as f32 * scale);
        Viewport { x: ((sw - w) / 2.0).floor(), y: ((sh - h) / 2.0).floor(), width: w, height: h }
    }

    pub fn render(&mut self) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let Some(planes) = &self.planes {
                let vp = self.viewport();
                pass.set_viewport(vp.x, vp.y, vp.width, vp.height, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &planes.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
    }
}
