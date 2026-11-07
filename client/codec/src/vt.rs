//! H.264 with VideoToolbox (macOS): the media engine of Apple Silicon, or Quick
//! Sync on Intel Macs. Retina and 5K screens are heavy for OpenH264.
//!
//! - Low-latency rate control (`EnableLowLatencyRateControl`, macOS 11.3+) when the
//!   Mac has it: one frame out for every frame in, no reordering, Constrained
//!   Baseline; otherwise a real-time session in Main profile without B-frames,
//!   as Media Foundation on Windows.
//! - Input in BGRA as it comes from the capture: the encoder converts to YUV on
//!   the hardware, with the BT.601 matrix the viewer expects.
//! - Output in AVCC (lengths before the NAL units) turned into Annex B with
//!   SPS/PPS before every keyframe, as the other encoders give.
//! - Frames are given and waited for one by one (the callback hands the frame over
//!   a channel), so the latency stays as with the other encoders.

use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use tracing::{debug, info};

use crate::{BgraImage, EncodedFrame, EncoderSettings, VideoEncoder};

type OSStatus = i32;
type VTCompressionSessionRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;
type CVPixelBufferPoolRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CMTIME_VALID: u32 = 1;
const CMTIME_INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };
const CODEC_H264: u32 = u32::from_be_bytes(*b"avc1");
const PIXEL_BGRA: i32 = i32::from_be_bytes(*b"BGRA");
/// `kVTEncodeInfo_FrameDropped`.
const FRAME_DROPPED: u32 = 1 << 1;
/// `kCVAttachmentMode_ShouldPropagate`.
const ATTACH_PROPAGATE: u32 = 1;
/// How long to wait for the encoded frame before forcing it out.
const OUTPUT_WAIT: Duration = Duration::from_millis(40);
const OUTPUT_WAIT_FORCED: Duration = Duration::from_secs(1);

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CMSampleBufferRef);

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFStringRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;

    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec: u32,
        encoder_specification: CFDictionaryRef,
        source_image_attributes: CFDictionaryRef,
        compressed_data_allocator: *const c_void,
        callback: Option<OutputCallback>,
        refcon: *mut c_void,
        session: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: *mut c_void, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTSessionCopyProperty(
        session: *mut c_void,
        key: CFStringRef,
        allocator: *const c_void,
        value: *mut CFTypeRef,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionGetPixelBufferPool(session: VTCompressionSessionRef) -> CVPixelBufferPoolRef;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image: CVPixelBufferRef,
        pts: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_refcon: *mut c_void,
        info_flags: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: VTCompressionSessionRef, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    static kCVImageBufferYCbCrMatrixKey: CFStringRef;
    static kCVImageBufferColorPrimariesKey: CFStringRef;
    static kCVImageBufferTransferFunctionKey: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFStringRef;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;

    fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: *const c_void,
        pool: CVPixelBufferPoolRef,
        buffer: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: CVPixelBufferRef) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: CVPixelBufferRef) -> usize;
    fn CVBufferSetAttachment(buffer: CVPixelBufferRef, key: CFStringRef, value: CFTypeRef, mode: u32);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetDataBuffer(sample: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetFormatDescription(sample: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMBlockBufferGetDataLength(buffer: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        buffer: CMBlockBufferRef,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        description: CMFormatDescriptionRef,
        index: usize,
        pointer: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_length: *mut i32,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

/// What the output callback hands to `encode`: Annex B data, `None` if dropped.
type Output = Result<Option<Vec<u8>>, OSStatus>;

extern "C" fn on_output(
    refcon: *mut c_void,
    _source: *mut c_void,
    status: OSStatus,
    flags: u32,
    sample: CMSampleBufferRef,
) {
    // SAFETY: `refcon` is the sender boxed by `Session::new`, alive until the session is invalidated.
    let tx = unsafe { &*(refcon as *const Sender<Output>) };
    let output = if status != 0 {
        Err(status)
    } else if flags & FRAME_DROPPED != 0 || sample.is_null() {
        Ok(None)
    } else {
        // SAFETY: a valid sample buffer for the duration of the callback.
        Ok(unsafe { annex_b(sample) })
    };
    let _ = tx.send(output);
}

/// The sample as Annex B; with SPS/PPS first when it is a keyframe.
unsafe fn annex_b(sample: CMSampleBufferRef) -> Option<Vec<u8>> {
    let block = unsafe { CMSampleBufferGetDataBuffer(sample) };
    if block.is_null() {
        return None;
    }
    let len = unsafe { CMBlockBufferGetDataLength(block) };
    let mut avcc = vec![0u8; len];
    if unsafe { CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr().cast()) } != 0 {
        return None;
    }
    let description = unsafe { CMSampleBufferGetFormatDescription(sample) };
    let mut header_len = 4i32;
    let mut parameter_sets = Vec::new();
    if !description.is_null() {
        let mut count = 0usize;
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                description,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut count,
                &mut header_len,
            )
        };
        if status == 0 {
            for index in 0..count {
                let (mut p, mut size) = (ptr::null(), 0usize);
                let status = unsafe {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        description,
                        index,
                        &mut p,
                        &mut size,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                };
                if status == 0 && !p.is_null() {
                    parameter_sets.push(unsafe { std::slice::from_raw_parts(p, size) }.to_vec());
                }
            }
        }
    }
    Some(avcc_to_annex_b(&avcc, header_len.clamp(1, 4) as usize, &parameter_sets))
}

