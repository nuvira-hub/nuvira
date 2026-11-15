//! The technician's microphone (the viewer's default input device), as Opus
//! packets for the controlled computer, which plays them with [`crate::player`].
//! Through cpal everywhere: WASAPI on Windows, ALSA (PipeWire's or PulseAudio's
//! plugin) on Linux, Core Audio on macOS (which asks the person once for the
//! microphone).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tracing::{info, warn};

use crate::opus::Encoder;
use crate::resample::{Resampler, to_stereo};
use crate::{CHANNELS, FRAME, RATE};

/// Whether this computer has a microphone (a default input device).
pub fn available() -> bool {
    cpal::default_host().default_input_device().is_some()
}

/// Captures until dropped.
pub struct Microphone {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Microphone {
    /// Starts capturing: `sink` gets each packet (on the capture thread).
    pub fn start(sink: impl FnMut(Vec<u8>) + Send + 'static) -> Result<Microphone> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new().name("microphone".into()).spawn(move || run(stop, sink, ready_tx))?
        };
        ready_rx.recv().map_err(|_| anyhow!("the microphone stopped while starting"))??;
        Ok(Microphone { stop, thread: Some(thread) })
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(stop: Arc<AtomicBool>, mut sink: impl FnMut(Vec<u8>), ready: mpsc::Sender<Result<()>>) {
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let opened = open(tx).and_then(|(stream, channels, rate)| {
        Ok((stream, channels, Resampler::new(rate, RATE, CHANNELS)?, Encoder::voice()?))
    });
    let (stream, channels, mut resampler, mut encoder) = match opened {
        Ok(o) => {
            let _ = ready.send(Ok(()));
            o
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut pcm: Vec<f32> = Vec::with_capacity(FRAME * CHANNELS * 2);
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(data) => match resampler.process(&to_stereo(&data, channels)) {
                Ok(s) => pcm.extend(s),
                Err(e) => return warn!("microphone: {e:#}"),
            },
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return warn!("microphone: the device went away"),
        }
        while pcm.len() >= FRAME * CHANNELS {
            let frame: Vec<f32> = pcm.drain(..FRAME * CHANNELS).collect();
            match encoder.encode(&frame) {
                Ok(packet) => sink(packet),
                Err(e) => return warn!("microphone: {e:#}"),
            }
        }
    }
    drop(stream);
    info!("microphone stopped");
}

/// The default input device, sending what it records to `tx`: the stream, its
/// channels and rate.
fn open(tx: mpsc::Sender<Vec<f32>>) -> Result<(cpal::Stream, usize, u32)> {
    let device = cpal::default_host().default_input_device().context("no microphone")?;
    let supported = device.default_input_config()?;
    let (channels, rate) = (supported.channels() as usize, supported.sample_rate());
    let format = supported.sample_format();
    let config = supported.config();
    let err = |e: cpal::Error| tracing::debug!("microphone: {e}");
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
    info!(channels, rate, ?format, "microphone started");
    Ok((stream, channels, rate))
}
