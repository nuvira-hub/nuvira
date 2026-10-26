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
                #[cfg(windows)]
                texture: None,
            },
            interval: Duration::from_secs(1) / fps.max(1),
            next: Instant::now(),
            tick: 0,
        }
    }

    /// Colore dello sfondo fisso nel punto (x, y).
    fn background(x: usize, y: usize) -> [u8; 4] {
        [(x % 256) as u8, (y % 256) as u8, 96, 255]
    }

    /// Ridisegna le colonne `from..to` con lo sfondo o con la barra.
    fn paint_columns(&mut self, from: usize, to: usize, bar: bool) {
        let w = self.frame.width as usize;
        let (from, to) = (from.min(w), to.min(w));
        for y in 0..self.frame.height as usize {
            let row = &mut self.frame.data[y * self.frame.stride..][..w * 4];
            for x in from..to {
                let c = if bar { [255, 255, 255, 255] } else { Self::background(x, y) };
                row[x * 4..x * 4 + 4].copy_from_slice(&c);
            }
        }
    }

    /// Sfondo fisso e una barra verticale che lo attraversa: come una finestra
    /// trascinata su un desktop, cambia solo una piccola parte dell'immagine.
    fn draw(&mut self) {
        const BAR: usize = 24;
        let w = self.frame.width as usize;
        if self.tick == 1 {
            self.paint_columns(0, w, false);
        } else {
            let old = ((self.tick as usize - 1) * 8) % w;
            self.paint_columns(old, old + BAR, false);
        }
        let bar = (self.tick as usize * 8) % w;
        self.paint_columns(bar, bar + BAR, true);
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
