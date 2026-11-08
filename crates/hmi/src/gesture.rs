//! Swipe direction from the four photodiode channels of an APDS-9960 style gesture engine.
//!
//! While a hand passes over the sensor, its gesture engine records datasets of four channel
//! levels (up, down, left, right photodiodes). The hand reaches the photodiodes on one side first
//! and leaves the opposite ones last. [`Decoder`] measures *when* each photodiode saw the hand: the
//! time-weighted center of its signal over the gesture (its centroid). The axis whose two centers
//! lie further apart, relative to the gesture's length, gives the direction; the order of the two
//! centers gives its sign.
//!
//! Timing beats comparing levels: close to the sensor every channel saturates (255) and the
//! balance between them carries no direction, and an IR LED placed off-center makes one side
//! read stronger than the other, so a swipe one way reads weaker than the other way. The centers
//! use every sample and neither effect moves them. The level swing between the first and the last
//! sample (the SparkFun library's method) is still computed, for comparison ([`Trace`]).
//!
//! The direction naming follows the SparkFun APDS-9960 library (the reference most hardware is
//! tested against): the up photodiode seeing the hand later than the down one is
//! [`Gesture::Down`], the left one later than the right one [`Gesture::Right`]. How these map onto
//! the product's own up, down, left and right depends on how the sensor is mounted; the firmware
//! driver remaps them.

/// A swipe direction, in the sensor's frame (see the [module docs](self)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    Up,
    Down,
    Left,
    Right,
}

impl core::fmt::Display for Gesture {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Gesture::Up => "up",
            Gesture::Down => "down",
            Gesture::Left => "left",
            Gesture::Right => "right",
        })
    }
}

/// One gesture-engine dataset: the four photodiode levels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub up: u8,
    pub down: u8,
    pub left: u8,
    pub right: u8,
}

/// What one gesture's samples amounted to ([`Decoder::trace`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trace {
    /// Samples pushed.
    pub samples: u16,
    /// Samples with every channel above the threshold.
    pub valid: u16,
    /// The brightest reading of any channel (255 is saturated).
    pub peak: u8,
    /// How much later the up photodiode saw the hand than the down one, in thousandths of the
    /// gesture's length (positive is [`Gesture::Down`]).
    pub lag_ud: i32,
    /// How much later the left photodiode saw the hand than the right one, thousandths (positive
    /// is [`Gesture::Right`]).
    pub lag_lr: i32,
    /// The up/down level balance's swing from the first valid sample to the last, percent
    /// (-200..=200; for comparison, not used to decide).
    pub ud: i32,
    /// The left/right level balance's swing, percent.
    pub lr: i32,
}

/// Decides a swipe from the samples of one gesture (see the [module docs](self)). The defaults
/// come from labeled runs on the LCD Module: every real swipe had at least 7 samples and a
/// brightest reading of 154 or more, and a lag of 54 or more on its axis; the hand travelling back
/// to start the next swipe left short, dim flickers (5 to 18 samples, peaks of 40 to 64).
#[derive(Clone, Copy, Debug)]
pub struct Decoder {
    /// Levels up to this are noise: subtracted from every reading.
    pub threshold: u8,
    /// Smallest lag between opposite photodiodes, in thousandths of the gesture's length, that is
    /// a swipe.
    pub min_lag: i32,
    /// Fewest samples a swipe takes (a flicker at the edge of the sensor's range takes fewer).
    pub min_samples: u16,
    /// Dimmest brightest reading a swipe has (a hand passing at the edge of the range, on its way
    /// back to start the next swipe, reads dimmer).
    pub min_peak: u8,
    samples: u16,
    peak: u8,
    sum: [u32; 4],
    moment: [u64; 4],
    first: Option<Sample>,
    last: Option<Sample>,
    valid: u16,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            threshold: 10,
            min_lag: 40,
            min_samples: 6,
            min_peak: 80,
            samples: 0,
            peak: 0,
            sum: [0; 4],
            moment: [0; 4],
            first: None,
            last: None,
            valid: 0,
        }
    }

    /// Forget the samples (start of a new gesture), keeping the settings.
    pub fn reset(&mut self) {
        *self = Self {
            threshold: self.threshold,
            min_lag: self.min_lag,
            min_samples: self.min_samples,
            min_peak: self.min_peak,
            ..Self::new()
        };
    }

    pub fn push(&mut self, s: Sample) {
        let i = self.samples as u64;
        for (k, v) in [s.up, s.down, s.left, s.right].into_iter().enumerate() {
            let w = v.saturating_sub(self.threshold) as u32;
            self.sum[k] += w;
            self.moment[k] += i * w as u64;
        }
        self.samples = self.samples.saturating_add(1);
        self.peak = self.peak.max(s.up).max(s.down).max(s.left).max(s.right);
        let t = self.threshold;
        if s.up > t && s.down > t && s.left > t && s.right > t {
            self.first.get_or_insert(s);
            self.last = Some(s);
            self.valid = self.valid.saturating_add(1);
        }
    }

    /// What the samples since the last reset amount to (for tuning against real hands).
    pub fn trace(&self) -> Trace {
        // Centers in 1/16 sample; a silent channel sits in the middle.
        let n = self.samples.max(1) as i64;
        let center = |k: usize| {
            if self.sum[k] == 0 {
                n * 8
            } else {
                (self.moment[k] * 16 / self.sum[k] as u64) as i64
            }
        };
        let lag = |a: usize, b: usize| ((center(a) - center(b)) * 1000 / (16 * n)) as i32;
        let (ud, lr) = match (self.first, self.last) {
            (Some(a), Some(b)) => (
                balance(b.up, b.down) - balance(a.up, a.down),
                balance(b.left, b.right) - balance(a.left, a.right),
            ),
            _ => (0, 0),
        };
        Trace {
            samples: self.samples,
            valid: self.valid,
            peak: self.peak,
            lag_ud: lag(0, 1),
            lag_lr: lag(2, 3),
            ud,
            lr,
        }
    }

    /// The gesture the samples since the last reset describe, if any; resets.
    pub fn finish(&mut self) -> Option<Gesture> {
        let t = self.trace();
        let weak = t.samples < self.min_samples || t.peak < self.min_peak;
        let out = if weak || (t.lag_ud.abs() < self.min_lag && t.lag_lr.abs() < self.min_lag) {
            None
        } else if t.lag_ud.abs() >= t.lag_lr.abs() {
            Some(if t.lag_ud > 0 { Gesture::Down } else { Gesture::Up })
        } else {
            Some(if t.lag_lr > 0 {
                Gesture::Right
            } else {
                Gesture::Left
            })
        };
        self.reset();
        out
    }
}

