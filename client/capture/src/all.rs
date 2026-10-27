//! All the screens at once: one capturer per screen, composed into a single image
//! laid out as in the virtual desktop (gaps between screens of different sizes are
//! black). The image goes through RAM: the screens may be on different graphics
//! cards, and composing on the CPU costs a copy of each changed screen.

use std::time::Duration;

use anyhow::{Result, bail};

use crate::{Capturer, Display, Frame};

/// Index of the "all screens" pseudo-display.
pub const ALL_DISPLAYS: usize = 0xFFFF;

pub struct AllCapturer {
    parts: Vec<Box<dyn Capturer>>,
    display: Display,
    frame: Frame,
}

impl AllCapturer {
    pub fn new(open: impl Fn(usize) -> Result<Box<dyn Capturer>>, screens: &[Display]) -> Result<Self> {
        if screens.len() < 2 {
            bail!("only one screen");
        }
        let left = screens.iter().map(|d| d.left).min().unwrap_or(0);
        let top = screens.iter().map(|d| d.top).min().unwrap_or(0);
        let right = screens.iter().map(|d| d.left + d.width as i32).max().unwrap_or(0);
        let bottom = screens.iter().map(|d| d.top + d.height as i32).max().unwrap_or(0);
        // Even sizes: the encoders work on 2x2 chroma blocks.
        let (width, height) = (((right - left) as u32 + 1) & !1, ((bottom - top) as u32 + 1) & !1);
        let parts = screens.iter().map(|d| open(d.index)).collect::<Result<Vec<_>>>()?;
        let display = Display {
            index: ALL_DISPLAYS,
            name: "all screens".into(),
            left,
            top,
            width,
            height,
            primary: false,
        };
        let stride = width as usize * 4;
        let frame = Frame {
            width,
            height,
            stride,
            data: vec![0; stride * height as usize],
            captured_us: 0,
            #[cfg(windows)]
            texture: None,
        };
        Ok(Self { parts, display, frame })
    }
}

impl Capturer for AllCapturer {
    fn display(&self) -> &Display {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<&Frame>> {
        // The wait is shared between the screens, so that a change on any of them is
        // seen within the timeout.
        let share = timeout / self.parts.len() as u32;
        let mut changed = false;
        for part in &mut self.parts {
            let (left, top) = (part.display().left, part.display().top);
            let Some(image) = part.next_frame(share)? else { continue };
            let (x, y) = ((left - self.display.left) as usize, (top - self.display.top) as usize);
            let row = (image.width as usize * 4).min(self.frame.stride - x * 4);
            let rows = (image.height as usize).min(self.frame.height as usize - y);
            for r in 0..rows {
                let src = &image.data[r * image.stride..][..row];
                let start = (y + r) * self.frame.stride + x * 4;
                self.frame.data[start..start + row].copy_from_slice(src);
            }
            self.frame.captured_us = image.captured_us;
            changed = true;
        }
        Ok(changed.then_some(&self.frame))
    }

    fn last_frame(&self) -> Option<&Frame> {
        (self.frame.captured_us != 0).then_some(&self.frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntheticCapturer;

    #[test]
    fn screens_side_by_side() {
        // A 64x32 screen and, to its right, a 32x16 one: 96x32 with a black corner.
        let screens = [
            Display { index: 0, name: "a".into(), left: 0, top: 0, width: 64, height: 32, primary: true },
            Display { index: 1, name: "b".into(), left: 64, top: 0, width: 32, height: 16, primary: false },
        ];
        let open = |i: usize| -> Result<Box<dyn Capturer>> {
            let d = &screens[i];
            let mut c = SyntheticCapturer::new(d.width, d.height, 1000);
            c.set_position(d.left, d.top, i);
            Ok(Box::new(c))
        };
        let mut all = AllCapturer::new(open, &screens).expect("all");
        assert_eq!((all.display().width, all.display().height, all.display().index), (96, 32, ALL_DISPLAYS));
        let frame = all.next_frame(Duration::from_millis(50)).expect("frame").expect("changed");
        // Bottom-right corner, under the small screen: black.
        let px = |x: usize, y: usize| &frame.data[y * frame.stride + x * 4..][..4];
        assert_eq!(px(90, 30), [0, 0, 0, 0]);
        // Inside each screen something was drawn.
        assert!((0..32).any(|y| px(10, y) != [0, 0, 0, 0]));
        assert!((0..16).any(|y| px(70, y) != [0, 0, 0, 0]));
    }
}
