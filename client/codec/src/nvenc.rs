//! H.264 with NVENC (Linux): NVIDIA graphics cards with the proprietary driver.
//!
//! `libnvidia-encode.so.1` and `libcuda.so.1` come with NVIDIA's driver and are
//! loaded at run time: without them (any other computer) VA-API or OpenH264 is used.
//! Nothing is linked and no SDK is needed.
//!
//! - API 12.0 (driver 520 or later); the structures are byte buffers with the
//!   offsets of NVIDIA's header in [`crate::nvenc_layout`], generated from it by
//!   `tools/nvenc-layout.c`.
//! - A CUDA context of its own on the first GPU, made current only around NVENC's
//!   calls (the encoder may move between threads).
//! - Input: the BGRA image as it is (`NV_ENC_BUFFER_FORMAT_ARGB`) in an input
//!   buffer of NVENC's: the GPU converts it to YUV, nothing on the CPU.
//! - Preset P2 with the ultra-low-latency tuning, profile Main (CABAC), CBR, no B
//!   frames, an infinite GOP (keyframes when asked), SPS and PPS with every IDR;
//!   the bitrate is changed during the session with `nvEncReconfigureEncoder`.
//!   Linux has no asynchronous mode: one frame in, one frame out.
//! - If the driver fails during the session the encoder goes on with OpenH264.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail, ensure};
use tracing::{debug, info, warn};

use crate::nvenc_layout as l;
use crate::{BgraImage, EncodedFrame, EncoderSettings, OpenH264Encoder, VideoEncoder};

type Status = c_int;
type Handle = *mut c_void;

const NV_ENC_SUCCESS: Status = 0;
const CUDA_SUCCESS: c_int = 0;
/// Lowest API the program speaks (12.0), as `NvEncodeAPIGetMaxSupportedVersion` gives it.
const MIN_DRIVER_API: u32 = 12 << 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

impl Guid {
    /// From the bytes of the structure in memory (little-endian), as the layout gives them.
    const fn from_bytes(b: [u8; 16]) -> Self {
        Self {
            data1: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            data2: u16::from_le_bytes([b[4], b[5]]),
            data3: u16::from_le_bytes([b[6], b[7]]),
            data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        }
    }
}

const _: () = assert!(size_of::<Guid>() == 16);

/// An NVENC structure: zeroed bytes of the header's size, aligned for pointers,
/// with its version in the first word.
struct Params(Vec<u64>);

impl Params {
    fn new(size: usize, version: u32) -> Self {
        let mut p = Self(vec![0; size.div_ceil(8)]);
        p.put_u32(0, version);
        p
    }

    fn bytes(&mut self) -> &mut [u8] {
        let len = self.0.len() * 8;
        // SAFETY: the vector's memory, as bytes.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), len) }
    }

