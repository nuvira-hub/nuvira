//! Scelta automatica dell'encoder.
//!
//! Non sempre l'encoder hardware è il più veloce: su un i7-10710U con Intel UHD,
//! OpenH264 con NASM comprime un 1080p in ~9 ms contro gli ~11 di Quick Sync, e la
//! scheda video può essere occupata da altro. All'avvio si provano entrambi su
//! un'immagine tipo desktop e si sceglie il più rapido; la decisione resta valida
//! per tutta la vita del processo.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tracing::info;

use crate::{BgraImage, EncoderChoice, EncoderSettings, VideoEncoder};

/// Frame di prova per encoder; i primi `WARMUP` non contano (avvio, primo keyframe).
const FRAMES: usize = 24;
const WARMUP: usize = 4;
/// Ritmo di arrivo dei frame durante la prova, come una cattura a 60 fps.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// L'hardware viene preferito a meno che il software sia più veloce di questo fattore:
/// a parità di tempi lascia libera la CPU.
const SOFTWARE_MARGIN: f64 = 1.25;

fn test_frames(w: usize, h: usize) -> [Vec<u8>; 2] {
    let frame = |shift: usize| {
        let mut data = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = &mut data[(y * w + x) * 4..][..4];
                // Sfondo, finestra con righe di "testo" spostata di `shift` pixel.
                let in_window = (200 + shift..1000 + shift).contains(&x) && (150..750).contains(&y);
                let c = if in_window && (y / 18) % 2 == 0 && (x / 7) % 3 != 0 {
                    [20, 20, 20, 255]
                } else if in_window {
                    [250, 250, 250, 255]
                } else {
                    [((x + y) % 256) as u8, 120, 40, 255]
                };
                p.copy_from_slice(&c);
            }
        }
        data
    };
    [frame(0), frame(40)]
}

/// Tempo medio di compressione a regime, in microsecondi.
fn measure(encoder: &mut dyn VideoEncoder, frames: &[Vec<u8>; 2], w: u32, h: u32) -> anyhow::Result<f64> {
    let mut total = Duration::ZERO;
    for i in 0..FRAMES {
        let image = BgraImage { width: w, height: h, stride: w as usize * 4, data: &frames[i % 2] };
        let t = Instant::now();
        encoder.encode(&image, i == 0)?;
        let elapsed = t.elapsed();
        if i >= WARMUP {
            total += elapsed;
        }
        std::thread::sleep(FRAME_INTERVAL.saturating_sub(elapsed));
    }
    Ok(total.as_micros() as f64 / (FRAMES - WARMUP) as f64)
}

/// Quale encoder usare in modalità automatica (calcolato una volta sola).
pub(crate) fn auto_choice(settings: EncoderSettings) -> EncoderChoice {
    static CHOICE: OnceLock<EncoderChoice> = OnceLock::new();
    *CHOICE.get_or_init(|| {
        let (w, h) = (1920u32, 1080u32);
        let frames = test_frames(w as usize, h as usize);
        let hardware = crate::create_specific(EncoderChoice::Hardware, settings)
            .and_then(|mut e| measure(e.as_mut(), &frames, w, h));
        let software = crate::create_specific(EncoderChoice::Software, settings)
            .and_then(|mut e| measure(e.as_mut(), &frames, w, h));
        let choice = match (&hardware, &software) {
            (Ok(hw), Ok(sw)) if sw * SOFTWARE_MARGIN < *hw => EncoderChoice::Software,
            (Ok(_), _) => EncoderChoice::Hardware,
            (Err(_), _) => EncoderChoice::Software,
        };
        let fmt = |r: &anyhow::Result<f64>| match r {
            Ok(us) => format!("{:.1} ms", us / 1000.0),
            Err(e) => format!("non disponibile ({e:#})"),
        };
        info!("prova encoder 1080p: hardware {}, software {} → {:?}", fmt(&hardware), fmt(&software), choice);
        choice
    })
}
