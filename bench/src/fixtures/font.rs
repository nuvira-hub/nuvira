//! Bench pseudo-font: 8x16 glyphs built from a fixed table of 1-pixel strokes.
//!
//! The glyphs are not real letters, but they have what matters to a video encoder
//! about text: thin 1 px horizontal, vertical and diagonal strokes with sharp edges
//! on a flat background, a baseline, descenders and dots. Which strokes a character
//! has comes from a fixed hash of its code, so the font never changes unless
//! `GENERATOR_VERSION` does.

use std::sync::OnceLock;

pub const GLYPH_W: usize = 8;
pub const GLYPH_H: usize = 16;

/// Strokes inside the 8x16 cell: (x0, y0, x1, y1), inclusive, 1 px wide.
const STROKES: [(i32, i32, i32, i32); 12] = [
    (1, 4, 6, 4),   // top
    (1, 8, 6, 8),   // middle
    (1, 12, 6, 12), // bottom (baseline)
    (1, 4, 1, 8),   // left upper
    (1, 8, 1, 12),  // left lower
    (6, 4, 6, 8),   // right upper
    (6, 8, 6, 12),  // right lower
    (3, 4, 3, 12),  // centre
    (1, 12, 6, 4),  // diagonal /
    (1, 4, 6, 12),  // diagonal \
    (6, 12, 6, 15), // descender
    (3, 1, 3, 2),   // dot / ascender mark
];

const FONT_SEED: u64 = 0x666f_6e74_2d38_7831; // "font-8x1"

/// Rows of every glyph for ASCII 0..128, bit 7 = leftmost pixel.
pub fn glyphs() -> &'static [[u8; GLYPH_H]; 128] {
    static G: OnceLock<[[u8; GLYPH_H]; 128]> = OnceLock::new();
    G.get_or_init(|| {
        let mut out = [[0u8; GLYPH_H]; 128];
        for (c, glyph) in out.iter_mut().enumerate() {
            if c <= 32 || c == 127 {
                continue; // control characters and space are blank
            }
            let mut mask = crate::rng::mix64(FONT_SEED ^ c as u64) as u32 & 0x0fff;
            // At least three strokes, so every glyph is visible and distinct enough.
            let mut k = 0;
            while mask.count_ones() < 3 {
                mask |= 1 << ((c + 5 * k) % 12);
                k += 1;
            }
            // Descender and dot only on some characters (as g, j, p, q, i).
            if c % 5 != 0 {
                mask &= !(1 << 10);
            }
            if c % 7 != 0 {
                mask &= !(1 << 11);
            }
            for (i, &(x0, y0, x1, y1)) in STROKES.iter().enumerate() {
                if mask & (1 << i) == 0 {
                    continue;
                }
                let steps = (x1 - x0).abs().max((y1 - y0).abs());
                for s in 0..=steps {
                    // Integer line: exact and identical everywhere.
                    let x = x0 + (x1 - x0) * s / steps.max(1);
                    let y = y0 + (y1 - y0) * s / steps.max(1);
                    glyph[y as usize] |= 0x80 >> x;
                }
            }
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_glyphs_are_visible_and_space_is_blank() {
        let g = glyphs();
        assert!(g[b' ' as usize].iter().all(|&r| r == 0));
        for (c, glyph) in g.iter().enumerate().take(127).skip(33) {
            let px: u32 = glyph.iter().map(|r| r.count_ones()).sum();
            assert!(px >= 6, "glyph {c} has {px} pixels");
        }
        let distinct: std::collections::HashSet<_> = (33..127).map(|c| g[c]).collect();
        assert!(distinct.len() > 60, "{}", distinct.len());
    }
}
