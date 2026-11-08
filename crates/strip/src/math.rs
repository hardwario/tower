//! Integer math the effects share: a compile-time sine table, raised-cosine bumps, smooth value
//! noise, a hash, and an integer square root. No float at run time.

/// A quarter sine wave in 65 steps: `round(255 * sin(k/64 * pi/2))`, computed at compile time.
static QUARTER: [u8; 65] = quarter_table();

const fn quarter_table() -> [u8; 65] {
    let mut table = [0u8; 65];
    let mut k = 0;
    while k <= 64 {
        let x = k as f64 / 64.0 * core::f64::consts::FRAC_PI_2;
        table[k] = (255.0 * sin_taylor(x) + 0.5) as u8;
        k += 1;
    }
    table
}

/// `sin(x)` for `x` in `0..=pi/2` from its Taylor series to `x^13` (error below `1e-9` there).
/// Compile-time only: it fills [`QUARTER`].
const fn sin_taylor(x: f64) -> f64 {
    let x2 = x * x;
    let mut term = x;
    let mut sum = x;
    let mut n = 1;
    while n < 7 {
        term = -term * x2 / ((2 * n) as f64 * (2 * n + 1) as f64);
        sum += term;
        n += 1;
    }
    sum
}

/// A quarter turn of sine: `p` in `0..=256` (0 to pi/2) gives `0..=255`, linearly interpolated
/// between the 65 table steps. `p` above 256 counts as 256.
pub fn quarter_sine(p: u32) -> u8 {
    let p = p.min(256);
    let (k, f) = ((p >> 2) as usize, p & 3);
    let a = QUARTER[k] as u32;
    let b = QUARTER[(k + 1).min(64)] as u32; // the table rises, b >= a
    (a + ((b - a) * f + 2) / 4) as u8
}

/// Half a sine wave, the positive hump: `p` in `0..512` gives `0..=255..=0` (sin over `0..pi`).
/// Wraps every 512.
pub fn half_sine(p: u32) -> u8 {
    let p = p % 512;
    quarter_sine(if p <= 256 { p } else { 512 - p })
}

/// A full sine wave around 128: `x` is one turn per 256 (`0` is 128, `64` is 255, `128` is
/// 128, `192` is 0).
pub fn sin8(x: u8) -> u8 {
    let x = x as u32;
    let (quadrant, p) = (x / 64, (x % 64) * 4);
    let s = match quadrant {
        0 | 2 => quarter_sine(p),
        _ => quarter_sine(256 - p),
    } as u32;
    if quadrant < 2 {
        (128 + s * 127 / 255) as u8
    } else {
        (128 - s * 128 / 255) as u8
    }
}

/// A raised-cosine bump: `p` in `0..len` rises from 0 to 255 at `len / 2` and falls back, with
/// a smooth (zero-slope) start and end; 0 outside. The shape of a twinkle or a breath.
pub fn bump(p: u32, len: u32) -> u8 {
    if len == 0 || p >= len {
        return 0;
    }
    // p < len, so p * 512 fits u32 for any len below 2^23: a 32-bit division (the u64 one is a
    // slow library call on the M0+).
    let s = half_sine(if len < 1 << 23 {
        p * 512 / len
    } else {
        (p as u64 * 512 / len as u64) as u32
    }) as u32;
    (s * s / 255) as u8 // sin² = (1 - cos) / 2: the raised cosine
}

/// A 32-bit integer hash with good avalanche (the "lowbias32" mixer): any change of the input
/// changes about half the output bits. Not cryptographic; for per-pixel randomness that is a pure
/// function of the pixel index (and so needs no stored state).
pub const fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// Smooth 1-D value noise: a random value at every lattice point (every 256 units of `x`),
/// joined by a smoothstep curve, so it wanders continuously between them. Returns `0..=255`.
/// Advance `x` slowly for a slow wander, quickly for a flicker.
pub fn noise(x: u32) -> u8 {
    let (n, f) = (x >> 8, x & 0xFF);
    let a = (hash(n) & 0xFF) as i32;
    let b = (hash(n.wrapping_add(1)) & 0xFF) as i32;
    let s = ((f * f * (768 - 2 * f)) >> 16) as i32; // smoothstep 3u² - 2u³ in 0..=256
    (a + (b - a) * s / 256) as u8
}

/// `floor(sqrt(n))`.
pub fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    // Newton from an over-estimate: monotonically decreasing to the floor root.
    let mut x = 1u64 << (n.ilog2() / 2 + 1);
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_sine_matches_float() {
        for p in 0..=256u32 {
            let want = 255.0 * (p as f64 / 256.0 * core::f64::consts::FRAC_PI_2).sin();
            let got = quarter_sine(p) as f64;
            assert!((got - want).abs() <= 1.0, "p={p}: {got} vs {want}");
        }
        assert_eq!(quarter_sine(0), 0);
        assert_eq!(quarter_sine(256), 255);
        assert_eq!(quarter_sine(1000), 255);
    }

    #[test]
    fn quarter_sine_is_monotonic() {
        for p in 0..256u32 {
            assert!(quarter_sine(p) <= quarter_sine(p + 1));
        }
    }

    #[test]
    fn half_and_full_sine_shapes() {
        assert_eq!(half_sine(0), 0);
        assert_eq!(half_sine(256), 255);
        assert_eq!(half_sine(512), 0);
        assert_eq!(half_sine(100), half_sine(412));
        assert_eq!(sin8(0), 128);
        assert_eq!(sin8(64), 255);
        assert_eq!(sin8(128), 128);
        assert_eq!(sin8(192), 0);
        for x in 0..=255u8 {
            let mirrored = sin8(x.wrapping_neg()) as i32; // sin(-x) = -sin(x) around 128
            assert!((sin8(x) as i32 + mirrored - 256).abs() <= 2, "x={x}");
        }
    }

    #[test]
    fn bump_is_smooth_and_bounded() {
        assert_eq!(bump(0, 100), 0);
        assert_eq!(bump(50, 100), 255);
        assert_eq!(bump(100, 100), 0);
        assert_eq!(bump(5, 0), 0);
        for p in 0..99u32 {
            let d = (bump(p + 1, 100) as i32 - bump(p, 100) as i32).abs();
            assert!(d <= 11, "step {p}: {d}"); // the steepest slope is 255 pi / 100 = 8 a step
        }
    }

    #[test]
    fn noise_is_continuous_and_spread() {
        let mut lo = 255u8;
        let mut hi = 0u8;
        for x in 0..100_000u32 {
            let d = (noise(x + 1) as i32 - noise(x) as i32).abs();
            assert!(d <= 2, "x={x}: jump {d}");
            lo = lo.min(noise(x));
            hi = hi.max(noise(x));
        }
        assert!(lo < 20 && hi > 235, "range {lo}..{hi}");
    }

    #[test]
    fn hash_differs_for_neighbours() {
        for i in 0..1000u32 {
            assert_ne!(hash(i), hash(i + 1));
        }
    }

    #[test]
    fn isqrt_is_the_floor_root() {
        for n in 0..10_000u64 {
            let r = isqrt(n);
            assert!(r * r <= n && (r + 1) * (r + 1) > n, "n={n}: {r}");
        }
        assert_eq!(isqrt(u64::MAX), u32::MAX as u64);
    }
}
