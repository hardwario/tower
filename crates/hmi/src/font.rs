//! Monospaced bitmap fonts.
//!
//! A [`Font`] is a cell size plus a list of [`Block`]s, each a set of code points with their
//! glyph bitmaps. Every glyph is a full cell: `height` rows of [`Font::stride`] bytes, the most
//! significant bit the leftmost pixel, a set bit ink. Splitting the glyphs into blocks lets an
//! application pay flash only for the characters it uses: the linker keeps a block only when a
//! font of the application lists it. The [`fonts`](crate::fonts) module holds the Spleen sizes
//! and their blocks; compose a font of your own from them with the size module's `with`:
//!
//! ```
//! use tower_hmi::font::Font;
//! use tower_hmi::fonts::spleen_8x16;
//!
//! // ASCII plus the Central European letters (Czech, Slovak, Polish, ...).
//! static TEXT: Font = spleen_8x16::with(&[&spleen_8x16::ASCII, &spleen_8x16::LATIN1, &spleen_8x16::LATIN_EXT_A]);
//! assert!(TEXT.glyph('ř').is_some());
//! ```

/// One set of glyphs of a font: a run of consecutive code points ([`dense`](Self::dense)) or a
/// sorted table of them ([`sparse`](Self::sparse)), and their bitmaps in the same order.
pub struct Block {
    first: u32,
    count: u16,
    codes: &'static [u16],
    data: &'static [u8],
}

impl Block {
    /// The `count` code points `first..first + count`, all present.
    pub const fn dense(first: u32, count: u16, data: &'static [u8]) -> Self {
        Self {
            first,
            count,
            codes: &[],
            data,
        }
    }

    /// The code points of `codes` (sorted ascending, all in the Basic Multilingual Plane).
    pub const fn sparse(codes: &'static [u16], data: &'static [u8]) -> Self {
        let mut i = 1;
        while i < codes.len() {
            assert!(
                codes[i - 1] < codes[i],
                "Block::sparse: codes must be sorted and unique"
            );
            i += 1;
        }
        Self {
            first: 0,
            count: codes.len() as u16,
            codes,
            data,
        }
    }

    /// Number of glyphs.
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Index of `cp` among this block's glyphs.
    fn index(&self, cp: u32) -> Option<usize> {
        if self.codes.is_empty() {
            let i = cp.checked_sub(self.first)?;
            (i < self.count as u32).then_some(i as usize)
        } else {
            let cp = u16::try_from(cp).ok()?;
            self.codes.binary_search(&cp).ok()
        }
    }
}

/// A monospaced bitmap font: every glyph is `width` x `height` pixels and advances the pen by
/// `width`.
pub struct Font {
    /// Cell width in pixels (the advance).
    pub width: u8,
    /// Cell height in pixels (the line height).
    pub height: u8,
    /// Rows from the top of the cell to the baseline.
    pub baseline: u8,
    blocks: &'static [&'static Block],
}

impl Font {
    /// A font of `width` x `height` cells over `blocks` (looked up in this order). Checks at
    /// compile time (in a `static`) that every block's bitmap data holds exactly its glyphs.
    pub const fn new(width: u8, height: u8, baseline: u8, blocks: &'static [&'static Block]) -> Self {
        let glyph = height as usize * (width as usize).div_ceil(8);
        let mut i = 0;
        while i < blocks.len() {
            assert!(
                blocks[i].data.len() == blocks[i].count as usize * glyph,
                "Font::new: block size mismatch"
            );
            i += 1;
        }
        Self {
            width,
            height,
            baseline,
            blocks,
        }
    }

    /// Bytes per glyph row.
    pub const fn stride(&self) -> usize {
        (self.width as usize).div_ceil(8)
    }

    /// The bitmap of `ch` (`height` rows of [`stride`](Self::stride) bytes), or `None` when no
    /// block of this font has it.
    pub fn glyph(&self, ch: char) -> Option<&'static [u8]> {
        let size = self.height as usize * self.stride();
        let cp = ch as u32;
        self.blocks
            .iter()
            .find_map(|b| b.index(cp).map(|i| &b.data[i * size..(i + 1) * size]))
    }

    /// The bitmap of `ch`, else of `?`, else `None` (a font without `?` draws a blank cell).
    pub fn glyph_or_fallback(&self, ch: char) -> Option<&'static [u8]> {
        self.glyph(ch).or_else(|| self.glyph('?'))
    }

    /// Width in pixels of the widest line of `text` (lines split at `\n`).
    pub fn text_width(&self, text: &str) -> i32 {
        text.split('\n')
            .map(|l| l.chars().count() as i32 * self.width as i32)
            .max()
            .unwrap_or(0)
    }

    /// Size in pixels of `text`, `(width of the widest line, lines x height)`.
    pub fn text_size(&self, text: &str) -> (i32, i32) {
        let lines = text.split('\n').count() as i32;
        (self.text_width(text), lines * self.height as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static DATA: [u8; 3] = [1, 2, 3];
    static DENSE: Block = Block::dense(0x41, 3, &DATA);
    static SPARSE_CODES: [u16; 2] = [0x20AC, 0x2190];
    static SPARSE_DATA: [u8; 2] = [7, 8];
    static SPARSE: Block = Block::sparse(&SPARSE_CODES, &SPARSE_DATA);
    // 1 x 1 pixel cells: one byte per glyph.
    static TINY: Font = Font::new(1, 1, 1, &[&DENSE, &SPARSE]);

    #[test]
    fn lookup() {
        assert_eq!(TINY.glyph('A'), Some(&[1u8][..]));
        assert_eq!(TINY.glyph('C'), Some(&[3u8][..]));
        assert_eq!(TINY.glyph('D'), None);
        assert_eq!(TINY.glyph('@'), None);
        assert_eq!(TINY.glyph('€'), Some(&[7u8][..]));
        assert_eq!(TINY.glyph('←'), Some(&[8u8][..]));
        assert_eq!(TINY.glyph('→'), None);
        assert_eq!(TINY.glyph('😀'), None);
        assert_eq!(TINY.glyph_or_fallback('Z'), None); // no '?' in this font
    }

    #[test]
    fn measure() {
        assert_eq!(TINY.text_width("ABC"), 3);
        assert_eq!(TINY.text_size("AB\nABCD\n"), (4, 3));
        assert_eq!(TINY.text_width("ří"), 2); // chars, not bytes
    }
}
