//! H.264 with VA-API (Linux): Intel (iHD, i965) and AMD (Mesa) graphics cards.
//!
//! libva is loaded at run time (`libva.so.2`, `libva-drm.so.2`): every desktop
//! distribution has it, and without it (or without an encoding driver) the
//! software encoder is used. Nothing else is needed (no GStreamer or FFmpeg).
//!
//! - Device: the first DRM render node (`/dev/dri/renderD128`…) whose driver
//!   encodes H.264; `NUVIRA_VAAPI_DEVICE` picks one.
//! - Profile Main (CABAC), else High, else Constrained Baseline; slice encoding,
//!   else the low-power one (newer Intel). Only P frames referring to the previous
//!   one, keyframes when asked (as the other encoders): one frame out for every
//!   frame in, no reordering.
//! - CBR rate control (VBR if missing), bitrate changed during the session with a
//!   new rate-control parameter. A driver with only constant QP (iHD on Intel Gen9
//!   without the HuC firmware) is used too, the QP of each frame chosen here from
//!   the size of the previous ones (`QpControl`).
//! - Headers: SPS and PPS written here and given as packed headers when the
//!   driver takes them (as FFmpeg and GStreamer do). Mesa's drivers (AMD
//!   radeonsi, D3D12 under WSL) need them: their VA layer reads the profile and
//!   level only from a packed SPS (Mesa 26: level 0 and a crash in the D3D12
//!   driver without it). Otherwise the driver writes them. Slice headers are
//!   always the driver's. The first keyframe is checked to carry SPS and PPS,
//!   otherwise the encoder is refused at start.
//! - Input: BGRA converted to NV12 (BT.601 limited, like the other encoders).
//!   Where the driver has video processing (`VAEntrypointVideoProc`) the BGRA
//!   picture is copied as it is into an RGB surface and the GPU's video engine
//!   converts it into the encoder's surface (`Vpp`): checked once against the
//!   CPU's conversion on a test picture, and given up for the CPU if the colours
//!   differ or the driver refuses (`NUVIRA_VAAPI_VPP=0` turns it off). Otherwise
//!   converted on the CPU and copied into the surface through `vaDeriveImage`, or
//!   `vaPutImage` where the surface cannot be mapped.
//! - The display is opened once for the process and never terminated (see
//!   `shared_display`).
//! - If the driver fails during the session (screen too large, GPU reset) the
//!   encoder goes on with OpenH264, since the pipeline does not recreate it.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};

use crate::{BgraImage, EncodedFrame, EncoderSettings, OpenH264Encoder, VideoEncoder};

type VADisplay = *mut c_void;
type VAStatus = c_int;
type VAId = c_uint;

const VA_STATUS_SUCCESS: VAStatus = 0;
const VA_INVALID_ID: VAId = 0xffff_ffff;

const PROFILE_MAIN: c_int = 6;
const PROFILE_HIGH: c_int = 7;
const PROFILE_CONSTRAINED_BASELINE: c_int = 13;
const ENTRY_ENC_SLICE: c_int = 6;
const ENTRY_ENC_SLICE_LP: c_int = 8;

const ATTRIB_RT_FORMAT: c_int = 0;
const ATTRIB_RATE_CONTROL: c_int = 5;
const ATTRIB_ENC_PACKED_HEADERS: c_int = 10;
/// `VA_ENC_PACKED_HEADER_SEQUENCE` and `_PICTURE`; also the packed header types
/// `VAEncPackedHeaderSequence` (1) and `VAEncPackedHeaderPicture` (2).
const PACKED_SEQUENCE: u32 = 1;
const PACKED_PICTURE: u32 = 2;
const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
const RT_FORMAT_YUV420: u32 = 1;
const RC_CBR: u32 = 2;
const RC_VBR: u32 = 4;
/// Constant QP: the bitrate is then followed here (see [`QpControl`]).
const RC_CQP: u32 = 16;
const PROGRESSIVE: c_int = 1;

const BUF_CODED: c_int = 21;
const BUF_SEQUENCE: c_int = 22;
const BUF_PICTURE: c_int = 23;
const BUF_SLICE: c_int = 24;
const BUF_PACKED_HEADER_PARAMETER: c_int = 25;
const BUF_PACKED_HEADER_DATA: c_int = 26;
const BUF_MISC: c_int = 27;
const MISC_FRAME_RATE: u32 = 0;
const MISC_RATE_CONTROL: u32 = 1;
const MISC_HRD: u32 = 5;

/// Video processing (VPP): the BGRA → NV12 conversion on the GPU.
const PROFILE_NONE: c_int = -1;
const ENTRY_VIDEO_PROC: c_int = 10;
const RT_FORMAT_RGB32: u32 = 0x0002_0000;
const BUF_PROC_PIPELINE: c_int = 41;
const SURFACE_ATTRIB_PIXEL_FORMAT: c_int = 1;
const SURFACE_ATTRIB_MEMORY_TYPE: c_int = 6;
const SURFACE_ATTRIB_EXTERNAL_BUFFER: c_int = 7;
const SURFACE_ATTRIB_SETTABLE: u32 = 2;
const GENERIC_VALUE_INTEGER: c_int = 1;
const GENERIC_VALUE_POINTER: c_int = 3;
/// `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`: a surface made from DMA-BUFs.
const MEM_TYPE_DRM_PRIME_2: i64 = 0x4000_0000;
const COLOR_STANDARD_BT601: i32 = 1;
const SOURCE_RANGE_REDUCED: u8 = 1;
const SOURCE_RANGE_FULL: u8 = 2;
/// `VAProcPipelineParameterBuffer` (libva 2.x, 64-bit): written as bytes at the
/// offsets of the C header (checked with libva 2.17's `offsetof`).
const PIPELINE_SIZE: usize = 224;
const PIPELINE_SURFACE: usize = 0;
const PIPELINE_OUTPUT_REGION: usize = 24;
const PIPELINE_BACKGROUND: usize = 32;
const PIPELINE_OUTPUT_STANDARD: usize = 36;
const PIPELINE_INPUT_RANGE: usize = 133;
const PIPELINE_OUTPUT_PROPERTIES: usize = 140;

const PICTURE_INVALID: u32 = 1;
const PICTURE_SHORT_TERM_REFERENCE: u32 = 8;
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

const SLICE_P: u8 = 0;
const SLICE_I: u8 = 2;
/// `log2_max_frame_num_minus4`: frame_num counts up to 2^16, then a keyframe.
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 12;
const MAX_FRAME_NUM: u32 = 1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4);
/// Keyframe period given to the rate control only (keyframes are on demand).
const INTRA_PERIOD: u32 = 3600;

#[repr(C)]
#[derive(Clone, Copy)]
struct VAConfigAttrib {
    type_: c_int,
    value: u32,
}

