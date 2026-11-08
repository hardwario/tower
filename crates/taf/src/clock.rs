//! Wall-clock time as an offset over the node's crystal clock.
//!
//! The node's own clock (`tower::clock::now_ms`, milliseconds on the 32.768 kHz crystal since
//! boot) is monotonic and never set: the US 915 hop schedule runs on it. Wall-clock time is that
//! clock plus an offset learned from the host's time sync (`NodeCmd::Time`: Unix seconds and the
//! host's UTC offset), so a sync never moves the crystal clock, and between syncs the time drifts
//! only by the crystal (about 20 ppm, 1.7 s a day). Local time is what a weekly program needs; the
//! host sends its UTC offset with every sync, so a daylight-saving change arrives with the next one.

/// Seconds from the Unix epoch to 2000-01-01 (not used for arithmetic, kept for reference by the
/// firmware's RTC calendar, which counts from 2000).
pub const UNIX_2000: u64 = 946_684_800;

/// A local date and time, as a weekly program sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Local {
    /// Local seconds since the Unix epoch (Unix time plus the UTC offset).
    pub seconds: i64,
    /// 0 = Monday ... 6 = Sunday.
    pub weekday: u8,
    /// 0..=1439.
    pub minute: u16,
    /// 0..=59.
    pub second: u8,
}

impl Local {
    /// The local time `seconds` (local seconds since the Unix epoch) stands for.
    pub fn from_seconds(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let sod = seconds.rem_euclid(86_400);
        // 1970-01-01 was a Thursday (weekday 3 with Monday = 0).
        let weekday = (days + 3).rem_euclid(7) as u8;
        Self {
            seconds,
            weekday,
            minute: (sod / 60) as u16,
            second: (sod % 60) as u8,
        }
    }

    pub fn hour(&self) -> u8 {
        (self.minute / 60) as u8
    }

    pub fn minute_of_hour(&self) -> u8 {
        (self.minute % 60) as u8
    }
}

/// Wall-clock time over a monotonic millisecond clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WallClock {
    /// Local milliseconds since the Unix epoch minus the monotonic clock, from the last sync.
    offset_ms: Option<i64>,
    /// The monotonic time of the last sync.
    synced_at_ms: u64,
}

impl WallClock {
    pub const fn new() -> Self {
        Self {
            offset_ms: None,
            synced_at_ms: 0,
        }
    }

    /// Apply a time sync received at monotonic time `now_ms`: Unix seconds and the UTC offset in
    /// minutes.
    pub fn sync(&mut self, unix: u32, utc_offset_min: i16, now_ms: u64) {
        let local_ms = (i64::from(unix) + 60 * i64::from(utc_offset_min)) * 1000;
        self.offset_ms = Some(local_ms - now_ms as i64);
        self.synced_at_ms = now_ms;
    }

    /// Whether a sync has arrived since boot.
    pub fn is_set(&self) -> bool {
        self.offset_ms.is_some()
    }

    /// The local time at monotonic time `now_ms`, once synced.
    pub fn local(&self, now_ms: u64) -> Option<Local> {
        let offset = self.offset_ms?;
        Some(Local::from_seconds((now_ms as i64 + offset).div_euclid(1000)))
    }

    /// Whether the node should ask for a sync: never synced, or the last sync is older than
    /// `max_age_ms`.
    pub fn wants_sync(&self, now_ms: u64, max_age_ms: u64) -> bool {
        !self.is_set() || now_ms.saturating_sub(self.synced_at_ms) >= max_age_ms
    }
}
