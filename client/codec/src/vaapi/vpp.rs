//! Colour conversion on the GPU's video engine (`VAEntrypointVideoProc`): the
//! BGRA picture, or the compositor's DMA-BUF, converted into the encoder's NV12
//! surface, and the check of its colours against the CPU's conversion.

use std::ffi::{c_int, c_uint, c_void};

use anyhow::{Context, Result, anyhow, bail};
use tracing::debug;

use super::{
    BUF_PROC_PIPELINE, COLOR_STANDARD_BT601, Display, FOURCC_NV12, GENERIC_VALUE_INTEGER,
    GENERIC_VALUE_POINTER, Image, ImageFormat, MEM_TYPE_DRM_PRIME_2, PIPELINE_BACKGROUND,
    PIPELINE_INPUT_RANGE, PIPELINE_OUTPUT_PROPERTIES, PIPELINE_OUTPUT_REGION, PIPELINE_OUTPUT_STANDARD,
    PIPELINE_SIZE, PIPELINE_SURFACE, PROGRESSIVE, RT_FORMAT_RGB32, RT_FORMAT_YUV420, SOURCE_RANGE_FULL,
    SOURCE_RANGE_REDUCED, SURFACE_ATTRIB_EXTERNAL_BUFFER, SURFACE_ATTRIB_MEMORY_TYPE,
    SURFACE_ATTRIB_PIXEL_FORMAT, SURFACE_ATTRIB_SETTABLE, VA_INVALID_ID, VA_STATUS_SUCCESS, VAId,
};
use crate::BgraImage;

/// Largest difference allowed between the GPU's and the CPU's NV12 on flat colours
/// (rounding and the GPU's own coefficients).
pub(super) const VPP_TOLERANCE: u8 = 4;

/// The test picture: bands of flat colours 16 pixels wide (chroma is compared
/// inside the bands, away from the edges, where filters may differ).
fn vpp_pattern(w: usize, h: usize) -> Vec<u8> {
    const COLOURS: [[u8; 4]; 8] = [
        [0, 0, 0, 255],
        [255, 255, 255, 255],
        [0, 0, 255, 255],    // red (BGRA)
        [0, 255, 0, 255],    // green
        [255, 0, 0, 255],    // blue
        [40, 180, 230, 255], // orange
        [128, 128, 128, 255],
        [200, 60, 120, 255],
    ];
    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let c = COLOURS[(x / 16 + y / 16) % COLOURS.len()];
            data[(y * w + x) * 4..][..4].copy_from_slice(&c);
        }
    }
    data
}

/// Converts the test picture on the GPU and returns the largest difference from
/// the CPU's NV12 inside the colour blocks.
pub(super) fn vpp_check(d: &Display, w: u32, h: u32) -> Result<u8> {
    let data = vpp_pattern(w as usize, h as usize);
    let image = BgraImage { width: w, height: h, stride: w as usize * 4, data: &data };
    let (wu, hu) = (w as usize, h as usize);
    let mut cpu = vec![0u8; wu * hu * 3 / 2];
    crate::convert::bgra_to_nv12(&image, &mut cpu, wu, hu);
    let gpu = vpp_convert_read(d, &image)?;
    let mut diff = 0u8;
    // Inner pixels of each 16x16 block (rows and columns 4..12), luma and chroma.
    for y in 0..hu {
        for x in 0..wu {
            if !(4..12).contains(&(x % 16)) || !(4..12).contains(&(y % 16)) {
                continue;
            }
            diff = diff.max(cpu[y * wu + x].abs_diff(gpu[y * wu + x]));
            if y % 2 == 0 && x % 2 == 0 {
                let c = wu * hu + (y / 2) * wu + x;
                diff = diff.max(cpu[c].abs_diff(gpu[c])).max(cpu[c + 1].abs_diff(gpu[c + 1]));
            }
        }
    }
    Ok(diff)
}

/// Converts `image` into a new NV12 surface on the GPU and reads it back packed
/// (Y then UV, pitch = width).
fn vpp_convert_read(d: &Display, image: &BgraImage) -> Result<Vec<u8>> {
    let va = d.va;
    let (w, h) = (image.width, image.height);
    let mut surface = VA_INVALID_ID;
    // SAFETY: plain libva calls; the surface and the converter are released below.
    unsafe {
        va.check(
            (va.create_surfaces)(d.dpy, RT_FORMAT_YUV420, w, h, &mut surface, 1, std::ptr::null_mut(), 0),
            "vaCreateSurfaces",
        )?;
        let result = (|| -> Result<Vec<u8>> {
            let mut vpp = Vpp::new(d, w, h, surface)?;
            let converted = vpp.convert(d, image, surface).and_then(|()| {
                va.check((va.sync_surface)(d.dpy, surface), "vaSyncSurface")?;
                read_nv12(d, surface, w as usize, h as usize)
            });
            vpp.destroy(d);
            converted
        })();
        (va.destroy_surfaces)(d.dpy, &mut surface, 1);
        result
    }
}