/// `VAEncPackedHeaderParameterBuffer`.
#[repr(C)]
struct PackedHeaderParams {
    type_: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VAPictureH264 {
    picture_id: VAId,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
    reserved: [u32; 4],
}

const INVALID_PICTURE: VAPictureH264 = VAPictureH264 {
    picture_id: VA_INVALID_ID,
    frame_idx: 0,
    flags: PICTURE_INVALID,
    top_field_order_cnt: 0,
    bottom_field_order_cnt: 0,
    reserved: [0; 4],
};

#[repr(C)]
struct SequenceParams {
    seq_parameter_set_id: u8,
    level_idc: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    max_num_ref_frames: u32,
    picture_width_in_mbs: u16,
    picture_height_in_mbs: u16,
    seq_fields: u32,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames_in_pic_order_cnt_cycle: u8,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: [i32; 256],
    frame_cropping_flag: u8,
    frame_crop_left_offset: u32,
    frame_crop_right_offset: u32,
    frame_crop_top_offset: u32,
    frame_crop_bottom_offset: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct PictureParams {
    curr_pic: VAPictureH264,
    reference_frames: [VAPictureH264; 16],
    coded_buf: VAId,
    pic_parameter_set_id: u8,
    seq_parameter_set_id: u8,
    last_picture: u8,
    frame_num: u16,
    pic_init_qp: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct SliceParams {
    macroblock_address: u32,
    num_macroblocks: u32,
    macroblock_info: VAId,
    slice_type: u8,
    pic_parameter_set_id: u8,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_active_override_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [VAPictureH264; 32],
    ref_pic_list1: [VAPictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    reserved: [u32; 4],
}

/// `VAEncMiscParameterBuffer` header followed by one of the parameters below.
#[repr(C)]
struct Misc<T> {
    type_: u32,
    data: T,
}

#[repr(C)]
#[derive(Default)]
struct RateControl {
    bits_per_second: u32,
    target_percentage: u32,
    window_size: u32,
    initial_qp: u32,
    min_qp: u32,
    basic_unit_size: u32,
    rc_flags: u32,
    icq_quality_factor: u32,
    max_qp: u32,
    quality_factor: u32,
    target_frame_size: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Default)]
struct FrameRate {
    framerate: u32,
    framerate_flags: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Default)]
struct Hrd {
    initial_buffer_fullness: u32,
    buffer_size: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct CodedSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut c_void,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Image {
    image_id: VAId,
    format: ImageFormat,
    buf: VAId,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: i32,
    entry_bytes: i32,
    component_order: [i8; 4],
    reserved: [u32; 4],
}

// Sizes of libva 2.x (checked against the C headers).
const _: () = assert!(size_of::<VAPictureH264>() == 36);
const _: () = assert!(size_of::<PackedHeaderParams>() == 28);
const _: () = assert!(size_of::<SequenceParams>() == 1132);
const _: () = assert!(size_of::<PictureParams>() == 648);
const _: () = assert!(size_of::<SliceParams>() == 3140);
const _: () = assert!(size_of::<RateControl>() == 60);
const _: () = assert!(size_of::<FrameRate>() == 24);
const _: () = assert!(size_of::<Hrd>() == 24);
const _: () = assert!(size_of::<CodedSegment>() == 48);
const _: () = assert!(size_of::<Image>() == 120);
const _: () = assert!(std::mem::offset_of!(SequenceParams, frame_cropping_flag) == 1068);
const _: () = assert!(std::mem::offset_of!(SequenceParams, time_scale) == 1112);
const _: () = assert!(std::mem::offset_of!(PictureParams, coded_buf) == 612);
const _: () = assert!(std::mem::offset_of!(PictureParams, frame_num) == 620);
const _: () = assert!(std::mem::offset_of!(PictureParams, pic_fields) == 628);
const _: () = assert!(std::mem::offset_of!(SliceParams, luma_log2_weight_denom) == 2340);
const _: () = assert!(std::mem::offset_of!(SliceParams, cabac_init_idc) == 3118);
const _: () = assert!(std::mem::offset_of!(Image, pitches) == 68);

/// The libva functions in use, loaded once.
struct Va {
    _va: libloading::Library,
    _drm: libloading::Library,
    get_display_drm: unsafe extern "C" fn(c_int) -> VADisplay,
    initialize: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    terminate: unsafe extern "C" fn(VADisplay) -> VAStatus,
    error_str: unsafe extern "C" fn(VAStatus) -> *const c_char,
    vendor: unsafe extern "C" fn(VADisplay) -> *const c_char,
    max_entrypoints: unsafe extern "C" fn(VADisplay) -> c_int,
    query_entrypoints: unsafe extern "C" fn(VADisplay, c_int, *mut c_int, *mut c_int) -> VAStatus,
    get_config_attributes:
        unsafe extern "C" fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int) -> VAStatus,
    create_config:
        unsafe extern "C" fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int, *mut VAId) -> VAStatus,
    destroy_config: unsafe extern "C" fn(VADisplay, VAId) -> VAStatus,
    create_surfaces: unsafe extern "C" fn(
        VADisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut VAId,
        c_uint,
        *mut c_void,
        c_uint,
    ) -> VAStatus,
    destroy_surfaces: unsafe extern "C" fn(VADisplay, *mut VAId, c_int) -> VAStatus,
    create_context:
        unsafe extern "C" fn(VADisplay, VAId, c_int, c_int, c_int, *mut VAId, c_int, *mut VAId) -> VAStatus,
    destroy_context: unsafe extern "C" fn(VADisplay, VAId) -> VAStatus,
    create_buffer:
        unsafe extern "C" fn(VADisplay, VAId, c_int, c_uint, c_uint, *mut c_void, *mut VAId) -> VAStatus,
    destroy_buffer: unsafe extern "C" fn(VADisplay, VAId) -> VAStatus,
    map_buffer: unsafe extern "C" fn(VADisplay, VAId, *mut *mut c_void) -> VAStatus,
    unmap_buffer: unsafe extern "C" fn(VADisplay, VAId) -> VAStatus,
    begin_picture: unsafe extern "C" fn(VADisplay, VAId, VAId) -> VAStatus,
    render_picture: unsafe extern "C" fn(VADisplay, VAId, *mut VAId, c_int) -> VAStatus,
    end_picture: unsafe extern "C" fn(VADisplay, VAId) -> VAStatus,
    sync_surface: unsafe extern "C" fn(VADisplay, VAId) -> VAStatus,
    derive_image: unsafe extern "C" fn(VADisplay, VAId, *mut Image) -> VAStatus,
    create_image: unsafe extern "C" fn(VADisplay, *mut ImageFormat, c_int, c_int, *mut Image) -> VAStatus,
    put_image: unsafe extern "C" fn(
        VADisplay,
        VAId,
        VAId,
        c_int,
        c_int,
        c_uint,
        c_uint,
        c_int,
        c_int,
        c_uint,
        c_uint,
    ) -> VAStatus,
    destroy_image: unsafe extern "C" fn(VADisplay, VAId) -> VAStatus,
    /// `vaExportSurfaceHandle` (libva 2.1): absent from older libraries.
    export_surface: Option<unsafe extern "C" fn(VADisplay, VAId, u32, u32, *mut c_void) -> VAStatus>,
}

fn va() -> Result<&'static Va> {
    static VA: OnceLock<Result<Va, String>> = OnceLock::new();
    VA.get_or_init(|| load().map_err(|e| format!("{e:#}"))).as_ref().map_err(|e| anyhow!("{e}"))
}

fn load() -> Result<Va> {
    // SAFETY: libva's own libraries; the symbols get the signatures of va.h.
    unsafe {
        let va = libloading::Library::new("libva.so.2").context("libva.so.2 not found")?;
        let drm = libloading::Library::new("libva-drm.so.2").context("libva-drm.so.2 not found")?;
        macro_rules! sym {
            ($lib:expr, $name:literal) => {
                *$lib.get(concat!($name, "\0").as_bytes()).context($name)?
            };
        }
        let export_surface = va.get(b"vaExportSurfaceHandle\0").ok().map(|s| *s);
        Ok(Va {
            export_surface,
            get_display_drm: sym!(drm, "vaGetDisplayDRM"),
            initialize: sym!(va, "vaInitialize"),
            terminate: sym!(va, "vaTerminate"),
            error_str: sym!(va, "vaErrorStr"),
            vendor: sym!(va, "vaQueryVendorString"),
            max_entrypoints: sym!(va, "vaMaxNumEntrypoints"),
            query_entrypoints: sym!(va, "vaQueryConfigEntrypoints"),
            get_config_attributes: sym!(va, "vaGetConfigAttributes"),
            create_config: sym!(va, "vaCreateConfig"),
            destroy_config: sym!(va, "vaDestroyConfig"),
            create_surfaces: sym!(va, "vaCreateSurfaces"),
            destroy_surfaces: sym!(va, "vaDestroySurfaces"),
            create_context: sym!(va, "vaCreateContext"),
            destroy_context: sym!(va, "vaDestroyContext"),
            create_buffer: sym!(va, "vaCreateBuffer"),
            destroy_buffer: sym!(va, "vaDestroyBuffer"),
            map_buffer: sym!(va, "vaMapBuffer"),
            unmap_buffer: sym!(va, "vaUnmapBuffer"),
            begin_picture: sym!(va, "vaBeginPicture"),
            render_picture: sym!(va, "vaRenderPicture"),
            end_picture: sym!(va, "vaEndPicture"),
            sync_surface: sym!(va, "vaSyncSurface"),
            derive_image: sym!(va, "vaDeriveImage"),
            create_image: sym!(va, "vaCreateImage"),
            put_image: sym!(va, "vaPutImage"),
            destroy_image: sym!(va, "vaDestroyImage"),
            _va: va,
            _drm: drm,
        })
    }
}

