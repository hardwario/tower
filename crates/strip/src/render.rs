//! The render pipeline: from a frame of intent to the levels sent on the wire.
//!
//! Per pixel, in order:
//!
//! 1. **Orientation.** [`Render::reversed`] sends the frame last pixel first, for a strip
//!    mounted the other way round.
//! 2. **Brightness and gamma.** Each channel's intent times the global brightness goes through
//!    the [`Gamma`] curve to *linear light*, kept in 8.8 fixed point (a fraction of a level).
//! 3. **White extraction** (RGBW only). The white part, `w = min(r, g, b)` in linear light, moves
//!    to the W LED and the colour channels give it up. Light adds linearly, so the colour stays
//!    the same (up to the tint of the white LED); white lights the real white LED instead of three
//!    colour dies, at about a third of the current. Saturated colours are unchanged.
//! 4. **Current limit.** With [`Render::power_limit_ma`] set, a frame whose estimated current
//!    (from its linear light and the [`PowerModel`]) exceeds the limit is scaled down as a whole,
//!    in linear light, so its colours keep their balance.
//! 5. **Dithering** and quantization. The LEDs take 8-bit levels, and after gamma the dim end
//!    gets only a few (1, 2, 3 of 255 are big steps to the eye), so a fade near black visibly
//!    steps. With [`Render::dither`] on, each channel's fraction rounds up or down against a fixed
//!    per-pixel threshold (the bit-reversed pixel index, spread evenly along any run of pixels): a
//!    uniform level of 1.5 lights half the pixels at 1 and half at 2, so the strip's overall
//!    brightness moves in steps up to 256 times finer. The pattern is fixed (spatial only), so
//!    it never flickers; the cost is that a static colour at a fractional level is not perfectly
//!    uniform pixel to pixel (neighbours differ by one level). Off rounds to the nearest level.

use crate::color::{Rgb, Rgbw};
use crate::gamma::Gamma;

/// Which LED type the strip has (sets the wire format).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LedKind {
    /// WS2812B: 24-bit RGB.
    Rgb,
    /// SK6812 RGBW: 32-bit, the white part of each colour drives the W LED.
    Rgbw,
}

/// How much current the strip draws, for the limiter and the estimate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PowerModel {
    /// One channel (one LED die) at full duty, in mA.
    pub ma_per_channel: u16,
    /// What one LED's controller draws even when dark, in µA.
    pub idle_ua_per_led: u16,
}

impl PowerModel {
    /// Conservative figures for WS2812B and SK6812 at 5 V: 20 mA a channel (the datasheets'
    /// maximum; a real strip often draws less) and 1 mA idle per LED. Overestimating keeps the
    /// limiter on the safe side.
    pub const DEFAULT: Self = Self {
        ma_per_channel: 20,
        idle_ua_per_led: 1000,
    };
}

impl Default for PowerModel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Fixed-point one for the [`power_factor`](Render::power_factor) (16.16).
pub const FACTOR_ONE: u32 = 1 << 16;

/// The render settings of a strip (see the module docs for the pipeline).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Render {
    pub kind: LedKind,
    /// Global brightness, 0 to 100 % (more counts as 100). Applied to intent, before gamma, so it
    /// is roughly perceptual (with gamma 2.0, 50 % looks about half as bright).
    pub brightness: u8,
    pub gamma: Gamma,
    /// Spatial dithering (on by default).
    pub dither: bool,
    /// Send the frame last pixel first (the strip is mounted the other way round).
    pub reversed: bool,
    /// The current budget in mA for the whole strip, idle draw included; 0 = no limit.
    pub power_limit_ma: u32,
    pub power_model: PowerModel,
}

impl Render {
    /// Defaults: gamma 2.0, dithering on, not reversed, no current limit.
    pub const fn new(kind: LedKind, brightness: u8) -> Self {
        Self {
            kind,
            brightness,
            gamma: Gamma::G2_0,
            dither: true,
            reversed: false,
            power_limit_ma: 0,
            power_model: PowerModel::DEFAULT,
        }
    }

    /// Brightness as an 8.8 multiplier (256 = 100 %).
    fn scale(&self) -> u32 {
        (self.brightness.min(100) as u32 * 256 + 50) / 100
    }

    /// Linear light (8.8 per channel: r, g, b, w) of one intent pixel, white extracted on RGBW.
    fn light(&self, p: Rgb, scale: u32) -> [u32; 4] {
        let g = self.gamma;
        let (r, gr, b) = (
            g.light(p.r as u32 * scale),
            g.light(p.g as u32 * scale),
            g.light(p.b as u32 * scale),
        );
        match self.kind {
            LedKind::Rgb => [r, gr, b, 0],
            LedKind::Rgbw => {
                let w = r.min(gr).min(b);
                [r - w, gr - w, b - w, w]
            }
        }
    }

