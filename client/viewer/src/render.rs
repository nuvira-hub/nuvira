//! Displaying I420 frames with wgpu.
//!
//! The three planes Y, U, V are uploaded as R8 textures and converted to RGB by
//! the shader (BT.601, limited range). The image is fitted to the window keeping
//! its proportions, or shown at its real size (1:1) and panned with the mouse.
//!
//! When the image is shrunk each window pixel averages several samples spread over
//! the source pixels it covers: a single bilinear sample skips pixels and thin text
//! (1-pixel strokes) breaks up. Measured case: a 1280x1024 screen on a 1366x768
//! laptop, shrunk to 75%.

use std::sync::Arc;

use anyhow::{Context, Result};
use nuvira_codec::YuvFrame;
use winit::window::Window;

const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    // Triangle covering the whole viewport.
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

struct Params {
    // Part of the image shown (texture coordinates of the corners).
    uv0: vec2<f32>,
    uv1: vec2<f32>,
    // Size of one window pixel in texture coordinates.
    pixel: vec2<f32>,
    // Samples per side averaged for each window pixel (1 = no shrinking).
    taps: u32,
    _pad: u32,
};
@group(0) @binding(4) var<uniform> params: Params;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = mix(params.uv0, params.uv1, in.uv);
    var yuv = vec3<f32>(0.0);
    let n = max(params.taps, 1u);
    for (var i = 0u; i < n; i++) {
        for (var j = 0u; j < n; j++) {
            let offset = (vec2<f32>(f32(i), f32(j)) + 0.5) / f32(n) - 0.5;
            let at = uv + offset * params.pixel;
            yuv += vec3<f32>(
                textureSampleLevel(tex_y, samp, at, 0.0).r,
                textureSampleLevel(tex_u, samp, at, 0.0).r,
                textureSampleLevel(tex_v, samp, at, 0.0).r,
            );
        }
    }
    yuv /= f32(n * n);
    let y = (yuv.x - 16.0 / 255.0) * 1.164383;
    let u = yuv.y - 0.5;
    let v = yuv.z - 0.5;
    let rgb = vec3<f32>(
        y + 1.596027 * v,
        y - 0.391762 * u - 0.812968 * v,
        y + 2.017232 * u,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// Rectangle the image takes in the window, in physical pixels. At real size it
/// can be larger than the window and start before it (negative `x`/`y`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    /// Normalised 0..1 position in the image, if the point falls inside it.
    pub fn normalize(&self, px: f64, py: f64) -> Option<(f32, f32)> {
        if self.width <= 0.0 || self.height <= 0.0 {
            return None;
        }
        let x = (px as f32 - self.x) / self.width;
        let y = (py as f32 - self.y) / self.height;
        ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some((x, y))
    }

    /// Part inside a `sw`x`sh` window: the visible rectangle and the matching
    /// texture coordinates (top-left and bottom-right corners).
    pub fn visible(&self, sw: u32, sh: u32) -> Option<(Viewport, [f32; 2], [f32; 2])> {
        let x0 = self.x.max(0.0);
        let y0 = self.y.max(0.0);
        let x1 = (self.x + self.width).min(sw as f32);
        let y1 = (self.y + self.height).min(sh as f32);
        if self.width <= 0.0 || self.height <= 0.0 || x1 <= x0 || y1 <= y0 {
            return None;
        }
        let u = |x: f32| (x - self.x) / self.width;
        let v = |y: f32| (y - self.y) / self.height;
        Some((Viewport { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }, [u(x0), v(y0)], [u(x1), v(y1)]))
    }
}

/// How the image is placed in the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct View {
    /// Real size (1:1) instead of fitting the window.
    pub actual: bool,
    /// At real size, which part is shown when the image is larger than the window:
    /// 0..1 per axis, from the mouse position (left edge → left side of the image).
    pub pan: (f32, f32),
}

/// Logs at trace level how long the block it lives in takes.
struct TraceTime(&'static str, std::time::Instant);

impl Drop for TraceTime {
    fn drop(&mut self) {
        tracing::trace!(elapsed = ?self.1.elapsed(), "{}", self.0);
    }
}

/// Graphics APIs tried in turn: the native one of each system, then OpenGL where it
/// exists. One at a time, because enumerating a backend loads its drivers: the Intel
/// OpenGL driver (ig9icd64.dll) sometimes crashes when the process exits, so it is
/// loaded only on cards without DirectX 12. On Windows Vulkan is excluded: some Intel
/// drivers crash already at enumeration. `WGPU_BACKEND` (e.g. `vulkan`, `gl`) still
/// allows forcing one.
fn native_backends() -> &'static [wgpu::Backends] {
    if cfg!(windows) {
        &[wgpu::Backends::DX12, wgpu::Backends::GL]
    } else if cfg!(target_os = "macos") {
        &[wgpu::Backends::METAL]
    } else {
        &[wgpu::Backends::VULKAN, wgpu::Backends::GL]
    }
}