/// AVCC (NAL units each after its length, `header_len` bytes big-endian) to Annex B.
/// The parameter sets go before the frame if it has an IDR slice.
fn avcc_to_annex_b(avcc: &[u8], header_len: usize, parameter_sets: &[Vec<u8>]) -> Vec<u8> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let mut nals = Vec::new();
    let mut rest = avcc;
    while rest.len() > header_len {
        let size = rest[..header_len].iter().fold(0usize, |n, &b| (n << 8) | b as usize);
        rest = &rest[header_len..];
        let Some(nal) = rest.get(..size) else { break };
        nals.push(nal);
        rest = &rest[size..];
    }
    let keyframe = nals.iter().any(|n| n.first().is_some_and(|b| b & 0x1f == 5));
    let mut out = Vec::with_capacity(avcc.len() + 64);
    if keyframe {
        for set in parameter_sets {
            out.extend_from_slice(&START);
            out.extend_from_slice(set);
        }
    }
    for nal in nals {
        out.extend_from_slice(&START);
        out.extend_from_slice(nal);
    }
    out
}

fn number(n: i64) -> CFType {
    CFNumber::from(n).as_CFType()
}

/// `CFStringRef` constant of a framework as a `CFString`.
unsafe fn constant(s: CFStringRef) -> CFString {
    unsafe { CFString::wrap_under_get_rule(s) }
}

/// A compression session for one image size.
struct Session {
    session: VTCompressionSessionRef,
    outputs: Receiver<Output>,
    /// Sender given to the callback (freed after the session is invalidated).
    refcon: *mut Sender<Output>,
    width: u32,
    height: u32,
    low_latency: bool,
    /// `None`: not said by the session (the low-latency one does not).
    hardware: Option<bool>,
}

impl Session {
    fn new(width: u32, height: u32, settings: EncoderSettings, require_hardware: bool) -> Result<Self> {
        // Tests: NUVIRA_VT_LOW_LATENCY=0 leaves the low-latency rate control out.
        if std::env::var("NUVIRA_VT_LOW_LATENCY").is_ok_and(|v| v == "0") {
            return Self::create(width, height, settings, require_hardware, false);
        }
        match Self::create(width, height, settings, require_hardware, true) {
            Ok(s) => Ok(s),
            Err(e) => {
                debug!("VideoToolbox without low-latency rate control ({e:#})");
                Self::create(width, height, settings, require_hardware, false)
            }
        }
    }