impl Va {
    fn check(&self, status: VAStatus, what: &str) -> Result<()> {
        if status == VA_STATUS_SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr returns a static string.
        let text = unsafe { CStr::from_ptr((self.error_str)(status)) }.to_string_lossy();
        bail!("{what}: {text} ({status:#x})")
    }
}

/// An initialised VA display on a render node, with the chosen profile.
struct Display {
    va: &'static Va,
    dpy: VADisplay,
    _file: File,
    profile: c_int,
    entrypoint: c_int,
    rate_control: u32,
    /// The driver takes SPS and PPS from us.
    packed_headers: bool,
    /// Video processing configuration, if the driver has one.
    vpp_config: VAId,
    /// Whether the GPU conversion gave the CPU's colours (checked once).
    vpp_verdict: OnceLock<bool>,
    vendor: String,
    path: String,
}

// SAFETY: a VA display may be used from any thread. The drivers serialise the
// calls on one display (Mesa holds a lock for each call, iHD one for each context),
// and every encoder has its own configuration, context and surfaces.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

/// The display of this process, opened at the first encoder and never closed
/// (`vaTerminate`): Mesa's D3D12 driver (WSL) leaves a destructor for the thread
/// that used it, which crashes when the thread ends if the driver was unloaded in
/// the meantime. Only a display that opened is kept: a later encoder tries again.
fn shared_display() -> Result<&'static Display> {
    static DISPLAY: std::sync::Mutex<Option<&'static Display>> = std::sync::Mutex::new(None);
    let mut slot = DISPLAY.lock().expect("VA display");
    if let Some(display) = *slot {
        return Ok(display);
    }
    let display: &'static Display = Box::leak(Box::new(Display::open()?));
    *slot = Some(display);
    Ok(display)
}

impl Display {
    fn open() -> Result<Self> {
        Self::open_with(true)
    }

    /// `encoder`: false only for the tests of the conversion, on drivers that
    /// process video without encoding H.264 (Mesa's D3D12 driver on an Intel UHD).
    fn open_with(encoder: bool) -> Result<Self> {
        let va = va()?;
        let paths: Vec<String> = match std::env::var("NUVIRA_VAAPI_DEVICE") {
            Ok(path) => vec![path],
            Err(_) => (128..136).map(|n| format!("/dev/dri/renderD{n}")).collect(),
        };
        let mut errors = Vec::new();
        for path in paths {
            if !std::path::Path::new(&path).exists() {
                continue;
            }
            match Self::open_one(va, &path, encoder) {
                Ok(display) => return Ok(display),
                Err(e) => errors.push(format!("{path}: {e:#}")),
            }
        }
        if errors.is_empty() {
            bail!("no DRM render node");
        }
        bail!("{}", errors.join("; "))
    }

    fn open_one(va: &'static Va, path: &str, encoder: bool) -> Result<Self> {
        let file = File::options().read(true).write(true).open(path).context("not accessible")?;
        // SAFETY: plain libva calls; the display lives in `Display` and is terminated on drop.
        unsafe {
            let dpy = (va.get_display_drm)(file.as_raw_fd());
            if dpy.is_null() {
                bail!("no VA display");
            }
            let (mut major, mut minor) = (0, 0);
            let status = (va.initialize)(dpy, &mut major, &mut minor);
            if status != VA_STATUS_SUCCESS {
                (va.terminate)(dpy);
                return Err(va.check(status, "vaInitialize").unwrap_err());
            }
            let vendor_ptr = (va.vendor)(dpy);
            let vendor = if vendor_ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(vendor_ptr).to_string_lossy().into()
            };
            let mut display = Self {
                va,
                dpy,
                _file: file,
                profile: 0,
                entrypoint: 0,
                rate_control: 0,
                packed_headers: false,
                vpp_config: VA_INVALID_ID,
                vpp_verdict: OnceLock::new(),
                vendor,
                path: path.into(),
            };
            if encoder {
                display.choose_profile()?;
            }
            display.vpp_config = display.find_vpp();
            Ok(display)
        }
    }

    /// The video processing configuration, if the driver offers one and
    /// `NUVIRA_VAAPI_VPP` is not `0`.
    fn find_vpp(&self) -> VAId {
        if std::env::var("NUVIRA_VAAPI_VPP").is_ok_and(|v| v == "0") {
            return VA_INVALID_ID;
        }
        let va = self.va;
        // SAFETY: buffers sized as libva asks.
        unsafe {
            let max = (va.max_entrypoints)(self.dpy).max(1) as usize;
            let mut list = vec![0 as c_int; max];
            let mut n = 0;
            if (va.query_entrypoints)(self.dpy, PROFILE_NONE, list.as_mut_ptr(), &mut n) != VA_STATUS_SUCCESS
                || !list[..n.max(0) as usize].contains(&ENTRY_VIDEO_PROC)
            {
                return VA_INVALID_ID;
            }
            let mut config = VA_INVALID_ID;
            let mut attrib = VAConfigAttrib { type_: ATTRIB_RT_FORMAT, value: RT_FORMAT_YUV420 };
            let status =
                (va.create_config)(self.dpy, PROFILE_NONE, ENTRY_VIDEO_PROC, &mut attrib, 1, &mut config);
            if status != VA_STATUS_SUCCESS {
                debug!("VA-API: no video processing configuration ({status:#x})");
                return VA_INVALID_ID;
            }
            config
        }
    }

    /// Converts a test picture on the GPU once and compares it with the CPU's
    /// conversion: a driver whose colours differ (another matrix or range) keeps
    /// the CPU conversion.
    fn vpp_usable(&self) -> bool {
        if self.vpp_config == VA_INVALID_ID {
            return false;
        }
        *self.vpp_verdict.get_or_init(|| match vpp_check(self, 256, 64) {
            Ok(diff) if diff <= VPP_TOLERANCE => {
                info!(diff, "VA-API: colour conversion on the GPU");
                let modifiers = dmabuf_modifiers(self);
                info!(?modifiers, "VA-API: DMA-BUF layouts it imports");
                crate::set_dmabuf_import_modifiers(modifiers);
                true
            }
            Ok(diff) => {
                warn!(diff, "VA-API: the GPU conversion gives other colours: on the CPU");
                false
            }
            Err(e) => {
                warn!("VA-API: no colour conversion on the GPU ({e:#}): on the CPU");
                false
            }
        })
    }
}

