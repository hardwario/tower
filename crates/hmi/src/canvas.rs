//! Drawing on a [`Frame`](crate::frame::Frame): primitives, text and images.
//!
//! A [`Canvas`] borrows a frame and draws in **logical** coordinates: `(0, 0)` is the top left of
//! the screen as the user sees it, after the frame's [`Rotation`]. Every operation is clipped to
//! the canvas's clip rectangle (the whole screen unless [`set_clip`](Canvas::set_clip) narrows
//! it) and marks only the rows whose bytes it actually changed, so redrawing identical content
//! costs a driver nothing to send.
//!
//! Fills, text and images take byte-wide fast paths on an unrotated frame; a rotated one falls
//! back to per-pixel work (correct, slower).
//!
//! ```
//! use tower_hmi::{Align, Color, Frame, Rect, TextStyle, fonts};
//!
//! let mut frame: Frame<16, 128> = Frame::new();
//! let mut c = frame.canvas();
//! let title = TextStyle::new(&fonts::SPLEEN_8X16).align(Align::Center).inverted();
//! c.fill_rect(Rect::new(0, 0, 128, 18), Color::Black);
//! c.text(64, 1, "TOWER", &title);
//! c.round_rect(Rect::new(4, 24, 120, 40), 6, Color::Black);
//! c.text_fmt(8, 36, format_args!("{:.1} °C", 23.4), &TextStyle::new(&fonts::SPLEEN_8X16));
//! ```

use core::fmt;

use crate::font::Font;
use crate::geom::Rect;
use crate::image::Image;

/// What a drawing operation does to the pixels it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// Ink (a set bit).
    Black,
    /// Paper (a clear bit).
    White,
    /// Flip each pixel (XOR): draws over any background and undoes itself when repeated.
    Invert,
}

/// How the canvas maps logical coordinates onto the panel, clockwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Rotation {
    #[default]
    R0 = 0,
    R90 = 1,
    R180 = 2,
    R270 = 3,
}

/// Horizontal text alignment relative to the `x` given to [`Canvas::text`]: the left edge, the
/// center, or the right edge (exclusive: the last pixel is at `x - 1`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// How [`Canvas::text`] renders: the font, the ink, an optional background filling each glyph
/// cell, and the alignment. Build one as a `const` and reuse it:
/// `const VALUE: TextStyle = TextStyle::new(&SPLEEN_8X16).align(Align::Right);`
#[derive(Clone, Copy)]
pub struct TextStyle {
    pub font: &'static Font,
    pub color: Color,
    /// Paint the cell pixels outside the glyph too (opaque text that overwrites what was there).
    pub background: Option<Color>,
    pub align: Align,
}

impl TextStyle {
    /// Black, transparent background, left-aligned.
    pub const fn new(font: &'static Font) -> Self {
        Self {
            font,
            color: Color::Black,
            background: None,
            align: Align::Left,
        }
    }

    pub const fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub const fn background(mut self, background: Color) -> Self {
        self.background = Some(background);
        self
    }

    /// Opaque black on white: overwrites the previous text in place.
    pub const fn opaque(self) -> Self {
        self.color(Color::Black).background(Color::White)
    }

    /// Opaque white on black.
    pub const fn inverted(self) -> Self {
        self.color(Color::White).background(Color::Black)
    }

    pub const fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }
}

/// Lines whose width [`Canvas::text_fmt`] measures for alignment; later lines are left-aligned.
const MAX_ALIGNED_LINES: usize = 8;

/// A drawing surface over a 1-bit frame buffer (see the [module docs](self)).
pub struct Canvas<'a> {
    buf: &'a mut [u8],
    dirty: &'a mut [u32],
    stride: usize,
    pw: i32,
    ph: i32,
    rot: Rotation,
    clip: Rect,
}