/// Reads an NV12 surface back through `vaDeriveImage`.
fn read_nv12(d: &Display, surface: VAId, w: usize, h: usize) -> Result<Vec<u8>> {
    let va = d.va;
    // SAFETY: the mapped image is read within its pitches and planes.
    unsafe {
        let mut img = Image::default();
        va.check((va.derive_image)(d.dpy, surface, &mut img), "vaDeriveImage")?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mapped = va.check((va.map_buffer)(d.dpy, img.buf, &mut ptr), "image map");
        let result = mapped.and_then(|()| {
            if img.format.fourcc != FOURCC_NV12 {
                bail!("surface read as {:#x}", img.format.fourcc);
            }
            let mut out = vec![0u8; w * h * 3 / 2];
            let base = ptr.cast::<u8>();
            for (plane, rows) in [(0usize, h), (1, h / 2)] {
                let src = base.add(img.offsets[plane] as usize);
                let pitch = img.pitches[plane] as usize;
                for row in 0..rows {
                    std::ptr::copy_nonoverlapping(
                        src.add(row * pitch),
                        out[plane * w * h + row * w..].as_mut_ptr(),
                        w,
                    );
                }
            }
            Ok(out)
        });
        if !ptr.is_null() {
            (va.unmap_buffer)(d.dpy, img.buf);
        }
        (va.destroy_image)(d.dpy, img.image_id);
        result
    }
}

/// `VASurfaceAttrib` with an integer value.
#[repr(C)]
struct SurfaceAttrib {
    type_: c_int,
    flags: u32,
    value_type: c_int,
    _pad: u32,
    value: i64,
}
const _: () = assert!(size_of::<SurfaceAttrib>() == 24);

/// `VADRMPRIMESurfaceDescriptor` (va_drmcommon.h): the DMA-BUFs of a surface.
#[repr(C)]
#[derive(Default)]
struct PrimeDescriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [PrimeObject; 4],
    num_layers: u32,
    layers: [PrimeLayer; 4],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PrimeObject {
    fd: c_int,
    size: u32,
    modifier: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PrimeLayer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}
const _: () = assert!(size_of::<PrimeDescriptor>() == 312);

/// `DRM_FORMAT_MOD_LINEAR` and `DRM_FORMAT_MOD_INVALID`.
pub(super) const MOD_LINEAR: u64 = 0;
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// `I915_FORMAT_MOD_Y_TILED`.
const MOD_I915_Y_TILED: u64 = 0x0100_0000_0000_0002;
/// `VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS`.
const EXPORT_READ_COMPOSED: u32 = 0x0001 | 0x0008;

/// The DMA-BUF layouts the driver imports as RGB surfaces, best first: the layout
/// of its own RGB surfaces (exported, then imported back to be sure) and linear.
/// A compositor that can give that layout spares the GPU a copy into a linear
/// buffer for every picture.
pub(super) fn dmabuf_modifiers(d: &Display) -> Vec<u64> {
    let mut list = Vec::new();
    match own_rgb_modifier(d) {
        Ok(m) if m != MOD_LINEAR && m != MOD_INVALID => list.push(m),
        Ok(m) => debug!(modifier = format_args!("{m:#x}"), "VA-API: its RGB surfaces are linear"),
        Err(e) => debug!("VA-API: RGB surface not exported: {e:#}"),
    }
    // Intel's drivers make linear RGB surfaces but import the Y-tiled buffers the
    // compositors render into; a refusal falls back to linear while running.
    if d.vendor.contains("Intel") && !list.contains(&MOD_I915_Y_TILED) {
        list.push(MOD_I915_Y_TILED);
    }
    // Measured on the Kaby Lake-R laptop (docs/20-linux.md): Mutter takes the tiled
    // layout whenever offered, and it saves little (encoding 4.8 ms against 5.2,
    // the agent 13 % of a core against 12 %): linear only, the layout tried the
    // longest, unless NUVIRA_DMABUF_TILED=1.
    if std::env::var("NUVIRA_DMABUF_TILED").as_deref() == Ok("1") {
        list.push(MOD_LINEAR);
        list
    } else {
        vec![MOD_LINEAR]
    }
}

