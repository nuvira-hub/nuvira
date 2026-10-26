//! Sorgente sintetica: un'immagine animata generata in memoria.
//!
//! Serve a collaudare compressione, rete e visualizzazione senza uno schermo
//! catturabile (PC bloccato, macchine virtuali senza GPU, test automatici).

use std::time::{Duration, Instant};

use crate::{Capturer, Display, Frame};

pub struct SyntheticCapturer {
    display: Display,
    frame: Frame,
    interval: Duration,
    next: Instant,
    tick: u32,
}

impl SyntheticCapturer {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            display: Display {
                index: usize::MAX,
                name: "sintetico".into(),
                left: 0,
                top: 0,
                width,
                height,
                primary: true,
            },
            frame: Frame {
                width,
                height,
                stride: width as usize * 4,
                data: vec![0; width as usize * height as usize * 4],
                captured_us: 0,
            },
            interval: Duration::from_secs(1) / fps.max(1),
            next: Instant::now(),
            tick: 0,
        }
    }

    fn draw(&mut self) {
        let (w, h) = (self.frame.width as usize, self.frame.height as usize);
        let t = self.tick as usize;
        // Sfondo a gradiente che scorre + una barra verticale che attraversa lo schermo.
        let bar = (t * 8) % w;
        for y in 0..h {
            let row = &mut self.frame.data[y * self.frame.stride..][..w * 4];
            for (x, px) in row.chunks_exact_mut(4).enumerate() {
                let on_bar = x >= bar && x < bar + 24;
                px[0] = if on_bar { 255 } else { ((x + t) % 256) as u8 };
                px[1] = if on_bar { 255 } else { ((y + t / 2) % 256) as u8 };
                px[2] = if on_bar { 255 } else { 96 };
                px[3] = 255;
            }
        }
    }
}

impl Capturer for SyntheticCapturer {
    fn display(&self) -> &Display {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<&Frame>> {
        let now = Instant::now();
        if self.next > now {
            let wait = self.next - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next += self.interval;
        // Se siamo molto in ritardo non recuperiamo i frame persi.
        if self.next < Instant::now() {
            self.next = Instant::now() + self.interval;
        }
        self.tick = self.tick.wrapping_add(1);
        self.draw();
        self.frame.captured_us = nexo_proto::now_us();
        Ok(Some(&self.frame))
    }

    fn last_frame(&self) -> Option<&Frame> {
        (self.tick > 0).then_some(&self.frame)
    }
}
