//! H.264 with the graphics card's hardware encoder through Media Foundation
//! (Intel Quick Sync, NVIDIA NVENC, AMD AMF: each registers its own MFT).
//!
//! Hardware MFTs are asynchronous: they signal with events when they accept an
//! input (`METransformNeedInput`) and when they have an output (`METransformHaveOutput`).
//! A dedicated thread reads the events and passes them to `encode` through a channel,
//! so waiting for the output has a time limit and cannot hang.
//!
//! The image comes either from RAM (converted to NV12 on the CPU) or as a D3D11
//! texture already on the GPU (see [`crate::gpu`]), without copies.
//!
//! While a session is open the system timer runs at 1 ms ([`FineTimer`]): Quick Sync
//! waits for timer ticks, and with the default 15.6 ms tick it took a frame only
//! every ~31 ms (32 fps at most, whatever the resolution; 93 fps with 1 ms, and
//! 8.9 instead of 15.6 ms per frame).

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{Interface, PWSTR};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling,
    SetProcessInformation,
};

use crate::gpu::{GpuDevice, GpuTexture, Nv12Converter};
use crate::{BgraImage, EncodedFrame, EncoderSettings, VideoEncoder, convert};

/// Maximum wait for a frame's output before returning `None`
/// (the frame will come out at the next call).
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(100);
/// Maximum wait for the encoder to accept a new frame.
const INPUT_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) fn startup() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow!("MFStartup: {e}"))
}

/// System timer at 1 ms while it lives (one per encoder session, so not outside sessions:
/// a finer timer costs some power).
struct FineTimer;

impl FineTimer {
    fn new() -> Self {
        static OPT_OUT: OnceLock<()> = OnceLock::new();
        OPT_OUT.get_or_init(|| {
            // Windows 11 may ignore the request for processes without visible windows
            // (the agent as a service): the process asks to be exempt.
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
                StateMask: 0,
            };
            if let Err(e) = unsafe {
                SetProcessInformation(
                    GetCurrentProcess(),
                    ProcessPowerThrottling,
                    (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                    size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
                )
            } {
                debug!("timer resolution exemption not available: {e}");
            }
        });
        let result = unsafe { timeBeginPeriod(1) };
        if result != 0 {
            warn!("1 ms timer not available ({result}): the hardware encoder will be slower");
        }
        Self
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        unsafe { timeEndPeriod(1) };
    }
}

/// Media Foundation objects can be used from any thread (MTA).
struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

enum TransformEvent {
    NeedInput,
    HaveOutput,
}

/// Names of the installed H.264 hardware encoders, in order of preference.
pub fn hardware_encoders() -> Result<Vec<String>> {
    Ok(enumerate()?.into_iter().map(|(name, _)| name).collect())
}

fn enumerate() -> Result<Vec<(String, IMFActivate)>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    startup()?;
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )?;
    }
    if list.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: MFTEnumEx returns an array of `count` pointers allocated with CoTaskMemAlloc.
    let items = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
    let result = items
        .iter_mut()
        .filter_map(|a| a.take())
        .map(|activate| (friendly_name(&activate), activate))
        .collect();
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    Ok(result)
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = PWSTR::null();
    let mut len = 0;
    unsafe {
        if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_err() {
            return "hardware encoder".into();
        }
        let s = name.to_string().unwrap_or_default();
        CoTaskMemFree(Some(name.0 as *const _));
        s
    }
}

fn set_codec_value(api: &ICodecAPI, key: &windows::core::GUID, value: VARIANT, what: &str) {
    if let Err(e) = unsafe { api.SetValue(key, &value) } {
        debug!("parameter {what} not supported by the encoder: {e}");
    }
}

/// Does it contain `00 00 01 xx` with xx = SPS (NAL type 7)?
fn has_sps(data: &[u8]) -> bool {
    data.windows(4).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && w[3] & 0x1f == 7)
}