/// Largest difference allowed between the GPU's and the CPU's NV12 on flat colours
/// (rounding and the GPU's own coefficients).
const VPP_TOLERANCE: u8 = 4;

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
fn vpp_check(d: &Display, w: u32, h: u32) -> Result<u8> {
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
const MOD_LINEAR: u64 = 0;
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// `I915_FORMAT_MOD_Y_TILED`.
const MOD_I915_Y_TILED: u64 = 0x0100_0000_0000_0002;
/// `VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS`.
const EXPORT_READ_COMPOSED: u32 = 0x0001 | 0x0008;

/// The DMA-BUF layouts the driver imports as RGB surfaces, best first: the layout
/// of its own RGB surfaces (exported, then imported back to be sure) and linear.
/// A compositor that can give that layout spares the GPU a copy into a linear
/// buffer for every picture.
fn dmabuf_modifiers(d: &Display) -> Vec<u64> {
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
    // Measured on the Kaby Lake-R laptop (docs/20-linux.md): the compositor takes a
    // tiled layout whenever offered, and importing it cost the agent more than
    // linear buffers: linear only, unless NUVIRA_DMABUF_TILED=1.
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
struct Vpp {
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
    fn new(d: &Display, width: u32, height: u32, mut target: VAId) -> Result<Self> {
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

    fn destroy(&mut self, d: &Display) {
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
    fn convert(&mut self, d: &Display, image: &BgraImage, target: VAId) -> Result<()> {
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
    fn convert_dmabuf(&mut self, d: &Display, image: &crate::DmaBufImage, target: VAId, out: (u32, u32)) -> Result<()> {
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

impl Display {
    /// The first profile and entrypoint whose rate control follows a bitrate (CBR,
    /// else VBR); failing that, one with only constant QP, the bitrate then followed
    /// here. iHD on Intel Gen9 (Skylake to Comet Lake) gives only constant QP on the
    /// low-power entrypoint unless the HuC firmware is loaded (i915.enable_guc=2),
    /// which the kernel does not do there by default (Kaby Lake-R, 2026-11-12).
    fn choose_profile(&mut self) -> Result<()> {
        if self.find_profile(false) || self.find_profile(true) {
            return Ok(());
        }
        bail!("the driver does not encode H.264")
    }

    fn find_profile(&mut self, cqp: bool) -> bool {
        let va = self.va;
        // SAFETY: buffers sized as libva asks.
        unsafe {
            let max = (va.max_entrypoints)(self.dpy).max(1) as usize;
            for profile in [PROFILE_MAIN, PROFILE_HIGH, PROFILE_CONSTRAINED_BASELINE] {
                let mut list = vec![0 as c_int; max];
                let mut n = 0;
                if (va.query_entrypoints)(self.dpy, profile, list.as_mut_ptr(), &mut n) != VA_STATUS_SUCCESS {
                    continue;
                }
                let list = &list[..n.max(0) as usize];
                for entrypoint in [ENTRY_ENC_SLICE, ENTRY_ENC_SLICE_LP] {
                    if !list.contains(&entrypoint) {
                        continue;
                    }
                    let mut attribs = [
                        VAConfigAttrib { type_: ATTRIB_RT_FORMAT, value: 0 },
                        VAConfigAttrib { type_: ATTRIB_RATE_CONTROL, value: 0 },
                        VAConfigAttrib { type_: ATTRIB_ENC_PACKED_HEADERS, value: 0 },
                    ];
                    let status =
                        (va.get_config_attributes)(self.dpy, profile, entrypoint, attribs.as_mut_ptr(), 3);
                    if status != VA_STATUS_SUCCESS
                        || attribs[0].value == ATTRIB_NOT_SUPPORTED
                        || attribs[0].value & RT_FORMAT_YUV420 == 0
                    {
                        continue;
                    }
                    let rc = attribs[1].value;
                    let rate_control = if cqp {
                        if rc == ATTRIB_NOT_SUPPORTED || rc & RC_CQP == 0 {
                            continue;
                        }
                        RC_CQP
                    } else if rc == ATTRIB_NOT_SUPPORTED {
                        0
                    } else if rc & RC_CBR != 0 {
                        RC_CBR
                    } else if rc & RC_VBR != 0 {
                        RC_VBR
                    } else {
                        continue; // only constant QP: second pass
                    };
                    self.profile = profile;
                    self.entrypoint = entrypoint;
                    self.rate_control = rate_control;
                    let packed = attribs[2].value;
                    let both = PACKED_SEQUENCE | PACKED_PICTURE;
                    self.packed_headers = packed != ATTRIB_NOT_SUPPORTED && packed & both == both;
                    return true;
                }
            }
        }
        false
    }
}

/// Size in macroblocks and buffers of one encoding size.
struct Session {
    width: u32,
    height: u32,
    mbs_w: u32,
    mbs_h: u32,
    config: VAId,
    context: VAId,
    /// Input surface (the NV12 image) and the two reconstructed pictures.
    input: VAId,
    recon: [VAId; 2],
    coded: VAId,
    coded_size: u32,
    /// `vaDeriveImage` does not work on this driver: `vaPutImage` from this image.
    put_image: Option<Image>,
    frame_num: u32,
    idr_id: u16,
    /// Index in `recon` of the last reconstructed picture, if there is one.
    last: Option<usize>,
    nv12: Vec<u8>,
    /// The conversion on the GPU, where it works; otherwise `nv12` on the CPU.
    vpp: Option<Vpp>,
}

impl Session {
    fn new(d: &Display, width: u32, height: u32) -> Result<Self> {
        let va = d.va;
        let (mbs_w, mbs_h) = (width.div_ceil(16), height.div_ceil(16));
        let (aw, ah) = (mbs_w * 16, mbs_h * 16);
        let mut s = Self {
            width,
            height,
            mbs_w,
            mbs_h,
            config: VA_INVALID_ID,
            context: VA_INVALID_ID,
            input: VA_INVALID_ID,
            recon: [VA_INVALID_ID; 2],
            coded: VA_INVALID_ID,
            coded_size: aw * ah * 3 / 2 + 64 * 1024,
            put_image: None,
            frame_num: 0,
            idr_id: 0,
            last: None,
            nv12: Vec::new(),
            vpp: None,
        };
        // SAFETY: libva calls with valid out-pointers; what is created is released
        // by `destroy` (also when a later step fails).
        let created = unsafe {
            (|| -> Result<()> {
                let mut attribs = vec![VAConfigAttrib { type_: ATTRIB_RT_FORMAT, value: RT_FORMAT_YUV420 }];
                if d.rate_control != 0 {
                    attribs.push(VAConfigAttrib { type_: ATTRIB_RATE_CONTROL, value: d.rate_control });
                }
                if d.packed_headers {
                    attribs.push(VAConfigAttrib {
                        type_: ATTRIB_ENC_PACKED_HEADERS,
                        value: PACKED_SEQUENCE | PACKED_PICTURE,
                    });
                }
                va.check(
                    (va.create_config)(
                        d.dpy,
                        d.profile,
                        d.entrypoint,
                        attribs.as_mut_ptr(),
                        attribs.len() as c_int,
                        &mut s.config,
                    ),
                    "vaCreateConfig",
                )?;
                let mut surfaces = [VA_INVALID_ID; 3];
                va.check(
                    (va.create_surfaces)(
                        d.dpy,
                        RT_FORMAT_YUV420,
                        aw,
                        ah,
                        surfaces.as_mut_ptr(),
                        3,
                        std::ptr::null_mut(),
                        0,
                    ),
                    "vaCreateSurfaces",
                )?;
                s.input = surfaces[0];
                s.recon = [surfaces[1], surfaces[2]];
                va.check(
                    (va.create_context)(
                        d.dpy,
                        s.config,
                        aw as c_int,
                        ah as c_int,
                        PROGRESSIVE,
                        surfaces.as_mut_ptr(),
                        3,
                        &mut s.context,
                    ),
                    "vaCreateContext",
                )?;
                va.check(
                    (va.create_buffer)(
                        d.dpy,
                        s.context,
                        BUF_CODED,
                        s.coded_size,
                        1,
                        std::ptr::null_mut(),
                        &mut s.coded,
                    ),
                    "coded buffer",
                )?;
                Ok(())
            })()
        };
        if let Err(e) = created {
            s.destroy(d);
            return Err(e);
        }
        if d.vpp_usable() {
            match Vpp::new(d, width, height, s.input) {
                Ok(vpp) => s.vpp = Some(vpp),
                Err(e) => warn!("VA-API: colour conversion on the CPU ({e:#})"),
            }
        }
        Ok(s)
    }

    fn destroy(&mut self, d: &Display) {
        let va = d.va;
        if let Some(mut vpp) = self.vpp.take() {
            vpp.destroy(d);
        }
        // SAFETY: only what was created, once (ids reset to invalid).
        unsafe {
            if let Some(image) = self.put_image.take() {
                (va.destroy_image)(d.dpy, image.image_id);
            }
            if self.coded != VA_INVALID_ID {
                (va.destroy_buffer)(d.dpy, self.coded);
            }
            if self.context != VA_INVALID_ID {
                (va.destroy_context)(d.dpy, self.context);
            }
            let mut surfaces = [self.input, self.recon[0], self.recon[1]];
            if surfaces[0] != VA_INVALID_ID {
                (va.destroy_surfaces)(d.dpy, surfaces.as_mut_ptr(), 3);
            }
            if self.config != VA_INVALID_ID {
                (va.destroy_config)(d.dpy, self.config);
            }
        }
        self.coded = VA_INVALID_ID;
        self.context = VA_INVALID_ID;
        self.input = VA_INVALID_ID;
        self.config = VA_INVALID_ID;
    }

    /// Copies the image into the input surface as NV12: converted by the GPU where
    /// it can, else on the CPU.
    fn upload(&mut self, d: &Display, image: &BgraImage) -> Result<()> {
        if let Some(vpp) = &mut self.vpp {
            match vpp.convert(d, image, self.input) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!("VA-API: GPU colour conversion failed ({e:#}): on the CPU from now on");
                    vpp.destroy(d);
                    self.vpp = None;
                }
            }
        }
        let va = d.va;
        let (w, h) = (self.width as usize, self.height as usize);
        self.nv12.resize(w * h * 3 / 2, 0);
        crate::convert::bgra_to_nv12(image, &mut self.nv12, w, h);
        // SAFETY: the mapped image is written within its pitches and planes.
        unsafe {
            let mut img = Image::default();
            let mut derived = self.put_image.is_none()
                && (va.derive_image)(d.dpy, self.input, &mut img) == VA_STATUS_SUCCESS;
            if derived && img.format.fourcc != FOURCC_NV12 {
                (va.destroy_image)(d.dpy, img.image_id);
                derived = false;
            }
            if !derived {
                if self.put_image.is_none() {
                    let mut format =
                        ImageFormat { fourcc: FOURCC_NV12, bits_per_pixel: 12, ..Default::default() };
                    let mut created = Image::default();
                    va.check(
                        (va.create_image)(d.dpy, &mut format, w as c_int, h as c_int, &mut created),
                        "vaCreateImage",
                    )?;
                    debug!("VA-API: surfaces not mappable, vaPutImage");
                    self.put_image = Some(created);
                }
                img = self.put_image.expect("image");
            }
            let mut ptr: *mut c_void = std::ptr::null_mut();
            let mapped = va.check((va.map_buffer)(d.dpy, img.buf, &mut ptr), "image map");
            if let Err(e) = mapped {
                if derived {
                    (va.destroy_image)(d.dpy, img.image_id);
                }
                return Err(e);
            }
            let base = ptr.cast::<u8>();
            for (plane, rows) in [(0usize, h), (1, h / 2)] {
                let src = &self.nv12[plane * w * h..];
                let dst = base.add(img.offsets[plane] as usize);
                let pitch = img.pitches[plane] as usize;
                for row in 0..rows {
                    std::ptr::copy_nonoverlapping(src[row * w..].as_ptr(), dst.add(row * pitch), w);
                }
            }
            (va.unmap_buffer)(d.dpy, img.buf);
            if derived {
                (va.destroy_image)(d.dpy, img.image_id);
            } else {
                va.check(
                    (va.put_image)(
                        d.dpy,
                        self.input,
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
                    "vaPutImage",
                )?;
            }
        }
        Ok(())
    }

    fn encode(
        &mut self,
        d: &Display,
        settings: &EncoderSettings,
        keyframe: bool,
        new_rate: bool,
        qp: Option<u8>,
    ) -> Result<Vec<u8>> {
        let va = d.va;
        let cqp = qp.is_some();
        let key = keyframe || self.last.is_none() || self.frame_num >= MAX_FRAME_NUM;
        if key {
            self.frame_num = 0;
            self.idr_id = self.idr_id.wrapping_add(1);
        }
        let target = match self.last {
            Some(i) => 1 - i,
            None => 0,
        };
        let cabac = d.profile != PROFILE_CONSTRAINED_BASELINE;
        let poc = (2 * self.frame_num) as i32;
        let current = VAPictureH264 {
            picture_id: self.recon[target],
            frame_idx: self.frame_num,
            flags: 0,
            top_field_order_cnt: poc,
            bottom_field_order_cnt: poc,
            reserved: [0; 4],
        };
        let reference = self.last.filter(|_| !key).map(|i| VAPictureH264 {
            picture_id: self.recon[i],
            frame_idx: self.frame_num - 1,
            flags: PICTURE_SHORT_TERM_REFERENCE,
            top_field_order_cnt: poc - 2,
            bottom_field_order_cnt: poc - 2,
            reserved: [0; 4],
        });

        let mut buffers: Vec<VAId> = Vec::with_capacity(10);
        let bits = settings.bitrate_bps;
        let fps = settings.max_fps.max(1);
        // SAFETY: parameter buffers copied by libva at creation; all destroyed below.
        let result = unsafe {
            (|| -> Result<()> {
                let mut add = |kind: c_int, data: *mut c_void, size: usize, what: &str| -> Result<()> {
                    let mut id = VA_INVALID_ID;
                    va.check(
                        (va.create_buffer)(d.dpy, self.context, kind, size as c_uint, 1, data, &mut id),
                        what,
                    )?;
                    buffers.push(id);
                    Ok(())
                };
                if key {
                    let crop = self.mbs_w * 16 != self.width || self.mbs_h * 16 != self.height;
                    let mut seq: SequenceParams = std::mem::zeroed();
                    seq.level_idc = level(self.mbs_w * self.mbs_h);
                    seq.intra_period = INTRA_PERIOD;
                    seq.intra_idr_period = INTRA_PERIOD;
                    seq.ip_period = 1;
                    seq.bits_per_second = if cqp { 0 } else { bits };
                    seq.max_num_ref_frames = 1;
                    seq.picture_width_in_mbs = self.mbs_w as u16;
                    seq.picture_height_in_mbs = self.mbs_h as u16;
                    // chroma 4:2:0, frame_mbs_only, direct_8x8_inference, frame_num bits, POC type 2.
                    seq.seq_fields = 1 | (1 << 2) | (1 << 5) | (LOG2_MAX_FRAME_NUM_MINUS4 << 6) | (2 << 10);
                    if crop {
                        seq.frame_cropping_flag = 1;
                        // In chroma units (2 pixels) for 4:2:0.
                        seq.frame_crop_right_offset = (self.mbs_w * 16 - self.width) / 2;
                        seq.frame_crop_bottom_offset = (self.mbs_h * 16 - self.height) / 2;
                    }
                    seq.num_units_in_tick = 1;
                    seq.time_scale = fps * 2;
                    // VUI as in our SPS: timing, bitstream restriction (no reordering:
                    // decoders show every frame at once), log2_max_mv_length 15 and 15,
                    // motion vectors over the picture's boundaries.
                    seq.vui_parameters_present_flag = 1;
                    seq.vui_fields = (1 << 1) | (1 << 2) | (15 << 3) | (15 << 8) | (1 << 15);
                    add(BUF_SEQUENCE, (&raw mut seq).cast(), size_of::<SequenceParams>(), "sequence")?;
                    if d.packed_headers {
                        let sets = ParameterSets {
                            profile: d.profile,
                            level: seq.level_idc,
                            mbs_w: self.mbs_w,
                            mbs_h: self.mbs_h,
                            crop_right: seq.frame_crop_right_offset,
                            crop_bottom: seq.frame_crop_bottom_offset,
                            time_scale: seq.time_scale,
                            cabac,
                        };
                        for (kind, mut nal, what) in [
                            (PACKED_SEQUENCE, sets.sps(), "packed SPS"),
                            (PACKED_PICTURE, sets.pps(), "packed PPS"),
                        ] {
                            let mut params = PackedHeaderParams {
                                type_: kind,
                                bit_length: (nal.len() * 8) as u32,
                                has_emulation_bytes: 1,
                                reserved: [0; 4],
                            };
                            let size = size_of::<PackedHeaderParams>();
                            add(BUF_PACKED_HEADER_PARAMETER, (&raw mut params).cast(), size, what)?;
                            add(BUF_PACKED_HEADER_DATA, nal.as_mut_ptr().cast(), nal.len(), what)?;
                        }
                    }
                }
                if (key || new_rate) && !cqp {
                    let mut rc = Misc {
                        type_: MISC_RATE_CONTROL,
                        data: RateControl {
                            bits_per_second: bits,
                            target_percentage: 100,
                            window_size: 1000,
                            // reset (new bitrate during the session), disable_frame_skip.
                            rc_flags: u32::from(new_rate) | 2,
                            ..Default::default()
                        },
                    };
                    add(BUF_MISC, (&raw mut rc).cast(), size_of::<Misc<RateControl>>(), "rate control")?;
                    let mut hrd = Misc {
                        type_: MISC_HRD,
                        // One second of buffer, half full at start.
                        data: Hrd {
                            initial_buffer_fullness: bits / 2,
                            buffer_size: bits,
                            ..Default::default()
                        },
                    };
                    add(BUF_MISC, (&raw mut hrd).cast(), size_of::<Misc<Hrd>>(), "HRD")?;
                    let mut rate = Misc {
                        type_: MISC_FRAME_RATE,
                        data: FrameRate { framerate: fps, ..Default::default() },
                    };
                    add(BUF_MISC, (&raw mut rate).cast(), size_of::<Misc<FrameRate>>(), "frame rate")?;
                }

                let mut pic: PictureParams = std::mem::zeroed();
                pic.curr_pic = current;
                pic.reference_frames = [INVALID_PICTURE; 16];
                if let Some(r) = reference {
                    pic.reference_frames[0] = r;
                }
                pic.coded_buf = self.coded;
                pic.frame_num = self.frame_num as u16;
                pic.pic_init_qp = 26;
                // idr_pic_flag, reference_pic_flag = 1, CABAC, deblocking control present.
                pic.pic_fields = u32::from(key) | (1 << 1) | (u32::from(cabac) << 3) | (1 << 9);
                add(BUF_PICTURE, (&raw mut pic).cast(), size_of::<PictureParams>(), "picture")?;

                let mut slice: SliceParams = std::mem::zeroed();
                slice.num_macroblocks = self.mbs_w * self.mbs_h;
                slice.macroblock_info = VA_INVALID_ID;
                slice.slice_type = if key { SLICE_I } else { SLICE_P };
                slice.idr_pic_id = self.idr_id;
                // Constant QP: the frame's QP as a delta from the PPS's 26, so that our
                // packed PPS (pic_init_qp 26) stays true whatever the QP.
                if let Some(qp) = qp {
                    slice.slice_qp_delta = qp as i8 - 26;
                }
                slice.ref_pic_list0 = [INVALID_PICTURE; 32];
                slice.ref_pic_list1 = [INVALID_PICTURE; 32];
                if let Some(r) = reference {
                    slice.ref_pic_list0[0] = r;
                }
                add(BUF_SLICE, (&raw mut slice).cast(), size_of::<SliceParams>(), "slice")?;

                va.check((va.begin_picture)(d.dpy, self.context, self.input), "vaBeginPicture")?;
                let rendered = va.check(
                    (va.render_picture)(d.dpy, self.context, buffers.as_mut_ptr(), buffers.len() as c_int),
                    "vaRenderPicture",
                );
                va.check((va.end_picture)(d.dpy, self.context), "vaEndPicture")?;
                rendered?;
                va.check((va.sync_surface)(d.dpy, self.input), "vaSyncSurface")
            })()
        };
        // SAFETY: the parameter buffers of this frame, no longer used.
        unsafe {
            for id in buffers {
                (va.destroy_buffer)(d.dpy, id);
            }
        }
        result?;

        let mut out = Vec::new();
        // SAFETY: the coded buffer is a list of segments while mapped.
        unsafe {
            let mut ptr: *mut c_void = std::ptr::null_mut();
            va.check((va.map_buffer)(d.dpy, self.coded, &mut ptr), "coded buffer map")?;
            let mut segment = ptr.cast::<CodedSegment>();
            while !segment.is_null() {
                let s = &*segment;
                if !s.buf.is_null() && s.size > 0 {
                    out.extend_from_slice(std::slice::from_raw_parts(s.buf.cast::<u8>(), s.size as usize));
                }
                segment = s.next.cast();
            }
            (va.unmap_buffer)(d.dpy, self.coded);
        }
        self.last = Some(target);
        self.frame_num += 1;
        Ok(out)
    }
}

/// H.264 level for a frame of `mbs` macroblocks (frame size limit only).
fn level(mbs: u32) -> u8 {
    match mbs {
        0..=8192 => 41,
        8193..=36864 => 51,
        _ => 60,
    }
}

/// SPS and PPS matching the parameters given to the driver, as Annex B NAL units
/// (start code, emulation prevention bytes).
struct ParameterSets {
    profile: c_int,
    level: u8,
    mbs_w: u32,
    mbs_h: u32,
    crop_right: u32,
    crop_bottom: u32,
    time_scale: u32,
    cabac: bool,
}

impl ParameterSets {
    fn sps(&self) -> Vec<u8> {
        let mut b = Bits::default();
        let (profile_idc, constraints) = match self.profile {
            PROFILE_HIGH => (100, 0),
            PROFILE_CONSTRAINED_BASELINE => (66, 0b11_0000), // constraint_set0 and set1
            _ => (77, 0),
        };
        b.u(8, profile_idc);
        b.u(6, constraints);
        b.u(2, 0);
        b.u(8, u32::from(self.level));
        b.ue(0); // seq_parameter_set_id
        if profile_idc == 100 {
            b.ue(1); // chroma_format_idc 4:2:0
            b.ue(0); // bit_depth_luma_minus8
            b.ue(0); // bit_depth_chroma_minus8
            b.u(1, 0); // qpprime_y_zero_transform_bypass_flag
            b.u(1, 0); // seq_scaling_matrix_present_flag
        }
        b.ue(LOG2_MAX_FRAME_NUM_MINUS4);
        b.ue(2); // pic_order_cnt_type
        b.ue(1); // max_num_ref_frames
        b.u(1, 0); // gaps_in_frame_num_value_allowed_flag
        b.ue(self.mbs_w - 1);
        b.ue(self.mbs_h - 1);
        b.u(1, 1); // frame_mbs_only_flag
        b.u(1, 1); // direct_8x8_inference_flag
        let crop = self.crop_right != 0 || self.crop_bottom != 0;
        b.u(1, u32::from(crop));
        if crop {
            b.ue(0);
            b.ue(self.crop_right);
            b.ue(0);
            b.ue(self.crop_bottom);
        }
        b.u(1, 1); // vui_parameters_present_flag
        b.u(1, 0); // aspect_ratio_info_present_flag
        b.u(1, 0); // overscan_info_present_flag
        b.u(1, 0); // video_signal_type_present_flag
        b.u(1, 0); // chroma_loc_info_present_flag
        b.u(1, 1); // timing_info_present_flag
        b.u(32, 1); // num_units_in_tick
        b.u(32, self.time_scale);
        b.u(1, 0); // fixed_frame_rate_flag
        b.u(1, 0); // nal_hrd_parameters_present_flag
        b.u(1, 0); // vcl_hrd_parameters_present_flag
        b.u(1, 0); // pic_struct_present_flag
        b.u(1, 1); // bitstream_restriction_flag
        b.u(1, 1); // motion_vectors_over_pic_boundaries_flag
        b.ue(0); // max_bytes_per_pic_denom
        b.ue(0); // max_bits_per_mb_denom
        b.ue(15); // log2_max_mv_length_horizontal
        b.ue(15); // log2_max_mv_length_vertical
        b.ue(0); // max_num_reorder_frames
        b.ue(1); // max_dec_frame_buffering
        b.nal(0x67) // nal_ref_idc 3, SPS
    }

    fn pps(&self) -> Vec<u8> {
        let mut b = Bits::default();
        b.ue(0); // pic_parameter_set_id
        b.ue(0); // seq_parameter_set_id
        b.u(1, u32::from(self.cabac)); // entropy_coding_mode_flag
        b.u(1, 0); // bottom_field_pic_order_in_frame_present_flag
        b.ue(0); // num_slice_groups_minus1
        b.ue(0); // num_ref_idx_l0_default_active_minus1
        b.ue(0); // num_ref_idx_l1_default_active_minus1
        b.u(1, 0); // weighted_pred_flag
        b.u(2, 0); // weighted_bipred_idc
        b.se(0); // pic_init_qp_minus26 (pic_init_qp 26)
        b.se(0); // pic_init_qs_minus26
        b.se(0); // chroma_qp_index_offset
        b.u(1, 1); // deblocking_filter_control_present_flag
        b.u(1, 0); // constrained_intra_pred_flag
        b.u(1, 0); // redundant_pic_cnt_present_flag
        b.nal(0x68) // nal_ref_idc 3, PPS
    }
}

/// Bit writer for parameter sets (RBSP).
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    acc: u8,
    n: u32,
}

impl Bits {
    fn u(&mut self, bits: u32, value: u32) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1) as u8;
            self.n += 1;
            if self.n == 8 {
                self.bytes.push(self.acc);
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self, value: u32) {
        let v = u64::from(value) + 1;
        let len = 64 - v.leading_zeros();
        self.u(len - 1, 0);
        for i in (0..len).rev() {
            self.u(1, ((v >> i) & 1) as u32);
        }
    }

    /// Signed Exp-Golomb.
    fn se(&mut self, value: i32) {
        let mapped = if value > 0 { 2 * value.unsigned_abs() - 1 } else { 2 * value.unsigned_abs() };
        self.ue(mapped);
    }

    /// Ends the RBSP (stop bit, alignment) and wraps it in a NAL unit with this
    /// header byte: start code and emulation prevention bytes.
    fn nal(mut self, header: u8) -> Vec<u8> {
        self.u(1, 1);
        while self.n != 0 {
            self.u(1, 0);
        }
        let mut out = vec![0, 0, 0, 1, header];
        let mut zeros = 0;
        for byte in self.bytes {
            if zeros == 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            out.push(byte);
        }
        out
    }
}

/// NAL unit types present in Annex B data.
pub(crate) fn nal_types(data: &[u8]) -> Vec<u8> {
    data.windows(4).filter(|w| w[0] == 0 && w[1] == 0 && w[2] == 1).map(|w| w[3] & 0x1f).collect()
}

/// Bitrate followed by changing the QP, for drivers with only constant QP.
///
/// Each frame's size is compared with its share of the bitrate (the time since
/// the previous frame, from one frame at the highest rate to 100 ms; a keyframe
/// may use eight times as much). Six QP steps halve the size, so the error in QP
/// is `6 * log2(size / share)`: the QP goes up by half of it at once (at most 3
/// a frame), and down by 15 % of it, at most one a frame; nearly empty frames
/// (a still screen) say nothing about the picture and lower it only by 0.1.
struct QpControl {
    qp: f32,
    bitrate: u32,
    max_fps: u32,
    last: Option<std::time::Instant>,
}

impl QpControl {
    const MIN: f32 = 20.0;
    const MAX: f32 = 46.0;

    fn new(settings: &EncoderSettings) -> Self {
        Self { qp: 28.0, bitrate: settings.bitrate_bps.max(1), max_fps: settings.max_fps.max(1), last: None }
    }

    fn qp(&self) -> u8 {
        self.qp.round() as u8
    }

    fn set_bitrate(&mut self, bps: u32) {
        let bps = bps.max(1);
        // Half of the change at once, the rest from the next frames.
        let change = 6.0 * (self.bitrate as f32 / bps as f32).log2() * 0.5;
        self.qp = (self.qp + change).clamp(Self::MIN, Self::MAX);
        self.bitrate = bps;
    }

    fn frame(&mut self, bytes: usize, keyframe: bool) {
        let now = std::time::Instant::now();
        let min = 1.0 / self.max_fps as f32;
        let dt = self.last.map_or(min, |t| (now - t).as_secs_f32()).clamp(min, 0.1f32.max(min));
        self.last = Some(now);
        let mut share = self.bitrate as f32 * dt;
        if keyframe {
            share *= 8.0;
        }
        let ratio = (bytes as f32 * 8.0 / share).max(1e-3);
        let error = 6.0 * ratio.log2();
        let step = if error > 0.0 {
            (error * 0.5).min(3.0)
        } else if ratio < 0.125 {
            -0.1
        } else {
            (error * 0.15).max(-1.0)
        };
        self.qp = (self.qp + step).clamp(Self::MIN, Self::MAX);
    }
}

pub struct VaapiEncoder {
    display: &'static Display,
    settings: EncoderSettings,
    session: Option<Session>,
    force_key: bool,
    new_rate: bool,
    /// The driver has only constant QP: the bitrate is followed here.
    qp_control: Option<QpControl>,
    /// The driver failed during the session: OpenH264 from then on.
    fallback: Option<OpenH264Encoder>,
    /// DMA-BUF pictures are imported (until an import fails).
    dmabuf: bool,
}

impl VaapiEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        driver_survives(settings)?;
        let display = shared_display()?;
        let qp_control = (display.rate_control == RC_CQP).then(|| QpControl::new(&settings));
        let mut enc = Self {
            display,
            settings,
            session: None,
            force_key: true,
            new_rate: false,
            qp_control,
            fallback: None,
            dmabuf: true,
        };
        // The keyframe must carry the parameter sets (ours or the driver's).
        let (w, h) = (320u32, 180u32);
        let data = vec![128u8; (w * h * 4) as usize];
        let first =
            enc.encode_va(&BgraImage { width: w, height: h, stride: w as usize * 4, data: &data }, true)?;
        let types = nal_types(&first.data);
        if !(types.contains(&7) && types.contains(&8) && types.contains(&5)) {
            bail!("the driver gives no SPS/PPS (NAL units {types:?})");
        }
        enc.session_drop();
        info!(
            device = %enc.display.path,
            driver = %enc.display.vendor,
            profile = enc.display.profile,
            low_power = enc.display.entrypoint == ENTRY_ENC_SLICE_LP,
            rate_control = enc.display.rate_control,
            packed_headers = enc.display.packed_headers,
            "VA-API encoder"
        );
        Ok(enc)
    }

    pub fn device_name(&self) -> &str {
        &self.display.vendor
    }

    fn session_drop(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.destroy(self.display);
        }
    }

    fn session_for(&mut self, w: u32, h: u32) -> Result<()> {
        if self.session.as_ref().is_none_or(|s| (s.width, s.height) != (w, h)) {
            self.session_drop();
            self.session = Some(Session::new(self.display, w, h)?);
            debug!(w, h, "VA-API session");
            self.force_key = true;
        }
        Ok(())
    }

    fn encode_va(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<EncodedFrame> {
        let (w, h) = crate::convert::even_size(image);
        let (w, h) = (w as u32, h as u32);
        self.session_for(w, h)?;
        let session = self.session.as_mut().context("VA-API session")?;
        session.upload(self.display, image)?;
        self.encode_input(force_keyframe)
    }

    /// A DMA-BUF picture converted (and reduced to `w`x`h`) on the GPU, then encoded.
    fn encode_dmabuf_va(
        &mut self,
        image: &crate::DmaBufImage,
        w: u32,
        h: u32,
        force_keyframe: bool,
    ) -> Result<EncodedFrame> {
        self.session_for(w, h)?;
        let display = self.display;
        let session = self.session.as_mut().context("VA-API session")?;
        let input = session.input;
        let vpp = session.vpp.as_mut().context("no video processing on this driver")?;
        vpp.convert_dmabuf(display, image, input, (w, h))?;
        self.encode_input(force_keyframe)
    }

    /// Encodes the session's input surface, already filled.
    fn encode_input(&mut self, force_keyframe: bool) -> Result<EncodedFrame> {
        // Both taken: a keyframe asked for also satisfies the pending one.
        let pending = std::mem::take(&mut self.force_key);
        let key = force_keyframe || pending;
        let new_rate = std::mem::take(&mut self.new_rate);
        let session = self.session.as_mut().context("VA-API session")?;
        let qp = self.qp_control.as_ref().map(QpControl::qp);
        let data = session.encode(self.display, &self.settings, key, new_rate, qp)?;
        let keyframe = nal_types(&data).contains(&5);
        if let Some(control) = &mut self.qp_control {
            control.frame(data.len(), keyframe);
        }
        Ok(EncodedFrame { data, keyframe, ..Default::default() })
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        self.session_drop();
    }
}

impl VideoEncoder for VaapiEncoder {
    fn name(&self) -> &'static str {
        if self.fallback.is_some() { "openh264 (VA-API failed)" } else { "vaapi" }
    }

    fn set_bitrate(&mut self, bps: u32) {
        if let Some(sw) = &mut self.fallback {
            sw.set_bitrate(bps);
            return;
        }
        if bps != self.settings.bitrate_bps {
            self.settings.bitrate_bps = bps;
            self.new_rate = true;
            if let Some(control) = &mut self.qp_control {
                control.set_bitrate(bps);
            }
        }
    }

    fn encode_dmabuf(
        &mut self,
        image: &crate::DmaBufImage,
        size: (u32, u32),
        force_keyframe: bool,
    ) -> Option<Result<Option<EncodedFrame>>> {
        if self.fallback.is_some() || !self.dmabuf {
            return None;
        }
        let w = size.0.min(image.width) & !1;
        let h = size.1.min(image.height) & !1;
        if w < 2 || h < 2 {
            return None;
        }
        match self.encode_dmabuf_va(image, w, h, force_keyframe) {
            Ok(frame) if frame.data.is_empty() => Some(Ok(None)),
            Ok(frame) => Some(Ok(Some(frame))),
            // A tiled layout the driver does not take after all: linear from now on
            // (the capture negotiates again with the compositor).
            Err(e) if image.modifier != MOD_LINEAR => {
                warn!(modifier = format_args!("{:#x}", image.modifier), "VA-API: DMA-BUF import failed ({e:#}): linear buffers from now on");
                crate::set_dmabuf_import_modifiers(vec![MOD_LINEAR]);
                None
            }
            Err(e) => {
                warn!("VA-API: DMA-BUF import failed ({e:#}): the pictures go through RAM from now on");
                self.dmabuf = false;
                None
            }
        }
    }

    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        if let Some(sw) = &mut self.fallback {
            return sw.encode(image, force_keyframe);
        }
        if image.width < 2 || image.height < 2 {
            return Ok(None);
        }
        match self.encode_va(image, force_keyframe) {
            Ok(frame) if !frame.data.is_empty() => Ok(Some(frame)),
            Ok(_) => Ok(None),
            Err(e) => {
                warn!("VA-API encoding failed ({e:#}): OpenH264 from now on");
                self.session_drop();
                let mut sw = OpenH264Encoder::new(self.settings)?;
                let frame = sw.encode(image, true);
                self.fallback = Some(sw);
                frame
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GPU conversion against the CPU's, and their times at 1080p. Skipped
    /// without a VA driver that processes video. On a driver without H.264
    /// encoding too (Mesa's D3D12 driver on the NUC's Intel UHD, in WSL).
    #[test]
    fn vpp_matches_cpu() {
        let d = match Display::open_with(false) {
            Ok(d) if d.vpp_config != VA_INVALID_ID => d,
            Ok(_) => return eprintln!("no video processing: skipped"),
            Err(e) => return eprintln!("no VA-API ({e:#}): skipped"),
        };
        eprintln!("driver: {}", d.vendor);
        // Mesa's D3D12 driver in WSL has no RGB surfaces (only NV12 and P010): it
        // fails here and the encoder keeps the CPU conversion.
        let diff = match vpp_check(&d, 256, 64) {
            Ok(diff) => diff,
            Err(e) => return eprintln!("GPU conversion refused ({e:#}): the encoder converts on the CPU"),
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

    #[test]
    fn exp_golomb_and_nal() {
        let mut b = Bits::default();
        b.ue(0); // 1
        b.ue(1); // 010
        b.ue(2); // 011
        b.se(-1); // ue(2) 011
        b.se(1); // ue(1) 010
        assert_eq!(b.bytes, [0b1010_0110]);
        assert_eq!(b.n, 5);
        // Emulation prevention after two zero bytes.
        let mut b = Bits::default();
        b.u(24, 1);
        assert_eq!(b.nal(0x67), [0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0x80]);
    }

    #[test]
    fn sps_of_1080p() {
        let sets = ParameterSets {
            profile: PROFILE_MAIN,
            level: 41,
            mbs_w: 120,
            mbs_h: 68,
            crop_right: 0,
            crop_bottom: 4,
            time_scale: 120,
            cabac: true,
        };
        let sps = sets.sps();
        assert_eq!(&sps[..8], [0, 0, 0, 1, 0x67, 77, 0, 41]);
        assert_eq!(nal_types(&sps), [7]);
        let pps = sets.pps();
        assert_eq!(nal_types(&pps), [8]);
        // pps_id 0, sps_id 0, CABAC: 1 1 1 ...
        assert_eq!(pps[5] >> 5, 0b111);
    }

    #[test]
    fn levels() {
        assert_eq!(level(120 * 68), 41); // 1080p
        assert_eq!(level(240 * 135), 51); // 4K
        assert_eq!(level(320 * 180), 60); // 5K
    }

    #[test]
    fn qp_control() {
        let settings = EncoderSettings { bitrate_bps: 3_000_000, max_fps: 30, ltr: false };
        let mut c = QpControl::new(&settings);
        // 100 kB a frame at 30 fps (24 Mbit/s against 3): the QP goes up 3 a frame.
        c.frame(100_000, false);
        assert_eq!(c.qp(), 31);
        for _ in 0..10 {
            c.frame(100_000, false);
        }
        assert_eq!(c.qp(), 46);
        // A frame on its share (12.5 kB) leaves it where it is.
        let mut c = QpControl::new(&settings);
        c.frame(12_500, false);
        assert_eq!(c.qp(), 28);
        // A keyframe may be eight times larger.
        c.frame(100_000, true);
        assert_eq!(c.qp(), 28);
        // Half the share: down by 0.9.
        c.frame(6_250, false);
        assert!((c.qp - 27.1).abs() < 0.01, "{}", c.qp);
        // A still screen: down slowly.
        c.frame(100, false);
        assert!((c.qp - 27.0).abs() < 0.01, "{}", c.qp);
        // A third of the bitrate: half of 9.5 QP up at once.
        c.set_bitrate(1_000_000);
        assert_eq!(c.qp(), 32);
    }

    /// Needs a VA-API device that encodes H.264; skipped elsewhere.
    #[test]
    fn encodes_and_decodes() {
        let mut enc = match VaapiEncoder::new(EncoderSettings::default()) {
            Ok(enc) => enc,
            Err(e) => {
                eprintln!("no VA-API encoder here: {e:#}");
                return;
            }
        };
        let (w, h) = (322u32, 180u32);
        let stride = w as usize * 4 + 16;
        let mut data = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                data[y * stride + x * 4..][..4].copy_from_slice(&[
                    (x % 256) as u8,
                    (y % 256) as u8,
                    128,
                    255,
                ]);
            }
        }
        let image = BgraImage { width: w, height: h, stride, data: &data };
        let mut dec = crate::OpenH264Decoder::new().unwrap();
        let mut decoded = None;
        for i in 0..10 {
            if i == 5 {
                enc.set_bitrate(2_000_000);
            }
            let frame = enc.encode(&image, i == 0).unwrap().expect("a frame for every frame");
            eprintln!("frame {i}: {} bytes, NAL units {:?}", frame.data.len(), nal_types(&frame.data));
            assert_eq!(frame.keyframe, i == 0, "frame {i}");
            if let Some(yuv) = crate::VideoDecoder::decode(&mut dec, &frame.data).unwrap() {
                decoded = Some((yuv.width, yuv.height, yuv.y[yuv.width as usize * 90 + 100]));
            }
        }
        assert_eq!(enc.name(), "vaapi");
        let (dw, dh, luma) = decoded.expect("OpenH264 decodes the VA-API stream");
        assert_eq!((dw, dh), (322, 180));
        assert!((90..=118).contains(&luma), "luma {luma}");
    }
}

/// Set in the child of [`driver_survives`]: it tries the driver itself.
static IN_PROBE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the driver opens and encodes without killing the process: the first
/// time, a child process (fork) does what [`VaapiEncoder::new`] does. Some drivers
/// abort instead of failing: i965 2.4.1 on an Intel Braswell (Celeron N3xxx),
/// `intel_enc_hw_context_init: Assertion 'encoder_context->mfc_context' failed` in
/// vaCreateContext, also with ffmpeg (2026-11-12); without this the agent died at
/// its start. A driver that only fails is left to the attempt in this process,
/// which tells why; one that crashes or hangs is not used again in this process.
fn driver_survives(settings: EncoderSettings) -> Result<()> {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    static VERDICT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    if IN_PROBE.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let verdict = VERDICT.get_or_init(|| {
        // SAFETY: the child only runs the encoder test and leaves with _exit (no
        // destructors, no atexit handlers of the parent).
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Ok(()); // no child: tried in this process
        }
        if pid == 0 {
            IN_PROBE.store(true, std::sync::atomic::Ordering::Relaxed);
            let ok = tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
                VaapiEncoder::new(settings).is_ok()
            });
            unsafe { libc::_exit(if ok { 0 } else { 1 }) }
        }
        let started = Instant::now();
        let mut status = 0;
        loop {
            // SAFETY: waiting for our own child.
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == pid {
                break;
            }
            if r < 0 {
                return Ok(());
            }
            if started.elapsed() > Duration::from_secs(15) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                return Err("the VA-API driver did not answer within 15 s".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if libc::WIFSIGNALED(status) {
            return Err(format!(
                "the VA-API driver crashed while trying it (signal {})",
                libc::WTERMSIG(status)
            ));
        }
        Ok(())
    });
    verdict.clone().map_err(|e| anyhow::anyhow!(e))
}