    /// The strip's idle draw (dark LEDs), in mA.
    pub fn idle_ma(&self, leds: usize) -> u32 {
        (leds as u64 * self.power_model.idle_ua_per_led as u64 / 1000) as u32
    }

    /// The current of `frame` as rendered without a limit, idle draw included, in mA.
    pub fn estimate_ma(&self, frame: &[Rgb]) -> u32 {
        let scale = self.scale();
        let light: u64 = frame
            .iter()
            .map(|&p| self.light(p, scale).iter().sum::<u32>() as u64)
            .sum();
        self.idle_ma(frame.len()) + self.light_ma(light)
    }

    /// mA of a sum of linear light (8.8, 65280 = one channel at full duty).
    fn light_ma(&self, light: u64) -> u32 {
        (light * self.power_model.ma_per_channel as u64 / 65280) as u32
    }

    /// mA of a sum of output levels (255 = one channel at full duty), idle draw included: the
    /// current of a frame as it went out, from the levels the encoder saw.
    pub fn levels_ma(&self, levels: u32, leds: usize) -> u32 {
        self.idle_ma(leds) + (levels as u64 * self.power_model.ma_per_channel as u64 / 255) as u32
    }

    /// The factor (16.16, [`FACTOR_ONE`] = no change) that brings `frame` within the current
    /// limit; [`FACTOR_ONE`] without a limit or when it already fits. A limit below the idle draw
    /// gives 0 (all dark: the most the strip can do).
    pub fn power_factor(&self, frame: &[Rgb]) -> u32 {
        if self.power_limit_ma == 0 {
            return FACTOR_ONE;
        }
        let scale = self.scale();
        let light: u64 = frame
            .iter()
            .map(|&p| self.light(p, scale).iter().sum::<u32>() as u64)
            .sum();
        let dynamic = self.light_ma(light) as u64;
        let budget = self.power_limit_ma.saturating_sub(self.idle_ma(frame.len())) as u64;
        if dynamic <= budget {
            FACTOR_ONE
        } else {
            (budget * FACTOR_ONE as u64 / dynamic) as u32
        }
    }

    /// The wire pixels of `frame`, in wire order, with `factor` from
    /// [`power_factor`](Self::power_factor) (or [`FACTOR_ONE`]). On an RGB strip `w` is 0.
    pub fn output<'a>(&'a self, frame: &'a [Rgb], factor: u32) -> impl Iterator<Item = Rgbw> + 'a {
        let scale = self.scale();
        let n = frame.len();
        (0..n).map(move |k| {
            let i = if self.reversed { n - 1 - k } else { k };
            let mut l = self.light(frame[i], scale);
            if factor < FACTOR_ONE {
                for c in &mut l {
                    *c = (*c * factor) >> 16; // fits: 65280 * 65535 < 2^32
                }
            }
            let t = if self.dither { threshold(k) } else { 128 };
            Rgbw::new(
                quantize(l[0], t),
                quantize(l[1], t),
                quantize(l[2], t),
                quantize(l[3], t),
            )
        })
    }
}

/// 8.8 linear light to the 8-bit output level: the fraction rounds up when it reaches
/// `threshold` (`0..=255`; 128 = round to nearest). Full scale stays 255.
fn quantize(light: u32, threshold: u32) -> u8 {
    ((light + threshold) >> 8).min(255) as u8
}

/// Ordered-dither threshold of the pixel at wire position `k` (`0..=255`): the bit-reversed
/// position, so any run of consecutive pixels holds thresholds spread evenly over the range.
/// From a table: the M0+ has no bit-reverse instruction.
fn threshold(k: usize) -> u32 {
    BIT_REVERSED[k & 0xFF] as u32
}

