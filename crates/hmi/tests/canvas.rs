//! Canvas fast paths against per-pixel references, rotation, clipping and the bundled fonts.

use tower_hmi::fonts::{self, SPLEEN_5X8, SPLEEN_8X16, SPLEEN_12X24};
use tower_hmi::{Align, Color, Frame, Image, Rect, Rotation, TextStyle, image};

type F = Frame<16, 128>;

/// A small deterministic PRNG (xorshift32).
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo) as u32) as i32
    }
}

fn pixels(f: &mut F) -> Vec<bool> {
    let c = f.canvas();
    (0..128)
        .flat_map(|y| (0..128).map(move |x| (x, y)))
        .map(|(x, y)| c.get_pixel(x, y))
        .collect()
}

fn noise(f: &mut F, seed: u32) {
    let mut r = Rng(seed);
    let mut c = f.canvas();
    for _ in 0..3000 {
        let (x, y) = (r.range(0, 128), r.range(0, 128));
        c.pixel(x, y, Color::Black);
    }
}

#[test]
fn fill_rect_matches_pixels() {
    let mut r = Rng(7);
    for color in [Color::Black, Color::White, Color::Invert] {
        for _ in 0..300 {
            let rect = Rect::new(
                r.range(-20, 140),
                r.range(-20, 140),
                r.range(-5, 60),
                r.range(-5, 40),
            );
            let (mut a, mut b) = (F::new(), F::new());
            noise(&mut a, 99);
            noise(&mut b, 99);
            a.canvas().fill_rect(rect, color);
            let mut cb = b.canvas();
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    cb.pixel(x, y, color);
                }
            }
            assert_eq!(pixels(&mut a), pixels(&mut b), "{rect:?} {color:?}");
        }
    }
}