    fn put_u32(&mut self, offset: usize, value: u32) {
        self.bytes()[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn put_ptr(&mut self, offset: usize, value: *mut c_void) {
        self.bytes()[offset..offset + 8].copy_from_slice(&(value as usize as u64).to_ne_bytes());
    }

    fn put_guid(&mut self, offset: usize, guid: [u8; 16]) {
        self.bytes()[offset..offset + 16].copy_from_slice(&guid);
    }

    fn set_bit(&mut self, (offset, bit): (usize, u32), on: bool) {
        let word = self.get_u32(offset);
        self.put_u32(offset, if on { word | 1 << bit } else { word & !(1 << bit) });
    }

    fn get_u32(&mut self, offset: usize) -> u32 {
        u32::from_ne_bytes(self.bytes()[offset..offset + 4].try_into().expect("4 bytes"))
    }

    fn get_ptr(&mut self, offset: usize) -> *mut c_void {
        u64::from_ne_bytes(self.bytes()[offset..offset + 8].try_into().expect("8 bytes")) as usize
            as *mut c_void
    }

    fn ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr().cast()
    }
}

const _: () = assert!(size_of::<*mut c_void>() == 8, "the layout is of a 64-bit build");

type OpenSession = unsafe extern "C" fn(*mut c_void, *mut Handle) -> Status;
type PresetConfig = unsafe extern "C" fn(Handle, Guid, Guid, c_uint, *mut c_void) -> Status;
type WithParams = unsafe extern "C" fn(Handle, *mut c_void) -> Status;
type Destroy = unsafe extern "C" fn(Handle) -> Status;
type LastError = unsafe extern "C" fn(Handle) -> *const c_char;

/// NVENC's functions and CUDA's few, from the driver's libraries.
struct Nv {
    open_session: OpenSession,
    preset_config: PresetConfig,
    initialize: WithParams,
    create_input: WithParams,
    destroy_input: WithParams,
    create_bitstream: WithParams,
    destroy_bitstream: WithParams,
    lock_input: WithParams,
    unlock_input: WithParams,
    encode: WithParams,
    lock_bitstream: WithParams,
    unlock_bitstream: WithParams,
    reconfigure: WithParams,
    destroy: Destroy,
    last_error: LastError,
    cu_init: unsafe extern "C" fn(c_uint) -> c_int,
    cu_device_get: unsafe extern "C" fn(*mut c_int, c_int) -> c_int,
    cu_device_name: unsafe extern "C" fn(*mut c_char, c_int, c_int) -> c_int,
    cu_ctx_create: unsafe extern "C" fn(*mut Handle, c_uint, c_int) -> c_int,
    cu_ctx_destroy: unsafe extern "C" fn(Handle) -> c_int,
    cu_ctx_push: unsafe extern "C" fn(Handle) -> c_int,
    cu_ctx_pop: unsafe extern "C" fn(*mut Handle) -> c_int,
    _encode_lib: libloading::Library,
    _cuda_lib: libloading::Library,
}

fn nv() -> Result<&'static Nv> {
    static NV: OnceLock<Result<Nv, String>> = OnceLock::new();
    NV.get_or_init(|| load().map_err(|e| format!("{e:#}"))).as_ref().map_err(|e| anyhow!("{e}"))
}

fn load() -> Result<Nv> {
    // SAFETY: NVIDIA's own libraries; the symbols get the signatures of nvEncodeAPI.h
    // and cuda.h. The function list is filled by the driver for API 12.0.
    unsafe {
        let encode_lib =
            libloading::Library::new("libnvidia-encode.so.1").context("libnvidia-encode.so.1 not found")?;
        let cuda_lib = libloading::Library::new("libcuda.so.1").context("libcuda.so.1 not found")?;
        let max_version: libloading::Symbol<unsafe extern "C" fn(*mut u32) -> Status> =
            encode_lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0")?;
        let mut driver = 0u32;
        ensure!(max_version(&mut driver) == NV_ENC_SUCCESS, "NvEncodeAPIGetMaxSupportedVersion failed");
        ensure!(
            driver >= MIN_DRIVER_API,
            "the driver's NVENC is {}.{}, 12.0 is needed (driver 520 or later)",
            driver >> 4,
            driver & 0xf
        );
        let create: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> Status> =
            encode_lib.get(b"NvEncodeAPICreateInstance\0")?;
        let mut list = Params::new(l::FUNCTION_LIST_SIZE, l::FUNCTION_LIST_VER);
        ensure!(create(list.ptr()) == NV_ENC_SUCCESS, "NvEncodeAPICreateInstance failed");
        macro_rules! func {
            ($offset:expr, $ty:ty) => {{
                let f = list.get_ptr($offset);
                ensure!(!f.is_null(), "NVENC function at {} missing", $offset);
                std::mem::transmute::<*mut c_void, $ty>(f)
            }};
        }
        macro_rules! sym {
            ($name:literal) => {
                *cuda_lib.get(concat!($name, "\0").as_bytes()).context($name)?
            };
        }
        Ok(Nv {
            open_session: func!(l::FN_OPEN_SESSION, OpenSession),
            preset_config: func!(l::FN_PRESET_CONFIG, PresetConfig),
            initialize: func!(l::FN_INITIALIZE, WithParams),
            create_input: func!(l::FN_CREATE_INPUT, WithParams),
            destroy_input: func!(l::FN_DESTROY_INPUT, WithParams),
            create_bitstream: func!(l::FN_CREATE_BITSTREAM, WithParams),
            destroy_bitstream: func!(l::FN_DESTROY_BITSTREAM, WithParams),
            lock_input: func!(l::FN_LOCK_INPUT, WithParams),
            unlock_input: func!(l::FN_UNLOCK_INPUT, WithParams),
            encode: func!(l::FN_ENCODE, WithParams),
            lock_bitstream: func!(l::FN_LOCK_BITSTREAM, WithParams),
            unlock_bitstream: func!(l::FN_UNLOCK_BITSTREAM, WithParams),
            reconfigure: func!(l::FN_RECONFIGURE, WithParams),
            destroy: func!(l::FN_DESTROY, Destroy),
            last_error: func!(l::FN_LAST_ERROR, LastError),
            cu_init: sym!("cuInit"),
            cu_device_get: sym!("cuDeviceGet"),
            cu_device_name: sym!("cuDeviceGetName"),
            cu_ctx_create: sym!("cuCtxCreate_v2"),
            cu_ctx_destroy: sym!("cuCtxDestroy_v2"),
            cu_ctx_push: sym!("cuCtxPushCurrent_v2"),
            cu_ctx_pop: sym!("cuCtxPopCurrent_v2"),
            _encode_lib: encode_lib,
            _cuda_lib: cuda_lib,
        })
    }
}