/// `(a - b) / (a + b)` in percent.
fn balance(a: u8, b: u8) -> i32 {
    let (a, b) = (a as i32, b as i32);
    (a - b) * 100 / (a + b).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(up: u8, down: u8, left: u8, right: u8) -> Sample {
        Sample {
            up,
            down,
            left,
            right,
        }
    }

    #[test]
    fn swipes() {
        let mut d = Decoder::new();
        // The up photodiode sees the hand first, the down one last: up.
        for x in [
            s(200, 40, 100, 100),
            s(190, 60, 100, 100),
            s(180, 80, 100, 100),
            s(150, 150, 100, 100),
            s(80, 180, 100, 100),
            s(40, 200, 100, 100),
        ] {
            d.push(x);
        }
        assert_eq!(d.finish(), Some(Gesture::Up));
        // Left later than right: right.
        for x in [
            s(100, 100, 30, 200),
            s(100, 100, 40, 180),
            s(100, 100, 60, 160),
            s(100, 100, 160, 60),
            s(100, 100, 180, 40),
            s(100, 100, 200, 30),
        ] {
            d.push(x);
        }
        assert_eq!(d.finish(), Some(Gesture::Right));
    }

    #[test]
    fn saturation_and_imbalance_dont_hide_the_direction() {
        // Saturated in the middle, and the right side reading half the left's level throughout:
        // the order still says right.
        let mut d = Decoder::new();
        d.push(s(40, 40, 15, 60));
        d.push(s(90, 90, 40, 120));
        for _ in 0..6 {
            d.push(s(255, 255, 255, 255));
        }
        d.push(s(90, 90, 200, 50));
        d.push(s(40, 40, 120, 20));
        assert_eq!(d.finish(), Some(Gesture::Right));
        // The same, mirrored: left.
        d.push(s(40, 40, 120, 20));
        d.push(s(90, 90, 200, 50));
        for _ in 0..6 {
            d.push(s(255, 255, 255, 255));
        }
        d.push(s(90, 90, 40, 120));
        d.push(s(40, 40, 15, 60));
        assert_eq!(d.finish(), Some(Gesture::Left));
    }

    #[test]
    fn weak_short_or_still_is_none() {
        let mut d = Decoder::new();
        for x in [s(200, 5, 100, 100), s(100, 100, 100, 100), s(5, 200, 100, 100)] {
            d.push(x);
        }
        assert_eq!(d.finish(), None); // a clear down, but too few samples: a flicker
        for x in [
            s(60, 10, 30, 30),
            s(50, 20, 30, 30),
            s(40, 30, 30, 30),
            s(30, 40, 30, 30),
            s(20, 50, 30, 30),
        ] {
            d.push(x);
        }
        d.push(s(10, 60, 30, 30));
        assert_eq!(d.finish(), None); // a clear up, but dim: a hand at the edge of the range
        for _ in 0..20 {
            d.push(s(120, 110, 100, 105)); // a hand held still
        }
        assert_eq!(d.finish(), None);
        assert_eq!(d.trace(), Trace::default()); // reset after finish
    }
}