#[test]
fn text_matches_font_bitmaps() {
    for font in [&SPLEEN_5X8, &SPLEEN_8X16, &SPLEEN_12X24] {
        for x in [0, 1, 3, 7, 8, 13] {
            let mut f = F::new();
            let style = TextStyle::new(font);
            let area = f.canvas().text(x, 5, "Ag%", &style);
            assert_eq!(area, Rect::new(x, 5, 3 * font.width as i32, font.height as i32));
            let c = f.canvas();
            for (i, ch) in "Ag%".chars().enumerate() {
                let g = font.glyph(ch).unwrap();
                for gy in 0..font.height as usize {
                    for gx in 0..font.width as usize {
                        let ink = g[gy * font.stride() + gx / 8] & (0x80 >> (gx % 8)) != 0;
                        let px = x + (i * font.width as usize + gx) as i32;
                        assert_eq!(
                            c.get_pixel(px, 5 + gy as i32),
                            ink,
                            "{ch} at x {x}, glyph ({gx}, {gy})"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn opaque_text_overwrites_and_is_idempotent() {
    let mut f = F::new();
    let style = TextStyle::new(&SPLEEN_8X16).opaque();
    f.canvas().fill_rect(Rect::new(0, 0, 128, 40), Color::Black);
    f.canvas().text(3, 2, "12", &style);
    f.clear_all_dirty();
    // Drawing the very same text again changes nothing, so no row is dirty.
    f.canvas().text(3, 2, "12", &style);
    assert!(!f.any_dirty());
    f.canvas().text(3, 2, "13", &style);
    assert!(f.any_dirty());
}

#[test]
fn clipped_and_rotated_text_matches_unrotated() {
    // Draw the same text unrotated and in every rotation; reading it back through the rotated
    // canvas must give the same logical picture.
    let style = TextStyle::new(&SPLEEN_8X16);
    let mut reference = F::new();
    reference.canvas().text(10, 20, "Hi!", &style);
    let want = pixels(&mut reference);
    for rot in [Rotation::R90, Rotation::R180, Rotation::R270] {
        let mut f = F::new();
        f.set_rotation(rot);
        f.canvas().text(10, 20, "Hi!", &style);
        assert_eq!(pixels(&mut f), want, "{rot:?}");
    }
    // A clip across the middle of a glyph takes the per-pixel path; outside it nothing changes.
    let mut f = F::new();
    let clip = Rect::new(13, 0, 9, 128);
    f.canvas().with_clip(clip, |c| {
        c.text(10, 20, "Hi!", &style);
    });
    let got = pixels(&mut f);
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        let (x, y) = ((i % 128) as i32, (i / 128) as i32);
        let inside = clip.contains(tower_hmi::Point::new(x, y));
        assert_eq!(*g, *w && inside, "({x}, {y})");
    }
}

#[test]
fn rotation_maps_corners() {
    for (rot, phys) in [
        (Rotation::R0, (0, 0)),
        (Rotation::R90, (127, 0)),
        (Rotation::R180, (127, 127)),
        (Rotation::R270, (0, 127)),
    ] {
        let mut f = F::new();
        f.set_rotation(rot);
        f.canvas().pixel(0, 0, Color::Black);
        let row = f.row(phys.1);
        assert_ne!(row[phys.0 / 8] & (0x80 >> (phys.0 % 8)), 0, "{rot:?}");
    }
}

#[test]
fn alignment() {
    let mut f = F::new();
    let base = TextStyle::new(&SPLEEN_8X16);
    assert_eq!(
        f.canvas().text(64, 0, "abcd", &base.align(Align::Center)).x,
        64 - 16
    );
    assert_eq!(
        f.canvas().text(128, 20, "abc", &base.align(Align::Right)).x,
        128 - 24
    );
    let r = f.canvas().text_fmt(
        128,
        40,
        format_args!("{}\n{}", 7, 1234),
        &base.align(Align::Right),
    );
    assert_eq!(r, Rect::new(128 - 32, 40, 32, 32));
}

#[test]
fn image_fast_path_matches_pixels() {
    const LOGO: Image = image!(
        "..#######..",
        ".#.......#.",
        "#..##.##..#",
        "#.........#",
        "#..#...#..#",
        ".#..###..#.",
        "..#######..",
    );
    let mut r = Rng(3);
    for _ in 0..200 {
        let (x, y) = (r.range(-12, 130), r.range(-8, 130));
        for bg in [None, Some(Color::White), Some(Color::Invert)] {
            let (mut a, mut b) = (F::new(), F::new());
            noise(&mut a, 5);
            noise(&mut b, 5);
            a.canvas().image(x, y, &LOGO, Color::Black, bg);
            let mut cb = b.canvas();
            for iy in 0..LOGO.height as i32 {
                for ix in 0..LOGO.width as i32 {
                    if LOGO.pixel(ix, iy) {
                        cb.pixel(x + ix, y + iy, Color::Black);
                    } else if let Some(bg) = bg {
                        cb.pixel(x + ix, y + iy, bg);
                    }
                }
            }
            assert_eq!(pixels(&mut a), pixels(&mut b), "({x}, {y}) {bg:?}");
        }
    }
}

#[test]
fn circle_outline_bounds_the_fill() {
    // Every outline pixel is inside the filled disk, and the disk's boundary pixels are all on
    // the outline.
    for r in 0..20 {
        let (mut a, mut b) = (F::new(), F::new());
        a.canvas().circle(60, 60, r, Color::Black);
        b.canvas().fill_circle(60, 60, r, Color::Black);
        let (ca, cb) = (a.canvas(), b.canvas());
        for y in 30..91 {
            for x in 30..91 {
                if ca.get_pixel(x, y) {
                    assert!(cb.get_pixel(x, y), "r {r}: outline ({x}, {y}) outside the disk");
                }
                let edge = cb.get_pixel(x, y)
                    && [(1, 0), (-1, 0), (0, 1), (0, -1)]
                        .iter()
                        .any(|(dx, dy)| !cb.get_pixel(x + dx, y + dy));
                if edge {
                    assert!(
                        ca.get_pixel(x, y),
                        "r {r}: edge ({x}, {y}) missing from the outline"
                    );
                }
            }
        }
        // Symmetric and the right size.
        assert!(cb.get_pixel(60 + r, 60) && cb.get_pixel(60 - r, 60) && !cb.get_pixel(61 + r, 60));
    }
}

#[test]
fn invert_round_rect_outline_has_no_double_strikes() {
    // Drawing an outline with Invert twice must restore the original exactly; drawing it once
    // must flip the same pixels a Black outline sets (no pixel is struck twice).
    for (w, h, rad) in [(30, 20, 5), (9, 9, 4), (1, 7, 0), (40, 3, 1), (2, 2, 1)] {
        let rect = Rect::new(10, 10, w, h);
        let (mut a, mut b) = (F::new(), F::new());
        a.canvas().round_rect(rect, rad, Color::Invert);
        b.canvas().round_rect(rect, rad, Color::Black);
        assert_eq!(pixels(&mut a), pixels(&mut b), "{w}x{h} r{rad}");
        a.canvas().round_rect(rect, rad, Color::Invert);
        assert!(pixels(&mut a).iter().all(|p| !p));
    }
}

#[test]
fn line_endpoints_and_symmetry() {
    let mut f = F::new();
    f.canvas().line(5, 7, 40, 20, Color::Black);
    let mut g = F::new();
    g.canvas().line(40, 20, 5, 7, Color::Black);
    let (c, d) = (f.canvas(), g.canvas());
    assert!(c.get_pixel(5, 7) && c.get_pixel(40, 20));
    let count = |c: &tower_hmi::Canvas| {
        (0..128)
            .flat_map(|y| (0..128).map(move |x| (x, y)))
            .filter(|&(x, y)| c.get_pixel(x, y))
            .count()
    };
    assert_eq!(count(&c), 36); // one pixel per column of the major axis
    assert_eq!(count(&d), 36);
}

#[test]
fn every_bundled_font_is_consistent() {
    // Font::new checks block sizes at compile time; here: every preset has its ASCII and the
    // fallback, and the numeric subsets render the degree sign.
    let all = [
        &fonts::SPLEEN_5X8,
        &fonts::SPLEEN_6X12,
        &fonts::SPLEEN_8X16,
        &fonts::SPLEEN_12X24,
        &fonts::SPLEEN_16X32,
        &fonts::SPLEEN_32X64,
    ];
    for f in all {
        for ch in ' '..='~' {
            let g = f
                .glyph(ch)
                .unwrap_or_else(|| panic!("{}x{} lacks {ch:?}", f.width, f.height));
            // Only the space is blank (the generator drops upstream's empty placeholders).
            assert_eq!(g.iter().all(|b| *b == 0), ch == ' ', "{ch:?}");
        }
        assert!(f.glyph('←').is_none()); // not in the preset
        // Upstream 5x8 has no Latin-1 glyphs: the degree sign falls back to '?'.
        assert_eq!(f.glyph('°').is_some(), f.width > 5);
        assert_eq!(f.glyph_or_fallback('°'), f.glyph('°').or(f.glyph('?')));
    }
    for f in [&fonts::SPLEEN_16X32_NUMERIC, &fonts::SPLEEN_32X64_NUMERIC] {
        assert!(f.glyph('°').is_some() && f.glyph('7').is_some() && f.glyph('A').is_none());
    }
    assert!(fonts::spleen_8x16::LATIN_EXT_A.len() == 128 && fonts::spleen_8x16::SYMBOLS.len() > 50);
}

/// A screen with every kind of drawing: tall glyphs across band edges, aligned and opaque text,
/// fills, outlines, lines, an image, an inverted bar, a clipped region, a menu.
fn scene(c: &mut tower_hmi::Canvas<'_>) {
    use tower_hmi::menu::{Item, Menu, MenuState, MenuStyle, Table};
    static MENU: Menu = Menu::new(
        "Menu",
        &[
            Item::choice("Mode", 1, &["auto", "manual", "off"]),
            Item::number("Setpoint", 2, 50, 300, 5).decimals(1),
            Item::action("Pair", 3),
        ],
    );
    const LOGO: Image = image!("#..#.", ".##..", "#..#.");
    c.clear(Color::White);
    c.text(
        64,
        3,
        "21.5",
        &TextStyle::new(&fonts::SPLEEN_32X64_NUMERIC).align(Align::Center),
    );
    c.text(
        127,
        70,
        "°C",
        &TextStyle::new(&SPLEEN_12X24).align(Align::Right).opaque(),
    );
    c.fill_round_rect(Rect::new(2, 96, 60, 30), 6, Color::Black);
    c.text(
        32,
        103,
        "auto",
        &TextStyle::new(&SPLEEN_8X16).inverted().align(Align::Center),
    );
    c.circle(96, 110, 13, Color::Black);
    c.line(70, 127, 127, 90, Color::Invert);
    c.image(5, 70, &LOGO, Color::Black, Some(Color::White));
    c.with_clip(Rect::new(0, 60, 40, 9), |c| {
        c.text(0, 55, "clipped", &TextStyle::new(&SPLEEN_8X16));
    });
    c.invert_rect(Rect::new(0, 88, 128, 5));
    // A menu over the lower part, as a full-screen menu would draw.
    let mut menu = MenuState::new(&MENU);
    let style = MenuStyle::new(&SPLEEN_5X8);
    menu.render_in(
        c,
        Rect::new(64, 0, 64, 60),
        &Table::new([(1, 1), (2, 215)]),
        &style,
    );
}

#[test]
fn bands_draw_what_the_frame_holds() {
    use tower_hmi::Window;
    // What the LCD driver does in page mode: each band of 8 rows rendered between the line
    // headers of its transfer buffer (a gate byte, 16 pixel bytes, a dummy byte per line).
    const LINE: usize = 18;
    for rot in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270] {
        let mut f = F::new();
        f.set_rotation(rot);
        scene(&mut f.canvas());
        for band in 0..16 {
            let mut buf = [0xA5u8; 1 + 8 * LINE + 1];
            for line in 0..8 {
                buf[1 + line * LINE + 1..1 + line * LINE + 17].fill(0);
            }
            let mut dirty = [0u32; 1];
            let window = Window {
                width: 128,
                height: 128,
                y0: band * 8,
                rows: 8,
            };
            let mut c = tower_hmi::Canvas::window(&mut buf[2..], &mut dirty, LINE, window, rot);
            scene(&mut c);
            for line in 0..8 {
                let y = band as usize * 8 + line;
                let at = 1 + line * LINE;
                assert_eq!(&buf[at + 1..at + 17], f.row(y), "{rot:?} row {y}");
                // The line headers and the dummy bytes are left alone.
                assert_eq!((buf[at], buf[at + 17]), (0xA5, 0xA5), "{rot:?} row {y}");
            }
            assert_eq!(buf[0], 0xA5);
        }
    }
}

#[test]
fn window_clips_to_its_rows() {
    use tower_hmi::Window;
    let mut buf = [0u8; 2 * 16];
    let mut dirty = [0u32; 1];
    let window = Window {
        width: 128,
        height: 128,
        y0: 10,
        rows: 2,
    };
    let mut c = tower_hmi::Canvas::window(&mut buf, &mut dirty, 16, window, Rotation::R0);
    assert_eq!(c.area(), Rect::new(0, 10, 128, 2));
    assert_eq!(c.bounds(), Rect::new(0, 0, 128, 128));
    c.fill_rect(Rect::new(0, 0, 128, 128), Color::Black);
    assert!(c.get_pixel(5, 11) && !c.get_pixel(5, 12) && !c.get_pixel(5, 9));
    c.set_clip(Rect::new(0, 0, 64, 128));
    assert_eq!(c.clip(), Rect::new(0, 10, 64, 2));
    c.reset_clip();
    assert_eq!(c.clip(), c.area());
    assert_eq!(dirty[0], 0b11);
    assert!(buf.iter().all(|b| *b == 0xFF));
    // Turned a quarter, the same physical rows are two logical columns.
    let mut dirty = [0u32; 1];
    let c = tower_hmi::Canvas::window(&mut buf, &mut dirty, 16, window, Rotation::R90);
    assert_eq!(c.area(), Rect::new(10, 0, 2, 128));
}
