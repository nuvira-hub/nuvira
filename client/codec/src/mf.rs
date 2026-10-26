//! H.264 con l'encoder hardware della scheda video tramite Media Foundation
//! (Intel Quick Sync, NVIDIA NVENC, AMD AMF: ognuno registra il proprio MFT).
//!
//! Gli MFT hardware sono asincroni: segnalano con eventi quando accettano un
//! input (`METransformNeedInput`) e quando hanno un output (`METransformHaveOutput`).
//! Un thread dedicato legge gli eventi e li passa a `encode` tramite un canale,
//! così l'attesa dell'output ha un limite di tempo e non può bloccarsi.
//!
//! Fase 0: l'immagine viene convertita in NV12 sulla CPU e passata in memoria di
//! sistema. Il passo successivo è passare la texture D3D11 della cattura senza copie.

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

use crate::{BgraImage, EncodedFrame, EncoderSettings, VideoEncoder, convert};

/// Tempo massimo di attesa dell'output di un frame prima di restituire `None`
/// (il frame uscirà alla chiamata successiva).
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(100);
/// Tempo massimo di attesa perché l'encoder accetti un nuovo frame.
const INPUT_TIMEOUT: Duration = Duration::from_secs(2);

fn startup() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow!("MFStartup: {e}"))
}

/// Gli oggetti Media Foundation sono utilizzabili da qualunque thread (MTA).
struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

enum TransformEvent {
    NeedInput,
    HaveOutput,
}

/// Nomi degli encoder H.264 hardware installati, in ordine di preferenza.
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
    // SAFETY: MFTEnumEx restituisce un array di `count` puntatori allocato con CoTaskMemAlloc.
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
            return "encoder hardware".into();
        }
        let s = name.to_string().unwrap_or_default();
        CoTaskMemFree(Some(name.0 as *const _));
        s
    }
}

fn set_codec_value(api: &ICodecAPI, key: &windows::core::GUID, value: VARIANT, what: &str) {
    if let Err(e) = unsafe { api.SetValue(key, &value) } {
        debug!("parametro {what} non supportato dall'encoder: {e}");
    }
}

/// Contiene `00 00 01 xx` con xx = SPS (tipo NAL 7)?
fn has_sps(data: &[u8]) -> bool {
    data.windows(4).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && w[3] & 0x1f == 7)
}

struct Session {
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    events: Receiver<TransformEvent>,
    event_thread: Option<std::thread::JoinHandle<()>>,
    width: u32,
    height: u32,
    provides_samples: bool,
    output_size: u32,
    need_input: u32,
    sequence_header: Vec<u8>,
}