fn own_rgb_modifier(d: &Display) -> Result<u64> {
    let va = d.va;
    let export = va.export_surface.context("vaExportSurfaceHandle missing")?;
    let (w, h) = (256u32, 64u32);
    let mut surface = VA_INVALID_ID;
    let mut attrib = SurfaceAttrib {
        type_: SURFACE_ATTRIB_PIXEL_FORMAT,
        flags: SURFACE_ATTRIB_SETTABLE,
        value_type: GENERIC_VALUE_INTEGER,
        _pad: 0,
        value: u32::from_le_bytes(*b"BGRX") as i64,
    };
    // SAFETY: libva calls with valid pointers; the surfaces and the exported
    // descriptors are released below.
    unsafe {
        va.check(
            (va.create_surfaces)(d.dpy, RT_FORMAT_RGB32, w, h, &mut surface, 1, (&mut attrib as *mut SurfaceAttrib).cast(), 1),
            "RGB surface",
        )?;
        let mut desc = PrimeDescriptor::default();
        let exported = va.check(
            export(d.dpy, surface, MEM_TYPE_DRM_PRIME_2 as u32, EXPORT_READ_COMPOSED, (&raw mut desc).cast()),
            "vaExportSurfaceHandle",
        );
        (va.destroy_surfaces)(d.dpy, &mut surface, 1);
        exported?;
        let fds: Vec<c_int> = desc.objects[..desc.num_objects.min(4) as usize].iter().map(|o| o.fd).collect();
        let modifier = desc.objects[0].modifier;
        // Imported back as the compositor's pictures will be (one object, one plane).
        let result = if desc.num_objects == 1 && desc.num_layers == 1 && desc.layers[0].num_planes == 1 {
            let mut attribs = [
                SurfaceAttrib {
                    type_: SURFACE_ATTRIB_MEMORY_TYPE,
                    flags: SURFACE_ATTRIB_SETTABLE,
                    value_type: GENERIC_VALUE_INTEGER,
                    _pad: 0,
                    value: MEM_TYPE_DRM_PRIME_2,
                },
                SurfaceAttrib {
                    type_: SURFACE_ATTRIB_EXTERNAL_BUFFER,
                    flags: SURFACE_ATTRIB_SETTABLE,
                    value_type: GENERIC_VALUE_POINTER,
                    _pad: 0,
                    value: (&raw mut desc) as i64,
                },
            ];
            let mut back = VA_INVALID_ID;
            let imported = va.check(
                (va.create_surfaces)(d.dpy, RT_FORMAT_RGB32, w, h, &mut back, 1, attribs.as_mut_ptr().cast(), attribs.len() as c_uint),
                "import of the exported surface",
            );
            if imported.is_ok() {
                (va.destroy_surfaces)(d.dpy, &mut back, 1);
            }
            imported.map(|()| modifier)
        } else {
            Err(anyhow!("exported as {} objects, {} layers", desc.num_objects, desc.num_layers))
        };
        for fd in fds {
            libc::close(fd);
        }
        result
    }
}

/// The BGRA picture on the GPU and the video processing context that converts it
/// into an NV12 surface (with the GPU's video engine, not its shaders).
pub(super) struct Vpp {
    rgb: VAId,
    context: VAId,
    width: u32,
    height: u32,
    /// `vaDeriveImage` does not work on the RGB surface: `vaPutImage` from this.
    put_image: Option<Image>,
}