    fn create(
        width: u32,
        height: u32,
        settings: EncoderSettings,
        require_hardware: bool,
        low_latency: bool,
    ) -> Result<Self> {
        let mut spec: Vec<(CFString, CFType)> = Vec::new();
        unsafe {
            if low_latency {
                spec.push((
                    constant(kVTVideoEncoderSpecification_EnableLowLatencyRateControl),
                    CFBoolean::true_value().as_CFType(),
                ));
            }
            if require_hardware {
                spec.push((
                    constant(kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder),
                    CFBoolean::true_value().as_CFType(),
                ));
            }
        }
        let spec = CFDictionary::from_CFType_pairs(&spec);
        let surface: CFDictionary<CFString, CFType> = CFDictionary::from_CFType_pairs(&[]);
        let attributes = unsafe {
            CFDictionary::from_CFType_pairs(&[
                (constant(kCVPixelBufferPixelFormatTypeKey), number(i64::from(PIXEL_BGRA))),
                (constant(kCVPixelBufferWidthKey), number(i64::from(width))),
                (constant(kCVPixelBufferHeightKey), number(i64::from(height))),
                (constant(kCVPixelBufferIOSurfacePropertiesKey), surface.as_CFType()),
            ])
        };
        let (tx, outputs) = channel::<Output>();
        let refcon = Box::into_raw(Box::new(tx));
        let mut session: VTCompressionSessionRef = ptr::null_mut();
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                width as i32,
                height as i32,
                CODEC_H264,
                spec.as_concrete_TypeRef(),
                attributes.as_concrete_TypeRef(),
                ptr::null(),
                Some(on_output),
                refcon.cast(),
                &mut session,
            )
        };
        if status != 0 || session.is_null() {
            drop(unsafe { Box::from_raw(refcon) });
            bail!("VTCompressionSessionCreate: {status}");
        }
        let mut this = Self { session, outputs, refcon, width, height, low_latency, hardware: None };
        this.configure(settings)?;
        Ok(this)
    }

    fn set(&self, key: CFStringRef, value: &CFType, what: &str) -> Result<()> {
        let status = unsafe { VTSessionSetProperty(self.session, key, value.as_CFTypeRef()) };
        if status != 0 {
            bail!("{what}: {status}");
        }
        Ok(())
    }

    /// Like [`set`](Self::set), for the properties an encoder may not have.
    fn try_set(&self, key: CFStringRef, value: &CFType, what: &str) {
        if let Err(e) = self.set(key, value, what) {
            debug!("VideoToolbox property not set: {e}");
        }
    }

    fn configure(&mut self, settings: EncoderSettings) -> Result<()> {
        let yes = CFBoolean::true_value().as_CFType();
        let no = CFBoolean::false_value().as_CFType();
        let fps = i64::from(settings.max_fps.max(1));
        unsafe {
            self.set(kVTCompressionPropertyKey_RealTime, &yes, "real time")?;
            let profile = if self.low_latency {
                kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel
            } else {
                kVTProfileLevel_H264_Main_AutoLevel
            };
            self.set(kVTCompressionPropertyKey_ProfileLevel, &constant(profile).as_CFType(), "profile")?;
            self.try_set(kVTCompressionPropertyKey_AllowFrameReordering, &no, "no B-frames");
            // Periodic keyframe every 10 s as a safety net; the others on request.
            self.try_set(kVTCompressionPropertyKey_MaxKeyFrameInterval, &number(fps * 10), "GOP");
            self.try_set(kVTCompressionPropertyKey_ExpectedFrameRate, &number(fps), "frame rate");
            self.try_set(
                kVTCompressionPropertyKey_YCbCrMatrix,
                &constant(kCVImageBufferYCbCrMatrix_ITU_R_601_4).as_CFType(),
                "BT.601 matrix",
            );
            self.try_set(
                kVTCompressionPropertyKey_ColorPrimaries,
                &constant(kCVImageBufferColorPrimaries_ITU_R_709_2).as_CFType(),
                "primaries",
            );
            self.try_set(
                kVTCompressionPropertyKey_TransferFunction,
                &constant(kCVImageBufferTransferFunction_ITU_R_709_2).as_CFType(),
                "transfer function",
            );
        }
        self.set_bitrate(settings.bitrate_bps);
        let status = unsafe { VTCompressionSessionPrepareToEncodeFrames(self.session) };
        if status != 0 {
            bail!("VTCompressionSessionPrepareToEncodeFrames: {status}");
        }
        self.hardware = self.using_hardware();
        Ok(())
    }

    fn set_bitrate(&self, bps: u32) {
        unsafe {
            self.try_set(kVTCompressionPropertyKey_AverageBitRate, &number(i64::from(bps)), "bitrate");
            // At most 1.5 times the average in any second: no bursts that fill the link.
            let limits = CFArray::from_CFTypes(&[number(i64::from(bps) * 3 / 16), number(1)]);
            self.try_set(kVTCompressionPropertyKey_DataRateLimits, &limits.as_CFType(), "rate limit");
        }
    }

    fn using_hardware(&self) -> Option<bool> {
        let mut value: CFTypeRef = ptr::null();
        let status = unsafe {
            VTSessionCopyProperty(
                self.session,
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                ptr::null(),
                &mut value,
            )
        };
        if status != 0 || value.is_null() {
            return None;
        }
        let value = unsafe { CFType::wrap_under_create_rule(value) };
        value.downcast::<CFBoolean>().map(bool::from)
    }

    /// Copies the image into a buffer of the session's pool.
    fn pixel_buffer(&self, image: &BgraImage) -> Result<CVPixelBufferRef> {
        let pool = unsafe { VTCompressionSessionGetPixelBufferPool(self.session) };
        if pool.is_null() {
            bail!("VideoToolbox without a buffer pool");
        }
        let mut buffer: CVPixelBufferRef = ptr::null_mut();
        let status = unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), pool, &mut buffer) };
        if status != 0 || buffer.is_null() {
            bail!("CVPixelBufferPoolCreatePixelBuffer: {status}");
        }
        unsafe {
            if CVPixelBufferLockBaseAddress(buffer, 0) != 0 {
                CFRelease(buffer);
                bail!("CVPixelBufferLockBaseAddress");
            }
            let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
            let pitch = CVPixelBufferGetBytesPerRow(buffer);
            let row = (self.width as usize * 4).min(image.stride).min(pitch);
            for y in 0..self.height as usize {
                let src = &image.data[y * image.stride..][..row];
                ptr::copy_nonoverlapping(src.as_ptr(), base.add(y * pitch), row);
            }
            CVPixelBufferUnlockBaseAddress(buffer, 0);
            // The colours of the source, so the conversion to YUV uses BT.601.
            CVBufferSetAttachment(
                buffer,
                kCVImageBufferYCbCrMatrixKey,
                kCVImageBufferYCbCrMatrix_ITU_R_601_4.cast(),
                ATTACH_PROPAGATE,
            );
            CVBufferSetAttachment(
                buffer,
                kCVImageBufferColorPrimariesKey,
                kCVImageBufferColorPrimaries_ITU_R_709_2.cast(),
                ATTACH_PROPAGATE,
            );
            CVBufferSetAttachment(
                buffer,
                kCVImageBufferTransferFunctionKey,
                kCVImageBufferTransferFunction_ITU_R_709_2.cast(),
                ATTACH_PROPAGATE,
            );
        }
        Ok(buffer)
    }

    fn encode(&mut self, image: &BgraImage, pts_us: i64, force_keyframe: bool) -> Result<Option<Vec<u8>>> {
        // Leftovers of a frame that came out late.
        while self.outputs.try_recv().is_ok() {}
        let buffer = self.pixel_buffer(image)?;
        let properties = force_keyframe.then(|| unsafe {
            CFDictionary::from_CFType_pairs(&[(
                constant(kVTEncodeFrameOptionKey_ForceKeyFrame),
                CFBoolean::true_value().as_CFType(),
            )])
        });
        let pts = CMTime { value: pts_us, timescale: 1_000_000, flags: CMTIME_VALID, epoch: 0 };
        let mut flags = 0u32;
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                buffer,
                pts,
                CMTIME_INVALID,
                properties.as_ref().map_or(ptr::null(), |p| p.as_concrete_TypeRef()),
                ptr::null_mut(),
                &mut flags,
            )
        };
        unsafe { CFRelease(buffer) };
        if status != 0 {
            bail!("VTCompressionSessionEncodeFrame: {status}");
        }
        let output = match self.outputs.recv_timeout(OUTPUT_WAIT) {
            Ok(output) => output,
            Err(RecvTimeoutError::Timeout) => {
                // Held back by the encoder: it gives it out now.
                unsafe { VTCompressionSessionCompleteFrames(self.session, CMTIME_INVALID) };
                match self.outputs.recv_timeout(OUTPUT_WAIT_FORCED) {
                    Ok(output) => output,
                    Err(_) => return Ok(None),
                }
            }
            Err(RecvTimeoutError::Disconnected) => bail!("VideoToolbox output closed"),
        };
        output.map_err(|status| anyhow!("VideoToolbox encoding: {status}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, CMTIME_INVALID);
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session);
            // No callback can come any more.
            drop(Box::from_raw(self.refcon));
        }
    }
}

