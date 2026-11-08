//! Colours: the [`Rgb`] intent pixel and the [`Rgbw`] wire pixel, named colours, a
//! constant-power [`hue`] wheel, [`hsv`], blending and parsing.
//!
//! Colours here are *intent*, the values an effect means before brightness and gamma; the
//! [`render`](crate::render) pipeline turns them into light. With the default gamma 2.0, light
//! is the square of intent, so intent is roughly *perceptual*: half the value looks about half as
//! bright.

use crate::math::quarter_sine;

/// An RGB pixel (intent, `0..=255` per channel). WS2812B strips take it G, R, B on the wire.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// An RGBW pixel, for SK6812 RGBW strips (sent G, R, B, W).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Rgbw {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub w: u8,
}

impl Rgbw {
    pub const fn new(r: u8, g: u8, b: u8, w: u8) -> Self {
        Self { r, g, b, w }
    }
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// From `0xRRGGBB`.
    pub const fn from_hex(rgb: u32) -> Self {
        Self::new((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }

    /// As `0xRRGGBB`.
    pub const fn to_hex(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }

    /// Every channel times `factor / 255`, rounded down, so repeated scaling (a fade) always
    /// reaches black.
    pub const fn scale(self, factor: u8) -> Self {
        let f = factor as u32;
        Self::new(
            div255(self.r as u32 * f),
            div255(self.g as u32 * f),
            div255(self.b as u32 * f),
        )
    }

    /// The colour `amount / 255` of the way from `self` to `other` (0 = `self`, 255 = `other`).
    pub const fn blend(self, other: Self, amount: u8) -> Self {
        const fn lerp(a: u8, b: u8, t: u8) -> u8 {
            div255(a as u32 * (255 - t as u32) + b as u32 * t as u32)
        }
        Self::new(
            lerp(self.r, other.r, amount),
            lerp(self.g, other.g, amount),
            lerp(self.b, other.b, amount),
        )
    }

    /// Channel-wise saturating sum: light adds, so overlapping dots brighten.
    pub const fn add(self, other: Self) -> Self {
        Self::new(
            self.r.saturating_add(other.r),
            self.g.saturating_add(other.g),
            self.b.saturating_add(other.b),
        )
    }

    /// Channel-wise maximum.
    pub const fn max(self, other: Self) -> Self {
        const fn m(a: u8, b: u8) -> u8 {
            if a > b { a } else { b }
        }
        Self::new(m(self.r, other.r), m(self.g, other.g), m(self.b, other.b))
    }
}

/// `x / 255` rounded down, for `x` up to `255 * 255`, by shifts: the M0+ has no divider and
/// every blend and scale divides by 255 per channel (exact over that range, host-tested).
pub(crate) const fn div255(x: u32) -> u8 {
    ((x + 1 + (x >> 8)) >> 8) as u8
}

// Named colours (full intent; brightness and gamma are applied at render).
pub const OFF: Rgb = Rgb::new(0, 0, 0);
pub const RED: Rgb = Rgb::new(255, 0, 0);
pub const GREEN: Rgb = Rgb::new(0, 255, 0);
pub const BLUE: Rgb = Rgb::new(0, 0, 255);
pub const YELLOW: Rgb = Rgb::new(255, 255, 0);
pub const CYAN: Rgb = Rgb::new(0, 255, 255);
pub const MAGENTA: Rgb = Rgb::new(255, 0, 255);
pub const ORANGE: Rgb = Rgb::new(255, 96, 0);
pub const PURPLE: Rgb = Rgb::new(128, 0, 255);
pub const PINK: Rgb = Rgb::new(255, 48, 128);
/// A warm (incandescent-like) white; on an RGBW strip mostly the W LED, tinted.
pub const WARM_WHITE: Rgb = Rgb::new(255, 160, 72);
pub const WHITE: Rgb = Rgb::new(255, 255, 255);

/// The named colours [`parse_color`] accepts, with their names.
pub const NAMED: [(&str, Rgb); 13] = [
    ("off", OFF),
    ("red", RED),
    ("green", GREEN),
    ("blue", BLUE),
    ("yellow", YELLOW),
    ("cyan", CYAN),
    ("magenta", MAGENTA),
    ("orange", ORANGE),
    ("purple", PURPLE),
    ("pink", PINK),
    ("warm", WARM_WHITE),
    ("white", WHITE),
    ("black", OFF),
];

/// A colour from a name in [`NAMED`] (`red`, `warm`, ...) or six hex digits (`ff8000`, with or
/// without a leading `#`).
pub fn parse_color(s: &str) -> Option<Rgb> {
    if let Some((_, c)) = NAMED.iter().find(|(name, _)| name.eq_ignore_ascii_case(s)) {
        return Some(*c);
    }
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(Rgb::from_hex)
}

/// A fully saturated colour on the colour wheel, `h` = 0 red, 85 green, 170 blue, back to red at
/// 256. **Constant power:** between two primaries the channels cross-fade along a cosine and a
/// sine (`a = 255 cos θ`, `b = 255 sin θ`), so after the default gamma 2.0 the light adds up to
/// the same total everywhere (`cos² + sin² = 1`). The usual "sum = 255" wheel instead gives the
/// mixed colours (yellow, cyan, magenta) half the light of the primaries, which shows as dark
/// bands in a rainbow. (Equal light is not equal *perceived* brightness: green looks brighter
/// than blue at the same power.)
pub fn hue(h: u8) -> Rgb {
    let x = h as u32 * 3; // 0..=765: three sectors of 256
    let (sector, p) = (x >> 8, x & 0xFF);
    let fading = quarter_sine(256 - p); // cos
    let rising = quarter_sine(p); // sin
    match sector {
        0 => Rgb::new(fading, rising, 0),
        1 => Rgb::new(0, fading, rising),
        _ => Rgb::new(rising, 0, fading),
    }
}

/// Hue, saturation, value: [`hue`] `h`, mixed toward white as `s` drops (255 = fully saturated,
/// 0 = grey), then scaled by `v`.
pub fn hsv(h: u8, s: u8, v: u8) -> Rgb {
    let c = hue(h);
    let white = 255 - s as u16;
    let sat = |x: u8| ((x as u16 * s as u16 / 255 + white) as u32 * v as u32 / 255) as u8;
    Rgb::new(sat(c.r), sat(c.g), sat(c.b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_hits_the_primaries() {
        assert_eq!(hue(0), RED);
        let g = hue(85); // 85 * 3 = 255: just before the green sector starts
        assert!(g.g >= 254 && g.r <= 2 && g.b == 0, "{g:?}");
        let b = hue(171); // 171 * 3 = 513: just into the blue-to-red sector
        assert!(b.b >= 254 && b.r <= 5 && b.g == 0, "{b:?}");
    }

    #[test]
    fn hue_has_constant_power_under_gamma_2() {
        // light = sum of squares (gamma 2.0): within 2 % of a primary's everywhere.
        let full = 255.0f64 * 255.0;
        for h in 0..=255u8 {
            let c = hue(h);
            let light = (c.r as f64).powi(2) + (c.g as f64).powi(2) + (c.b as f64).powi(2);
            assert!(
                (light / full - 1.0).abs() < 0.02,
                "h={h}: {c:?} -> {}",
                light / full
            );
        }
    }

    #[test]
    fn hue_is_continuous() {
        for h in 0..=255u8 {
            let (a, b) = (hue(h), hue(h.wrapping_add(1)));
            let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
            assert!(
                d(a.r, b.r) <= 8 && d(a.g, b.g) <= 8 && d(a.b, b.b) <= 8,
                "h={h}: {a:?} {b:?}"
            );
        }
    }

    #[test]
    fn hsv_desaturates_and_dims() {
        assert_eq!(hsv(0, 255, 255), RED);
        assert_eq!(hsv(123, 0, 255), WHITE);
        assert_eq!(hsv(40, 0, 100), Rgb::new(100, 100, 100));
        assert_eq!(hsv(40, 255, 0), OFF);
    }

    #[test]
    fn div255_is_exact() {
        for x in 0..=255 * 255u32 {
            assert_eq!(div255(x) as u32, x / 255, "x={x}");
        }
    }

    #[test]
    fn scale_blend_add() {
        assert_eq!(WHITE.scale(255), WHITE);
        assert_eq!(WHITE.scale(0), OFF);
        // A fade by repeated scaling reaches black.
        let mut c = WHITE;
        for _ in 0..100 {
            c = c.scale(200);
        }
        assert_eq!(c, OFF);
        assert_eq!(RED.blend(BLUE, 0), RED);
        assert_eq!(RED.blend(BLUE, 255), BLUE);
        assert_eq!(
            Rgb::new(200, 10, 0).add(Rgb::new(100, 10, 5)),
            Rgb::new(255, 20, 5)
        );
    }

    #[test]
    fn parse_names_and_hex() {
        assert_eq!(parse_color("red"), Some(RED));
        assert_eq!(parse_color("Warm"), Some(WARM_WHITE));
        assert_eq!(parse_color("ff8000"), Some(Rgb::new(255, 128, 0)));
        assert_eq!(parse_color("#00FF7f"), Some(Rgb::new(0, 255, 127)));
        assert_eq!(parse_color("ff80"), None);
        assert_eq!(parse_color("gg0000"), None);
        assert_eq!(parse_color(""), None);
        assert_eq!(Rgb::from_hex(0x123456).to_hex(), 0x123456);
    }
}