/// CUDA context of one encoder, current on this thread while it lives.
struct Current<'a> {
    nv: &'a Nv,
}

impl<'a> Current<'a> {
    fn push(nv: &'a Nv, ctx: Handle) -> Result<Self> {
        // SAFETY: a context made by this encoder and not destroyed yet.
        let status = unsafe { (nv.cu_ctx_push)(ctx) };
        ensure!(status == CUDA_SUCCESS, "cuCtxPushCurrent: {status}");
        Ok(Self { nv })
    }
}

impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut popped = std::ptr::null_mut();
        // SAFETY: pops the context pushed by `push` on this same thread.
        unsafe { (self.nv.cu_ctx_pop)(&mut popped) };
    }
}

/// One encoding size: the encoder, its configuration and buffers.
struct Session {
    width: u32,
    height: u32,
    encoder: Handle,
    input: Handle,
    output: Handle,
    /// Kept for `nvEncReconfigureEncoder`; `init` points into `config`.
    init: Params,
    config: Params,
    frame: u32,
}

impl Session {
    fn new(nv: &Nv, ctx: Handle, settings: &EncoderSettings, width: u32, height: u32) -> Result<Self> {
        let mut s = Self {
            width,
            height,
            encoder: std::ptr::null_mut(),
            input: std::ptr::null_mut(),
            output: std::ptr::null_mut(),
            init: Params::new(l::INIT_SIZE, l::INIT_VER),
            config: Params::new(l::CONFIG_SIZE, l::CONFIG_VER),
            frame: 0,
        };
        if let Err(e) = s.open(nv, ctx, settings) {
            s.close(nv);
            return Err(e);
        }
        Ok(s)
    }

