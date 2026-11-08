//! [`Frame`]: one frame of the strip as RGB intent, with composition and the animated effects.
//!
//! An effect fills the whole frame for a step counter `t` (see the crate docs, "Animation
//! time"); compose further on top (a [`dot`](Frame::dot), a [`bar`](Frame::bar)) and hand the
//! frame to a renderer. Pixel 0 is the input end; moving effects travel away from it.

use crate::color::{OFF, Rgb, hue};
use crate::math::{bump, half_sine, hash, noise, sin8};
use crate::palette::{Palette, SUNRISE};
use crate::rng::Rng;

/// One run of a [compound fill](Frame::segments): `len` pixels of `color`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Segment {
    pub len: usize,
    pub color: Rgb,
}

impl Segment {
    pub const fn new(len: usize, color: Rgb) -> Self {
        Self { len, color }
    }
}

/// One frame of `NUM` pixels.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Frame<const NUM: usize> {
    px: [Rgb; NUM],
}

impl<const NUM: usize> Default for Frame<NUM> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const NUM: usize> Frame<NUM> {
    /// All pixels off.
    pub const fn new() -> Self {
        Self { px: [OFF; NUM] }
    }

    /// Number of pixels.
    pub const fn len(&self) -> usize {
        NUM
    }

    /// Whether the frame has no pixels (`NUM == 0`).
    pub const fn is_empty(&self) -> bool {
        NUM == 0
    }

    /// The pixels, pixel 0 first.
    pub fn pixels(&self) -> &[Rgb; NUM] {
        &self.px
    }

    /// The pixels, for effects of your own.
    pub fn pixels_mut(&mut self) -> &mut [Rgb; NUM] {
        &mut self.px
    }

    /// Pixel `i` ([`OFF`] out of range).
    pub fn get(&self, i: usize) -> Rgb {
        self.px.get(i).copied().unwrap_or(OFF)
    }

    /// Set pixel `i` (out of range is ignored).
    pub fn set(&mut self, i: usize, color: Rgb) {
        if let Some(p) = self.px.get_mut(i) {
            *p = color;
        }
    }

    // --- Composition ----------------------------------------------------------------------

    /// All pixels off.
    pub fn clear(&mut self) {
        self.px = [OFF; NUM];
    }

    /// One colour across the strip.
    pub fn fill(&mut self, color: Rgb) {
        self.px = [color; NUM];
    }

    /// Consecutive runs, e.g. `&[Segment::new(20, RED), Segment::new(80, GREEN)]`. Pixels past
    /// the last run are cleared; runs past the end of the strip are cut.
    pub fn segments(&mut self, segments: &[Segment]) {
        let mut i = 0usize;
        for seg in segments {
            let end = i.saturating_add(seg.len).min(NUM);
            self.px[i..end].fill(seg.color);
            i = end;
        }
        self.px[i..].fill(OFF);
    }

    /// A linear blend from `from` (pixel 0) to `to` (the last pixel).
    pub fn gradient(&mut self, from: Rgb, to: Rgb) {
        let last = NUM.saturating_sub(1).max(1);
        for (i, p) in self.px.iter_mut().enumerate() {
            *p = from.blend(to, (i * 255 / last) as u8);
        }
    }

    /// `palette` spread once along the strip, starting at palette index `offset`.
    pub fn fill_palette(&mut self, palette: &Palette, offset: u8) {
        let step = Self::index_step();
        for (i, p) in self.px.iter_mut().enumerate() {
            *p = palette.color((((i as u32 * step) >> 16) as u8).wrapping_add(offset));
        }
    }

    /// 256 / NUM in 16.16: pixel `i` sits at index `i * step >> 16` of a 0..256 run along the
    /// strip (one division per frame instead of one per pixel).
    fn index_step() -> u32 {
        (256 << 16) / NUM.max(1) as u32
    }

    /// Dim every pixel by `by / 255` (rounded so that repeating it reaches black): the trail of
    /// an effect that redraws only its moving parts.
    pub fn fade(&mut self, by: u8) {
        let keep = 255 - by;
        for p in &mut self.px {
            *p = p.scale(keep);
        }
    }