/// Encoder parameters and the H.264 output / NV12 input types (the output first, as
/// encoders want).
fn set_types(
    transform: &IMFTransform,
    codec_api: Option<&ICodecAPI>,
    (width, height): (u32, u32),
    settings: EncoderSettings,
) -> Result<()> {
    let fps = settings.max_fps.max(1) as u64;
    let frame_size = ((width as u64) << 32) | height as u64;
    if let Some(api) = codec_api {
        set_codec_value(api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low latency");
        set_codec_value(
            api,
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            "constant bitrate",
        );
        set_codec_value(
            api,
            &CODECAPI_AVEncCommonMeanBitRate,
            VARIANT::from(settings.bitrate_bps),
            "bitrate",
        );
        set_codec_value(api, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "no B-frames");
        // Periodic keyframe every 10 s as a safety net; the others on request.
        set_codec_value(api, &CODECAPI_AVEncMPVGOPSize, VARIANT::from((fps * 10) as u32), "GOP");
    }
    unsafe {
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate_bps)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
        out.SetUINT64(&MF_MT_FRAME_RATE, (fps << 32) | 1)?;
        out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
        transform.SetOutputType(0, &out, 0).context("H.264 output type")?;

        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        inp.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
        inp.SetUINT64(&MF_MT_FRAME_RATE, (fps << 32) | 1)?;
        inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        transform.SetInputType(0, &inp, 0).context("NV12 input type")?;
    }
    Ok(())
}

struct Session {
    activate: IMFActivate,
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    events: Receiver<TransformEvent>,
    event_thread: Option<std::thread::JoinHandle<()>>,
    /// Size of the images given (GPU textures may be larger: they are scaled down).
    input: (u32, u32),
    width: u32,
    height: u32,
    provides_samples: bool,
    output_size: u32,
    need_input: u32,
    sequence_header: Vec<u8>,
    /// Input from GPU textures: NV12 conversion on the card + device manager for the MFT.
    gpu: Option<(Nv12Converter, IMFDXGIDeviceManager)>,
    /// The next frame must be a keyframe (stream started again by `reconfigure`).
    force_key: bool,
    _timer: FineTimer,
}

impl Session {
    fn new(
        activate: &IMFActivate,
        input: (u32, u32),
        (width, height): (u32, u32),
        settings: EncoderSettings,
        device: Option<&ID3D11Device>,
    ) -> Result<Self> {
        let timer = FineTimer::new();
        let t0 = Instant::now();
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.context("ActivateObject")?;
        let t_activate = t0.elapsed();
        unsafe {
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("asynchronous MFT unlock")?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        // With a D3D11 device the MFT reads the NV12 textures straight from the GPU.
        // The manager must be handed over before setting the input and output types.
        let manager = match device {
            Some(device) => unsafe {
                let mut token = 0u32;
                let mut manager = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                let manager = manager.ok_or_else(|| anyhow!("missing DXGI manager"))?;
                manager.ResetDevice(device, token)?;
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .context("the encoder does not accept D3D11 textures")?;
                Some(manager)
            },
            None => None,
        };
        let t_manager = t0.elapsed();
        let codec_api = transform.cast::<ICodecAPI>().ok();
        set_types(&transform, codec_api.as_ref(), (width, height), settings)?;
        let t_types = t0.elapsed();
        let gpu = match (device, manager) {
            (Some(device), Some(manager)) => {
                Some((Nv12Converter::new(device, input, (width, height))?, manager))
            }
            _ => None,
        };

        let t_converter = t0.elapsed();
        let info = unsafe { transform.GetOutputStreamInfo(0) }?;
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

        let generator: IMFMediaEventGenerator = transform.cast()?;
        let (tx, events) = channel();
        let generator = SendPtr(generator);
        let event_thread = std::thread::Builder::new().name("eventi-mft".into()).spawn(move || {
            let generator = generator;
            loop {
                // Blocking: ends when the MFT is shut down (MF_E_SHUTDOWN).
                let event = match unsafe { generator.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
                    Ok(e) => e,
                    Err(e) => {
                        if e.code() != MF_E_SHUTDOWN {
                            warn!("encoder events interrupted: {e}");
                        }
                        return;
                    }
                };
                let kind = unsafe { event.GetType() }.unwrap_or(0);
                let msg = if kind == METransformNeedInput.0 as u32 {
                    TransformEvent::NeedInput
                } else if kind == METransformHaveOutput.0 as u32 {
                    TransformEvent::HaveOutput
                } else {
                    continue;
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
        })?;

        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        debug!(
            ?t_activate,
            ?t_manager,
            ?t_types,
            ?t_converter,
            total = ?t0.elapsed(),
            "hardware encoder session opened"
        );

        Ok(Self {
            activate: activate.clone(),
            transform,
            codec_api,
            events,
            event_thread: Some(event_thread),
            input,
            width,
            height,
            provides_samples,
            output_size: info.cbSize.max(1024 * 1024),
            need_input: 0,
            sequence_header: Vec::new(),
            gpu,
            force_key: false,
            _timer: timer,
        })
    }

    /// Applies a new bitrate or size to the open session. Opening one takes 0.8-1 s
    /// with Quick Sync (2026-11-01: ActivateObject 440-880 ms, handing over the GPU
    /// 150-250 ms, the types ~50 ms), and the image stopped meanwhile at every bitrate
    /// change: here the encoder and its link to the GPU are kept, the stream is stopped,
    /// the types set again and the converter remade if the size changed.
    fn reconfigure(
        &mut self,
        input: (u32, u32),
        (width, height): (u32, u32),
        settings: EncoderSettings,
        device: Option<&ID3D11Device>,
    ) -> Result<()> {
        let t = Instant::now();
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)?;
        }
        // Requests and outputs of the stream just stopped.
        while self.events.try_recv().is_ok() {}
        if (width, height) != (self.width, self.height) {
            // Quick Sync refuses an output type of another size over the current one
            // (E_FAIL): the types are cleared first.
            unsafe {
                self.transform.SetInputType(0, None, 0).context("clearing the input type")?;
                self.transform.SetOutputType(0, None, 0).context("clearing the output type")?;
            }
        }
        set_types(&self.transform, self.codec_api.as_ref(), (width, height), settings)?;
        if ((width, height) != (self.width, self.height) || input != self.input)
            && let (Some(device), Some((converter, _))) = (device, self.gpu.as_mut())
        {
            *converter = Nv12Converter::new(device, input, (width, height))?;
        }
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }?;
        self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        self.output_size = info.cbSize.max(1024 * 1024);
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        (self.input, self.width, self.height) = (input, width, height);
        self.need_input = 0;
        self.sequence_header.clear();
        self.force_key = true;
        debug!(elapsed = ?t.elapsed(), "hardware encoder reconfigured");
        Ok(())
    }