pub struct VtEncoder {
    settings: EncoderSettings,
    require_hardware: bool,
    session: Option<Session>,
    start: Instant,
    /// The next frame must be a keyframe (new session).
    force_key: bool,
}

// SAFETY: the session is used by one thread at a time (through `&mut self`);
// VideoToolbox sessions are not tied to the thread that made them.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    /// `require_hardware`: fail instead of falling back to Apple's software encoder.
    pub fn new(settings: EncoderSettings, require_hardware: bool) -> Result<Self> {
        // A small session right away, to fail now if VideoToolbox cannot encode H.264.
        let probe = Session::new(640, 360, settings, require_hardware).context("VideoToolbox H.264")?;
        info!(hardware = ?probe.hardware, low_latency = probe.low_latency, "VideoToolbox encoder");
        Ok(Self { settings, require_hardware, session: None, start: Instant::now(), force_key: true })
    }
}

impl VideoEncoder for VtEncoder {
    fn name(&self) -> &'static str {
        "videotoolbox"
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.settings.bitrate_bps = bps;
        if let Some(session) = &self.session {
            session.set_bitrate(bps);
        }
    }

    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        let (w, h) = crate::convert::even_size(image);
        let (w, h) = (w as u32, h as u32);
        if w == 0 || h == 0 {
            return Ok(None);
        }
        if self.session.as_ref().is_none_or(|s| (s.width, s.height) != (w, h)) {
            self.session = None;
            let session = Session::new(w, h, self.settings, self.require_hardware)?;
            debug!(w, h, hardware = ?session.hardware, "VideoToolbox session");
            self.session = Some(session);
            self.force_key = true;
        }
        let key = force_keyframe || std::mem::take(&mut self.force_key);
        let pts = self.start.elapsed().as_micros() as i64;
        let session = self.session.as_mut().context("VideoToolbox session")?;
        let Some(data) = session.encode(image, pts, key)? else { return Ok(None) };
        let keyframe = has_idr(&data);
        Ok(Some(EncodedFrame { data, keyframe, ..Default::default() }))
    }
}