    /// Spread light to the neighbours: each pixel keeps `255 - amount` of itself and gets
    /// `amount / 2` of each neighbour (a 1-D blur; the ends lose a little to the outside).
    pub fn blur(&mut self, amount: u8) {
        let keep = 255 - amount as u32;
        let share = amount as u32 / 2;
        let mut prev = OFF; // the left neighbour's original value
        for i in 0..NUM {
            let cur = self.px[i];
            let next = if i + 1 < NUM { self.px[i + 1] } else { OFF };
            let mix = |c: u8, l: u8, r: u8| ((c as u32 * keep + (l as u32 + r as u32) * share) / 255) as u8;
            self.px[i] = Rgb::new(
                mix(cur.r, prev.r, next.r),
                mix(cur.g, prev.g, next.g),
                mix(cur.b, prev.b, next.b),
            );
            prev = cur;
        }
    }

    /// Copy the first half onto the second, mirrored, so the strip is symmetric about its centre.
    pub fn mirror(&mut self) {
        for i in 0..NUM / 2 {
            self.px[NUM - 1 - i] = self.px[i];
        }
    }

    /// A bar from pixel 0 covering `level / 65535` of the strip, coloured by position from `from`
    /// to `to` (so its colour also shows the level); the pixel at the bar's end is lit by the
    /// fraction it covers, so the bar grows smoothly. Clears the rest.
    pub fn bar(&mut self, level: u16, from: Rgb, to: Rgb) {
        let len = level as u32 * NUM as u32; // in 1/65536 pixel
        let (full, frac) = ((len >> 16) as usize, ((len >> 8) & 0xFF) as u8);
        let last = NUM.saturating_sub(1).max(1);
        for (i, p) in self.px.iter_mut().enumerate() {
            let c = from.blend(to, (i * 255 / last) as u8);
            *p = match i.cmp(&full) {
                core::cmp::Ordering::Less => c,
                core::cmp::Ordering::Equal => c.scale(frac),
                core::cmp::Ordering::Greater => OFF,
            };
        }
    }

    /// Add an anti-aliased dot: centred at `center` (in 1/256 pixel, so it can sit between
    /// pixels and move smoothly), fading linearly to nothing at `radius` (1/256 pixel). Light
    /// adds, so overlapping dots brighten.
    pub fn dot(&mut self, center: i32, radius: u32, color: Rgb) {
        // i64 throughout: any center and radius, no overflow.
        let (center, radius) = (center as i64, radius.clamp(1, 1 << 24) as i64);
        let first = ((center - radius) >> 8).max(0);
        let last = ((center + radius) >> 8).min(NUM as i64 - 1);
        for i in first..=last {
            let d = (i * 256 + 128 - center).abs(); // pixel centre to dot centre
            if d < radius {
                let w = (255 * (radius - d) / radius) as u8;
                self.px[i as usize] = self.px[i as usize].add(color.scale(w));
            }
        }
    }

    // --- Effects: pure functions of `t` -----------------------------------------------------

    /// The colour wheel once along the strip, scrolling away from the input by one hue step per
    /// `t`. Constant power ([`hue`]), so no dark bands between the primaries.
    pub fn rainbow(&mut self, t: u32) {
        let step = Self::index_step();
        for (i, p) in self.px.iter_mut().enumerate() {
            let h = ((i as u32 * step) >> 16).wrapping_sub(t);
            *p = hue(h as u8);
        }
    }

    /// `palette` once along the strip, scrolling away from the input by one index per `t`.
    pub fn palette_scroll(&mut self, palette: &Palette, t: u32) {
        let step = Self::index_step();
        for (i, p) in self.px.iter_mut().enumerate() {
            let idx = ((i as u32 * step) >> 16).wrapping_sub(t);
            *p = palette.color(idx as u8);
        }
    }

    /// Fill one pixel per `t`, then start over.
    pub fn color_wipe(&mut self, color: Rgb, t: u32) {
        let lit = (t % (NUM as u32 + 1)) as usize;
        for (i, p) in self.px.iter_mut().enumerate() {
            *p = if i < lit { color } else { OFF };
        }
    }

    /// Fill one pixel per `t`, then empty from the input end the same way: `2 * NUM` steps a
    /// cycle.
    pub fn wipe_in_out(&mut self, color: Rgb, t: u32) {
        let n = NUM as u32;
        let x = t % (2 * n).max(1);
        for (i, p) in self.px.iter_mut().enumerate() {
            let i = i as u32;
            let lit = if x < n { i <= x } else { i > x - n };
            *p = if lit { color } else { OFF };
        }
    }