impl Vpp {
    /// For pictures of `width`x`height`, into `target` (an NV12 surface at least
    /// that large).
    pub(super) fn new(d: &Display, width: u32, height: u32, mut target: VAId) -> Result<Self> {
        let va = d.va;
        let mut vpp = Self { rgb: VA_INVALID_ID, context: VA_INVALID_ID, width, height, put_image: None };
        // SAFETY: libva calls with valid out-pointers; released by `destroy` on failure.
        let created = unsafe {
            (|| -> Result<()> {
                // BGRX: the capture's alpha means nothing; BGRA where BGRX is missing.
                let mut last = Err(anyhow!("no RGB surface"));
                for fourcc in [*b"BGRX", *b"BGRA"] {
                    let mut attrib = SurfaceAttrib {
                        type_: SURFACE_ATTRIB_PIXEL_FORMAT,
                        flags: SURFACE_ATTRIB_SETTABLE,
                        value_type: GENERIC_VALUE_INTEGER,
                        _pad: 0,
                        value: u32::from_le_bytes(fourcc) as i64,
                    };
                    last = va.check(
                        (va.create_surfaces)(
                            d.dpy,
                            RT_FORMAT_RGB32,
                            width,
                            height,
                            &mut vpp.rgb,
                            1,
                            (&mut attrib as *mut SurfaceAttrib).cast(),
                            1,
                        ),
                        "RGB surface",
                    );
                    if last.is_ok() {
                        break;
                    }
                }
                last?;
                va.check(
                    (va.create_context)(
                        d.dpy,
                        d.vpp_config,
                        width as c_int,
                        height as c_int,
                        PROGRESSIVE,
                        &mut target,
                        1,
                        &mut vpp.context,
                    ),
                    "video processing context",
                )
            })()
        };
        if let Err(e) = created {
            vpp.destroy(d);
            return Err(e);
        }
        Ok(vpp)
    }

    pub(super) fn destroy(&mut self, d: &Display) {
        let va = d.va;
        // SAFETY: only what was created, once.
        unsafe {
            if let Some(image) = self.put_image.take() {
                (va.destroy_image)(d.dpy, image.image_id);
            }
            if self.context != VA_INVALID_ID {
                (va.destroy_context)(d.dpy, self.context);
            }
            if self.rgb != VA_INVALID_ID {
                (va.destroy_surfaces)(d.dpy, &mut self.rgb, 1);
            }
        }
        self.context = VA_INVALID_ID;
        self.rgb = VA_INVALID_ID;
    }

    /// Copies `image` (its top-left `width`x`height`) to the GPU and converts it
    /// into `target` as NV12, BT.601 limited range like the CPU conversion.
    pub(super) fn convert(&mut self, d: &Display, image: &BgraImage, target: VAId) -> Result<()> {
        let va = d.va;
        let (w, h) = (self.width as usize, self.height as usize);
        // SAFETY: the mapped image is written within its pitch; the parameter
        // buffer and the rectangle it points to live until vaEndPicture.
        unsafe {
            let mut img = Image::default();
            let mut derived =
                self.put_image.is_none() && (va.derive_image)(d.dpy, self.rgb, &mut img) == VA_STATUS_SUCCESS;
            if derived && (img.format.bits_per_pixel != 32 || img.pitches[0] < (w * 4) as u32) {
                (va.destroy_image)(d.dpy, img.image_id);
                derived = false;
            }
            if !derived {
                if self.put_image.is_none() {
                    let mut format = ImageFormat {
                        fourcc: u32::from_le_bytes(*b"BGRX"),
                        byte_order: 1,
                        bits_per_pixel: 32,
                        depth: 24,
                        red_mask: 0x00ff_0000,
                        green_mask: 0x0000_ff00,
                        blue_mask: 0x0000_00ff,
                        ..Default::default()
                    };
                    let mut created = Image::default();
                    va.check(
                        (va.create_image)(d.dpy, &mut format, w as c_int, h as c_int, &mut created),
                        "vaCreateImage (RGB)",
                    )?;
                    debug!("VA-API: RGB surface not mappable, vaPutImage");
                    self.put_image = Some(created);
                }
                img = self.put_image.expect("image");
            }
            let mut ptr: *mut c_void = std::ptr::null_mut();
            let mapped = va.check((va.map_buffer)(d.dpy, img.buf, &mut ptr), "RGB image map");
            if let Err(e) = mapped {
                if derived {
                    (va.destroy_image)(d.dpy, img.image_id);
                }
                return Err(e);
            }
            let dst = ptr.cast::<u8>().add(img.offsets[0] as usize);
            let pitch = img.pitches[0] as usize;
            for row in 0..h {
                std::ptr::copy_nonoverlapping(
                    image.data[row * image.stride..].as_ptr(),
                    dst.add(row * pitch),
                    w * 4,
                );
            }
            (va.unmap_buffer)(d.dpy, img.buf);
            if derived {
                (va.destroy_image)(d.dpy, img.image_id);
            } else {
                va.check(
                    (va.put_image)(
                        d.dpy,
                        self.rgb,
                        img.image_id,
                        0,
                        0,
                        w as c_uint,
                        h as c_uint,
                        0,
                        0,
                        w as c_uint,
                        h as c_uint,
                    ),
                    "vaPutImage (RGB)",
                )?;
            }
        }
        self.process(d, self.rgb, target, (self.width, self.height))
    }

