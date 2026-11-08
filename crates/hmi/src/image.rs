//! 1-bit images (icons, logos) and the [`image!`](macro@crate::image) macro that builds one from ASCII
//! art at compile time.
//!
//! The pixel format is the glyph format of [`font`](crate::font): rows of `ceil(width / 8)`
//! bytes, the most significant bit the leftmost pixel, a set bit ink. That is the "horizontal,
//! 1 bit per pixel, MSB first" output of the common bitmap converters (image2cpp, LCD Assistant),
//! so a converted logo drops in as [`Image::new`] over its bytes.
//!
//! ```
//! use tower_hmi::{image, image::Image};
//!
//! const BELL: Image = image!(
//!     "...##...",
//!     "..####..",
//!     ".######.",
//!     ".######.",
//!     "########",
//!     "...##...",
//! );
//! assert_eq!((BELL.width, BELL.height), (8, 6));
//! assert!(BELL.pixel(3, 0) && !BELL.pixel(0, 0));
//! ```

/// A 1-bit image in flash.
#[derive(Clone, Copy)]
pub struct Image {
    pub width: u16,
    pub height: u16,
    data: &'static [u8],
}

impl Image {
    /// An image over `data`: `height` rows of `ceil(width / 8)` bytes, MSB first, set bit = ink.
    pub const fn new(width: u16, height: u16, data: &'static [u8]) -> Self {
        assert!(
            data.len() == height as usize * (width as usize).div_ceil(8),
            "Image::new: data length does not match the size"
        );
        Self { width, height, data }
    }

    /// Bytes per row.
    pub const fn stride(&self) -> usize {
        (self.width as usize).div_ceil(8)
    }

    /// The packed bytes of row `y`.
    pub fn row(&self, y: usize) -> &'static [u8] {
        let s = self.stride();
        &self.data[y * s..(y + 1) * s]
    }

    /// Whether pixel `(x, y)` is ink (`false` outside the image).
    pub fn pixel(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return false;
        }
        self.row(y as usize)[x as usize / 8] & (0x80 >> (x % 8)) != 0
    }
}

/// Pack ASCII-art rows into image bytes: `#`, `X`, `@`, `*`, `O` and `1` are ink; `.`, space,
/// `_`, `-` and `0` are paper. Every row must be as long as the first; `N` must be
/// `rows.len() * ceil(width / 8)`. Any other character, a ragged row or a wrong `N` fails the
/// build when evaluated in a constant (the [`image!`](macro@crate::image) macro does so).
pub const fn pack<const N: usize>(rows: &[&str]) -> [u8; N] {
    assert!(!rows.is_empty(), "image!: no rows");
    let width = rows[0].len();
    let stride = width.div_ceil(8);
    assert!(N == rows.len() * stride, "image!: wrong buffer size");
    let mut out = [0u8; N];
    let mut y = 0;
    while y < rows.len() {
        let row = rows[y].as_bytes();
        assert!(
            row.len() == width,
            "image!: every row must have the same length (ASCII only)"
        );
        let mut x = 0;
        while x < width {
            let ink = match row[x] {
                b'#' | b'X' | b'@' | b'*' | b'O' | b'1' => true,
                b'.' | b' ' | b'_' | b'-' | b'0' => false,
                _ => panic!("image!: use # X @ * O 1 for ink and . space _ - 0 for paper"),
            };
            if ink {
                out[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
            x += 1;
        }
        y += 1;
    }
    out
}

/// Build an [`Image`](crate::image::Image) from ASCII-art string rows at compile time (see the
/// [module docs](mod@crate::image)). Usable in a `const` or `static` initializer.
#[macro_export]
macro_rules! image {
    ($($row:literal),+ $(,)?) => {{
        const ROWS: &[&str] = &[$($row),+];
        const W: usize = ROWS[0].len();
        const N: usize = ROWS.len() * W.div_ceil(8);
        const DATA: [u8; N] = $crate::image::pack::<N>(ROWS);
        $crate::image::Image::new(W as u16, ROWS.len() as u16, &DATA)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARROW: Image = crate::image!("#........#", ".#......#.", "..######..");

    #[test]
    fn pack_rows() {
        assert_eq!((ARROW.width, ARROW.height, ARROW.stride()), (10, 3, 2));
        assert_eq!(ARROW.row(0), &[0b1000_0000, 0b0100_0000]);
        assert_eq!(ARROW.row(1), &[0b0100_0000, 0b1000_0000]);
        assert_eq!(ARROW.row(2), &[0b0011_1111, 0b0000_0000]);
        assert!(ARROW.pixel(9, 0) && !ARROW.pixel(8, 0) && !ARROW.pixel(10, 0) && !ARROW.pixel(-1, 0));
    }
}