    fn check(&self, nv: &Nv, status: Status, what: &str) -> Result<()> {
        if status == NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = if self.encoder.is_null() {
            String::new()
        } else {
            // SAFETY: the encoder is open; the string is NVENC's own, NUL-terminated.
            unsafe {
                let text = (nv.last_error)(self.encoder);
                if text.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(text).to_string_lossy().into_owned()
                }
            }
        };
        bail!("{what}: NVENC error {status} {detail}")
    }

    fn open(&mut self, nv: &Nv, ctx: Handle, settings: &EncoderSettings) -> Result<()> {
        let preset = l::GUID_PRESET_P2;
        // SAFETY: NVENC calls with structures of the header's size and version; the
        // pointers given stay valid for each call.
        unsafe {
            let mut open = Params::new(l::OPEN_SESSION_SIZE, l::OPEN_SESSION_VER);
            open.put_u32(l::OPEN_SESSION_DEVICE_TYPE, l::DEVICE_TYPE_CUDA);
            open.put_ptr(l::OPEN_SESSION_DEVICE, ctx);
            open.put_u32(l::OPEN_SESSION_API_VERSION, l::API_VERSION);
            let status = (nv.open_session)(open.ptr(), &mut self.encoder);
            if status != NV_ENC_SUCCESS {
                self.encoder = std::ptr::null_mut();
                bail!("nvEncOpenEncodeSessionEx: NVENC error {status}");
            }

            let mut preset_config = Params::new(l::PRESET_CONFIG_SIZE, l::PRESET_CONFIG_VER);
            preset_config.put_u32(l::PRESET_CONFIG_CONFIG, l::CONFIG_VER);
            let status = (nv.preset_config)(
                self.encoder,
                Guid::from_bytes(l::GUID_H264),
                Guid::from_bytes(preset),
                l::TUNING_ULTRA_LOW_LATENCY,
                preset_config.ptr(),
            );
            self.check(nv, status, "nvEncGetEncodePresetConfigEx")?;
            let at = l::PRESET_CONFIG_CONFIG;
            let preset_bytes = preset_config.bytes()[at..at + l::CONFIG_SIZE].to_vec();
            self.config.bytes()[..l::CONFIG_SIZE].copy_from_slice(&preset_bytes);
            self.config.put_u32(0, l::CONFIG_VER);
            self.config.put_guid(l::CONFIG_PROFILE, l::GUID_PROFILE_MAIN);
            self.config.put_u32(l::CONFIG_GOP_LENGTH, l::INFINITE_GOP);
            self.config.put_u32(l::CONFIG_FRAME_INTERVAL_P, 1);
            self.config.put_u32(l::CONFIG_H264_IDR_PERIOD, l::INFINITE_GOP);
            self.config.put_u32(l::CONFIG_H264_ENTROPY, l::ENTROPY_CABAC);
            self.config.put_u32(l::CONFIG_H264_MAX_REF, 1);
            self.config.set_bit(l::CONFIG_H264_REPEAT_SPSPPS, true);
            self.config.put_u32(l::CONFIG_RC_MODE, l::RC_CBR);
            set_rate(&mut self.config, settings);

            let fps = settings.max_fps.max(1);
            self.init.put_guid(l::INIT_ENCODE_GUID, l::GUID_H264);
            self.init.put_guid(l::INIT_PRESET_GUID, preset);
            self.init.put_u32(l::INIT_WIDTH, self.width);
            self.init.put_u32(l::INIT_HEIGHT, self.height);
            self.init.put_u32(l::INIT_DAR_WIDTH, self.width);
            self.init.put_u32(l::INIT_DAR_HEIGHT, self.height);
            self.init.put_u32(l::INIT_MAX_WIDTH, self.width);
            self.init.put_u32(l::INIT_MAX_HEIGHT, self.height);
            self.init.put_u32(l::INIT_FRAME_RATE_NUM, fps);
            self.init.put_u32(l::INIT_FRAME_RATE_DEN, 1);
            self.init.put_u32(l::INIT_ASYNC, 0);
            self.init.put_u32(l::INIT_PTD, 1);
            self.init.put_u32(l::INIT_TUNING, l::TUNING_ULTRA_LOW_LATENCY);
            let config = self.config.ptr();
            self.init.put_ptr(l::INIT_CONFIG, config);
            let status = (nv.initialize)(self.encoder, self.init.ptr());
            self.check(nv, status, "nvEncInitializeEncoder")?;

            let mut input = Params::new(l::CREATE_INPUT_SIZE, l::CREATE_INPUT_VER);
            input.put_u32(l::CREATE_INPUT_WIDTH, self.width);
            input.put_u32(l::CREATE_INPUT_HEIGHT, self.height);
            input.put_u32(l::CREATE_INPUT_FORMAT, l::BUFFER_FORMAT_ARGB);
            let status = (nv.create_input)(self.encoder, input.ptr());
            self.check(nv, status, "nvEncCreateInputBuffer")?;
            self.input = input.get_ptr(l::CREATE_INPUT_BUFFER);

            let mut output = Params::new(l::CREATE_BITSTREAM_SIZE, l::CREATE_BITSTREAM_VER);
            let status = (nv.create_bitstream)(self.encoder, output.ptr());
            self.check(nv, status, "nvEncCreateBitstreamBuffer")?;
            self.output = output.get_ptr(l::CREATE_BITSTREAM_BUFFER);
        }
        Ok(())
    }

    fn close(&mut self, nv: &Nv) {
        // SAFETY: only what was created, once (handles reset to null).
        unsafe {
            if !self.encoder.is_null() {
                if !self.input.is_null() {
                    (nv.destroy_input)(self.encoder, self.input);
                }
                if !self.output.is_null() {
                    (nv.destroy_bitstream)(self.encoder, self.output);
                }
                (nv.destroy)(self.encoder);
            }
        }
        self.encoder = std::ptr::null_mut();
        self.input = std::ptr::null_mut();
        self.output = std::ptr::null_mut();
    }

    fn reconfigure(&mut self, nv: &Nv, settings: &EncoderSettings) -> Result<()> {
        set_rate(&mut self.config, settings);
        let mut params = Params::new(l::RECONFIGURE_SIZE, l::RECONFIGURE_VER);
        let init = self.init.bytes()[..l::INIT_SIZE].to_vec();
        params.bytes()[l::RECONFIGURE_INIT..l::RECONFIGURE_INIT + l::INIT_SIZE].copy_from_slice(&init);
        // SAFETY: the initialisation parameters point to `self.config`, alive.
        let status = unsafe { (nv.reconfigure)(self.encoder, params.ptr()) };
        self.check(nv, status, "nvEncReconfigureEncoder")
    }

    fn encode(&mut self, nv: &Nv, image: &BgraImage, keyframe: bool) -> Result<(Vec<u8>, bool)> {
        let (w, h) = (self.width as usize, self.height as usize);
        // SAFETY: NVENC calls on this session's buffers; the locked input is written
        // within its pitch and height, the locked bitstream read within its size.
        unsafe {
            let start = std::time::Instant::now();
            let mut lock = Params::new(l::LOCK_INPUT_SIZE, l::LOCK_INPUT_VER);
            lock.put_ptr(l::LOCK_INPUT_BUFFER, self.input);
            let status = (nv.lock_input)(self.encoder, lock.ptr());
            self.check(nv, status, "nvEncLockInputBuffer")?;
            let data = lock.get_ptr(l::LOCK_INPUT_DATA).cast::<u8>();
            let pitch = lock.get_u32(l::LOCK_INPUT_PITCH) as usize;
            for row in 0..h {
                let src = &image.data[row * image.stride..][..w * 4];
                std::ptr::copy_nonoverlapping(src.as_ptr(), data.add(row * pitch), w * 4);
            }
            (nv.unlock_input)(self.encoder, self.input);
            let copied = start.elapsed();

            let mut pic = Params::new(l::PIC_SIZE, l::PIC_VER);
            pic.put_u32(l::PIC_WIDTH, self.width);
            pic.put_u32(l::PIC_HEIGHT, self.height);
            pic.put_u32(l::PIC_PITCH, pitch as u32);
            if keyframe {
                pic.put_u32(l::PIC_FLAGS, l::PIC_FLAG_FORCEIDR | l::PIC_FLAG_OUTPUT_SPSPPS);
            }
            pic.put_u32(l::PIC_FRAME_IDX, self.frame);
            pic.put_u32(l::PIC_TIMESTAMP, self.frame);
            pic.put_ptr(l::PIC_INPUT, self.input);
            pic.put_ptr(l::PIC_OUTPUT, self.output);
            pic.put_u32(l::PIC_FORMAT, l::BUFFER_FORMAT_ARGB);
            pic.put_u32(l::PIC_STRUCT, l::PIC_STRUCT_FRAME);
            let status = (nv.encode)(self.encoder, pic.ptr());
            self.check(nv, status, "nvEncEncodePicture")?;
            self.frame = self.frame.wrapping_add(1);
            let submitted = start.elapsed();

            let mut out = Params::new(l::LOCK_BITSTREAM_SIZE, l::LOCK_BITSTREAM_VER);
            out.put_ptr(l::LOCK_BITSTREAM_OUTPUT, self.output);
            let status = (nv.lock_bitstream)(self.encoder, out.ptr());
            self.check(nv, status, "nvEncLockBitstream")?;
            let size = out.get_u32(l::LOCK_BITSTREAM_BYTES) as usize;
            let ptr = out.get_ptr(l::LOCK_BITSTREAM_PTR).cast::<u8>();
            let bytes =
                if ptr.is_null() { Vec::new() } else { std::slice::from_raw_parts(ptr, size).to_vec() };
            let idr = out.get_u32(l::LOCK_BITSTREAM_PICTURE_TYPE) == l::PIC_TYPE_IDR;
            (nv.unlock_bitstream)(self.encoder, self.output);
            let total = start.elapsed();
            if total.as_millis() >= 20 {
                debug!(?copied, ?submitted, ?total, "NVENC slow encode");
            }
            Ok((bytes, idr))
        }
    }
}

