//! H.264 decoding with VideoToolbox (macOS): the media engine of the Mac decodes,
//! and the picture (NV12 in an IOSurface) is read into the I420 [`YuvFrame`] the
//! renderers take.
//!
//! Not the default, as on Windows (`mfdec`): measured on a Mac mini M1 (example
//! `decbench`, 240 frames at 1080p) it took 4.6 ms a frame and about 1.8 ms of
//! CPU, against 1.0 ms and 1.0 ms for OpenH264; the pictures were identical. It
//! `NUVIRA_DECODER=hardware` selects it. With the Metal renderer the IOSurface is
//! drawn as it is ([`VtDecoder::native`], no reading): a session at 1080p and
//! 60 fps from PC-LABORATORIO's synthetic source on the M1 took the same CPU
//! (7.4 s in 20 s), latency (45 ms) and fps (53-58) as OpenH264, so neither is
//! the default.
//!
//! The stream is Annex B (start codes) with the parameter sets before each
//! keyframe: a new format description (and session) is made when they change,
//! and the frames are passed in AVCC form (4-byte lengths), one sample each,
//! decoded synchronously.

use std::ffi::c_void;
use std::ptr;

use anyhow::{Result, bail};
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;

use crate::{VideoDecoder, YuvFrame};

type OSStatus = i32;
type CFTypeRef = *const c_void;
type CMFormatDescriptionRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CVImageBufferRef = *mut c_void;
type VTDecompressionSessionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
struct OutputCallback {
    callback: extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CVImageBufferRef, CMTime, CMTime),
    refcon: *mut c_void,
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: *const c_void,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_length_size: i32,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: *const c_void,
        memory: *mut c_void,
        length: usize,
        block_allocator: *const c_void,
        custom: *const c_void,
        offset: usize,
        data_length: usize,
        flags: u32,
        out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: *const c_void,
        data: CMBlockBufferRef,
        format: CMFormatDescriptionRef,
        samples: isize,
        timings: isize,
        timing: *const c_void,
        sizes: isize,
        size: *const usize,
        out: *mut CMSampleBufferRef,
    ) -> OSStatus;
    static kCFAllocatorNull: *const c_void;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: *const c_void,
        format: CMFormatDescriptionRef,
        decoder_spec: *const c_void,
        image_attributes: *const c_void,
        callback: *const OutputCallback,
        out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample: CMSampleBufferRef,
        flags: u32,
        frame_refcon: *mut c_void,
        info_flags: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: VTDecompressionSessionRef) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferLockBaseAddress(buffer: CVImageBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVImageBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetWidth(buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetHeight(buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: CVImageBufferRef) -> u32;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVImageBufferRef, plane: usize) -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVImageBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetIOSurface(buffer: CVImageBufferRef) -> *mut c_void;
    static kCVPixelBufferIOSurfacePropertiesKey: *const c_void;
    static kCVPixelBufferMetalCompatibilityKey: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: CFTypeRef);
    fn CFRetain(object: CFTypeRef) -> CFTypeRef;
}

/// A decoded picture kept on the GPU: VideoToolbox's buffer (an IOSurface,
/// NV12), retained while a renderer uses it.
pub struct DecodedSurface(CVImageBufferRef);

// SAFETY: CoreVideo buffers are reference counted and may be read from any
// thread; nobody writes this one after the decoder.
unsafe impl Send for DecodedSurface {}
unsafe impl Sync for DecodedSurface {}

impl DecodedSurface {
    fn retain(image: CVImageBufferRef) -> Self {
        // SAFETY: a valid buffer, released in `drop`.
        unsafe { CFRetain(image) };
        Self(image)
    }

    /// Its IOSurface (owned by the buffer, valid while it lives).
    pub fn io_surface(&self) -> *mut c_void {
        // SAFETY: a valid buffer.
        unsafe { CVPixelBufferGetIOSurface(self.0) }
    }

    /// Its pixels into I420 planes.
    pub fn read(&self, frame: &mut YuvFrame) {
        read_nv12(self.0, frame);
    }
}