    fn handle(&mut self, event: TransformEvent, outputs: &mut VecDeque<EncodedFrame>) -> Result<()> {
        match event {
            TransformEvent::NeedInput => self.need_input += 1,
            TransformEvent::HaveOutput => {
                if let Some(frame) = self.pull_output()? {
                    outputs.push_back(frame);
                }
            }
        }
        Ok(())
    }

    fn pull_output(&mut self) -> Result<Option<EncodedFrame>> {
        let sample = if self.provides_samples {
            None
        } else {
            unsafe {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                Some(sample)
            }
        };
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0;
        let result =
            unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
        // SAFETY: the references returned in the structure are released only once.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { std::mem::ManuallyDrop::take(&mut buffer.pEvents) });

        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder proposes a new output type: it is accepted.
                unsafe {
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                }
                return Ok(None);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) => return Err(e).context("ProcessOutput"),
        }
        let Some(sample) = sample else { return Ok(None) };

        let mut data = Vec::new();
        unsafe {
            let media = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            media.Lock(&mut ptr, None, Some(&mut len))?;
            data.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
            media.Unlock()?;
        }
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;

        // Some encoders put SPS/PPS only in the output type and not in the stream:
        // the decoder needs them in front of every keyframe.
        if keyframe && !has_sps(&data) {
            if self.sequence_header.is_empty() {
                self.sequence_header = self.read_sequence_header().unwrap_or_default();
            }
            if !self.sequence_header.is_empty() {
                data.splice(0..0, self.sequence_header.iter().copied());
            }
        }
        Ok(Some(EncodedFrame { data, keyframe }))
    }

    fn read_sequence_header(&self) -> Result<Vec<u8>> {
        unsafe {
            let t = self.transform.GetOutputCurrentType(0)?;
            let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)?;
            let mut blob = vec![0u8; size as usize];
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)?;
            Ok(blob)
        }
    }

    fn input_sample(&self, image: &BgraImage, time_100ns: i64) -> Result<IMFSample> {
        let (w, h) = (self.width as usize, self.height as usize);
        let size = w * h * 3 / 2;
        unsafe {
            let buffer = MFCreateMemoryBuffer(size as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            convert::bgra_to_nv12(image, std::slice::from_raw_parts_mut(ptr, size), w, h);
            buffer.Unlock()?;
            buffer.SetCurrentLength(size as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time_100ns)?;
            Ok(sample)
        }
    }
}

