//! Turning the screen: the angle arithmetic of a rotation animation, and which way up a device is.
//!
//! A screen turned from one [`Rotation`] to another is the same picture turned by a multiple of
//! 90 degrees, so an animation draws a snapshot of the old picture turned by a growing angle
//! ([`Frame::draw_turned`](crate::frame::Frame::draw_turned)), eased in and out ([`ease`]) and,
//! optionally, shrunk while it turns so the corners stay on the screen ([`fit`]). At a multiple of
//! 90 degrees the turned picture is exactly what drawing in the new rotation gives, so the last
//! step needs no redraw.
//!
//! Angles are binary: [`TURN`] (1024) is a full turn, [`QUARTER`] (256) is 90 degrees; ratios
//! are Q16 (65536 = 1.0). Integer only: the M0+ has no FPU.
//!
//! [`orientation`] picks the rotation from the gravity an accelerometer reads, with hysteresis.

use crate::canvas::Rotation;

/// A full turn, in binary angle units.
pub const TURN: i32 = 1024;
/// 90 degrees.
pub const QUARTER: i32 = 256;
/// 1.0 in Q16.
pub const ONE: i32 = 1 << 16;

/// `sin` over the first quarter, Q16, every 1/64 of it (1.40625 degrees).
const SIN: [i32; 65] = [
    0, 1608, 3216, 4821, 6424, 8022, 9616, 11204, 12785, 14359, 15924, 17479, 19024, 20557, 22078, 23586,
    25080, 26558, 28020, 29466, 30893, 32303, 33692, 35062, 36410, 37736, 39040, 40320, 41576, 42806, 44011,
    45190, 46341, 47464, 48559, 49624, 50660, 51665, 52639, 53581, 54491, 55368, 56212, 57022, 57798, 58538,
    59244, 59914, 60547, 61145, 61705, 62228, 62714, 63162, 63572, 63944, 64277, 64571, 64827, 65043, 65220,
    65358, 65457, 65516, 65536,
];

/// `sin(a)` in Q16, interpolated from a quarter-wave table: within 0.01 % of the true value, and
/// exact (0, 1.0, -1.0) at multiples of 90 degrees.
pub fn sin(a: i32) -> i32 {
    let a = a.rem_euclid(TURN);
    let (quadrant, r) = (a / QUARTER, a % QUARTER);
    let r = if quadrant % 2 == 1 { QUARTER - r } else { r };
    let (i, f) = ((r / 4) as usize, r % 4);
    let v = if f == 0 {
        SIN[i]
    } else {
        SIN[i] + (SIN[i + 1] - SIN[i]) * f / 4
    };
    if quadrant >= 2 { -v } else { v }
}

/// `cos(a)` in Q16.
pub fn cos(a: i32) -> i32 {
    sin(a + QUARTER)
}

/// The scale (Q16) that keeps a square turned by `a` inside its own outline: `1 / (|cos| + |sin|)`,
/// 1.0 at multiples of 90 degrees, 0.707 at 45.
pub fn fit(a: i32) -> i32 {
    ((1i64 << 32) / (cos(a).abs() + sin(a).abs()) as i64) as i32
}

/// Smoothstep easing of a progress `t` (Q16, clamped to 0..=1.0): slow start, slow end.
pub fn ease(t: i32) -> i32 {
    let t = t.clamp(0, ONE) as i64;
    ((((t * t) >> 16) * (3 * ONE as i64 - 2 * t)) >> 16) as i32
}

/// The quarter turns from `from` to `to` the short way: -1, 0, 1 or 2 (the angle a picture turns
/// by, clockwise on the panel, is this times [`QUARTER`]).
pub fn quarters(from: Rotation, to: Rotation) -> i32 {
    match (to as i32 - from as i32).rem_euclid(4) {
        3 => -1,
        q => q,
    }
}

/// Which way up the screen should be drawn for an acceleration `(x, y)` in the panel's plane
/// (milli-g, the panel's own axes: `x` to the right and `y` down as the unrotated picture reads,
/// so a panel held upright reads about `(0, -1000)`: an accelerometer at rest reads the opposite
/// of gravity).
///
/// The rotation changes only when the new direction clearly dominates (its axis at least 1.5 times
/// the other, about 56 degrees past the diagonal) and the tilt out of the panel's plane leaves at
/// least 0.5 g in it; otherwise (lying flat, held at an angle, shaken) it stays `current`.
pub fn orientation(x: i32, y: i32, current: Rotation) -> Rotation {
    let (ax, ay) = (x.abs(), y.abs());
    let (major, minor) = (ax.max(ay), ax.min(ay));
    if major < 500 || 2 * major < 3 * minor {
        return current;
    }
    if ay >= ax {
        if y < 0 { Rotation::R0 } else { Rotation::R180 }
    } else if x > 0 {
        Rotation::R90
    } else {
        Rotation::R270
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_table() {
        for (a, s) in [
            (0, 0),
            (QUARTER, ONE),
            (2 * QUARTER, 0),
            (3 * QUARTER, -ONE),
            (-QUARTER, -ONE),
        ] {
            assert_eq!(sin(a), s, "{a}");
        }
        assert_eq!(cos(0), ONE);
        assert_eq!(cos(2 * QUARTER), -ONE);
        for a in -TURN..2 * TURN {
            let want = (a as f64 * core::f64::consts::TAU / TURN as f64).sin() * ONE as f64;
            assert!((sin(a) as f64 - want).abs() < 8.0, "{a}: {} vs {want}", sin(a));
        }
    }

    #[test]
    fn fit_and_ease() {
        assert_eq!(fit(0), ONE);
        assert_eq!(fit(QUARTER), ONE);
        assert!((fit(QUARTER / 2) - 46341).abs() < 8); // 1 / sqrt(2)
        assert_eq!((ease(0), ease(ONE), ease(ONE / 2)), (0, ONE, ONE / 2));
        assert!(ease(ONE / 4) < ONE / 4 && ease(3 * ONE / 4) > 3 * ONE / 4);
        assert_eq!(ease(2 * ONE), ONE);
    }

    #[test]
    fn quarter_turns() {
        use Rotation::*;
        assert_eq!(quarters(R0, R90), 1);
        assert_eq!(quarters(R90, R0), -1);
        assert_eq!(quarters(R0, R270), -1);
        assert_eq!(quarters(R270, R0), 1);
        assert_eq!(quarters(R90, R270), 2);
        assert_eq!(quarters(R180, R180), 0);
    }

    #[test]
    fn orientation_hysteresis() {
        use Rotation::*;
        assert_eq!(orientation(0, -1000, R90), R0);
        assert_eq!(orientation(1000, 0, R0), R90);
        assert_eq!(orientation(0, 1000, R0), R180);
        assert_eq!(orientation(-1000, 0, R0), R270);
        // Flat on a table: keep what it was.
        assert_eq!(orientation(30, -20, R270), R270);
        // Near the diagonal: keep; past ~56 degrees: switch.
        assert_eq!(orientation(700, -700, R0), R0);
        assert_eq!(orientation(700, -600, R0), R0);
        assert_eq!(orientation(800, -500, R0), R90);
        assert_eq!(orientation(800, -500, R90), R90);
    }
}