    /// A curtain: opens from the centre to both ends one pixel per side per `t`, then closes
    /// back to the centre.
    pub fn curtain(&mut self, color: Rgb, t: u32) {
        if NUM == 0 {
            return;
        }
        let half = (NUM as u32).div_ceil(2);
        let x = t % (2 * half);
        let width = if x < half { x + 1 } else { 2 * half - 1 - x }; // lit pixels per side
        for (i, p) in self.px.iter_mut().enumerate() {
            // Distance from the centre in half pixels (0 or 1 for the middle pixel(s)).
            let d = (2 * i as i32 - (NUM as i32 - 1)).unsigned_abs();
            *p = if d < 2 * width { color } else { OFF };
        }
    }

    /// Marquee: every third pixel lit, moving away from the input one pixel per `t`.
    pub fn theater_chase(&mut self, color: Rgb, t: u32) {
        let mut k = (3 - t % 3) % 3; // pixel i is lit when (i + k) % 3 == 0; counted, not divided
        for p in &mut self.px {
            *p = if k == 0 { color } else { OFF };
            k = if k == 2 { 0 } else { k + 1 };
        }
    }

    /// Waves of brightness rolling away from the input: a 16-pixel sine wave moving a quarter of
    /// a pixel per `t`.
    pub fn running_lights(&mut self, color: Rgb, t: u32) {
        for (i, p) in self.px.iter_mut().enumerate() {
            let phase = (i as u32 * 16).wrapping_sub(t.wrapping_mul(4));
            *p = color.scale(sin8(phase as u8));
        }
    }

    /// The whole strip breathing: a raised cosine of 512 steps of `t` (eased at the top and at
    /// black, like breath, rather than bouncing off them).
    pub fn breathe(&mut self, color: Rgb, t: u32) {
        let s = half_sine(t % 512) as u32;
        self.fill(color.scale((s * s / 255) as u8));
    }

    /// Larson / Cylon scanner: a head bouncing end to end one pixel per `t`, with a trail
    /// `tail` pixels long behind it that fades out (the trail follows the head's path, so it
    /// folds back over itself at the ends).
    pub fn scanner(&mut self, color: Rgb, t: u32, tail: u32) {
        let n = NUM as u32;
        let span = (2 * n).saturating_sub(2).max(1); // one round trip
        let x = t % span;
        let tail = tail.max(1);
        for (i, p) in self.px.iter_mut().enumerate() {
            let i = i as u32;
            // When the head was last here: on the way out (phase i) or back (phase span - i).
            let ago = |visit: u32| (x + span - visit % span) % span;
            let age = ago(i).min(ago(span - i));
            *p = if age < tail {
                color.scale((255 * (tail - age) / tail) as u8)
            } else {
                OFF
            };
        }
    }

    /// Stars: each pixel twinkles on its own (a smooth rise and fall of about 96 steps of `t`,
    /// at its own random brightness), lit `density / 255` of the time. Stateless: the timing
    /// comes from a hash of the pixel index.
    pub fn twinkle(&mut self, color: Rgb, t: u32, density: u8) {
        const ON: u32 = 96; // steps one twinkle lasts
        let period = ON * 256 / density.max(1) as u32;
        for (i, p) in self.px.iter_mut().enumerate() {
            let h = hash(i as u32 ^ 0x7a1e_5eed);
            let period = period + (h & 0xFFFF) % (period / 2 + 1); // up to 50 % slower, out of step
            let local = t.wrapping_add(h >> 12) % period;
            let peak = 96 + (h >> 24) * 159 / 255; // 96..=255
            *p = color.scale((bump(local, ON) as u32 * peak / 255) as u8);
        }
    }