/// CBR at the settings' bitrate, with a quarter of a second of buffer: room for the
/// keyframes of a screen without letting the frames queue up.
fn set_rate(config: &mut Params, settings: &EncoderSettings) {
    let bits = settings.bitrate_bps;
    config.put_u32(l::CONFIG_RC_AVERAGE, bits);
    config.put_u32(l::CONFIG_RC_MAX, bits);
    config.put_u32(l::CONFIG_RC_VBV_SIZE, bits / 4);
    config.put_u32(l::CONFIG_RC_VBV_DELAY, bits / 4);
}

pub struct NvencEncoder {
    nv: &'static Nv,
    ctx: Handle,
    device: String,
    settings: EncoderSettings,
    session: Option<Session>,
    force_key: bool,
    new_rate: bool,
    /// The driver failed during the session: OpenH264 from then on.
    fallback: Option<OpenH264Encoder>,
}

// SAFETY: the CUDA context and the NVENC session are used by one thread at a time
// (the encoder's owner), with the context pushed around every call.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        let nv = nv()?;
        let mut ctx = std::ptr::null_mut();
        let mut name = [0 as c_char; 128];
        // SAFETY: CUDA driver calls with valid out-pointers; the context is destroyed on drop.
        unsafe {
            let status = (nv.cu_init)(0);
            ensure!(status == CUDA_SUCCESS, "cuInit: {status} (no NVIDIA GPU usable)");
            let mut device = 0;
            let status = (nv.cu_device_get)(&mut device, 0);
            ensure!(status == CUDA_SUCCESS, "cuDeviceGet: {status}");
            (nv.cu_device_name)(name.as_mut_ptr(), name.len() as c_int, device);
            let status = (nv.cu_ctx_create)(&mut ctx, 0, device);
            ensure!(status == CUDA_SUCCESS, "cuCtxCreate: {status}");
            // Created current: made current again only around NVENC's calls.
            let mut popped = std::ptr::null_mut();
            (nv.cu_ctx_pop)(&mut popped);
        }
        // SAFETY: NUL-terminated by cuDeviceGetName (zeroed buffer otherwise).
        let device = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned();
        let mut enc = Self {
            nv,
            ctx,
            device,
            settings,
            session: None,
            force_key: true,
            new_rate: false,
            fallback: None,
        };
        // A test frame: the keyframe must carry SPS, PPS and an IDR slice.
        let (w, h) = (320u32, 180u32);
        let data = vec![128u8; (w * h * 4) as usize];
        let first =
            enc.encode_nv(&BgraImage { width: w, height: h, stride: w as usize * 4, data: &data }, true)?;
        let types = crate::vaapi::nal_types(&first.data);
        if !(types.contains(&7) && types.contains(&8) && types.contains(&5)) {
            bail!("NVENC gives no SPS/PPS (NAL units {types:?})");
        }
        enc.session_drop();
        info!(device = %enc.device, "NVENC encoder");
        Ok(enc)
    }

    pub fn device_name(&self) -> &str {
        &self.device
    }

    fn session_drop(&mut self) {
        if let Some(mut s) = self.session.take() {
            match Current::push(self.nv, self.ctx) {
                Ok(_current) => s.close(self.nv),
                Err(e) => warn!("NVENC session not closed: {e:#}"),
            }
        }
    }

    fn encode_nv(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<EncodedFrame> {
        let (w, h) = crate::convert::even_size(image);
        let (w, h) = (w as u32, h as u32);
        let nv = self.nv;
        if self.session.as_ref().is_none_or(|s| (s.width, s.height) != (w, h)) {
            self.session_drop();
            let _current = Current::push(nv, self.ctx)?;
            self.session = Some(Session::new(nv, self.ctx, &self.settings, w, h)?);
            debug!(w, h, "NVENC session");
            self.force_key = true;
            self.new_rate = false;
        }
        let pending = std::mem::take(&mut self.force_key);
        let key = force_keyframe || pending;
        let new_rate = std::mem::take(&mut self.new_rate);
        let start = std::time::Instant::now();
        let _current = Current::push(nv, self.ctx)?;
        let session = self.session.as_mut().context("NVENC session")?;
        if new_rate {
            session.reconfigure(nv, &self.settings)?;
        }
        let reconfigured = start.elapsed();
        let (data, keyframe) = session.encode(nv, image, key)?;
        let total = start.elapsed();
        if total.as_millis() >= 20 {
            debug!(?total, ?reconfigured, keyframe, bytes = data.len(), "NVENC slow frame");
        }
        Ok(EncodedFrame { data, keyframe, ..Default::default() })
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        self.session_drop();
        // SAFETY: the context made in `new`, no longer current anywhere.
        unsafe { (self.nv.cu_ctx_destroy)(self.ctx) };
    }
}