/// Annex B data with an IDR slice.
fn has_idr(data: &[u8]) -> bool {
    data.windows(4).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && w[3] & 0x1f == 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_becomes_annex_b() {
        let sps = vec![0x67, 1, 2];
        let pps = vec![0x68, 3];
        let avcc = [0, 0, 0, 2, 0x65, 9, 0, 0, 0, 1, 0x06];
        let out = avcc_to_annex_b(&avcc, 4, &[sps.clone(), pps.clone()]);
        assert_eq!(out, [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 0, 0, 0, 1, 0x06]);
        assert!(has_idr(&out));
        // Not a keyframe: no parameter sets.
        let out = avcc_to_annex_b(&[0, 0, 0, 1, 0x41], 4, &[sps, pps]);
        assert_eq!(out, [0, 0, 0, 1, 0x41]);
        assert!(!has_idr(&out));
    }

    #[test]
    fn encodes_and_decodes() {
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
        let mut enc = VtEncoder::new(EncoderSettings::default(), false).expect("VideoToolbox");
        let mut dec = crate::OpenH264Decoder::new().unwrap();
        let mut decoded = None;
        for i in 0..5 {
            if let Some(frame) = enc.encode(&image, i == 0).unwrap() {
                if i == 0 {
                    assert!(frame.keyframe, "first frame is a keyframe");
                }
                if let Some(yuv) = crate::VideoDecoder::decode(&mut dec, &frame.data).unwrap() {
                    decoded = Some((yuv.width, yuv.height, yuv.y[yuv.width as usize * 90 + 100]));
                }
            }
        }
        let (dw, dh, luma) = decoded.expect("OpenH264 decodes the VideoToolbox stream");
        assert_eq!((dw, dh), (322, 180));
        // x=100, y=90: B=100, G=90, R=128 → Y about 16 + 0.257*128 + 0.504*90 + 0.098*100 ≈ 104.
        assert!((90..=118).contains(&luma), "luma {luma}");
    }
}