impl Drop for DecodedSurface {
    fn drop(&mut self) {
        // SAFETY: retained in `retain`.
        unsafe { CFRelease(self.0) };
    }
}

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` and `...FullRange`: NV12.
const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
/// Read-only lock.
const READ_ONLY: u64 = 1;

pub struct VtDecoder {
    session: VTDecompressionSessionRef,
    format: CMFormatDescriptionRef,
    /// The parameter sets of the session (SPS, PPS).
    sets: Vec<Vec<u8>>,
    /// Where the callback writes the decoded picture (boxed: its address is the refcon).
    out: Box<Output>,
}

#[derive(Default)]
struct Output {
    frame: YuvFrame,
    ready: bool,
    status: OSStatus,
    /// Keep the picture in its IOSurface instead of reading it.
    native: bool,
}

// SAFETY: the session is used by one thread at a time (`&mut self`); decoding is
// synchronous, so the callback runs inside `decode`.
unsafe impl Send for VtDecoder {}

extern "C" fn on_frame(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: OSStatus,
    _flags: u32,
    image: CVImageBufferRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: `refcon` is the decoder's `Output`, alive while it decodes.
    let out = unsafe { &mut *refcon.cast::<Output>() };
    out.status = status;
    if status != 0 || image.is_null() {
        return;
    }
    // SAFETY: a decoded picture handed to the callback.
    let format = unsafe { CVPixelBufferGetPixelFormatType(image) };
    if format != NV12_VIDEO && format != NV12_FULL {
        out.status = -1;
        return;
    }
    if out.native {
        let f = &mut out.frame;
        // SAFETY: a valid buffer.
        unsafe {
            f.width = CVPixelBufferGetWidth(image) as u32;
            f.height = CVPixelBufferGetHeight(image) as u32;
        }
        f.y.clear();
        f.u.clear();
        f.v.clear();
        f.surface = Some(std::sync::Arc::new(DecodedSurface::retain(image)));
        out.ready = true;
        return;
    }
    if !read_nv12(image, &mut out.frame) {
        out.status = -2;
        return;
    }
    out.ready = true;
}

/// NV12 `image` into I420 planes; false if it could not be locked.
fn read_nv12(image: CVImageBufferRef, f: &mut YuvFrame) -> bool {
    // SAFETY: a valid NV12 buffer, locked while read.
    unsafe {
        if CVPixelBufferLockBaseAddress(image, READ_ONLY) != 0 {
            return false;
        }
        let (w, h) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y_base = CVPixelBufferGetBaseAddressOfPlane(image, 0);
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
        let uv_base = CVPixelBufferGetBaseAddressOfPlane(image, 1);
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 1);
        f.width = w as u32;
        f.height = h as u32;
        f.y.clear();
        f.u.clear();
        f.v.clear();
        f.y.reserve(w * h);
        f.u.reserve(cw * ch);
        f.v.reserve(cw * ch);
        for row in 0..h {
            f.y.extend_from_slice(std::slice::from_raw_parts(y_base.add(row * y_stride), w));
        }
        for row in 0..ch {
            let uv = std::slice::from_raw_parts(uv_base.add(row * uv_stride), cw * 2);
            for pair in uv.chunks_exact(2) {
                f.u.push(pair[0]);
                f.v.push(pair[1]);
            }
        }
        CVPixelBufferUnlockBaseAddress(image, READ_ONLY);
    }
    true
}

/// The NAL units of an Annex B stream (without their start codes).
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        // A 4-byte start code leaves a zero before the next one.
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            units.push(&data[s..end]);
        }
    }
    units
}

impl VtDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self { session: ptr::null_mut(), format: ptr::null_mut(), sets: Vec::new(), out: Box::default() })
    }

    /// Pictures kept in their IOSurfaces ([`YuvFrame::surface`]), for a Metal renderer.
    pub fn native() -> Result<Self> {
        let mut decoder = Self::new()?;
        decoder.out.native = true;
        Ok(decoder)
    }

    fn close(&mut self) {
        // SAFETY: objects made by this decoder, released once.
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
                self.session = ptr::null_mut();
            }
            if !self.format.is_null() {
                CFRelease(self.format);
                self.format = ptr::null_mut();
            }
        }
    }

    /// A session for these parameter sets (SPS first).
    fn open(&mut self, sets: Vec<Vec<u8>>) -> Result<()> {
        self.close();
        let pointers: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
        let sizes: Vec<usize> = sets.iter().map(Vec::len).collect();
        // SAFETY: arrays of `sets.len()` valid parameter sets.
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(),
                sets.len(),
                pointers.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut self.format,
            )
        };
        if status != 0 {
            bail!("CMVideoFormatDescriptionCreateFromH264ParameterSets: {status}");
        }
        let callback = OutputCallback { callback: on_frame, refcon: (&mut *self.out as *mut Output).cast() };
        // Native pictures: in IOSurfaces Metal can read.
        let attributes = self.out.native.then(|| {
            // SAFETY: CoreVideo's key constants.
            let (surface_key, metal_key) = unsafe {
                (
                    CFString::wrap_under_get_rule(kCVPixelBufferIOSurfacePropertiesKey.cast()),
                    CFString::wrap_under_get_rule(kCVPixelBufferMetalCompatibilityKey.cast()),
                )
            };
            let empty: CFDictionary<CFString, CFBoolean> = CFDictionary::from_CFType_pairs(&[]);
            CFDictionary::from_CFType_pairs(&[
                (surface_key, empty.as_CFType()),
                (metal_key, CFBoolean::true_value().as_CFType()),
            ])
        });
        let attributes_ref = attributes.as_ref().map_or(ptr::null(), |a| a.as_concrete_TypeRef().cast());
        // SAFETY: a valid format; no decoder requirements (NV12 in the decoder's
        // own format); the callback's refcon outlives the session.
        let status = unsafe {
            VTDecompressionSessionCreate(ptr::null(), self.format, ptr::null(), attributes_ref, &callback, &mut self.session)
        };
        if status != 0 || self.session.is_null() {
            self.session = ptr::null_mut();
            bail!("VTDecompressionSessionCreate: {status}");
        }
        self.sets = sets;
        Ok(())
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        self.close();
    }
}

impl VideoDecoder for VtDecoder {
    fn name(&self) -> &'static str {
        "videotoolbox"
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<&YuvFrame>> {
        let units = nal_units(data);
        let sets: Vec<Vec<u8>> =
            units.iter().filter(|n| matches!(n[0] & 0x1F, 7 | 8)).map(|n| n.to_vec()).collect();
        if sets.len() >= 2 && sets != self.sets {
            self.open(sets)?;
        }
        if self.session.is_null() {
            // No parameter sets yet: wait for a keyframe.
            return Ok(None);
        }
        // The picture's NAL units in AVCC form.
        let mut avcc = Vec::with_capacity(data.len() + 16);
        for n in units.iter().filter(|n| !matches!(n[0] & 0x1F, 7 | 8 | 9)) {
            avcc.extend_from_slice(&(n.len() as u32).to_be_bytes());
            avcc.extend_from_slice(n);
        }
        if avcc.is_empty() {
            return Ok(None);
        }
        self.out.ready = false;
        self.out.status = 0;
        let size = avcc.len();
        // SAFETY: the block points into `avcc` (no copy, not freed by CoreMedia:
        // kCFAllocatorNull), which outlives the synchronous decode; released after.
        unsafe {
            let mut block: CMBlockBufferRef = ptr::null_mut();
            let status = CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                avcc.as_mut_ptr().cast(),
                size,
                kCFAllocatorNull,
                ptr::null(),
                0,
                size,
                0,
                &mut block,
            );
            if status != 0 {
                bail!("CMBlockBufferCreateWithMemoryBlock: {status}");
            }
            let mut sample: CMSampleBufferRef = ptr::null_mut();
            let status =
                CMSampleBufferCreateReady(ptr::null(), block, self.format, 1, 0, ptr::null(), 1, &size, &mut sample);
            if status != 0 {
                CFRelease(block);
                bail!("CMSampleBufferCreateReady: {status}");
            }
            let mut info = 0u32;
            // Flags 0: synchronous, the callback runs before this returns.
            let status = VTDecompressionSessionDecodeFrame(self.session, sample, 0, ptr::null_mut(), &mut info);
            CFRelease(sample);
            CFRelease(block);
            if status != 0 {
                // A broken session (another stream, a lost reference): made again
                // at the next keyframe.
                self.sets.clear();
                self.close();
                bail!("VTDecompressionSessionDecodeFrame: {status}");
            }
        }
        if self.out.status != 0 {
            bail!("VideoToolbox decoding: {}", self.out.status);
        }
        Ok(self.out.ready.then_some(&self.out.frame))
    }
}