impl Session {
    fn texture_sample(&mut self, bgra: &ID3D11Texture2D, time_100ns: i64) -> Result<IMFSample> {
        let (converter, _) =
            self.gpu.as_mut().ok_or_else(|| anyhow!("encoder started without a D3D11 device"))?;
        let nv12 = converter.convert(bgra)?;
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &nv12, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time_100ns)?;
            Ok(sample)
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let t = Instant::now();
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            // Hardware MFTs created from an IMFActivate are closed with ShutdownObject;
            // without it, GetEvent in the events thread never returns.
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
        if let Some(thread) = self.event_thread.take() {
            // To be safe there is no endless wait: a stuck thread is abandoned.
            while !thread.is_finished() && t.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                warn!("the encoder events thread did not end");
            }
        }
        debug!("hardware encoder closed in {:?}", t.elapsed());
    }
}

pub struct MfEncoder {
    activate: IMFActivate,
    /// Bitrate the current session was started with, and when.
    session_bitrate: u32,
    session_started: Instant,
    name: String,
    settings: EncoderSettings,
    /// Present if the encoder takes GPU textures instead of images in RAM.
    device: Option<ID3D11Device>,
    session: Option<Session>,
    outputs: VecDeque<EncodedFrame>,
    started: Instant,
}

// Used by one thread at a time; MF objects are MTA.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// First available H.264 hardware encoder.
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        let (name, activate) =
            enumerate()?.into_iter().next().ok_or_else(|| anyhow!("no H.264 hardware encoder"))?;
        Ok(Self {
            activate,
            name,
            settings,
            device: None,
            session_bitrate: settings.bitrate_bps,
            session_started: Instant::now(),
            session: None,
            outputs: VecDeque::new(),
            started: Instant::now(),
        })
    }

    /// Encoder taking textures of `device` (capture on the GPU, see [`crate::gpu`]).
    pub fn with_device(settings: EncoderSettings, device: GpuDevice) -> Result<Self> {
        let mut enc = Self::new(settings)?;
        enc.device = Some(device.0);
        Ok(enc)
    }

    pub fn device_name(&self) -> &str {
        &self.name
    }

    /// Is the requested bitrate far enough from the session's to be worth a
    /// reconfiguration (a few ms, but a keyframe)? Decreases (congested network) are
    /// more urgent than increases.
    fn bitrate_restart_due(&self) -> bool {
        let ratio = self.settings.bitrate_bps as f64 / self.session_bitrate.max(1) as f64;
        let age = self.session_started.elapsed();
        (ratio < 0.85 && age > Duration::from_secs(1)) || (ratio > 1.25 && age > Duration::from_secs(3))
    }

    /// Encodes a frame prepared by `make_sample` (image in RAM or GPU texture).
    fn encode_sample(
        &mut self,
        input: (u32, u32),
        output: (u32, u32),
        force_keyframe: bool,
        make_sample: impl FnOnce(&mut Session, i64) -> Result<IMFSample>,
    ) -> Result<Option<EncodedFrame>> {
        let (w, h) = (output.0 & !1, output.1 & !1);
        let bitrate_due = self.session.is_some() && self.bitrate_restart_due();
        if let Some(session) = self.session.as_mut()
            && (bitrate_due || session.width != w || session.height != h || session.input != input)
        {
            info!(
                "reconfiguring the hardware encoder: {w}x{h}, bitrate from {} to {} kbit/s",
                self.session_bitrate / 1000,
                self.settings.bitrate_bps / 1000
            );
            match session.reconfigure(input, (w, h), self.settings, self.device.as_ref()) {
                Ok(()) => {
                    self.outputs.clear();
                    self.session_bitrate = self.settings.bitrate_bps;
                    self.session_started = Instant::now();
                }
                Err(e) => {
                    warn!("reconfiguring the hardware encoder failed: {e:#}; opening it again");
                    self.session = None;
                }
            }
        }
        if self.session.is_none() {
            self.outputs.clear();
            let session = Session::new(&self.activate, input, (w, h), self.settings, self.device.as_ref())
                .with_context(|| format!("starting {}", self.name))?;
            self.session_bitrate = self.settings.bitrate_bps;
            self.session_started = Instant::now();
            let source = if self.device.is_some() { "GPU texture" } else { "RAM" };
            info!("hardware encoder {} started at {w}x{h}, input from {source}", self.name);
            self.session = Some(session);
        }
        let session = self.session.as_mut().expect("session started");

        // Waits until the encoder is ready to take a frame.
        let t_wait = Instant::now();
        while session.need_input == 0 {
            match session.events.recv_timeout(INPUT_TIMEOUT) {
                Ok(event) => session.handle(event, &mut self.outputs)?,
                Err(RecvTimeoutError::Timeout) => bail!("the encoder does not accept frames"),
                Err(RecvTimeoutError::Disconnected) => bail!("encoder closed"),
            }
        }

        let force_keyframe = force_keyframe || std::mem::take(&mut session.force_key);
        if force_keyframe && let Some(api) = &session.codec_api {
            set_codec_value(api, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32), "keyframe");
        }
        let waited_input = t_wait.elapsed();
        let time = (self.started.elapsed().as_nanos() / 100) as i64;
        let t_convert = Instant::now();
        let sample = make_sample(session, time)?;
        let converted = t_convert.elapsed();
        let t_encode = Instant::now();
        unsafe { session.transform.ProcessInput(0, &sample, 0) }.context("ProcessInput")?;
        session.need_input -= 1;

        // Waits for this frame's output, with a limit: if it does not arrive in time
        // it will be returned at the next call.
        let deadline = Instant::now() + OUTPUT_TIMEOUT;
        while self.outputs.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            match session.events.recv_timeout(left) {
                Ok(event) => session.handle(event, &mut self.outputs)?,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => bail!("encoder closed"),
            }
        }
        tracing::trace!(
            ?waited_input,
            ?converted,
            encoded = ?t_encode.elapsed(),
            ready = self.outputs.len(),
            "hardware frame"
        );
        Ok(self.outputs.pop_front())
    }
}