    /// A DMA-BUF picture (the compositor's) imported as a surface and converted
    /// into `target`, reduced to `out`: no copy through RAM, and the scaling for
    /// the technician's window done by the video engine too.
    pub(super) fn convert_dmabuf(&mut self, d: &Display, image: &crate::DmaBufImage, target: VAId, out: (u32, u32)) -> Result<()> {
        let va = d.va;
        let fourcc = match &image.drm_format.to_le_bytes() {
            b"XR24" => *b"BGRX",
            b"AR24" => *b"BGRA",
            b"XB24" => *b"RGBX",
            b"AB24" => *b"RGBA",
            other => bail!("DMA-BUF format {:?} not supported", String::from_utf8_lossy(other)),
        };
        let mut desc = PrimeDescriptor {
            fourcc: u32::from_le_bytes(fourcc),
            width: image.width,
            height: image.height,
            num_objects: 1,
            num_layers: 1,
            ..Default::default()
        };
        desc.objects[0] = PrimeObject { fd: image.fd, size: image.size, modifier: image.modifier };
        desc.layers[0] = PrimeLayer {
            drm_format: image.drm_format,
            num_planes: 1,
            object_index: [0; 4],
            offset: [image.offset, 0, 0, 0],
            pitch: [image.stride, 0, 0, 0],
        };
        let mut attribs = [
            SurfaceAttrib {
                type_: SURFACE_ATTRIB_MEMORY_TYPE,
                flags: SURFACE_ATTRIB_SETTABLE,
                value_type: GENERIC_VALUE_INTEGER,
                _pad: 0,
                value: MEM_TYPE_DRM_PRIME_2,
            },
            SurfaceAttrib {
                type_: SURFACE_ATTRIB_EXTERNAL_BUFFER,
                flags: SURFACE_ATTRIB_SETTABLE,
                value_type: GENERIC_VALUE_POINTER,
                _pad: 0,
                value: (&raw mut desc) as i64,
            },
        ];
        let mut surface = VA_INVALID_ID;
        // SAFETY: the descriptor lives through the call (the driver imports the
        // buffer: the descriptor is ours to close); the surface is destroyed below.
        unsafe {
            va.check(
                (va.create_surfaces)(
                    d.dpy,
                    RT_FORMAT_RGB32,
                    image.width,
                    image.height,
                    &mut surface,
                    1,
                    attribs.as_mut_ptr().cast(),
                    attribs.len() as c_uint,
                ),
                "DMA-BUF import",
            )?;
        }
        let result = self.process(d, surface, target, out);
        // SAFETY: created above, once.
        unsafe { (va.destroy_surfaces)(d.dpy, &mut surface, 1) };
        result
    }