/// Where the `fw`x`fh` image goes in the `sw`x`sh` window.
pub fn place(fw: u32, fh: u32, sw: u32, sh: u32, view: View) -> Viewport {
    if !view.actual {
        return fit(fw, fh, sw, sh);
    }
    // Centred if it fits, otherwise moved by the pan.
    let axis = |f: u32, s: u32, pan: f32| {
        let (f, s) = (f as f32, s as f32);
        if f <= s { ((s - f) / 2.0).floor() } else { -((f - s) * pan.clamp(0.0, 1.0)).round() }
    };
    Viewport { x: axis(fw, sw, view.pan.0), y: axis(fh, sh, view.pan.1), width: fw as f32, height: fh as f32 }
}

/// Samples per side to average when `fw` image pixels go into `vw` window pixels.
pub fn taps(fw: u32, vw: f32) -> u32 {
    if vw <= 0.0 { 1 } else { (fw as f32 / vw).ceil().clamp(1.0, 4.0) as u32 }
}

/// Rectangle in which the `fw`x`fh` image fits into the `sw`x`sh` window
/// keeping its proportions, centred.
pub fn fit(fw: u32, fh: u32, sw: u32, sh: u32) -> Viewport {
    if fw == 0 || fh == 0 {
        return Viewport::default();
    }
    let (sw, sh) = (sw as f32, sh as f32);
    let scale = (sw / fw as f32).min(sh / fh as f32);
    let (w, h) = ((fw as f32 * scale).floor(), (fh as f32 * scale).floor());
    Viewport { x: ((sw - w) / 2.0).floor(), y: ((sh - h) / 2.0).floor(), width: w, height: h }
}

/// Size of the shader's `Params`: six f32 and two u32.
const PARAMS_SIZE: u64 = 32;

struct Planes {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct GpuRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// `Params` of the shader.
    params: wgpu::Buffer,
    planes: Option<Planes>,
    view: View,
    pub adapter_info: String,
    pub backend: wgpu::Backend,
}

