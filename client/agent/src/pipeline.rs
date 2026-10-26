//! Cattura e compressione su due thread.
//!
//! Il thread di cattura deposita ogni immagine nuova in uno slot; il thread di
//! compressione prende sempre la più recente. Se la compressione è più lenta
//! della cattura le immagini intermedie vengono saltate: i fps dipendono dalla
//! fase più lenta e non dalla somma delle due, e non si accumula ritardo.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use nexo_capture::{Capturer, Display};
use nexo_codec::{BgraImage, VideoEncoder};
use nexo_proto::v1::VideoFrame;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Immagine catturata in attesa di compressione.
struct Captured {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
    captured_us: u64,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Option<Captured>>,
    ready: Condvar,
    /// Buffer già allocati da riusare (evita 8 MB di allocazioni per frame a 1080p).
    spare: Mutex<Vec<Vec<u8>>>,
    stop: AtomicBool,
    captured: AtomicU64,
    skipped: AtomicU64,
}

impl Shared {
    fn recycle(&self, buf: Vec<u8>) {
        let mut spare = self.spare.lock().expect("spare");
        if spare.len() < 3 {
            spare.push(buf);
        }
    }
}

pub struct Pipeline {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Pipeline {
    /// Avvia i due thread. Bloccante finché cattura ed encoder non sono pronti.
    pub fn start(
        open_capturer: impl FnOnce() -> Result<Box<dyn Capturer>> + Send + 'static,
        open_encoder: impl FnOnce() -> Result<Box<dyn VideoEncoder>> + Send + 'static,
        max_fps: u32,
        frames: mpsc::Sender<VideoFrame>,
        keyframe: Arc<AtomicBool>,
    ) -> Result<(Display, Self)> {
        let shared = Arc::new(Shared::default());
        let mut pipeline = Self { shared: shared.clone(), threads: Vec::new() };

        // L'encoder si crea nel thread che lo userà (Media Foundation inizializza COM per thread).
        let (enc_ready_tx, enc_ready_rx) = std::sync::mpsc::channel();
        let s = shared.clone();
        pipeline.threads.push(std::thread::Builder::new().name("compressione".into()).spawn(move || {
            let encoder = match open_encoder() {
                Ok(e) => {
                    let _ = enc_ready_tx.send(Ok(e.name()));
                    e
                }
                Err(e) => {
                    let _ = enc_ready_tx.send(Err(e));
                    return;
                }
            };
            encode_loop(&s, encoder, max_fps, &frames, &keyframe);
        })?);

        let (cap_ready_tx, cap_ready_rx) = std::sync::mpsc::channel();
        let s = shared.clone();
        pipeline.threads.push(std::thread::Builder::new().name("cattura".into()).spawn(move || {
            let capturer = match open_capturer() {
                Ok(c) => {
                    let _ = cap_ready_tx.send(Ok(c.display().clone()));
                    c
                }
                Err(e) => {
                    let _ = cap_ready_tx.send(Err(e));
                    return;
                }
            };
            capture_loop(&s, capturer);
        })?);

        // In caso di errore `pipeline` viene distrutta e ferma l'altro thread.
        let encoder = enc_ready_rx.recv().map_err(|_| anyhow!("thread di compressione terminato"))??;
        let screen = cap_ready_rx.recv().map_err(|_| anyhow!("thread di cattura terminato"))??;
        info!("trasmetto {} {}x{} con {encoder}", screen.name, screen.width, screen.height);
        Ok((screen, pipeline))
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.ready.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn capture_loop(shared: &Shared, mut capturer: Box<dyn Capturer>) {
    while !shared.stop.load(Ordering::Relaxed) {
        let frame = match capturer.next_frame(Duration::from_millis(100)) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(e) => {
                // Es. desktop sicuro o schermo bloccato: si riprova senza chiudere la sessione.
                warn!("cattura non riuscita: {e:#}");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let mut data = shared.spare.lock().expect("spare").pop().unwrap_or_default();
        data.clear();
        data.extend_from_slice(&frame.data);
        let captured = Captured {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            data,
            captured_us: frame.captured_us,
        };
        shared.captured.fetch_add(1, Ordering::Relaxed);
        let old = shared.slot.lock().expect("slot").replace(captured);
        if let Some(old) = old {
            shared.skipped.fetch_add(1, Ordering::Relaxed);
            shared.recycle(old.data);
        }
        shared.ready.notify_one();
    }
}

fn encode_loop(
    shared: &Shared,
    mut encoder: Box<dyn VideoEncoder>,
    max_fps: u32,
    frames: &mpsc::Sender<VideoFrame>,
    keyframe: &AtomicBool,
) {
    let min_interval = Duration::from_secs(1) / max_fps.max(1);
    let mut last_encoded = Instant::now() - min_interval;
    // Ultima immagine compressa: serve a rifare un keyframe a schermo fermo.
    let mut last: Option<Captured> = None;
    let mut seq = 0u64;
    let mut stats = Stats::default();

    while !shared.stop.load(Ordering::Relaxed) {
        // Limite di frame al secondo: nel frattempo lo slot si aggiorna con l'immagine più recente.
        let wait = min_interval.saturating_sub(last_encoded.elapsed());
        if !wait.is_zero() && !keyframe.load(Ordering::Relaxed) {
            std::thread::sleep(wait);
        }

        let fresh = {
            let slot = shared.slot.lock().expect("slot");
            let (mut slot, _) = shared
                .ready
                .wait_timeout_while(slot, Duration::from_millis(100), |s| {
                    s.is_none() && !shared.stop.load(Ordering::Relaxed)
                })
                .expect("slot");
            slot.take()
        };
        let force_key = keyframe.load(Ordering::Relaxed);
        if let Some(fresh) = fresh {
            if let Some(old) = last.replace(fresh) {
                shared.recycle(old.data);
            }
        } else if !force_key {
            continue;
        }
        // Schermo fermo e keyframe richiesto: si ricomprime l'ultima immagine.
        let Some(image) = &last else { continue };

        let t = Instant::now();
        let bgra =
            BgraImage { width: image.width, height: image.height, stride: image.stride, data: &image.data };
        let encoded = match encoder.encode(&bgra, force_key) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => {
                warn!("compressione non riuscita: {e:#}");
                continue;
            }
        };
        last_encoded = Instant::now();
        if encoded.keyframe {
            keyframe.store(false, Ordering::Relaxed);
        }
        let encode_us = t.elapsed().as_micros() as u32;
        stats.add(encode_us, encoded.data.len());
        seq += 1;
        let msg = VideoFrame {
            seq,
            capture_us: image.captured_us,
            encode_us,
            keyframe: encoded.keyframe,
            data: encoded.data,
        };
        // Coda da un solo frame: se la rete è lenta si salta la cattura invece di accumulare ritardo.
        if frames.blocking_send(msg).is_err() {
            break;
        }
        stats.report(shared);
    }
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    bytes: usize,
    encode_us: u64,
    captured_before: u64,
    skipped_before: u64,
}

impl Stats {
    fn add(&mut self, encode_us: u32, bytes: usize) {
        self.since.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.bytes += bytes;
        self.encode_us += encode_us as u64;
    }

    fn report(&mut self, shared: &Shared) {
        let Some(since) = self.since else { return };
        let secs = since.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        let captured = shared.captured.load(Ordering::Relaxed);
        let skipped = shared.skipped.load(Ordering::Relaxed);
        info!(
            "{:.1} fps inviati ({:.1} catturati, {} saltati), compressione {:.1} ms/frame, {:.0} kbit/s",
            self.frames as f64 / secs,
            (captured - self.captured_before) as f64 / secs,
            skipped - self.skipped_before,
            self.encode_us as f64 / self.frames.max(1) as f64 / 1000.0,
            self.bytes as f64 * 8.0 / secs / 1000.0
        );
        *self = Stats { captured_before: captured, skipped_before: skipped, ..Default::default() };
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    #[test]
    fn produces_frames_and_stops() {
        let (tx, mut rx) = mpsc::channel(1);
        let keyframe = Arc::new(AtomicBool::new(true));
        let (display, pipeline) = Pipeline::start(
            || Ok(Box::new(nexo_capture::SyntheticCapturer::new(320, 180, 60)) as Box<dyn Capturer>),
            || nexo_codec::create_encoder(nexo_codec::EncoderChoice::Software, Default::default()),
            60,
            tx,
            keyframe,
        )
        .context("avvio")
        .unwrap();
        assert_eq!((display.width, display.height), (320, 180));
        let first = rx.blocking_recv().expect("primo frame");
        assert!(first.keyframe);
        assert!(rx.blocking_recv().is_some());
        drop(pipeline);
    }
}
