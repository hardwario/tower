//! Gamma curves: how intent (times brightness) becomes light.
//!
//! The eye's response is roughly the square root of light, so a curve `light = value^γ` with
//! γ around 2 makes equal steps of value look like equal steps of brightness. Each curve is a
//! 257-entry table computed at compile time with integer math, interpolated per channel, so
//! applying it costs a few multiplications and no division.

/// The response curve the [`Render`](crate::render::Render) applies: light =
/// (intent x brightness)^γ.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Gamma {
    /// γ = 1: no correction, light proportional to the value. Fades look fast near black and
    /// flat near full.
    Linear,
    /// γ = 2.0 (the default): close to the eye's response, and the cheapest curve to reason
    /// about (`light = value²`).
    #[default]
    G2_0,
    /// γ = 2.2: the sRGB/display standard, a little darker in the low and middle range.
    G2_2,
    /// γ = 2.8: steeper, common for LEDs; more of the range goes to the dark end, so low values
    /// are dimmer and fades linger there longer.
    G2_8,
}

impl Gamma {
    /// Every curve, with the name [`name`](Self::name) gives it.
    pub const ALL: [Gamma; 4] = [Gamma::Linear, Gamma::G2_0, Gamma::G2_2, Gamma::G2_8];

    /// The curve's short name: `linear`, `2.0`, `2.2`, `2.8`.
    pub const fn name(self) -> &'static str {
        match self {
            Gamma::Linear => "linear",
            Gamma::G2_0 => "2.0",
            Gamma::G2_2 => "2.2",
            Gamma::G2_8 => "2.8",
        }
    }

    /// The curve with this [`name`](Self::name).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.name() == name)
    }

    /// Intent x brightness `value` in 8.8 fixed point (`0..=65280` = `0.0..=255.0`) to linear
    /// light in the same 8.8 scale, keeping the fraction (for the dithering). Values above
    /// `65280` count as `65280`.
    pub fn light(self, value: u32) -> u32 {
        let value = value.min(65280);
        let table = match self {
            Gamma::Linear => return value,
            Gamma::G2_0 => &GAMMA_2_0,
            Gamma::G2_2 => &GAMMA_2_2,
            Gamma::G2_8 => &GAMMA_2_8,
        };
        let (i, f) = ((value >> 8) as usize, value & 0xFF);
        let (lo, hi) = (table[i] as u32, table[i + 1] as u32); // tables rise: hi >= lo
        lo + (((hi - lo) * f) >> 8)
    }
}

/// `65280 * (i/255)^(2 + a/5)` for `i` in `0..=255` (8.8 fixed point, 65280 = 255.0), so `a` =
/// 0, 1, 4 gives γ = 2.0, 2.2, 2.8; computed at compile time with integer math. Entry 256
/// repeats entry 255 so [`Gamma::light`] can always read `i + 1` (an 8.8 value reaches index 255
/// only at full scale, with a zero fraction).
const fn gamma_table(a: u32) -> [u16; 257] {
    let mut table = [0u16; 257];
    let mut i = 0;
    while i < 256 {
        let x = i as u128;
        // y = (i/255)^(a/5) in 0.16 fixed point: the largest y with (y/65536)^5 <= (i/255)^a,
        // i.e. y^5 * 255^a <= i^a * 2^80. With a <= 4 both sides stay under 2^113.
        let (mut lo, mut hi) = (0u128, 65536u128);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if mid.pow(5) * 255u128.pow(a) <= x.pow(a) << 80 {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        // 65280 * (i/255)^2 * y/65536 = i^2 * y / (255 * 256), rounded to nearest.
        table[i] = ((x * x * lo + 255 * 128) / (255 * 256)) as u16;
        i += 1;
    }
    table[256] = table[255];
    table
}

static GAMMA_2_0: [u16; 257] = gamma_table(0);
static GAMMA_2_2: [u16; 257] = gamma_table(1);
static GAMMA_2_8: [u16; 257] = gamma_table(4);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_span_the_range() {
        for g in Gamma::ALL {
            assert_eq!(g.light(0), 0, "{g:?}");
            assert_eq!(g.light(65280), 65280, "{g:?}");
            assert_eq!(g.light(70000), 65280, "{g:?}");
        }
    }

    #[test]
    fn curves_match_float_power() {
        for (g, exp) in [
            (Gamma::G2_0, 2.0f64),
            (Gamma::G2_2, 2.2),
            (Gamma::G2_8, 2.8),
            (Gamma::Linear, 1.0),
        ] {
            for v in (0..=65280u32).step_by(17) {
                let want = 65280.0 * (v as f64 / 65280.0).powf(exp);
                let got = g.light(v) as f64;
                // Within 0.01 of an output level (an output level is 256 here).
                assert!((got - want).abs() < 2.6, "{g:?} v={v}: {got} vs {want}");
            }
        }
    }

    #[test]
    fn curves_rise() {
        for g in Gamma::ALL {
            let mut prev = 0;
            for v in 0..=65280u32 {
                let l = g.light(v);
                assert!(l >= prev, "{g:?} v={v}");
                prev = l;
            }
        }
    }

    #[test]
    fn steeper_is_darker() {
        let mid = 128 * 256;
        assert!(Gamma::G2_8.light(mid) < Gamma::G2_2.light(mid));
        assert!(Gamma::G2_2.light(mid) < Gamma::G2_0.light(mid));
        assert!(Gamma::G2_0.light(mid) < Gamma::Linear.light(mid));
    }

    #[test]
    fn names_round_trip() {
        for g in Gamma::ALL {
            assert_eq!(Gamma::from_name(g.name()), Some(g));
        }
        assert_eq!(Gamma::from_name("3.0"), None);
        assert_eq!(Gamma::default(), Gamma::G2_0);
    }
}