impl Session {
    fn new(activate: &IMFActivate, width: u32, height: u32, settings: EncoderSettings) -> Result<Self> {
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.context("ActivateObject")?;
        unsafe {
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("sblocco MFT asincrono")?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let fps = settings.max_fps.max(1) as u64;
        let frame_size = ((width as u64) << 32) | height as u64;

        let codec_api = transform.cast::<ICodecAPI>().ok();
        if let Some(api) = &codec_api {
            set_codec_value(api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true), "bassa latenza");
            set_codec_value(
                api,
                &CODECAPI_AVEncCommonRateControlMode,
                VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                "bitrate costante",
            );
            set_codec_value(
                api,
                &CODECAPI_AVEncCommonMeanBitRate,
                VARIANT::from(settings.bitrate_bps),
                "bitrate",
            );
            set_codec_value(
                api,
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                VARIANT::from(0u32),
                "niente B-frame",
            );
            // Keyframe periodico ogni 10 s come rete di sicurezza; gli altri su richiesta.
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
            transform.SetOutputType(0, &out, 0).context("tipo di output H.264")?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, (fps << 32) | 1)?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &inp, 0).context("tipo di input NV12")?;
        }

        let info = unsafe { transform.GetOutputStreamInfo(0) }?;
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

        let generator: IMFMediaEventGenerator = transform.cast()?;
        let (tx, events) = channel();
        let generator = SendPtr(generator);
        let event_thread = std::thread::Builder::new().name("eventi-mft".into()).spawn(move || {
            let generator = generator;
            loop {
                // Bloccante: termina quando l'MFT viene chiuso (MF_E_SHUTDOWN).
                let event = match unsafe { generator.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
                    Ok(e) => e,
                    Err(e) => {
                        if e.code() != MF_E_SHUTDOWN {
                            warn!("eventi dell'encoder interrotti: {e}");
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

        Ok(Self {
            transform,
            codec_api,
            events,
            event_thread: Some(event_thread),
            width,
            height,
            provides_samples,
            output_size: info.cbSize.max(1024 * 1024),
            need_input: 0,
            sequence_header: Vec::new(),
        })
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
        // SAFETY: rilasciamo una sola volta i riferimenti restituiti nella struttura.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { std::mem::ManuallyDrop::take(&mut buffer.pEvents) });

        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // L'encoder propone un nuovo tipo di output: lo accettiamo.
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

        // Alcuni encoder mettono SPS/PPS solo nel tipo di output e non nel flusso:
        // il decoder ne ha bisogno davanti a ogni keyframe.
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

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
        if let Some(thread) = self.event_thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct MfEncoder {
    activate: IMFActivate,
    name: String,
    settings: EncoderSettings,
    session: Option<Session>,
    outputs: VecDeque<EncodedFrame>,
    started: Instant,
}

// Usato da un solo thread alla volta; gli oggetti MF sono MTA.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// Primo encoder H.264 hardware disponibile.
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        let (name, activate) =
            enumerate()?.into_iter().next().ok_or_else(|| anyhow!("nessun encoder H.264 hardware"))?;
        Ok(Self {
            activate,
            name,
            settings,
            session: None,
            outputs: VecDeque::new(),
            started: Instant::now(),
        })
    }

    pub fn device_name(&self) -> &str {
        &self.name
    }
}

impl VideoEncoder for MfEncoder {
    fn name(&self) -> &'static str {
        "mediafoundation"
    }

    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        let (w, h) = convert::even_size(image);
        let (w, h) = (w as u32, h as u32);
        if self.session.as_ref().is_none_or(|s| s.width != w || s.height != h) {
            self.session = None;
            self.outputs.clear();
            let session = Session::new(&self.activate, w, h, self.settings)
                .with_context(|| format!("avvio di {}", self.name))?;
            info!("encoder hardware {} avviato a {w}x{h}", self.name);
            self.session = Some(session);
        }
        let session = self.session.as_mut().expect("sessione avviata");

        // Attende che l'encoder sia pronto a ricevere un frame.
        let t_wait = Instant::now();
        while session.need_input == 0 {
            match session.events.recv_timeout(INPUT_TIMEOUT) {
                Ok(event) => session.handle(event, &mut self.outputs)?,
                Err(RecvTimeoutError::Timeout) => bail!("l'encoder non accetta frame"),
                Err(RecvTimeoutError::Disconnected) => bail!("encoder chiuso"),
            }
        }

        if force_keyframe && let Some(api) = &session.codec_api {
            set_codec_value(api, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32), "keyframe");
        }
        let waited_input = t_wait.elapsed();
        let time = (self.started.elapsed().as_nanos() / 100) as i64;
        let t_convert = Instant::now();
        let sample = session.input_sample(image, time)?;
        let converted = t_convert.elapsed();
        let t_encode = Instant::now();
        unsafe { session.transform.ProcessInput(0, &sample, 0) }.context("ProcessInput")?;
        session.need_input -= 1;

        // Attende l'output di questo frame, con un limite: se non arriva in tempo
        // verrà restituito alla prossima chiamata.
        let deadline = Instant::now() + OUTPUT_TIMEOUT;
        while self.outputs.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            match session.events.recv_timeout(left) {
                Ok(event) => session.handle(event, &mut self.outputs)?,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => bail!("encoder chiuso"),
            }
        }
        tracing::trace!(
            ?waited_input,
            ?converted,
            encoded = ?t_encode.elapsed(),
            ready = self.outputs.len(),
            "frame hardware"
        );
        Ok(self.outputs.pop_front())
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