impl VideoEncoder for MfEncoder {
    fn name(&self) -> &'static str {
        "mediafoundation"
    }

    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        if self.device.is_some() {
            bail!("encoder configured for GPU textures");
        }
        let size = (image.width, image.height);
        self.encode_sample(size, size, force_keyframe, |session, time| session.input_sample(image, time))
    }

    fn encode_texture(
        &mut self,
        texture: &GpuTexture,
        input: (u32, u32),
        output: (u32, u32),
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>> {
        self.encode_sample(input, output, force_keyframe, |session, time| {
            session.texture_sample(&texture.0, time)
        })
    }

    fn accepts_textures(&self) -> bool {
        self.device.is_some()
    }

    fn set_bitrate(&mut self, bps: u32) {
        // Quick Sync through Media Foundation ignores CODECAPI_AVEncCommonMeanBitRate
        // changes while streaming (measured: 1 Mbit/s asked, 6 produced): the new value is
        // applied by stopping the stream and setting the types again (`reconfigure`), with
        // hysteresis. Measured after that: 1.6 Mbit/s asked, 1.5-1.6 sent.
        self.settings.bitrate_bps = bps;
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<bool> {
        // Before the first frame the session does not exist yet: it is created in `encode`.
        let Some(session) = self.session.as_mut() else { return Ok(true) };
        let deadline = Instant::now() + timeout;
        while session.need_input == 0 {
            match session.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(event) => session.handle(event, &mut self.outputs)?,
                Err(RecvTimeoutError::Timeout) => return Ok(false),
                Err(RecvTimeoutError::Disconnected) => bail!("encoder closed"),
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sps() {
        assert!(has_sps(&[0, 0, 0, 1, 0x67, 1, 2]));
        assert!(!has_sps(&[0, 0, 0, 1, 0x65, 1, 2]));
    }
}