impl VideoEncoder for NvencEncoder {
    fn name(&self) -> &'static str {
        if self.fallback.is_some() { "openh264 (NVENC failed)" } else { "nvenc" }
    }

    fn set_bitrate(&mut self, bps: u32) {
        if let Some(sw) = &mut self.fallback {
            sw.set_bitrate(bps);
            return;
        }
        if bps != self.settings.bitrate_bps {
            self.settings.bitrate_bps = bps;
            self.new_rate = true;
        }
    }

    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        if let Some(sw) = &mut self.fallback {
            return sw.encode(image, force_keyframe);
        }
        if image.width < 2 || image.height < 2 {
            return Ok(None);
        }
        match self.encode_nv(image, force_keyframe) {
            Ok(frame) if !frame.data.is_empty() => Ok(Some(frame)),
            Ok(_) => Ok(None),
            Err(e) => {
                warn!("NVENC encoding failed ({e:#}): OpenH264 from now on");
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

    #[test]
    fn guid_bytes() {
        let g = Guid::from_bytes(l::GUID_H264);
        // NV_ENC_CODEC_H264_GUID {6BC82762-4E63-4ca4-AA85-1E50F321F6BF}
        assert_eq!((g.data1, g.data2, g.data3), (0x6bc8_2762, 0x4e63, 0x4ca4));
        assert_eq!(g.data4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
    }

    #[test]
    fn params_fields() {
        let mut p = Params::new(l::CONFIG_SIZE, l::CONFIG_VER);
        assert_eq!(p.get_u32(0), l::CONFIG_VER);
        p.set_bit(l::CONFIG_H264_REPEAT_SPSPPS, true);
        assert_eq!(p.get_u32(l::CONFIG_H264_REPEAT_SPSPPS.0), 1 << l::CONFIG_H264_REPEAT_SPSPPS.1);
        p.set_bit(l::CONFIG_H264_REPEAT_SPSPPS, false);
        assert_eq!(p.get_u32(l::CONFIG_H264_REPEAT_SPSPPS.0), 0);
    }

    /// Needs an NVIDIA GPU with its driver; skipped elsewhere.
    #[test]
    fn encodes_and_decodes() {
        let mut enc = match NvencEncoder::new(EncoderSettings::default()) {
            Ok(enc) => enc,
            Err(e) => {
                eprintln!("no NVENC here: {e:#}");
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
            eprintln!("frame {i}: {} bytes, keyframe {}", frame.data.len(), frame.keyframe);
            assert_eq!(frame.keyframe, i == 0, "frame {i}");
            if let Some(yuv) = crate::VideoDecoder::decode(&mut dec, &frame.data).unwrap() {
                decoded = Some((yuv.width, yuv.height, yuv.y[yuv.width as usize * 90 + 100]));
            }
        }
        assert_eq!(enc.name(), "nvenc");
        let (dw, dh, luma) = decoded.expect("OpenH264 decodes the NVENC stream");
        assert_eq!((dw, dh), (322, 180));
        // Same pixel as the VA-API test, BT.601 limited: about 104.
        eprintln!("luma {luma}");
        assert!((90..=118).contains(&luma), "luma {luma}");
    }
}