impl<'a> Canvas<'a> {
    /// A canvas over `buf` (`height` rows of `stride` bytes, MSB = leftmost pixel, set bit =
    /// ink) and `dirty` (one bit per row, at least `ceil(height / 32)` words). A
    /// [`Frame`](crate::frame::Frame) makes this for you.
    pub fn new(buf: &'a mut [u8], dirty: &'a mut [u32], stride: usize, height: usize, rot: Rotation) -> Self {
        assert!(buf.len() >= stride * height && dirty.len() * 32 >= height);
        let (pw, ph) = ((stride * 8) as i32, height as i32);
        let mut c = Self {
            buf,
            dirty,
            stride,
            pw,
            ph,
            rot,
            clip: Rect::default(),
        };
        c.clip = c.bounds();
        c
    }

    /// Logical width (after rotation).
    pub fn width(&self) -> i32 {
        if self.swapped() { self.ph } else { self.pw }
    }

    /// Logical height (after rotation).
    pub fn height(&self) -> i32 {
        if self.swapped() { self.pw } else { self.ph }
    }

    /// The whole screen, logical coordinates.
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width(), self.height())
    }

    pub fn rotation(&self) -> Rotation {
        self.rot
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Restrict drawing to `r` (within the screen) until [`reset_clip`](Self::reset_clip).
    pub fn set_clip(&mut self, r: Rect) {
        self.clip = r.intersect(&self.bounds());
    }

    pub fn reset_clip(&mut self) {
        self.clip = self.bounds();
    }

    /// Run `f` with drawing restricted to `r` (also within the current clip), then restore.
    pub fn with_clip<R>(&mut self, r: Rect, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.clip;
        self.clip = r.intersect(&saved);
        let out = f(self);
        self.clip = saved;
        out
    }

    /// Fill the clip area (the whole screen by default) with `color`.
    pub fn clear(&mut self, color: Color) {
        self.fill_rect(self.clip, color);
    }

    pub fn pixel(&mut self, x: i32, y: i32, color: Color) {
        if self.clip.contains(crate::geom::Point::new(x, y)) {
            let (px, py) = self.to_phys(x, y);
            if self.paint(
                py as usize * self.stride + (px >> 3) as usize,
                0x80 >> (px & 7),
                color,
            ) {
                self.mark(py);
            }
        }
    }

    /// Whether the pixel at `(x, y)` is ink (`false` outside the screen).
    pub fn get_pixel(&self, x: i32, y: i32) -> bool {
        if !self.bounds().contains(crate::geom::Point::new(x, y)) {
            return false;
        }
        let (px, py) = self.to_phys(x, y);
        self.buf[py as usize * self.stride + (px >> 3) as usize] & (0x80 >> (px & 7)) != 0
    }

    /// `w` pixels from `(x, y)` to the right.
    pub fn hline(&mut self, x: i32, y: i32, w: i32, color: Color) {
        self.fill_rect(Rect::new(x, y, w, 1), color);
    }

    /// `h` pixels from `(x, y)` down.
    pub fn vline(&mut self, x: i32, y: i32, h: i32, color: Color) {
        self.fill_rect(Rect::new(x, y, 1, h), color);
    }

    /// A one-pixel line from `(x0, y0)` to `(x1, y1)`, both ends included (Bresenham).
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: Color) {
        if y0 == y1 {
            return self.hline(x0.min(x1), y0, (x1 - x0).abs() + 1, color);
        }
        if x0 == x1 {
            return self.vline(x0, y0.min(y1), (y1 - y0).abs() + 1, color);
        }
        let (dx, sx) = ((x1 - x0).abs(), if x0 < x1 { 1 } else { -1 });
        let (dy, sy) = (-(y1 - y0).abs(), if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            self.pixel(x, y, color);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// The outline of `r`, one pixel wide.
    pub fn rect(&mut self, r: Rect, color: Color) {
        self.round_rect(r, 0, color);
    }

    pub fn fill_rect(&mut self, r: Rect, color: Color) {
        let r = r.intersect(&self.clip);
        if !r.is_empty() {
            let p = self.rect_to_phys(r);
            self.fill_phys(p, color);
        }
    }

    /// Flip every pixel of `r` (highlight a selection; doing it twice restores it).
    pub fn invert_rect(&mut self, r: Rect) {
        self.fill_rect(r, Color::Invert);
    }

    /// The outline of `r` with corners rounded to `radius` (clamped to half the shorter side).
    pub fn round_rect(&mut self, r: Rect, radius: i32, color: Color) {
        if r.is_empty() {
            return;
        }
        let rad = radius.clamp(0, (r.w.min(r.h) - 1) / 2);
        for i in 0..r.h {
            let (l, rt) = rounded_span(r, rad, i);
            let y = r.y + i;
            if i == 0 || i == r.h - 1 {
                self.hline(l, y, rt - l + 1, color);
                continue;
            }
            // A pixel is on the outline when the row above or below does not reach it.
            let (al, ar) = rounded_span(r, rad, i - 1);
            let (bl, br) = rounded_span(r, rad, i + 1);
            let left_end = (al.max(bl) - 1).max(l);
            self.hline(l, y, left_end - l + 1, color);
            let right_start = (ar.min(br) + 1).min(rt);
            if right_start > left_end {
                self.hline(right_start, y, rt - right_start + 1, color);
            }
        }
    }

    /// `r` filled, with corners rounded to `radius`.
    pub fn fill_round_rect(&mut self, r: Rect, radius: i32, color: Color) {
        if r.is_empty() {
            return;
        }
        let rad = radius.clamp(0, (r.w.min(r.h) - 1) / 2);
        for i in 0..r.h {
            let (l, rt) = rounded_span(r, rad, i);
            self.hline(l, r.y + i, rt - l + 1, color);
        }
    }

    /// A circle outline of radius `r` around `(cx, cy)` (`2r + 1` pixels across).
    pub fn circle(&mut self, cx: i32, cy: i32, r: i32, color: Color) {
        if r >= 0 {
            self.round_rect(Rect::new(cx - r, cy - r, 2 * r + 1, 2 * r + 1), r, color);
        }
    }

    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, color: Color) {
        if r >= 0 {
            self.fill_round_rect(Rect::new(cx - r, cy - r, 2 * r + 1, 2 * r + 1), r, color);
        }
    }

    /// Draw `img` with its top left corner at `(x, y)`: ink pixels in `color`, the others in
    /// `background` (or left untouched with `None`).
    pub fn image(&mut self, x: i32, y: i32, img: &Image, color: Color, background: Option<Color>) {
        let area = Rect::new(x, y, img.width as i32, img.height as i32);
        for row in 0..img.height as i32 {
            self.blit_row(
                x,
                y + row,
                img.row(row as usize),
                img.width as i32,
                &area,
                color,
                background,
            );
        }
    }

    /// Draw `text` (lines split at `\n`) with its first line's top at `y`, horizontally placed
    /// at `x` per the style's alignment. Characters the font lacks draw as `?`. Returns the area
    /// covered.
    pub fn text(&mut self, x: i32, y: i32, text: &str, style: &TextStyle) -> Rect {
        self.text_fmt(x, y, format_args!("{text}"), style)
    }

    /// [`text`](Self::text) of formatted output, rendered as it is formatted (no buffer):
    /// `c.text_fmt(0, 0, format_args!("{t:.1} °C"), &style)`.
    pub fn text_fmt(&mut self, x: i32, y: i32, args: fmt::Arguments<'_>, style: &TextStyle) -> Rect {
        let mut widths = [0i32; MAX_ALIGNED_LINES];
        if style.align != Align::Left {
            let mut m = Measure {
                widths: &mut widths,
                line: 0,
                chars: 0,
            };
            let _ = fmt::write(&mut m, args);
            m.end_line();
        }
        let mut pen = Pen {
            c: self,
            style,
            anchor: x,
            widths: &widths,
            line: 0,
            x: 0,
            y,
            covered: Rect::default(),
        };
        pen.x = pen.line_start();
        let _ = fmt::write(&mut pen, args);
        pen.covered
    }

    // ---- internals ----------------------------------------------------------------------------

    fn swapped(&self) -> bool {
        matches!(self.rot, Rotation::R90 | Rotation::R270)
    }

    fn to_phys(&self, x: i32, y: i32) -> (i32, i32) {
        match self.rot {
            Rotation::R0 => (x, y),
            Rotation::R90 => (self.pw - 1 - y, x),
            Rotation::R180 => (self.pw - 1 - x, self.ph - 1 - y),
            Rotation::R270 => (y, self.ph - 1 - x),
        }
    }

    /// A non-empty logical rectangle (inside the screen) in physical coordinates.
    fn rect_to_phys(&self, r: Rect) -> Rect {
        let (ax, ay) = self.to_phys(r.x, r.y);
        let (bx, by) = self.to_phys(r.right() - 1, r.bottom() - 1);
        Rect::from_corners(crate::geom::Point::new(ax, ay), crate::geom::Point::new(bx, by))
    }

    fn mark(&mut self, py: i32) {
        self.dirty[py as usize / 32] |= 1 << (py as usize % 32);
    }

    /// Apply `color` to the `mask` bits of byte `i`; whether the byte changed.
    fn paint(&mut self, i: usize, mask: u8, color: Color) -> bool {
        let old = self.buf[i];
        let new = match color {
            Color::Black => old | mask,
            Color::White => old & !mask,
            Color::Invert => old ^ mask,
        };
        self.buf[i] = new;
        new != old
    }

    /// Fill a physical rectangle that lies inside the panel, a byte at a time.
    fn fill_phys(&mut self, p: Rect, color: Color) {
        let (b0, b1) = ((p.x >> 3) as usize, ((p.right() - 1) >> 3) as usize);
        let m0 = 0xFFu8 >> (p.x & 7);
        let m1 = 0xFFu8 << (7 - ((p.right() - 1) & 7));
        for py in p.y..p.bottom() {
            let row = py as usize * self.stride;
            let mut changed = false;
            if b0 == b1 {
                changed |= self.paint(row + b0, m0 & m1, color);
            } else {
                changed |= self.paint(row + b0, m0, color);
                for b in b0 + 1..b1 {
                    changed |= self.paint(row + b, 0xFF, color);
                }
                changed |= self.paint(row + b1, m1, color);
            }
            if changed {
                self.mark(py);
            }
        }
    }

    /// Draw `n` pixels of packed `bits` (MSB first) at logical `(x, y)`. `area` is the whole
    /// bitmap being drawn: when it lies inside the clip on an unrotated frame, the row is shifted
    /// in a byte at a time; otherwise it goes pixel by pixel.
    #[allow(clippy::too_many_arguments)]
    fn blit_row(
        &mut self,
        x: i32,
        y: i32,
        bits: &[u8],
        n: i32,
        area: &Rect,
        color: Color,
        background: Option<Color>,
    ) {
        if self.rot == Rotation::R0 && area.intersect(&self.clip) == *area {
            let shift = (x & 7) as u32;
            let row = y as usize * self.stride;
            let mut changed = false;
            for (i, &b) in bits.iter().enumerate().take((n as usize).div_ceil(8)) {
                let byte = (x >> 3) as usize + i;
                let left = n - 8 * i as i32;
                let valid = if left >= 8 { 0xFF } else { 0xFFu8 << (8 - left) };
                let (ink, paper) = (b & valid, !b & valid);
                changed |= self.paint2(row + byte, ink >> shift, paper >> shift, color, background);
                if shift != 0 {
                    let (ink2, paper2) = (ink << (8 - shift), paper << (8 - shift));
                    if ink2 | paper2 != 0 {
                        changed |= self.paint2(row + byte + 1, ink2, paper2, color, background);
                    }
                }
            }
            if changed {
                self.mark(y);
            }
        } else {
            for i in 0..n {
                if bits[i as usize / 8] & (0x80 >> (i % 8)) != 0 {
                    self.pixel(x + i, y, color);
                } else if let Some(bg) = background {
                    self.pixel(x + i, y, bg);
                }
            }
        }
    }

    fn paint2(&mut self, i: usize, ink: u8, paper: u8, color: Color, background: Option<Color>) -> bool {
        let mut changed = self.paint(i, ink, color);
        if let Some(bg) = background {
            changed |= self.paint(i, paper, bg);
        }
        changed
    }

    fn glyph(&mut self, x: i32, y: i32, ch: char, style: &TextStyle) {
        let f = style.font;
        let (w, h) = (f.width as i32, f.height as i32);
        let cell = Rect::new(x, y, w, h);
        if cell.intersect(&self.clip).is_empty() {
            return;
        }
        match f.glyph_or_fallback(ch) {
            Some(bits) => {
                let stride = f.stride();
                for row in 0..h {
                    let r = row as usize * stride;
                    self.blit_row(
                        x,
                        y + row,
                        &bits[r..r + stride],
                        w,
                        &cell,
                        style.color,
                        style.background,
                    );
                }
            }
            None => {
                if let Some(bg) = style.background {
                    self.fill_rect(cell, bg);
                }
            }
        }
    }
}

