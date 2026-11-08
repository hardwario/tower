//! A small, fast pseudo-random generator for the random effects (sparkle, meteor, fire).

/// xorshift32: 4 bytes of state, a few cycles a number, deterministic per seed. Not
/// cryptographic. Seed it from a hardware source (the firmware's TRNG) for a pattern that
/// differs every boot.
#[derive(Clone, Debug)]
pub struct Rng(u32);

impl Rng {
    /// A generator from `seed` (any value; 0, which xorshift cannot leave, is replaced).
    pub const fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x1234_5678 } else { seed })
    }

    /// The next 32 random bits.
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// A random value in `0..n` (0 when `n` is 0). Multiply-shift instead of `%`: no division
    /// and no modulo bias worth the name.
    pub fn below(&mut self, n: u32) -> u32 {
        ((self.next_u32() as u64 * n as u64) >> 32) as u32
    }

    /// `true` with probability `p / 256`.
    pub fn chance(&mut self, p: u8) -> bool {
        (self.next_u32() >> 24) < p as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_stays_in_range_and_covers_it() {
        let mut r = Rng::new(42);
        let mut seen = [false; 7];
        for _ in 0..1000 {
            let v = r.below(7);
            assert!(v < 7);
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
        assert_eq!(r.below(0), 0);
    }

    #[test]
    fn zero_seed_still_runs() {
        let mut r = Rng::new(0);
        assert_ne!(r.next_u32(), 0);
    }

    #[test]
    fn chance_is_roughly_right() {
        let mut r = Rng::new(9);
        let hits = (0..10_000).filter(|_| r.chance(64)).count();
        assert!((2200..2800).contains(&hits), "{hits}");
    }
}
