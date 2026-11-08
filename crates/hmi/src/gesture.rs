//! Swipe direction from the four photodiode channels of an APDS-9960 style gesture engine.
//!
//! While a hand passes over the sensor, its gesture engine records datasets of four channel
//! levels (up, down, left, right photodiodes). The hand reaches one side first and leaves over
//! the opposite side, so the balance between opposite channels swings from one sign to the other
//! across the gesture. [`Decoder`] keeps the first and last dataset in which every channel saw the
//! hand, and decides by the larger swing.
//!
//! The direction naming follows the SparkFun APDS-9960 library (the reference most hardware is
//! tested against): a growing up/down balance is [`Gesture::Down`], a growing left/right balance
//! [`Gesture::Right`]. How these map onto the product's own up, down, left and right depends on
//! how the sensor is mounted; the firmware driver remaps them.

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

/// Decides a swipe from the samples of one gesture (see the [module docs](self)).
#[derive(Clone, Copy, Debug)]
pub struct Decoder {
    /// A sample counts only when every channel is above this level.
    pub threshold: u8,
    /// Smallest swing, in percent of the channel balance range (-100..=100), that is a swipe.
    pub sensitivity: i32,
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
            sensitivity: 30,
            first: None,
            last: None,
            valid: 0,
        }
    }

    /// Forget the samples (start of a new gesture).
    pub fn reset(&mut self) {
        self.first = None;
        self.last = None;
        self.valid = 0;
    }

    pub fn push(&mut self, s: Sample) {
        let t = self.threshold;
        if s.up > t && s.down > t && s.left > t && s.right > t {
            self.first.get_or_insert(s);
            self.last = Some(s);
            self.valid = self.valid.saturating_add(1);
        }
    }

    /// The gesture the samples since the last reset describe, if any; resets.
    pub fn finish(&mut self) -> Option<Gesture> {
        let out = match (self.first, self.last) {
            (Some(a), Some(b)) if self.valid >= 2 => {
                let ud = balance(b.up, b.down) - balance(a.up, a.down);
                let lr = balance(b.left, b.right) - balance(a.left, a.right);
                if ud.abs() < self.sensitivity && lr.abs() < self.sensitivity {
                    None
                } else if ud.abs() >= lr.abs() {
                    Some(if ud > 0 { Gesture::Down } else { Gesture::Up })
                } else {
                    Some(if lr > 0 { Gesture::Right } else { Gesture::Left })
                }
            }
            _ => None,
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
        // Up channel strong first, down channel strong last: the balance falls.
        for x in [s(200, 40, 100, 100), s(150, 150, 100, 100), s(40, 200, 100, 100)] {
            d.push(x);
        }
        assert_eq!(d.finish(), Some(Gesture::Up));
        for x in [s(100, 100, 30, 200), s(100, 100, 200, 30)] {
            d.push(x);
        }
        assert_eq!(d.finish(), Some(Gesture::Right));
    }

    #[test]
    fn weak_or_still_is_none() {
        let mut d = Decoder::new();
        d.push(s(5, 200, 200, 200)); // below threshold on one channel: ignored
        d.push(s(100, 100, 100, 100));
        assert_eq!(d.finish(), None); // one valid sample only
        d.push(s(100, 95, 100, 100));
        d.push(s(95, 100, 100, 100));
        assert_eq!(d.finish(), None); // a hand held still
    }
}
