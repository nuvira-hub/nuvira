//! Linux: what goes to the default output, read from its monitor
//! (`@DEFAULT_MONITOR@`) through libpulse-simple. PipeWire answers the same calls
//! (pipewire-pulse, the default on current distributions), so does PulseAudio.
//!
//! The library is loaded at run time: no build dependency, no package dependency;
//! without it the agent just does not offer the sound.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;

use anyhow::{Result, bail};
use libloading::Library;

use crate::{CHANNELS, FRAME, RATE};

const LIBRARY: &str = "libpulse-simple.so.0";
/// pa_sample_format_t PA_SAMPLE_FLOAT32LE.
const FLOAT32LE: c_int = 5;
/// pa_stream_direction_t PA_STREAM_RECORD.
const RECORD: c_int = 2;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type NewFn = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const SampleSpec,
    map: *const c_void,
    attr: *const BufferAttr,
    error: *mut c_int,
) -> *mut c_void;
type ReadFn =
    unsafe extern "C" fn(s: *mut c_void, data: *mut c_void, bytes: usize, error: *mut c_int) -> c_int;
type FreeFn = unsafe extern "C" fn(s: *mut c_void);
type StrErrorFn = unsafe extern "C" fn(error: c_int) -> *const c_char;

fn library() -> Option<&'static Library> {
    static LIB: OnceLock<Option<Library>> = OnceLock::new();
    // SAFETY: a plain C library without initialisers that matter.
    LIB.get_or_init(|| unsafe { Library::new(LIBRARY) }.ok()).as_ref()
}

pub fn available() -> bool {
    library().is_some()
}

/// A recording stream on the monitor of the default output.
pub struct Monitor {
    s: *mut c_void,
    read: ReadFn,
    free: FreeFn,
    strerror: StrErrorFn,
}

// The stream is used by one thread at a time.
unsafe impl Send for Monitor {}

impl Monitor {
    pub fn open() -> Result<Monitor> {
        let Some(lib) = library() else { bail!("{LIBRARY} not found") };
        // SAFETY: the signatures are those of pulse/simple.h and pulse/error.h.
        let (new, read, free, strerror) = unsafe {
            (
                *lib.get::<NewFn>(b"pa_simple_new\0")?,
                *lib.get::<ReadFn>(b"pa_simple_read\0")?,
                *lib.get::<FreeFn>(b"pa_simple_free\0")?,
                *lib.get::<StrErrorFn>(b"pa_strerror\0")?,
            )
        };
        let spec = SampleSpec { format: FLOAT32LE, rate: RATE, channels: CHANNELS as u8 };
        // One packet per read: the server hands the data over every 20 ms.
        let bytes = (FRAME * CHANNELS * size_of::<f32>()) as u32;
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: bytes,
        };
        let name = CString::new("NuviraHub")?;
        let dev = CString::new("@DEFAULT_MONITOR@")?;
        let stream = CString::new("remote audio")?;
        let mut err = 0;
        // SAFETY: valid strings and structs for the call; the result is checked.
        let s = unsafe {
            new(
                std::ptr::null(),
                name.as_ptr(),
                RECORD,
                dev.as_ptr(),
                stream.as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut err,
            )
        };
        let monitor = Monitor { s, read, free, strerror };
        if s.is_null() {
            bail!("sound server: {}", monitor.error(err));
        }
        Ok(monitor)
    }

    /// Fills `pcm` (48 kHz stereo interleaved), waiting for the sound to be there.
    pub fn read(&mut self, pcm: &mut [f32]) -> Result<()> {
        let mut err = 0;
        // SAFETY: s is open, pcm holds the bytes asked for.
        let r = unsafe { (self.read)(self.s, pcm.as_mut_ptr().cast(), size_of_val(pcm), &mut err) };
        if r < 0 {
            bail!("sound server: {}", self.error(err));
        }
        Ok(())
    }

    fn error(&self, err: c_int) -> String {
        // SAFETY: pa_strerror returns a static string (or null).
        let p = unsafe { (self.strerror)(err) };
        match p.is_null() {
            true => format!("error {err}"),
            false => unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned(),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        if !self.s.is_null() {
            // SAFETY: opened by pa_simple_new, freed once.
            unsafe { (self.free)(self.s) }
        }
    }
}
