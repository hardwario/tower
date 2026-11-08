//! Gradient palettes: 16 colours a 0..=255 (or 0..=65535) index blends across, so one index
//! (a position, a heat, a time) picks a colour from a designed range. Effects like
//! [`Frame::plasma`](crate::frame::Frame::plasma) and [`Fire`](crate::sim::Fire) take one.

use crate::color::Rgb;

/// 16 colours and how the index runs across them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Palette {
    pub colors: [Rgb; 16],
    /// `true`: cyclic, the last colour blends back into the first (for scrolling and noise);
    /// `false`: a ramp from the first colour to the last (for heat, sunrise, levels).
    pub wrap: bool,
}

impl Palette {
    pub const fn new(colors: [Rgb; 16], wrap: bool) -> Self {
        Self { colors, wrap }
    }

    /// The colour at `index` (0..=255), blended between neighbouring entries.
    pub fn color(&self, index: u8) -> Rgb {
        self.color16(u16::from_be_bytes([index, index])) // 255 -> 65535: the very end
    }

    /// The colour at a 16-bit `index`, for slow ramps (a 30-minute sunrise) that need finer
    /// steps than 256.
    pub fn color16(&self, index: u16) -> Rgb {
        let segments = if self.wrap { 16 } else { 15 };
        let pos = index as u32 * segments; // 16.16: the segment, then the fraction
        let seg = (pos >> 16) as usize;
        let frac = ((pos >> 8) & 0xFF) as u8;
        let next = if self.wrap {
            (seg + 1) % 16
        } else {
            (seg + 1).min(15)
        };
        self.colors[seg].blend(self.colors[next], frac)
    }
}

const fn c(rgb: u32) -> Rgb {
    Rgb::from_hex(rgb)
}

/// Molten rock: black, through dark red and orange, to a yellow core, and back (cyclic).
pub const LAVA: Palette = Palette::new(
    [
        c(0x000000),
        c(0x400000),
        c(0x800800),
        c(0xc81800),
        c(0xff3000),
        c(0xff6000),
        c(0xffa000),
        c(0xffdc28),
        c(0xffa000),
        c(0xff6000),
        c(0xff3000),
        c(0xc81800),
        c(0x800800),
        c(0x400000),
        c(0x180000),
        c(0x000000),
    ],
    true,
);

/// Deep blue water to a pale crest and back (cyclic).
pub const OCEAN: Palette = Palette::new(
    [
        c(0x000028),
        c(0x000060),
        c(0x0010a0),
        c(0x0030c8),
        c(0x0060dc),
        c(0x00a0dc),
        c(0x28c8e6),
        c(0x78e6ff),
        c(0x28c8e6),
        c(0x00a0dc),
        c(0x0060c8),
        c(0x0030a0),
        c(0x001078),
        c(0x000060),
        c(0x000040),
        c(0x000028),
    ],
    true,
);

/// Greens from shade to sunlit leaves (cyclic).
pub const FOREST: Palette = Palette::new(
    [
        c(0x002000),
        c(0x004000),
        c(0x006408),
        c(0x188000),
        c(0x40a000),
        c(0x78b400),
        c(0x408c00),
        c(0x006e14),
        c(0x005028),
        c(0x006e14),
        c(0x288c00),
        c(0x60aa00),
        c(0x287800),
        c(0x005a00),
        c(0x003c00),
        c(0x002800),
    ],
    true,
);

/// Saturated party colours around the wheel: purple, pink, red, orange, yellow, lime, aqua,
/// blue (cyclic).
pub const PARTY: Palette = Palette::new(
    [
        c(0x8000ff),
        c(0xc800c8),
        c(0xff0080),
        c(0xff0030),
        c(0xff3000),
        c(0xff8000),
        c(0xffc800),
        c(0xc8ff00),
        c(0x40ff20),
        c(0x00ff80),
        c(0x00dcdc),
        c(0x0080ff),
        c(0x0030ff),
        c(0x3000ff),
        c(0x6000ff),
        c(0x7000ff),
    ],
    true,
);

/// Black body heat: black, dark red, red, orange, yellow, white (a ramp); the colours of
/// [`Fire`](crate::sim::Fire).
pub const HEAT: Palette = Palette::new(
    [
        c(0x000000),
        c(0x200000),
        c(0x400000),
        c(0x6e0000),
        c(0xa00800),
        c(0xd21800),
        c(0xff2800),
        c(0xff4800),
        c(0xff6e00),
        c(0xff9600),
        c(0xffbe00),
        c(0xffdc14),
        c(0xfff03c),
        c(0xfffa78),
        c(0xffffbe),
        c(0xffffff),
    ],
    false,
);

/// Night to daylight: black, deep red, orange, warm white, near white (a ramp); the colours of
/// [`Frame::sunrise`](crate::frame::Frame::sunrise).
pub const SUNRISE: Palette = Palette::new(
    [
        c(0x000000),
        c(0x080000),
        c(0x180002),
        c(0x300204),
        c(0x500604),
        c(0x780c02),
        c(0xa01800),
        c(0xc82800),
        c(0xeb4000),
        c(0xff6008),
        c(0xff8018),
        c(0xffa030),
        c(0xffb850),
        c(0xffd078),
        c(0xffe4aa),
        c(0xfff5dc),
    ],
    false,
);

/// The built-in palettes, with their names.
pub const ALL: [(&str, &Palette); 6] = [
    ("lava", &LAVA),
    ("ocean", &OCEAN),
    ("forest", &FOREST),
    ("party", &PARTY),
    ("heat", &HEAT),
    ("sunrise", &SUNRISE),
];

/// A built-in palette by name.
pub fn by_name(name: &str) -> Option<&'static Palette> {
    ALL.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_hit_both_ends() {
        for p in [&HEAT, &SUNRISE] {
            assert_eq!(p.color(0), p.colors[0]);
            assert_eq!(p.color(255), p.colors[15]);
            assert_eq!(p.color16(0), p.colors[0]);
            assert_eq!(p.color16(u16::MAX), p.colors[15]);
        }
    }

    #[test]
    fn entries_sit_at_their_indices() {
        // Cyclic: entry k at index 16k. Ramp: entry k at 65535 * k / 15.
        for k in 0..16 {
            assert_eq!(LAVA.color16((k * 4096) as u16), LAVA.colors[k]);
            let at = (65535u32 * k as u32).div_ceil(15) as u16;
            assert_eq!(HEAT.color16(at), HEAT.colors[k], "k={k}");
        }
    }

    #[test]
    fn cyclic_palettes_are_continuous_across_the_seam() {
        for (_, p) in ALL.iter().filter(|(_, p)| p.wrap) {
            let (a, b) = (p.color(255), p.color(0));
            let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
            assert!(d(a.r, b.r) <= 40 && d(a.g, b.g) <= 40 && d(a.b, b.b) <= 40);
        }
    }

    #[test]
    fn heat_brightens_monotonically() {
        let mut prev = 0;
        for i in 0..=255u8 {
            let c = HEAT.color(i);
            let sum = c.r as u32 + c.g as u32 + c.b as u32;
            assert!(sum >= prev, "i={i}");
            prev = sum;
        }
    }

    #[test]
    fn lookup_by_name() {
        assert_eq!(by_name("ocean"), Some(&OCEAN));
        assert_eq!(by_name("nope"), None);
    }
}