static BIT_REVERSED: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = (i as u8).reverse_bits();
        i += 1;
    }
    table
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{BLUE, OFF, RED, WHITE};
    use std::vec::Vec;

    fn out(r: &Render, frame: &[Rgb]) -> Vec<Rgbw> {
        r.output(frame, r.power_factor(frame)).collect()
    }

    #[test]
    fn full_and_black_are_exact_with_or_without_dither() {
        for dither in [true, false] {
            for g in Gamma::ALL {
                let mut r = Render::new(LedKind::Rgb, 100);
                r.dither = dither;
                r.gamma = g;
                assert!(
                    out(&r, &[WHITE; 300])
                        .iter()
                        .all(|p| *p == Rgbw::new(255, 255, 255, 0))
                );
                assert!(out(&r, &[OFF; 300]).iter().all(|p| *p == Rgbw::default()));
            }
        }
    }

    #[test]
    fn brightness_zero_is_dark() {
        let r = Render::new(LedKind::Rgbw, 0);
        assert!(out(&r, &[WHITE; 50]).iter().all(|p| *p == Rgbw::default()));
    }

    #[test]
    fn rgbw_moves_the_white_part() {
        let r = Render::new(LedKind::Rgbw, 100);
        assert_eq!(out(&r, &[WHITE])[0], Rgbw::new(0, 0, 0, 255));
        assert_eq!(out(&r, &[RED])[0], Rgbw::new(255, 0, 0, 0));
        let p = out(&r, &[Rgb::new(255, 128, 0)])[0];
        assert_eq!((p.r, p.b, p.w), (255, 0, 0)); // no white in a colour with a zero channel
        // Pink: the common part goes to W, the rest stays red.
        let p = out(&r, &[Rgb::new(255, 128, 128)])[0];
        assert_eq!((p.g, p.b), (0, 0));
        assert!(p.w > 50 && p.r > 150, "{p:?}");
    }

    #[test]
    fn dithering_keeps_the_average_light() {
        // A uniform level of light between two output levels: the average over the strip matches
        // it to within 1/256 of a level per 256 pixels.
        let mut r = Render::new(LedKind::Rgb, 100);
        r.gamma = Gamma::Linear;
        for v in [1u8, 3, 10, 77] {
            let frame = [Rgb::new(v, 0, 0); 256];
            // Brightness 30 %: intent v * 0.3 (8.8) is a fractional level.
            r.brightness = 30;
            let exact = (v as u32 * 77) as f64 / 256.0; // scale = round(30 * 256 / 100) = 77
            let px = out(&r, &frame);
            let avg = px.iter().map(|p| p.r as f64).sum::<f64>() / px.len() as f64;
            assert!((avg - exact).abs() < 0.01, "v={v}: {avg} vs {exact}");
            // Neighbours differ by at most one level.
            assert!(px.iter().all(|p| (p.r as f64 - exact).abs() < 1.0));
        }
    }

    #[test]
    fn without_dither_every_pixel_rounds_alike() {
        let mut r = Render::new(LedKind::Rgb, 30);
        r.dither = false;
        let px = out(&r, &[Rgb::new(100, 0, 0); 64]);
        assert!(px.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn reversed_sends_last_pixel_first() {
        let mut r = Render::new(LedKind::Rgb, 100);
        r.dither = false;
        let frame = [RED, OFF, OFF, BLUE];
        assert_eq!(out(&r, &frame)[0], Rgbw::new(255, 0, 0, 0));
        r.reversed = true;
        let px = out(&r, &frame);
        assert_eq!(px[0], Rgbw::new(0, 0, 255, 0));
        assert_eq!(px[3], Rgbw::new(255, 0, 0, 0));
    }

    #[test]
    fn estimate_matches_the_model() {
        let r = Render::new(LedKind::Rgb, 100);
        // 10 white pixels: 30 channels at 20 mA + 10 x 1 mA idle.
        assert_eq!(r.estimate_ma(&[WHITE; 10]), 610);
        assert_eq!(r.estimate_ma(&[OFF; 10]), 10);
        // On RGBW the same white is one channel per pixel.
        let w = Render::new(LedKind::Rgbw, 100);
        assert_eq!(w.estimate_ma(&[WHITE; 10]), 210);
        assert_eq!(r.levels_ma(30 * 255, 10), 610);
    }

    #[test]
    fn limiter_keeps_within_budget() {
        let mut r = Render::new(LedKind::Rgb, 100);
        let frame = [WHITE; 144]; // 144 * 60 + 144 = 8784 mA unlimited
        assert_eq!(r.estimate_ma(&frame), 8784);
        r.power_limit_ma = 2000;
        let f = r.power_factor(&frame);
        assert!(f < FACTOR_ONE);
        let levels: u32 = r
            .output(&frame, f)
            .map(|p| p.r as u32 + p.g as u32 + p.b as u32)
            .sum();
        let ma = r.levels_ma(levels, frame.len());
        assert!(ma <= 2000 && ma > 1900, "{ma} mA");
        // A frame within the budget is untouched.
        assert_eq!(r.power_factor(&[RED; 10]), FACTOR_ONE);
        // A budget below the idle draw darkens everything.
        r.power_limit_ma = 100;
        assert_eq!(r.power_factor(&frame), 0);
        assert!(r.output(&frame, 0).all(|p| p == Rgbw::default()));
    }
}
