//! Effects with state of their own: a fire simulation and bouncing balls. Each one steps its
//! state, then draws it into a [`Frame`].

use crate::color::Rgb;
use crate::frame::Frame;
use crate::math::isqrt;
use crate::palette::{HEAT, Palette};
use crate::rng::Rng;

/// A fire burning up the strip from pixel 0: every cell holds a heat that cools, rises (heat
/// moves away from the input, spreading as it goes) and is fed by random sparks at the base,
/// then shows as a colour from a heat palette (black, red, orange, yellow, white). The classic
/// 1-D fire algorithm; on an RGBW strip the white-hot base lights the W LEDs.
///
/// One byte of state per pixel. Step it at a steady rate (25 to 60 times a second); a vertical
/// strip with the input at the bottom looks best.
#[derive(Clone, Debug)]
pub struct Fire<const NUM: usize> {
    heat: [u8; NUM],
    /// How fast the air cools (higher: shorter flames). 55 is a good start.
    pub cooling: u8,
    /// How often a spark feeds the base, out of 255 per step (higher: a busier fire). 120 is a
    /// good start.
    pub sparking: u8,
    /// The colours by heat; [`HEAT`] by default.
    pub palette: &'static Palette,
}

impl<const NUM: usize> Default for Fire<NUM> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const NUM: usize> Fire<NUM> {
    /// A cold fire with the default cooling, sparking and palette.
    pub const fn new() -> Self {
        Self {
            heat: [0; NUM],
            cooling: 55,
            sparking: 120,
            palette: &HEAT,
        }
    }

    /// The heat of each cell (0..=255), pixel 0 first.
    pub fn heat(&self) -> &[u8; NUM] {
        &self.heat
    }

    /// One step: cool, rise, spark.
    pub fn step(&mut self, rng: &mut Rng) {
        if NUM == 0 {
            return;
        }
        // Cool every cell a little; the cooling per step scales with the length so a long
        // strip still gets tall flames.
        let max_cool = (self.cooling as u32 * 10 / NUM as u32 + 2).min(255); // a u8 draw below
        for h in &mut self.heat {
            *h = h.saturating_sub(rng.below(max_cool + 1) as u8);
        }
        // Heat rises: each cell takes from the two below it, weighted toward the farther one so
        // the heat drifts upward while it spreads.
        for k in (2..NUM).rev() {
            self.heat[k] = ((self.heat[k - 1] as u16 + 2 * self.heat[k - 2] as u16) / 3) as u8;
        }
        // Sparks near the base.
        if rng.chance(self.sparking) {
            let y = rng.below(NUM.min(7) as u32) as usize;
            self.heat[y] = self.heat[y].saturating_add(160 + rng.below(96) as u8);
        }
    }

    /// Draw the heat into `frame` (replaces every pixel).
    pub fn draw(&self, frame: &mut Frame<NUM>) {
        for (p, &h) in frame.pixels_mut().iter_mut().zip(&self.heat) {
            *p = self.palette.color(h);
        }
    }
}

/// Position units per pixel (positions are fixed point, 20 fractional bits).
const UNIT: i64 = 1 << 20;

/// Balls bouncing on pixel 0 (the floor) under gravity, each losing a little energy at every
/// bounce until it is relaunched to the top, all slightly out of step. Time-based: step it with
/// the milliseconds since the last step, so the motion is real-time whatever the frame rate.
#[derive(Clone, Debug)]
pub struct Balls<const K: usize> {
    height: [i64; K],   // position, UNIT per pixel
    velocity: [i64; K], // UNIT per ms, up positive
    keep: [i64; K],     // speed kept at a bounce, of 65536
    /// Each ball's colour.
    pub colors: [Rgb; K],
    gravity: i64, // UNIT per ms²
    launch: i64,  // the speed that reaches the top
    top: i64,
}

impl<const K: usize> Balls<K> {
    /// Balls on a strip of `len` pixels that fall from the top to the floor in `fall_ms`
    /// milliseconds, in `colors`. They start on the floor, launched at slightly different speeds.
    pub fn new(len: usize, fall_ms: u32, colors: [Rgb; K]) -> Self {
        let top = len.saturating_sub(1) as i64 * UNIT;
        let t = fall_ms.clamp(1, 600_000) as i64; // up to 10 minutes: t² stays far from overflow
        let gravity = (2 * top / (t * t)).max(1); // top = g t² / 2
        let launch = isqrt((2 * gravity * top) as u64) as i64; // v² = 2 g h
        let mut velocity = [0; K];
        let mut keep = [0; K];
        for k in 0..K {
            velocity[k] = launch * (100 - 6 * (k as i64 % 8)) / 100;
            keep[k] = 65536 * (90 - 4 * (k as i64 % 5)) / 100; // 90 % down to 74 %
        }
        Self {
            height: [0; K],
            velocity,
            keep,
            colors,
            gravity,
            launch,
            top,
        }
    }

