//! Playback of the packets on the default output device.
//!
//! Packets go to a decoding thread; the device's callback takes the samples from a
//! buffer kept around a target, [`TARGET`] at first: it starts playing once that
//! much is there, plays silence when it runs dry (and waits to fill again), and drops
//! the oldest audio beyond the target plus [`SLACK`] so the delay does not grow. A
//! lost packet is rebuilt from the redundancy of the next one, or made up by Opus
//! (concealment).
//!
//! The target adapts: a slow or overloaded computer sends its sound in bursts (it
//! stops for a while, then everything at once), and each drop is a tear. Each drop
//! raises the target by [`STEP_UP`] up to [`TARGET_MAX`]; after [`CALM`] without
//! one it comes down by [`STEP_DOWN`]. On a normal computer it stays at [`TARGET`].

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tracing::{info, warn};

use crate::opus::Decoder;
use crate::resample::{Resampler, from_stereo};
use crate::{CHANNELS, RATE};

/// Audio kept ahead of the speakers, at first and at best.
pub const TARGET: Duration = Duration::from_millis(60);
/// More than the target plus this behind: the oldest is dropped.
pub const SLACK: Duration = Duration::from_millis(190);
/// Largest target, for the slowest computers.
pub const TARGET_MAX: Duration = Duration::from_millis(300);
const STEP_UP: Duration = Duration::from_millis(40);
const STEP_DOWN: Duration = Duration::from_millis(20);
/// Time without drops before the target comes down a step.
const CALM: Duration = Duration::from_secs(30);
/// Lost packets made up at most in a row (beyond it, silence: the sound stopped).
const MAX_CONCEALED: u32 = 5;

/// What the player saw, for the statistics.
#[derive(Default)]
pub struct Counters {
    pub packets: AtomicU64,
    pub lost: AtomicU64,
    pub underruns: AtomicU64,
    /// Times the buffer grew past the target plus SLACK and the oldest audio was dropped.
    pub trimmed: AtomicU64,
}

pub struct Player {
    tx: Option<mpsc::Sender<(u32, Vec<u8>)>>,
    muted: Arc<AtomicBool>,
    pub counters: Arc<Counters>,
    thread: Option<JoinHandle<()>>,
}

struct Buffer {
    samples: VecDeque<f32>,
    /// Waiting to reach `target` before playing (start, or after running dry).
    filling: bool,
    /// Samples to have before playing (the adaptive target).
    target: usize,
}

impl Player {
    /// Opens the default output device. `dump`: writes what is decoded to a WAV file
    /// instead (48 kHz stereo, automated tests: nothing is played, so a test on one
    /// computer does not capture its own playback).
    pub fn start(dump: Option<PathBuf>) -> Result<Player> {
        let (tx, rx) = mpsc::channel();
        let muted = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = {
            let (muted, counters) = (muted.clone(), counters.clone());
            std::thread::Builder::new()
                .name("audio playback".into())
                .spawn(move || run(rx, muted, counters, dump, ready_tx))?
        };
        ready_rx.recv().map_err(|_| anyhow!("the audio playback stopped while starting"))??;
        Ok(Player { tx: Some(tx), muted, counters, thread: Some(thread) })
    }