/// The inclusive horizontal span of row `i` of `r` with corners rounded to `rad`.
fn rounded_span(r: Rect, rad: i32, i: i32) -> (i32, i32) {
    let dy = if i < rad {
        rad - i
    } else if i >= r.h - rad {
        i - (r.h - 1 - rad)
    } else {
        0
    };
    // Pixels whose center is within rad + 1/2 of the corner center: x^2 + y^2 <= rad^2 + rad.
    let hw = isqrt(rad * rad + rad - dy * dy);
    (r.x + rad - hw, r.right() - 1 - rad + hw)
}

fn isqrt(v: i32) -> i32 {
    if v <= 0 {
        return 0;
    }
    let mut x = v;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + v / x) / 2;
    }
    x
}

/// First pass of [`Canvas::text_fmt`]: the length of each line, in characters.
struct Measure<'w> {
    widths: &'w mut [i32; MAX_ALIGNED_LINES],
    line: usize,
    chars: i32,
}

impl Measure<'_> {
    fn end_line(&mut self) {
        if let Some(w) = self.widths.get_mut(self.line) {
            *w = self.chars;
        }
    }
}

impl fmt::Write for Measure<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            if ch == '\n' {
                self.end_line();
                self.line += 1;
                self.chars = 0;
            } else {
                self.chars += 1;
            }
        }
        Ok(())
    }
}

