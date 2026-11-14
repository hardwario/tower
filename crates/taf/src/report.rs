//! When a measurement is worth an uplink.
//!
//! A sleeping node samples its sensors every `report-period` but spends airtime (and battery) only
//! when a value moved by at least `report-delta` since the last one it sent, or when the last
//! send is a heartbeat old, so the host never goes long without a fresh value.

/// [`Reporter::at`] of a value that was never sent.
const NEVER: u32 = u32::MAX;

/// The last value sent of one measurement. Eight bytes: the runtime keeps one per reported
/// property, in every app (an `Option` of the pair would be twelve).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reporter {
    /// The value last sent (meaningless while `at` is [`NEVER`]).
    value: i32,
    /// The second (since boot) it went out, [`NEVER`] before the first send; seconds fit 32
    /// bits for 136 years, and a time that late is recorded as the second before `NEVER`.
    at: u32,
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter {
    pub const fn new() -> Self {
        Self { value: 0, at: NEVER }
    }

    /// Whether `value` (sampled at `now_s`) should be sent: the first one, a change of at least
    /// `delta` (a `delta` of 0 sends every change), or `heartbeat_s` since the last send.
    pub fn due(&self, value: i32, now_s: u64, delta: i32, heartbeat_s: u64) -> bool {
        if self.at == NEVER {
            return true;
        }
        let moved = value.abs_diff(self.value);
        let delta = delta.unsigned_abs();
        (moved > 0 && moved >= delta) || now_s.saturating_sub(u64::from(self.at)) >= heartbeat_s
    }

    /// Record that `value` went out at `now_s`.
    pub fn sent(&mut self, value: i32, now_s: u64) {
        self.value = value;
        self.at = now_s.min(u64::from(NEVER - 1)) as u32;
    }

    /// Forget the last send (the next value is sent whatever it is).
    pub fn reset(&mut self) {
        self.at = NEVER;
    }

    /// The value last sent, if any.
    pub fn last(&self) -> Option<i32> {
        (self.at != NEVER).then_some(self.value)
    }
}
