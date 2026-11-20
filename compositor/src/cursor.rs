// SPDX-License-Identifier: GPL-3.0-or-later
//! The built-in default cursor (Stage 3).
//!
//! Worker 1 renders only a client-set cursor surface; when no client
//! sets one, nothing is drawn and the pointer is invisible. This module
//! is the fix: a small embedded arrow bitmap, built once at startup
//! into a [`MemoryRenderBuffer`], drawn whenever there is no
//! client-set cursor surface and the cursor is not hidden.
//!
//! The arrow is 16x16 logical pixels, white fill with a 1 px black
//! outline (the outline is the fill dilated by one pixel), hotspot
//! (0, 0) — the tip. There is no cursor theme system: every named
//! shape a client requests via `cursor-shape` renders as this same
//! arrow (see `server.rs`, `cursor_image`), which is named in the
//! README as a limitation, not hidden.
//!
//! The element is created with [`Kind::Cursor`], so the DRM
//! compositor considers it for the hardware cursor plane when it
//! fits the device's cursor size (16x16 always fits a real cursor
//! plane); otherwise it is composited by the renderer (the GL
//! fallback). That is the plan's "hardware cursor plane when
//! available, GL cursor fallback" for the no-client-cursor case.

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::{Size, Transform},
};

/// The default cursor's size in logical pixels (square).
pub const CURSOR_SIZE: i32 = 16;

/// The arrow's fill, one inclusive x-range per row, plus the diagonal
/// tail. Rows 0–8 are the arrowhead (a right triangle: tip at (0, 0),
/// widening down-right); rows 9–15 are the 2 px wide diagonal tail
/// running down-right from the head. Row 8 spans x 0–8 and the tail
/// starts at x 5–6 on row 9, so head and tail are one connected shape.
const FILL_ROWS: &[(i32, i32)] = &[
    (0, 0),   // row 0
    (0, 1),   // row 1
    (0, 2),   // row 2
    (0, 3),   // row 3
    (0, 4),   // row 4
    (0, 5),   // row 5
    (0, 6),   // row 6
    (0, 7),   // row 7
    (0, 8),   // row 8: head base
    (5, 6),   // row 9
    (6, 7),   // row 10
    (7, 8),   // row 11
    (8, 9),   // row 12
    (9, 10),  // row 13
    (10, 11), // row 14
    (11, 12), // row 15
];

fn is_fill(x: i32, y: i32) -> bool {
    if !(0..CURSOR_SIZE).contains(&y) || !(0..CURSOR_SIZE).contains(&x) {
        return false;
    }
    let (start, end) = FILL_ROWS[y as usize];
    (start..=end).contains(&x)
}

/// The cursor pixels as ARGB8888 bytes (`[B, G, R, A]` per pixel in
/// memory order on little-endian): white fill, black 1 px outline
/// (any transparent pixel 8-adjacent to a fill pixel), transparent
/// elsewhere.
pub fn default_cursor_pixels() -> Vec<u8> {
    let mut pixels = Vec::with_capacity((CURSOR_SIZE * CURSOR_SIZE * 4) as usize);
    for y in 0..CURSOR_SIZE {
        for x in 0..CURSOR_SIZE {
            if is_fill(x, y) {
                pixels.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                let outlined = (-1..=1)
                    .any(|dy| (-1..=1).any(|dx| (dx != 0 || dy != 0) && is_fill(x + dx, y + dy)));
                if outlined {
                    pixels.extend_from_slice(&[0, 0, 0, 255]);
                } else {
                    pixels.extend_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
    }
    pixels
}

/// Build the default cursor's render buffer (once, at startup; the
/// buffer is cloned into the state and re-used every frame — the
/// texture is imported once per element creation, not re-decoded).
pub fn build_default_cursor_buffer() -> MemoryRenderBuffer {
    let pixels = default_cursor_pixels();
    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        Size::from((CURSOR_SIZE, CURSOR_SIZE)),
        1,
        Transform::Normal,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_16x16_argb() {
        let pixels = default_cursor_pixels();
        assert_eq!(pixels.len(), 16 * 16 * 4);
    }

    #[test]
    fn cursor_tip_is_white_and_far_corner_is_transparent() {
        let pixels = default_cursor_pixels();
        let at = |x: i32, y: i32| -> [u8; 4] {
            let offset = ((y * CURSOR_SIZE + x) * 4) as usize;
            pixels[offset..offset + 4].try_into().unwrap()
        };
        // Tip of the arrow (hotspot): white, opaque.
        assert_eq!(at(0, 0), [255, 255, 255, 255]);
        // Inside the head: white.
        assert_eq!(at(2, 4), [255, 255, 255, 255]);
        // Just outside the head's edge: the black outline.
        assert_eq!(at(1, 0), [0, 0, 0, 255]);
        // Far corner: transparent.
        assert_eq!(at(15, 15), [0, 0, 0, 0]);
        // Tail pixel: white.
        assert_eq!(at(6, 10), [255, 255, 255, 255]);
    }
}