    /// Advance the motion by `dt_ms` milliseconds.
    pub fn step(&mut self, dt_ms: u32) {
        let dt = dt_ms.min(100) as i64; // a long stall must not tunnel a ball through the floor
        for k in 0..K {
            // Semi-implicit Euler: velocity first, then position (stable and energy-tight).
            self.velocity[k] -= self.gravity * dt;
            self.height[k] += self.velocity[k] * dt;
            if self.height[k] <= 0 {
                self.height[k] = 0;
                let v = -self.velocity[k] * self.keep[k] / 65536;
                // Too tired to bounce visibly: launch it again.
                self.velocity[k] = if v < self.launch / 6 { self.launch } else { v };
            } else if self.height[k] > self.top {
                self.height[k] = self.top;
                self.velocity[k] = -self.velocity[k].abs();
            }
        }
    }

    /// Ball `k`'s height in pixels (fractional, 1/256 pixel).
    pub fn height_256(&self, k: usize) -> i32 {
        (self.height[k] >> 12) as i32
    }

    /// Draw the balls into `frame` (clears it first; overlapping balls add).
    pub fn draw<const NUM: usize>(&self, frame: &mut Frame<NUM>) {
        frame.clear();
        for k in 0..K {
            frame.dot(self.height_256(k) + 128, 384, self.colors[k]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{BLUE, OFF, RED};

    #[test]
    fn fire_heats_the_base_and_rises() {
        let mut fire = Fire::<60>::new();
        let mut rng = Rng::new(3);
        for _ in 0..300 {
            fire.step(&mut rng);
        }
        let base: u32 = fire.heat()[..10].iter().map(|&h| h as u32).sum();
        let top: u32 = fire.heat()[50..].iter().map(|&h| h as u32).sum();
        assert!(base > top, "base {base} top {top}");
        assert!(base > 0);
        let mut f = Frame::<60>::new();
        fire.draw(&mut f);
        assert!(f.pixels()[..10].iter().any(|&p| p != OFF));
    }

    #[test]
    fn fire_dies_without_sparks() {
        let mut fire = Fire::<30>::new();
        let mut rng = Rng::new(5);
        for _ in 0..100 {
            fire.step(&mut rng);
        }
        fire.sparking = 0;
        for _ in 0..1000 {
            fire.step(&mut rng);
        }
        assert!(fire.heat().iter().all(|&h| h == 0));
    }

    #[test]
    fn balls_stay_on_the_strip_and_keep_bouncing() {
        let mut balls = Balls::new(144, 1200, [RED, BLUE, RED]);
        let mut highest = 0;
        let mut floor_hits = 0;
        for _ in 0..3000 {
            balls.step(20);
            for k in 0..3 {
                let h = balls.height_256(k);
                assert!((0..=143 * 256).contains(&h), "ball {k} at {h}");
                highest = highest.max(h);
                if h == 0 {
                    floor_hits += 1;
                }
            }
        }
        assert!(highest > 120 * 256, "never got high: {highest}");
        assert!(floor_hits > 30, "{floor_hits}");
    }

    #[test]
    fn a_ball_reaches_the_top_in_about_the_fall_time() {
        let mut balls = Balls::new(101, 1000, [RED]);
        let mut ms = 0;
        while balls.height_256(0) < 99 * 256 {
            balls.step(10);
            ms += 10;
            assert!(ms < 2000);
        }
        assert!((800..=1100).contains(&ms), "{ms} ms");
    }

    #[test]
    fn fire_cooling_stays_meaningful_on_short_strips() {
        // 200 * 10 / 4 + 2 = 502 would wrap as a u8 draw; capped, a hot cell still cools.
        let mut fire = Fire::<4>::new();
        fire.cooling = 200;
        fire.sparking = 0;
        fire.heat = [255; 4];
        let mut rng = Rng::new(11);
        for _ in 0..50 {
            fire.step(&mut rng);
        }
        assert!(fire.heat().iter().all(|&h| h < 255));
    }

    #[test]
    fn extreme_fall_times_do_not_overflow() {
        let mut balls = Balls::new(144, u32::MAX, [RED]);
        balls.step(100);
        let mut balls = Balls::new(144, 0, [RED]);
        balls.step(100);
    }

    #[test]
    fn balls_draw_as_dots() {
        let balls = Balls::new(20, 500, [RED]);
        let mut f = Frame::<20>::new();
        balls.draw(&mut f);
        assert_eq!(f.get(0), RED);
        assert!(f.get(1).r > 0 && f.get(2) == OFF);
    }
}