impl GpuRenderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let from_env = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        let attempts = if std::env::var_os("WGPU_BACKEND").is_some() {
            &[from_env.backends][..]
        } else {
            native_backends()
        };
        let mut found = None;
        for &backends in attempts {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
            desc.backends = backends;
            let instance = wgpu::Instance::new(desc);
            let surface = match instance.create_surface(window.clone()) {
                Ok(surface) => surface,
                Err(e) => {
                    tracing::debug!(?backends, "no surface: {e}");
                    continue;
                }
            };
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })) {
                // Windows' software rasterizer (WARP) answers for DX12 on cards whose driver
                // lacks it: ~670 ms per 1080p frame on an Intel HD Graphics, against ~6 ms for
                // the CPU renderer reached through the OpenGL attempt.
                Ok(adapter)
                    if adapter.get_info().device_type == wgpu::DeviceType::Cpu
                        && std::env::var_os("WGPU_BACKEND").is_none() =>
                {
                    tracing::debug!(?backends, "only a software adapter: {}", adapter.get_info().name);
                }
                Ok(adapter) => {
                    found = Some((instance, surface, adapter));
                    break;
                }
                Err(e) => tracing::debug!(?backends, "no graphics card: {e}"),
            }
        }
        let (_instance, surface, adapter) = found.context("no compatible graphics card")?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);
        let backend = info.backend;
        // Only the minimum limits (WebGL2's) plus the card's maximum resolution are requested:
        // the viewer uses no compute shaders, and cards reached through OpenGL do not offer them.
        let required_limits = wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor { required_limits, ..Default::default() }),
        )
        .with_context(|| format!("wgpu device on {adapter_info}"))?;

        let caps = surface.get_capabilities(&adapter);
        // The shader produces values already in sRGB gamma: a non-sRGB format is needed,
        // otherwise the GPU would convert them a second time (washed-out image).
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        // Mailbox: no tearing and no queue of old frames. Without Mailbox (e.g. OpenGL)
        // Immediate: some tearing, but Fifo would wait for vsync before and after every frame
        // (measured ~40 ms per frame on an Intel HD 2500).
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        tracing::debug!(?present_mode, available = ?caps.present_modes, "presentation");
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface configuration")?;
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
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
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
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: PARAMS_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            sampler,
            params,
            planes: None,
            view: View::default(),
            adapter_info,
            backend,
        })
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
                wgpu::BindGroupEntry { binding: 4, resource: self.params.as_entire_binding() },
            ],
        });
        Planes { width, height, y, u, v, bind_group }
    }

    /// Uploads a decoded frame to the GPU.
    pub fn upload(&mut self, frame: &YuvFrame) {
        let _t = TraceTime("upload", std::time::Instant::now());
        let fits = self.planes.as_ref().is_some_and(|p| p.width == frame.width && p.height == frame.height);
        if !fits {
            self.planes = Some(self.create_planes(frame.width, frame.height));
        }
        let planes = self.planes.as_ref().expect("planes created");
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

    /// Where the image ends up in the window.
    pub fn viewport(&self) -> Viewport {
        let Some(p) = &self.planes else { return Viewport::default() };
        place(p.width, p.height, self.config.width, self.config.height, self.view)
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
    }

    pub fn render(&mut self) {
        let t = std::time::Instant::now();
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
            if let Some(planes) = &self.planes
                && let Some((vp, uv0, uv1)) = self.viewport().visible(self.config.width, self.config.height)
            {
                let n = taps(planes.width, self.viewport().width);
                let pixel = [(uv1[0] - uv0[0]) / vp.width, (uv1[1] - uv0[1]) / vp.height];
                let mut data = Vec::with_capacity(PARAMS_SIZE as usize);
                for f in [uv0[0], uv0[1], uv1[0], uv1[1], pixel[0], pixel[1]] {
                    data.extend_from_slice(&f.to_le_bytes());
                }
                data.extend_from_slice(&n.to_le_bytes());
                data.extend_from_slice(&0u32.to_le_bytes());
                self.queue.write_buffer(&self.params, 0, &data);
                pass.set_viewport(vp.x, vp.y, vp.width, vp.height, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &planes.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        let t_present = std::time::Instant::now();
        self.queue.present(frame);
        tracing::trace!(acquire_submit = ?(t_present - t), present = ?t_present.elapsed(), "render");
    }
}

/// The drawing method chosen for this window.
pub enum Renderer {
    Gpu(Box<GpuRenderer>),
    Cpu(crate::cpu::CpuRenderer),
}

impl Renderer {
    /// GPU (wgpu) if there is a modern API; CPU if the card is reachable only through OpenGL or wgpu fails.
    /// `NUVIRA_RENDERER=gpu|cpu` forces the choice.
    pub fn new(window: Arc<Window>) -> Result<(Self, String)> {
        let forced = std::env::var("NUVIRA_RENDERER").ok();
        if forced.as_deref() != Some("cpu") {
            match GpuRenderer::new(window.clone()) {
                Ok(gpu) if gpu.backend != wgpu::Backend::Gl || forced.as_deref() == Some("gpu") => {
                    let info = gpu.adapter_info.clone();
                    return Ok((Renderer::Gpu(Box::new(gpu)), info));
                }
                Ok(gpu) => {
                    tracing::info!("{} available only through OpenGL: drawing with the CPU", gpu.adapter_info)
                }
                Err(e) if forced.as_deref() == Some("gpu") => return Err(e),
                Err(e) => tracing::info!("wgpu not available ({e:#}): drawing with the CPU"),
            }
        }
        let cpu = crate::cpu::CpuRenderer::new(window)?;
        Ok((Renderer::Cpu(cpu), "CPU (softbuffer)".into()))
    }

    pub fn upload(&mut self, frame: &YuvFrame) {
        match self {
            Renderer::Gpu(r) => r.upload(frame),
            Renderer::Cpu(r) => r.upload(frame),
        }
    }

    pub fn render(&mut self) {
        match self {
            Renderer::Gpu(r) => r.render(),
            Renderer::Cpu(r) => {
                if let Err(e) = r.render() {
                    tracing::debug!("presentation failed: {e:#}");
                }
            }
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        match self {
            Renderer::Gpu(r) => r.resize(width, height),
            Renderer::Cpu(r) => r.resize(width, height),
        }
    }

    pub fn viewport(&self) -> Viewport {
        match self {
            Renderer::Gpu(r) => r.viewport(),
            Renderer::Cpu(r) => r.viewport(),
        }
    }

    pub fn set_view(&mut self, view: View) {
        match self {
            Renderer::Gpu(r) => r.set_view(view),
            Renderer::Cpu(r) => r.set_view(view),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_size_follows_the_pan() {
        // 1280x1024 screen in a 1366x768 window (the test laptop).
        let at = |pan| place(1280, 1024, 1366, 768, View { actual: true, pan });
        let top = at((0.0, 0.0));
        assert_eq!((top.x, top.y, top.width, top.height), (43.0, 0.0, 1280.0, 1024.0));
        let bottom = at((1.0, 1.0));
        assert_eq!((bottom.x, bottom.y), (43.0, -256.0));
        // The mouse at the bottom of the window points at the bottom of the screen.
        assert_eq!(bottom.normalize(700.0, 768.0).map(|(_, y)| y), Some(1.0));
        let (vis, uv0, uv1) = bottom.visible(1366, 768).expect("visible");
        assert_eq!((vis.x, vis.y, vis.width, vis.height), (43.0, 0.0, 1280.0, 768.0));
        assert_eq!((uv0, uv1), ([0.0, 0.25], [1.0, 1.0]));
    }

    #[test]
    fn shrinking_averages_samples() {
        // Fitted: 1024 rows into 768, 75%: two samples per side.
        let fitted = place(1280, 1024, 1366, 768, View::default());
        assert_eq!(fitted.height, 768.0);
        assert_eq!(taps(1280, fitted.width), 2);
        assert_eq!(taps(1280, 1280.0), 1);
        assert_eq!(taps(3840, 640.0), 4);
    }
}
