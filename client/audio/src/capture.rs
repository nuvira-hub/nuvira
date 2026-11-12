//! What the computer plays, as Opus packets.
//!
//! - Windows: WASAPI loopback of the default output device (cpal opens an output
//!   device as input that way). The device's format is converted to 48 kHz stereo.
//! - Linux: the monitor of the default output through PipeWire or PulseAudio
//!   ([`crate::pulse`]), asked for at 48 kHz stereo directly.
//! - macOS: not yet (ScreenCaptureKit).
//!
//! Silence is not sent (WASAPI gives nothing while nothing plays, the Linux monitor
//! gives zeros): the viewer plays silence when no packet comes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tracing::{info, warn};

use crate::opus::Encoder;
use crate::{CHANNELS, FRAME};

/// How often the gaps are logged.
const REPORT: Duration = Duration::from_secs(10);

/// Captures until dropped.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Whether this system can capture its sound.
pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    return crate::pulse::available();
    #[cfg(not(target_os = "linux"))]
    return cfg!(windows);
}

impl Capture {
    /// Starts capturing: `sink` gets each packet (on the capture thread).
    pub fn start(sink: impl FnMut(Vec<u8>) + Send + 'static) -> Result<Capture> {
        if !available() {
            bail!("capturing the sound is not available on this system");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("audio capture".into())
                .spawn(move || run(stop, sink, ready_tx))?
        };
        ready_rx.recv().map_err(|_| anyhow!("the audio capture stopped while starting"))??;
        Ok(Capture { stop, thread: Some(thread) })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Gives 48 kHz stereo samples in pieces of any size: `Ok(None)` when nothing came
/// for a while (the loop then looks at the stop flag), an error ends the capture.
type Source = Box<dyn FnMut() -> Result<Option<Vec<f32>>>>;

fn run(stop: Arc<AtomicBool>, mut sink: impl FnMut(Vec<u8>), ready: mpsc::Sender<Result<()>>) {
    let gaps_seen = Arc::new(AtomicU64::new(0));
    let started = open(gaps_seen.clone()).and_then(|source| Ok((source, Encoder::new()?)));
    let (mut source, mut encoder) = match started {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut pcm: Vec<f32> = Vec::with_capacity(FRAME * CHANNELS * 2);
    let mut last_report = Instant::now();
    let mut gaps_reported = 0;
    while !stop.load(Ordering::Relaxed) {
        if last_report.elapsed() >= REPORT {
            let gaps = gaps_seen.load(Ordering::Relaxed);
            if gaps != gaps_reported {
                warn!(
                    gaps = gaps - gaps_reported,
                    "audio capture: the system dropped some sound (computer busy)"
                );
                gaps_reported = gaps;
            }
            last_report = Instant::now();
        }
        match source() {
            Ok(Some(s)) => pcm.extend(s),
            Ok(None) => continue,
            Err(e) => return warn!("audio capture: {e:#}"),
        }
        while pcm.len() >= FRAME * CHANNELS {
            let frame: Vec<f32> = pcm.drain(..FRAME * CHANNELS).collect();
            if frame.iter().all(|s| *s == 0.0) {
                continue;
            }
            match encoder.encode(&frame) {
                Ok(packet) => sink(packet),
                Err(e) => return warn!("audio capture: {e:#}"),
            }
        }
    }
    info!("audio capture stopped");
}

#[cfg(target_os = "linux")]
fn open(_gaps: Arc<AtomicU64>) -> Result<Source> {
    let mut monitor = crate::pulse::Monitor::open()?;
    info!("audio capture started (monitor of the default output)");
    Ok(Box::new(move || {
        let mut pcm = vec![0f32; FRAME * CHANNELS];
        monitor.read(&mut pcm)?;
        Ok(Some(pcm))
    }))
}

#[cfg(windows)]
fn open(gaps: Arc<AtomicU64>) -> Result<Source> {
    use anyhow::Context;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use crate::RATE;
    use crate::resample::{Resampler, to_stereo};

    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let device = cpal::default_host().default_output_device().context("no output device to capture")?;
    let supported = device.default_output_config()?;
    let (channels, rate) = (supported.channels() as usize, supported.sample_rate());
    let format = supported.sample_format();
    let mut config = supported.config();
    // 100 ms of room in Windows's buffer (the default is about 10 ms): when the
    // computer is busy (a video playing, the screen being encoded) this thread can
    // be late, and with a small buffer Windows drops the sound meanwhile (a gap).
    // It does not add delay: the data is read as soon as it is there.
    config.buffer_size = cpal::BufferSize::Fixed(rate / 10);
    let err = move |e: cpal::Error| {
        // Windows reports each gap ("buffer underrun or overrun"): counted, logged
        // every 10 s instead of once each.
        gaps.fetch_add(1, Ordering::Relaxed);
        tracing::debug!("audio capture: {e}");
    };
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            move |data, _| {
                let _ = tx.send(data.to_vec());
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |data, _| {
                let _ = tx.send(data.iter().map(|s| *s as f32 / 32768.0).collect());
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I32 => device.build_input_stream::<i32, _, _>(
            config,
            move |data, _| {
                let _ = tx.send(data.iter().map(|s| *s as f32 / 2_147_483_648.0).collect());
            },
            err,
            None,
        )?,
        other => bail!("sample format {other:?} not handled"),
    };
    stream.play()?;
    info!(channels, rate, ?format, "audio capture started");
    let mut resampler = Resampler::new(rate, RATE, CHANNELS)?;
    Ok(Box::new(move || {
        // The stream lives as long as the source.
        let _ = &stream;
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(data) => Ok(Some(resampler.process(&to_stereo(&data, channels))?)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the sound device went away"),
        }
    }))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn open(_gaps: Arc<AtomicU64>) -> Result<Source> {
    bail!("capturing the sound is not available on this system yet")
}
