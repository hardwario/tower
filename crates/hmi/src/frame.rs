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

    /// All rows' bytes, top to bottom (a snapshot to turn with [`draw_turned`](Self::draw_turned)).
    pub fn bytes(&self) -> &[u8] {
        self.rows.as_flattened()
    }

    /// Draw `src` (the [`bytes`](Self::bytes) of a frame of the same size) turned by `angle`
    /// ([`turn`](crate::turn) units, clockwise on the panel) and scaled by `scale` (Q16) about the
    /// center, replacing the whole picture; what falls outside `src` is paper. One step of a
    /// rotation animation. At a multiple of 90 degrees and a scale of 1.0 it is exact: the same
    /// pixels as [`turn_quarters`](Self::turn_quarters), the same as drawing in the rotation that
    /// many quarters on.
    ///
    /// Inverse mapping with nearest-neighbour sampling, integer only: about 20 cycles per pixel.
    pub fn draw_turned(&mut self, src: &[u8], angle: i32, scale: i32) {
        assert!(
            src.len() == STRIDE * H,
            "Frame::draw_turned: source of another size"
        );
        let (w, h) = (Self::WIDTH, Self::HEIGHT);
        let inv = (1i64 << 32) / scale.max(1) as i64;
        let c = (crate::turn::cos(angle) as i64 * inv) >> 16;
        let s = (crate::turn::sin(angle) as i64 * inv) >> 16;
        // Pixel centers relative to the frame center, Q16: (2i + 1 - size) / 2.
        let (cx, cy) = ((w as i64) << 15, (h as i64) << 15);
        let dx0 = (1i64 << 15) - cx;
        for y in 0..H {
            let dy = ((2 * y as i64 + 1) << 15) - cy;
            // The source point of each destination pixel: the destination turned back by `angle`.
            let mut sx = (cx + ((c * dx0 + s * dy) >> 16)) as i32;
            let mut sy = (cy + ((-s * dx0 + c * dy) >> 16)) as i32;
            let (step_x, step_y) = (c as i32, -s as i32);
            let mut changed = false;
            for b in 0..STRIDE {
                let mut v = 0u8;
                for bit in 0..8 {
                    let (px, py) = (sx >> 16, sy >> 16);
                    if px >= 0
                        && py >= 0
                        && px < w
                        && py < h
                        && src[py as usize * STRIDE + (px >> 3) as usize] & (0x80 >> (px & 7)) != 0
                    {
                        v |= 0x80 >> bit;
                    }
                    sx += step_x;
                    sy += step_y;
                }
                if self.rows[y][b] != v {
                    self.rows[y][b] = v;
                    changed = true;
                }
            }
            if changed {
                self.mark_dirty(y);
            }
        }
    }

    /// Turn the picture by `quarters` x 90 degrees clockwise on the panel, in place (no second
    /// buffer): what changing the rotation by that many quarters does to a picture drawn before.
    /// Square frames only (a non-square one is left as it is).
    pub fn turn_quarters(&mut self, quarters: i32) {
        let q = quarters.rem_euclid(4) as usize;
        let n = H;
        if q == 0 || Self::WIDTH as usize != n {
            return;
        }
        // The pixel a quarter turn brings to (x, y) comes from (y, n - 1 - x). Each pixel's orbit
        // under that map has four points, one in the top left quadrant: walk those.
        let from = |(x, y): (usize, usize)| (y, n - 1 - x);
        for y in 0..n / 2 {
            for x in 0..n / 2 {
                let p = [(x, y), from((x, y)), from(from((x, y))), from(from(from((x, y))))];
                let v = p.map(|(px, py)| self.rows[py][px / 8] & (0x80 >> (px % 8)) != 0);
                for i in 0..4 {
                    let (px, py) = p[i];
                    let m = 0x80 >> (px % 8);
                    if v[(i + q) % 4] {
                        self.rows[py][px / 8] |= m;
                    } else {
                        self.rows[py][px / 8] &= !m;
                    }
                }
            }
        }
        self.mark_all_dirty();
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
