//! When a measurement is worth an uplink.
//!
//! A sleeping node samples its sensors every `report-period` but spends airtime (and battery) only
//! when a value moved by at least `report-delta` since the last one it sent, or when the last
//! send is a heartbeat old, so the host never goes long without a fresh value.

/// The last value sent of one measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reporter {
    /// The value and the second (since boot) it went out; seconds fit 32 bits for 136 years.
    last: Option<(i32, u32)>,
}

impl Reporter {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Whether `value` (sampled at `now_s`) should be sent: the first one, a change of at least
    /// `delta` (a `delta` of 0 sends every change), or `heartbeat_s` since the last send.
    pub fn due(&self, value: i32, now_s: u64, delta: i32, heartbeat_s: u64) -> bool {
        match self.last {
            None => true,
            Some((last, at)) => {
                let moved = value.abs_diff(last);
                let delta = delta.unsigned_abs();
                (moved > 0 && moved >= delta) || now_s.saturating_sub(u64::from(at)) >= heartbeat_s
            }
        }
    }

    /// Record that `value` went out at `now_s`.
    pub fn sent(&mut self, value: i32, now_s: u64) {
        self.last = Some((value, now_s.min(u64::from(u32::MAX)) as u32));
    }

    /// Forget the last send (the next value is sent whatever it is).
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// The value last sent, if any.
    pub fn last(&self) -> Option<i32> {
        self.last.map(|(v, _)| v)
    }
}