    /// The video engine converts `source` (all of it) into `target`'s top-left
    /// `out` rectangle, BT.601 limited range like the CPU conversion.
    fn process(&mut self, d: &Display, source: VAId, target: VAId, out: (u32, u32)) -> Result<()> {
        let va = d.va;
        let (w, h) = (out.0 as usize, out.1 as usize);
        // SAFETY: the parameter buffer and the rectangle it points to live until
        // vaEndPicture.
        unsafe {
            // VARectangle: x, y (i16), width, height (u16).
            let region: [u16; 4] = [0, 0, w as u16, h as u16];
            let mut params = [0u8; PIPELINE_SIZE];
            params[PIPELINE_SURFACE..][..4].copy_from_slice(&source.to_le_bytes());
            params[PIPELINE_OUTPUT_REGION..][..8].copy_from_slice(&(region.as_ptr() as u64).to_le_bytes());
            params[PIPELINE_BACKGROUND..][..4].copy_from_slice(&0xff00_0000u32.to_le_bytes());
            params[PIPELINE_OUTPUT_STANDARD..][..4].copy_from_slice(&COLOR_STANDARD_BT601.to_le_bytes());
            params[PIPELINE_INPUT_RANGE] = SOURCE_RANGE_FULL;
            // VAProcColorProperties: chroma siting, range, primaries, transfer, matrix
            // (6: BT.601 / SMPTE 170M).
            params[PIPELINE_OUTPUT_PROPERTIES..][..5].copy_from_slice(&[0, SOURCE_RANGE_REDUCED, 6, 6, 6]);
            let mut buffer = VA_INVALID_ID;
            va.check((va.begin_picture)(d.dpy, self.context, target), "vaBeginPicture (VPP)")?;
            let mut result = va.check(
                (va.create_buffer)(
                    d.dpy,
                    self.context,
                    BUF_PROC_PIPELINE,
                    PIPELINE_SIZE as c_uint,
                    1,
                    params.as_mut_ptr().cast(),
                    &mut buffer,
                ),
                "pipeline buffer",
            );
            if result.is_ok() {
                result = va
                    .check((va.render_picture)(d.dpy, self.context, &mut buffer, 1), "vaRenderPicture (VPP)");
            }
            let ended = va.check((va.end_picture)(d.dpy, self.context), "vaEndPicture (VPP)");
            if buffer != VA_INVALID_ID {
                (va.destroy_buffer)(d.dpy, buffer);
            }
            result.and(ended)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{VPP_TOLERANCE, Vpp, vpp_check, vpp_convert_read, vpp_pattern};
    use crate::BgraImage;
    use crate::vaapi::{Display, RT_FORMAT_YUV420, VA_INVALID_ID};

    /// The GPU conversion against the CPU's, and their times at 1080p, on the node
    /// the encoder uses (`Display::open`), else on the first node that processes
    /// video without encoding H.264 (Mesa's D3D12 driver on the NUC's Intel UHD,
    /// in WSL). Skipped without a VA driver that processes video, unless
    /// `NUVIRA_EXPECT_VAAPI_VPP=1` (a machine whose driver has it): then a skip
    /// fails.
    #[test]
    fn vpp_matches_cpu() {
        let expected = std::env::var("NUVIRA_EXPECT_VAAPI_VPP").is_ok_and(|v| v == "1");
        let skip = |why: String| {
            assert!(!expected, "NUVIRA_EXPECT_VAAPI_VPP=1 but nothing was compared: {why}");
            eprintln!("{why}");
        };
        let d = match Display::open().or_else(|_| Display::open_with(false)) {
            Ok(d) if d.vpp_config != VA_INVALID_ID => d,
            Ok(_) => return skip("no video processing: skipped".into()),
            Err(e) => return skip(format!("no VA-API ({e:#}): skipped")),
        };
        eprintln!("driver: {} on {}", d.vendor, d.path);
        // Mesa's D3D12 driver in WSL has no RGB surfaces (only NV12 and P010): it
        // fails here and the encoder keeps the CPU conversion.
        let diff = match vpp_check(&d, 256, 64) {
            Ok(diff) => diff,
            Err(e) => {
                return skip(format!("GPU conversion refused ({e:#}): the encoder converts on the CPU"));
            }
        };
        eprintln!("largest difference on flat colours: {diff}");
        assert!(diff <= VPP_TOLERANCE, "GPU colours differ by {diff}");

        let (w, h) = (1920u32, 1080u32);
        let data = vpp_pattern(w as usize, h as usize);
        let image = BgraImage { width: w, height: h, stride: w as usize * 4, data: &data };
        let gpu = vpp_convert_read(&d, &image).expect("1080p");
        let mut cpu = vec![0u8; (w * h * 3 / 2) as usize];
        crate::convert::bgra_to_nv12(&image, &mut cpu, w as usize, h as usize);
        let max = cpu.iter().zip(&gpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        eprintln!("1080p: largest difference anywhere {max} (block edges included)");

        let va = d.va;
        let mut surface = VA_INVALID_ID;
        // SAFETY: test surface, released at the end.
        unsafe {
            va.check(
                (va.create_surfaces)(d.dpy, RT_FORMAT_YUV420, w, h, &mut surface, 1, std::ptr::null_mut(), 0),
                "surface",
            )
            .unwrap();
            let mut vpp = Vpp::new(&d, w, h, surface).unwrap();
            let runs = 60;
            let started = std::time::Instant::now();
            for _ in 0..runs {
                vpp.convert(&d, &image, surface).unwrap();
                (va.sync_surface)(d.dpy, surface);
            }
            let gpu_ms = started.elapsed().as_secs_f64() * 1000.0 / runs as f64;
            let started = std::time::Instant::now();
            for _ in 0..runs {
                crate::convert::bgra_to_nv12(&image, &mut cpu, w as usize, h as usize);
            }
            let cpu_ms = started.elapsed().as_secs_f64() * 1000.0 / runs as f64;
            eprintln!("1080p: GPU (copy + conversion) {gpu_ms:.2} ms, CPU conversion {cpu_ms:.2} ms");
            vpp.destroy(&d);
            (va.destroy_surfaces)(d.dpy, &mut surface, 1);
        }
    }
}