/// Second pass of [`Canvas::text_fmt`]: draws each character as the formatter emits it.
struct Pen<'p, 'a> {
    c: &'p mut Canvas<'a>,
    style: &'p TextStyle,
    anchor: i32,
    widths: &'p [i32; MAX_ALIGNED_LINES],
    line: usize,
    x: i32,
    y: i32,
    covered: Rect,
}

impl Pen<'_, '_> {
    fn line_start(&self) -> i32 {
        let w = self.widths.get(self.line).copied().unwrap_or(0) * self.style.font.width as i32;
        match self.style.align {
            Align::Left => self.anchor,
            Align::Center => self.anchor - w / 2,
            Align::Right => self.anchor - w,
        }
    }

    fn cover(&mut self, r: Rect) {
        self.covered = if self.covered.is_empty() {
            r
        } else {
            let x = self.covered.x.min(r.x);
            let y = self.covered.y.min(r.y);
            Rect::new(
                x,
                y,
                self.covered.right().max(r.right()) - x,
                self.covered.bottom().max(r.bottom()) - y,
            )
        };
    }
}

impl fmt::Write for Pen<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let (w, h) = (self.style.font.width as i32, self.style.font.height as i32);
        for ch in s.chars() {
            if ch == '\n' {
                self.line += 1;
                self.y += h;
                self.x = self.line_start();
            } else {
                self.c.glyph(self.x, self.y, ch, self.style);
                self.cover(Rect::new(self.x, self.y, w, h));
                self.x += w;
            }
        }
        Ok(())
    }
}