    /// Candle light: warm flames flickering on their own, with a slow wander and a quick
    /// shimmer from smooth noise; dimmer moments turn redder, as a real flame does.
    pub fn candle(&mut self, t: u32) {
        const EMBER: Rgb = Rgb::new(255, 40, 0);
        const FLAME: Rgb = Rgb::new(255, 150, 40);
        for (i, p) in self.px.iter_mut().enumerate() {
            let i = i as u32;
            // Neighbours sample nearby noise (90/256 of a lattice step apart), so flames span a
            // few pixels; the quick shimmer is per pixel.
            let slow = noise(t.wrapping_mul(10).wrapping_add(i * 90)) as u32;
            let quick = noise(t.wrapping_mul(45).wrapping_add(i * 1000).wrapping_add(1 << 30)) as u32;
            let level = (slow * 200 + quick * 55) / 255; // 0..=255
            let c = EMBER.blend(FLAME, level as u8);
            *p = c.scale((110 + level * 145 / 255) as u8);
        }
    }

    /// Plasma: two sine waves crossing at different speeds, the sum looked up in `palette`, for
    /// a slow organic flow.
    pub fn plasma(&mut self, palette: &Palette, t: u32) {
        for (i, p) in self.px.iter_mut().enumerate() {
            let i = i as u32;
            let a = sin8((i * 4).wrapping_add(t) as u8) as u32;
            let b = sin8((i * 7 / 2).wrapping_sub(t.wrapping_mul(2)).wrapping_add(64) as u8) as u32;
            let idx = (a + b) / 2 + t / 4;
            *p = palette.color(idx as u8);
        }
    }

    /// A sunrise: the whole strip at `progress` (0 = night, 65535 = daylight) along the
    /// [`SUNRISE`] palette, from black through deep red and orange to a warm near-white. 16-bit,
    /// so a 30-minute wake-up light moves in steps far below one level; render it with dithering
    /// (and gamma 2.8 for an even longer dark start).
    pub fn sunrise(&mut self, progress: u16) {
        self.fill(SUNRISE.color16(progress));
    }

    /// A clock face along the strip, pixel 0 at twelve: faint hour marks, the hour as a red dot,
    /// the minute green, the second blue, each moving smoothly between pixels.
    pub fn clock(&mut self, hours: u8, minutes: u8, seconds: u8) {
        self.clear();
        let n = NUM as u32;
        for k in 0..12 {
            self.set((k * n / 12) as usize, Rgb::new(24, 24, 24));
        }
        let (h, m, s) = ((hours % 12) as u32, (minutes % 60) as u32, (seconds % 60) as u32);
        // Positions in 1/256 pixel, at pixel centres.
        let at = |num: u32, den: u32| (num as u64 * n as u64 * 256 / den as u64) as i32 + 128;
        self.dot(at(h * 3600 + m * 60 + s, 43_200), 384, Rgb::new(255, 0, 0));
        self.dot(at(m * 60 + s, 3600), 320, Rgb::new(0, 255, 0));
        self.dot(at(s, 60), 256, Rgb::new(0, 0, 255));
    }

    // --- Effects that keep their trail in the frame ----------------------------------------

    /// Random flashes of `color` that fade out: each call dims everything a little and lights one
    /// random pixel. Draws over what is there, so call it every frame without clearing.
    pub fn sparkle(&mut self, color: Rgb, rng: &mut Rng) {
        self.fade(55);
        if NUM > 0 {
            let i = rng.below(NUM as u32) as usize;
            self.px[i] = color;
        }
    }

