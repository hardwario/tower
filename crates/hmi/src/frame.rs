//! A 1-bit framebuffer with per-row dirty tracking.
//!
//! Rows of `STRIDE` bytes, `H` rows; the most significant bit of a byte is its leftmost pixel and
//! a set bit is ink (black on a memory LCD), so a zeroed frame is all paper and a `static` frame
//! lives in `.bss` (no flash copy of its initial value). Every drawing operation that changes a
//! byte marks that row dirty; a display driver sends only the dirty rows (a line-addressed panel
//! like the Sharp memory LCD rewrites single lines) and clears the marks.

use crate::canvas::{Canvas, Rotation};

/// The most rows a [`Frame`] can track.
pub const MAX_ROWS: usize = 256;

/// A `STRIDE * 8` x `H` pixel framebuffer (see the [module docs](self)). `H` is at most
/// [`MAX_ROWS`].
pub struct Frame<const STRIDE: usize, const H: usize> {
    rows: [[u8; STRIDE]; H],
    dirty: [u32; MAX_ROWS / 32],
    rotation: Rotation,
}

impl<const STRIDE: usize, const H: usize> Frame<STRIDE, H> {
    /// Physical width in pixels.
    pub const WIDTH: i32 = (STRIDE * 8) as i32;
    /// Physical height in pixels.
    pub const HEIGHT: i32 = H as i32;

    /// All paper, nothing dirty, unrotated (all zero bytes: a `static` frame costs no flash).
    pub const fn new() -> Self {
        assert!(H <= MAX_ROWS, "Frame: too many rows");
        Self {
            rows: [[0; STRIDE]; H],
            dirty: [0; MAX_ROWS / 32],
            rotation: Rotation::R0,
        }
    }

    /// A canvas drawing into this frame in its current [`rotation`](Self::rotation).
    pub fn canvas(&mut self) -> Canvas<'_> {
        Canvas::new(
            self.rows.as_flattened_mut(),
            &mut self.dirty,
            STRIDE,
            H,
            self.rotation,
        )
    }

    /// How canvases map logical to physical coordinates. Changing it does not move what is
    /// already drawn: redraw after a change.
    pub fn set_rotation(&mut self, rotation: Rotation) {
        self.rotation = rotation;
    }

    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    /// The bytes of physical row `y`.
    pub fn row(&self, y: usize) -> &[u8; STRIDE] {
        &self.rows[y]
    }

    pub fn is_dirty(&self, y: usize) -> bool {
        self.dirty[y / 32] & (1 << (y % 32)) != 0
    }

    pub fn any_dirty(&self) -> bool {
        self.dirty.iter().any(|w| *w != 0)
    }

    /// Mark row `y` changed (a driver that lost the panel's state marks everything).
    pub fn mark_dirty(&mut self, y: usize) {
        self.dirty[y / 32] |= 1 << (y % 32);
    }

    pub fn mark_all_dirty(&mut self) {
        for y in 0..H {
            self.mark_dirty(y);
        }
    }

    pub fn clear_dirty(&mut self, y: usize) {
        self.dirty[y / 32] &= !(1 << (y % 32));
    }

    pub fn clear_all_dirty(&mut self) {
        self.dirty = [0; MAX_ROWS / 32];
    }

    /// The dirty rows, top to bottom.
    pub fn dirty_rows(&self) -> impl Iterator<Item = usize> + '_ {
        (0..H).filter(|&y| self.is_dirty(y))
    }
}

impl<const STRIDE: usize, const H: usize> Default for Frame<STRIDE, H> {
    fn default() -> Self {
        Self::new()
    }
}

/// FNV-1a hash of a row, the signature a driver keeps per row of what the panel shows, so a row
/// that was redrawn to the very same pixels (clear, then draw the screen again) is not resent.
pub fn row_signature(row: &[u8]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &b in row {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Color;
    use crate::geom::Rect;

    #[test]
    fn dirty_marks_only_real_changes() {
        let mut f: Frame<2, 4> = Frame::new();
        assert!(!f.any_dirty());
        f.canvas().pixel(3, 1, Color::White); // already paper: no change
        assert!(!f.any_dirty());
        f.canvas().pixel(3, 1, Color::Black);
        assert_eq!(f.dirty_rows().collect::<std::vec::Vec<_>>(), [1]);
        assert_eq!(f.row(1), &[0b0001_0000, 0]);
        f.clear_all_dirty();
        f.canvas().fill_rect(Rect::new(0, 0, 16, 4), Color::Black);
        assert_eq!(f.dirty_rows().count(), 4);
    }

    #[test]
    fn signature_differs() {
        assert_ne!(
            row_signature(&[0; 16]),
            row_signature(&[0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        );
        assert_eq!(row_signature(&[]), 0x811C_9DC5);
    }
}