    /// A packet received (from [`crate::packet::decode`]).
    pub fn push(&self, seq: u32, packet: &[u8]) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((seq, packet.to_vec()));
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type Output = (Option<cpal::Stream>, usize, u32, Arc<Mutex<Buffer>>);

fn open_output(muted: Arc<AtomicBool>, counters: Arc<Counters>, need_device: bool) -> Result<Output> {
    let buffer = Arc::new(Mutex::new(Buffer { samples: VecDeque::new(), filling: true, target: 0 }));
    let device = match need_device {
        true => cpal::default_host().default_output_device().context("no output device")?,
        false => return Ok((None, CHANNELS, RATE, buffer)),
    };
    let supported = device.default_output_config()?;
    let (channels, rate) = (supported.channels() as usize, supported.sample_rate());
    buffer.lock().expect("audio buffer").target = samples(TARGET, rate, channels);
    let shared = buffer.clone();
    let fill = move |out: &mut [f32]| {
        let mut b = shared.lock().expect("audio buffer");
        if b.filling && b.samples.len() < b.target {
            out.fill(0.0);
            return;
        }
        b.filling = false;
        let n = out.len().min(b.samples.len());
        for (o, s) in out.iter_mut().zip(b.samples.drain(..n)) {
            *o = s;
        }
        out[n..].fill(0.0);
        if n < out.len() {
            b.filling = true;
            if n > 0 {
                counters.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        if muted.load(Ordering::Relaxed) {
            out.fill(0.0);
        }
    };
    let err = |e| warn!("audio playback: {e}");
    let config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            device.build_output_stream::<f32, _, _>(config, move |out, _| fill(out), err, None)?
        }
        cpal::SampleFormat::I16 => {
            let mut tmp = Vec::new();
            device.build_output_stream::<i16, _, _>(
                config,
                move |out, _| {
                    tmp.resize(out.len(), 0.0);
                    fill(&mut tmp);
                    for (o, s) in out.iter_mut().zip(&tmp) {
                        *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                },
                err,
                None,
            )?
        }
        other => bail!("sample format {other:?} not handled"),
    };
    stream.play()?;
    info!(channels, rate, "audio playback started");
    Ok((Some(stream), channels, rate, buffer))
}

fn run(
    rx: mpsc::Receiver<(u32, Vec<u8>)>,
    muted: Arc<AtomicBool>,
    counters: Arc<Counters>,
    dump: Option<PathBuf>,
    ready: mpsc::Sender<Result<()>>,
) {
    let started = (|| -> Result<_> {
        let output = open_output(muted, counters.clone(), dump.is_none())?;
        let wav = dump.map(Wav::create).transpose()?;
        Ok((output, Decoder::new()?, wav))
    })();
    let ((stream, channels, rate, buffer), mut decoder, mut wav) = match started {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut resampler = match Resampler::new(RATE, rate, CHANNELS) {
        Ok(r) => r,
        Err(e) => return warn!("audio playback: {e:#}"),
    };
    let mut delay = TARGET;
    let mut changed = std::time::Instant::now();
    let mut next: Option<u32> = None;
    let mut last_report = std::time::Instant::now();
    let mut reported = (0, 0, 0);
    while let Ok((seq, packet)) = rx.recv() {
        if last_report.elapsed() >= std::time::Duration::from_secs(10) {
            let now = (
                counters.lost.load(Ordering::Relaxed),
                counters.underruns.load(Ordering::Relaxed),
                counters.trimmed.load(Ordering::Relaxed),
            );
            if now != reported {
                info!(
                    lost = now.0 - reported.0,
                    underruns = now.1 - reported.1,
                    trimmed = now.2 - reported.2,
                    "audio playback in the last 10 s"
                );
                reported = now;
            }
            last_report = std::time::Instant::now();
        }
        counters.packets.fetch_add(1, Ordering::Relaxed);
        let mut pcm = Vec::new();
        match next {
            // Late or repeated: its time has passed.
            Some(n) if seq.wrapping_sub(n) > u32::MAX / 2 => continue,
            Some(n) if seq != n => {
                let missing = seq.wrapping_sub(n);
                counters.lost.fetch_add(missing as u64, Ordering::Relaxed);
                if missing <= MAX_CONCEALED {
                    for _ in 1..missing {
                        pcm.extend(decoder.decode(None, false).unwrap_or_default());
                    }
                    // The one just before this packet, from its redundancy.
                    pcm.extend(decoder.decode(Some(&packet), true).unwrap_or_default());
                }
            }
            _ => {}
        }
        next = Some(seq.wrapping_add(1));
        match decoder.decode(Some(&packet), false) {
            Ok(p) => pcm.extend(p),
            Err(e) => {
                warn!("audio: {e:#}");
                continue;
            }
        }
        if let Some(w) = wav.as_mut() {
            let _ = w.write(&pcm);
        }
        if stream.is_none() {
            continue;
        }
        let out = match resampler.process(&pcm) {
            Ok(s) => from_stereo(&s, channels),
            Err(e) => return warn!("audio playback: {e:#}"),
        };
        let mut b = buffer.lock().expect("audio buffer");
        b.samples.extend(out);
        if b.samples.len() > samples(delay + SLACK, rate, channels) {
            // A burst: more room from now on, then down to the (new) target.
            if delay < TARGET_MAX {
                delay = (delay + STEP_UP).min(TARGET_MAX);
                info!(ms = delay.as_millis() as u64, "audio buffer raised (sound arriving in bursts)");
            }
            changed = std::time::Instant::now();
            b.target = samples(delay, rate, channels);
            let drop = b.samples.len() - b.target;
            b.samples.drain(..drop);
            counters.trimmed.fetch_add(1, Ordering::Relaxed);
        } else if delay > TARGET && changed.elapsed() >= CALM {
            delay = delay.saturating_sub(STEP_DOWN).max(TARGET);
            changed = std::time::Instant::now();
            b.target = samples(delay, rate, channels);
            info!(ms = delay.as_millis() as u64, "audio buffer lowered");
        }
    }
    drop(stream);
    info!(
        packets = counters.packets.load(Ordering::Relaxed),
        lost = counters.lost.load(Ordering::Relaxed),
        underruns = counters.underruns.load(Ordering::Relaxed),
        trimmed = counters.trimmed.load(Ordering::Relaxed),
        "audio playback stopped"
    );
}

/// 16-bit 48 kHz stereo WAV, its sizes written when closed.
struct Wav {
    file: std::fs::File,
    bytes: u32,
}

impl Wav {
    fn create(path: PathBuf) -> Result<Wav> {
        let mut file = std::fs::File::create(&path).with_context(|| path.display().to_string())?;
        let rate = RATE;
        let block = (CHANNELS * 2) as u16;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&(CHANNELS as u16).to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * block as u32).to_le_bytes());
        h.extend_from_slice(&block.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data\0\0\0\0");
        file.write_all(&h)?;
        Ok(Wav { file, bytes: 0 })
    }

    fn write(&mut self, pcm: &[f32]) -> Result<()> {
        let data: Vec<u8> =
            pcm.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
        self.file.write_all(&data)?;
        self.bytes += data.len() as u32;
        Ok(())
    }
}

impl Drop for Wav {
    fn drop(&mut self) {
        let _ = (|| -> std::io::Result<()> {
            self.file.seek(SeekFrom::Start(4))?;
            self.file.write_all(&(36 + self.bytes).to_le_bytes())?;
            self.file.seek(SeekFrom::Start(40))?;
            self.file.write_all(&self.bytes.to_le_bytes())
        })();
    }
}

/// Signal checks shared by the tests and the test tools.
pub mod tests_support {
    /// Main frequency of a mono signal, from its zero crossings.
    pub fn frequency(samples: &[f32], rate: u32) -> f32 {
        let crossings = samples.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        crossings as f32 * rate as f32 / samples.len().max(1) as f32
    }
}

/// Interleaved samples in `d` at `rate` with `channels`.
fn samples(d: Duration, rate: u32, channels: usize) -> usize {
    (rate as f64 * d.as_secs_f64()) as usize * channels
}