    /// A meteor: a `size`-pixel head running away from the input one pixel per `t`, leaving a
    /// sparkling tail that decays at random per pixel. Draws over what is there (the tail is the
    /// frame's history), so call it every frame without clearing; with `t` advancing more than
    /// `size` per frame the tail shows gaps.
    pub fn meteor(&mut self, color: Rgb, t: u32, size: u32, rng: &mut Rng) {
        let n = NUM as u32;
        let head = t % (n + n / 2).max(1); // the extra half lets the tail run off the end
        for p in &mut self.px {
            if rng.next_u32() & 1 == 0 {
                *p = p.scale(170);
            }
        }
        for k in 0..size.max(1) {
            if let Some(i) = head.checked_sub(k)
                && i < n
            {
                self.px[i as usize] = color;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{BLUE, GREEN, RED, WHITE};

    fn lit<const N: usize>(f: &Frame<N>) -> std::vec::Vec<usize> {
        (0..N).filter(|&i| f.get(i) != OFF).collect()
    }

    #[test]
    fn segments_and_gradient() {
        let mut f = Frame::<10>::new();
        f.fill(WHITE);
        f.segments(&[Segment::new(3, RED), Segment::new(4, GREEN)]);
        assert_eq!(f.get(2), RED);
        assert_eq!(f.get(3), GREEN);
        assert_eq!(f.get(7), OFF);
        f.segments(&[Segment::new(20, BLUE)]);
        assert!(f.pixels().iter().all(|&p| p == BLUE));
        f.gradient(RED, BLUE);
        assert_eq!(f.get(0), RED);
        assert_eq!(f.get(9), BLUE);
    }

    #[test]
    fn fade_reaches_black_and_blur_spreads() {
        let mut f = Frame::<5>::new();
        f.set(2, WHITE);
        f.blur(128);
        assert!(f.get(1).r > 0 && f.get(3).r > 0 && f.get(2).r < 255);
        assert_eq!(f.get(1), f.get(3));
        for _ in 0..200 {
            f.fade(30);
        }
        assert!(f.pixels().iter().all(|&p| p == OFF));
    }

    #[test]
    fn mirror_is_symmetric() {
        let mut f = Frame::<7>::new();
        f.gradient(RED, BLUE);
        f.mirror();
        for i in 0..7 {
            assert_eq!(f.get(i), f.get(6 - i));
        }
    }

    #[test]
    fn bar_grows_smoothly() {
        let mut f = Frame::<10>::new();
        f.bar(0, RED, RED);
        assert!(lit(&f).is_empty());
        f.bar(u16::MAX, RED, RED);
        assert_eq!(lit(&f).len(), 10);
        f.bar(32768, RED, RED); // half: 5 pixels full
        assert_eq!(f.get(4), RED);
        assert_eq!(f.get(5), OFF);
        f.bar(32768 + 3277, RED, RED); // 5.5 pixels: the sixth at half
        let r = f.get(5).r;
        assert!((120..=135).contains(&r), "{r}");
    }

    #[test]
    fn dot_is_brightest_at_its_centre() {
        let mut f = Frame::<10>::new();
        f.dot(4 * 256 + 128, 512, WHITE); // centred on pixel 4, radius 2
        assert_eq!(f.get(4), WHITE);
        assert!(f.get(3).r > 0 && f.get(3) == f.get(5));
        assert_eq!(f.get(2), OFF);
        // Between pixels 4 and 5: both half lit.
        let mut f = Frame::<10>::new();
        f.dot(5 * 256, 256, WHITE);
        assert_eq!(f.get(4), f.get(5));
        assert!(f.get(4).r > 100);
        // Off the ends and at the extremes: no panic, nothing outside.
        f.dot(-1000, 512, WHITE);
        f.dot(100 * 256, 512, WHITE);
        f.dot(i32::MIN, u32::MAX, WHITE);
        f.dot(i32::MAX, u32::MAX, WHITE);
    }

    #[test]
    fn rainbow_moves_away_from_the_input() {
        let mut f = Frame::<64>::new();
        f.rainbow(0);
        let c0 = f.get(10);
        f.rainbow(4); // 64 pixels over 256 hues: 4 hue steps = 1 pixel
        assert_eq!(f.get(11), c0);
    }

    #[test]
    fn wipes_and_curtain() {
        let mut f = Frame::<6>::new();
        f.wipe_in_out(RED, 0);
        assert_eq!(lit(&f), [0]);
        f.wipe_in_out(RED, 5);
        assert_eq!(lit(&f).len(), 6);
        f.wipe_in_out(RED, 6);
        assert_eq!(lit(&f), [1, 2, 3, 4, 5]);
        f.wipe_in_out(RED, 11);
        assert!(lit(&f).is_empty());
        f.curtain(RED, 0);
        assert_eq!(lit(&f), [2, 3]);
        f.curtain(RED, 2);
        assert_eq!(lit(&f).len(), 6);
        f.curtain(RED, 4); // closing: back to the middle pair, then dark for a step
        assert_eq!(lit(&f), [2, 3]);
        f.curtain(RED, 5);
        assert!(lit(&f).is_empty());
        let mut odd = Frame::<5>::new();
        odd.curtain(RED, 0);
        assert_eq!(lit(&odd), [2]);
        odd.curtain(RED, 2);
        assert_eq!(lit(&odd).len(), 5);
    }

    #[test]
    fn chase_lights_every_third() {
        let mut f = Frame::<9>::new();
        f.theater_chase(RED, 1);
        assert_eq!(lit(&f), [1, 4, 7]);
    }

    #[test]
    fn breathe_eases_from_black_to_full() {
        let mut f = Frame::<1>::new();
        f.breathe(WHITE, 0);
        assert_eq!(f.get(0), OFF);
        f.breathe(WHITE, 256);
        assert_eq!(f.get(0), WHITE);
        f.breathe(WHITE, 4);
        assert!(f.get(0).r <= 1, "slow start: {:?}", f.get(0));
    }

    #[test]
    fn scanner_trail_follows_the_head() {
        let mut f = Frame::<20>::new();
        f.scanner(RED, 10, 4); // heading out, head at 10
        assert_eq!(f.get(10), RED);
        assert!(f.get(9).r > f.get(8).r && f.get(8).r > f.get(7).r && f.get(7).r > 0);
        assert_eq!(f.get(6), OFF);
        assert_eq!(f.get(11), OFF); // nothing ahead of the head
        f.scanner(RED, 21, 4); // span 38: back at 38 - 21 = 17, heading home
        assert_eq!(f.get(17), RED);
        assert!(f.get(18).r > 0 && f.get(16) == OFF);
        f.scanner(RED, 19, 4); // at the far end: the trail folds back
        assert_eq!(f.get(19), RED);
        assert!(f.get(18).r > 0);
    }

    #[test]
    fn twinkle_is_partial_and_varies() {
        let mut f = Frame::<200>::new();
        f.twinkle(WHITE, 1000, 64);
        let n = lit(&f).len();
        assert!((10..120).contains(&n), "{n} lit");
        let before = f.clone();
        f.twinkle(WHITE, 1010, 64);
        assert_ne!(f, before);
    }

    #[test]
    fn candle_is_warm_and_never_dark() {
        let mut f = Frame::<50>::new();
        for t in 0..500 {
            f.candle(t);
            for p in f.pixels() {
                assert!(p.r >= 100 && p.b <= p.g && p.g <= p.r, "{p:?}");
            }
        }
    }

    #[test]
    fn clock_places_the_hands() {
        let mut f = Frame::<144>::new();
        f.clock(3, 0, 0); // hour hand a quarter round: pixel 36
        assert!(f.get(36).r > 200);
        f.clock(0, 30, 15); // minute at 30.25 / 60 of 144 = 72.6; second at 36
        assert!(f.get(72).g > 100 || f.get(73).g > 100);
        assert!(f.get(36).b > 200);
    }

    #[test]
    fn sparkle_and_meteor_draw() {
        let mut rng = Rng::new(1);
        let mut f = Frame::<30>::new();
        f.sparkle(WHITE, &mut rng);
        assert_eq!(lit(&f).len(), 1);
        let mut f = Frame::<30>::new();
        for t in 0..10 {
            f.meteor(WHITE, t, 3, &mut rng);
        }
        assert_eq!(f.get(9), WHITE);
        assert_eq!(f.get(7), WHITE);
        assert!(f.get(10) == OFF);
    }

    #[test]
    fn effects_cope_with_tiny_strips() {
        let mut rng = Rng::new(7);
        let mut one = Frame::<1>::new();
        let mut none = Frame::<0>::new();
        for t in [0u32, 1, 2, 1000, u32::MAX] {
            one.rainbow(t);
            one.scanner(RED, t, 3);
            one.curtain(RED, t);
            one.wipe_in_out(RED, t);
            one.meteor(RED, t, 2, &mut rng);
            one.clock(1, 2, 3);
            one.running_lights(RED, t);
            one.plasma(&crate::palette::LAVA, t);
            one.twinkle(RED, t, 10);
            one.candle(t);
            one.breathe(RED, t);
            none.rainbow(t);
            none.scanner(RED, t, 3);
            none.curtain(RED, t);
            none.wipe_in_out(RED, t);
            none.sparkle(RED, &mut rng);
            none.meteor(RED, t, 2, &mut rng);
            none.bar(1000, RED, BLUE);
            none.clock(1, 2, 3);
        }
    }
}
