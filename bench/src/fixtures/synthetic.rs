//! Reference of the agent's synthetic source (plan 5.2).
//!
//! `client/capture/src/synthetic.rs` at the tag draws a background
//! `[(x % 256), (y % 256), 96, 255]` (BGRA) and a 24 px white bar at
//! `x = (tick * 8) % width`, clipped at the right edge. The bench copies that formula
//! here as its reference, so a real session on the synthetic source can be scored:
//! the bar is located in the decoded luma ([`locate_bar`]) and the reference frame
//! is rebuilt from its position ([`frame_with_bar`]). If Phase 6 changes the
//! synthetic source, the reference PSNR on the baseline build drops and the case
//! reports `invalid_reference` instead of a regression.

pub const BAR_WIDTH: u32 = 24;
pub const BAR_STEP: u32 = 8;

/// Bar position at capture tick `tick` (the first captured frame has tick 1).
pub fn bar_x(tick: u32, width: u32) -> u32 {
    ((tick as u64 * BAR_STEP as u64) % width.max(1) as u64) as u32
}

/// BGRA frame with the bar at column `bar` (stride = width * 4).
pub fn frame_with_bar(width: u32, height: u32, bar: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![0u8; w * h * 4];
    let (b0, b1) = (bar as usize, (bar + BAR_WIDTH).min(width) as usize);
    for y in 0..h {
        let row = &mut data[y * w * 4..][..w * 4];
        for x in 0..w {
            let c = if (b0..b1).contains(&x) {
                [255, 255, 255, 255]
            } else {
                [(x % 256) as u8, (y % 256) as u8, 96, 255]
            };
            row[x * 4..x * 4 + 4].copy_from_slice(&c);
        }
    }
    data
}

/// The frame captured at `tick`.
pub fn frame_at_tick(width: u32, height: u32, tick: u32) -> Vec<u8> {
    frame_with_bar(width, height, bar_x(tick, width))
}

/// Column where the white bar starts in a decoded luma plane (`stride` bytes per
/// row), or None when it cannot be located (the frame counts as `unaligned`).
///
/// The background's luma is at most ~195 (BT.601 limited range); white is 235. A
/// column belongs to the bar when its mean luma over sampled rows is above 220.
pub fn locate_bar(y_plane: &[u8], width: u32, height: u32, stride: usize) -> Option<u32> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || y_plane.len() < stride * (h - 1) + w {
        return None;
    }
    let rows: Vec<usize> = (0..16).map(|k| (k * 2 + 1) * h / 33).collect();
    let white: Vec<bool> = (0..w)
        .map(|x| {
            let sum: u32 = rows.iter().map(|&y| y_plane[y * stride + x] as u32).sum();
            sum / rows.len() as u32 > 220
        })
        .collect();
    // Longest run of white columns; the bar is 24 px (fewer at the right edge).
    let (mut best, mut best_len, mut start, mut len) = (0usize, 0usize, 0usize, 0usize);
    for (x, &is) in white.iter().enumerate() {
        if is {
            if len == 0 {
                start = x;
            }
            len += 1;
            if len > best_len {
                best = start;
                best_len = len;
            }
        } else {
            len = 0;
        }
    }
    let at_edge = best + best_len == w;
    let ok =
        (BAR_WIDTH as usize - 4..=BAR_WIDTH as usize + 4).contains(&best_len) || (at_edge && best_len >= 2);
    ok.then_some(best as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_is_found_where_drawn() {
        let (w, h) = (640u32, 360u32);
        for tick in [1u32, 7, 33, 79, 200] {
            let bgra = frame_at_tick(w, h, tick);
            let img = nuvira_codec::BgraImage { width: w, height: h, stride: w as usize * 4, data: &bgra };
            let r = crate::quality::Reference::from_bgra(&img);
            let y: Vec<u8> = r.y.iter().map(|v| v.round() as u8).collect();
            assert_eq!(locate_bar(&y, w, h, w as usize), Some(bar_x(tick, w)), "tick {tick}");
        }
    }
}
