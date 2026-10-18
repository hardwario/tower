//! Crystal time: milliseconds on the 32.768 kHz LSE crystal, for schedules two devices share.
//!
//! embassy's `Instant` runs off a general-purpose timer clocked by the HSI16 oscillator while the
//! MCU is awake; the RTC only bridges STOP. HSI16 is factory-trimmed to about ±1 %: two boards
//! measured 2500 ppm apart. That is harmless for timeouts, but a schedule shared over the air
//! drifts by it: a sleeping US 915 node predicting its gateway's hop slot after a few minutes
//! missed by more than a slot (the gateway, on USB, never sleeps, so its clock is HSI-only).
//! [`now_ms`] reads the RTC calendar instead, which the LSE crystal clocks in run and STOP alike
//! (±20 ppm at 25 °C), at the resolution of the RTC's sub-second counter (`board::init` sets
//! 1024 Hz: about 1 ms). Wait with embassy timers as usual, for short spans only: a 300 ms wait
//! is off by at most ~3 ms on HSI16.

use core::cell::Cell;

use critical_section::Mutex;
use embassy_stm32::pac::RTC;

/// The latest reading, so time never runs backwards (the calendar is never set after boot, but a
/// torn read across a midnight rollover must not move an anchor back a day).
static LAST_MS: Mutex<Cell<u64>> = Mutex::new(Cell::new(0));

/// Milliseconds on the crystal since the RTC calendar's origin (embassy starts it at boot), the
/// resolution of the RTC sub-second counter. Monotonic.
#[must_use]
pub fn now_ms() -> u64 {
    let r = RTC;
    let prediv_s = u64::from(r.prer().read().prediv_s());
    // The RTC runs with the shadow registers bypassed (embassy sets BYPSHAD), so the calendar
    // is read straight from the counters, current after STOP too; a second boundary between the
    // reads shows as a changed sub-second count, and the read is retried.
    let mut ss = r.ssr().read().ss();
    let mut ms = 0;
    for _ in 0..5 {
        let tr = r.tr().read();
        let dr = r.dr().read();
        let ss_after = r.ssr().read().ss();
        let bcd = |tens: u8, units: u8| u64::from(tens) * 10 + u64::from(units);
        let days = days_since_2000(
            bcd(dr.yt(), dr.yu()),
            bcd(dr.mt() as u8, dr.mu()),
            bcd(dr.dt(), dr.du()),
        );
        let secs =
            ((days * 24 + bcd(tr.ht(), tr.hu())) * 60 + bcd(tr.mnt(), tr.mnu())) * 60 + bcd(tr.st(), tr.su());
        let sub = prediv_s.saturating_sub(u64::from(ss)) * 1000 / (prediv_s + 1);
        ms = secs * 1000 + sub;
        if ss == ss_after {
            break;
        }
        ss = ss_after;
    }
    critical_section::with(|cs| {
        let last = LAST_MS.borrow(cs);
        let t = ms.max(last.get());
        last.set(t);
        t
    })
}

/// Days from 2000-01-01 to `year` (00..=99 = 2000..=2099) / `month` (1..=12) / `day` (1..=31).
fn days_since_2000(year: u64, month: u64, day: u64) -> u64 {
    const CUM: [u64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leap_days_before = year.div_ceil(4); // 2000 is a leap year, and every 4th after it
    let leap_this = year.is_multiple_of(4) && month > 2;
    year * 365 + leap_days_before + CUM[(month.clamp(1, 12) - 1) as usize] + u64::from(leap_this) + day.max(1)
        - 1
}
