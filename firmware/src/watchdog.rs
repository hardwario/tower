//! Optional independent watchdog (IWDG).
//!
//! Opt-in hardware safety net: an app calls [`enable`] once at start-up to arm a reset that
//! fires if the firmware wedges (an infinite loop, a deadlock, a peripheral that never returns).
//!
//! **Low-power aware.** The STM32L0 IWDG clocks from the LSI and *keeps counting in STOP*, so a
//! naive watchdog would reset a healthy battery node the moment it idled past the timeout. Instead
//! [`enable`] spawns a feeder task that pets the watchdog at half its shortest real timeout (below):
//! the low-power executor wakes for that timer even from STOP, pets, and drops back to STOP, so an
//! idle node stays alive while a genuinely hung one (its executor no longer running the feeder) is
//! reset.
//!
//! **The timeout is nominal; the LSI spreads it.** The IWDG counts the LSI divided by the prescaler
//! into a 12-bit reload, and embassy converts the requested timeout into those registers assuming a
//! 32 kHz LSI (`rcc::LSI_FREQ` on the L0). The STM32L0 datasheets give the LSI as 26 kHz min, 38 kHz
//! typ, 56 kHz max (LSI oscillator characteristics), so the real timeout is the nominal one times
//! 32/56 (0.57, fast corner) to 32/26 (1.23, slow corner): a nominal 26 s fires anywhere between
//! ~15 s and ~32 s. The feeder runs on the LSE time base (accurate), so it pets at half the
//! fast-corner timeout (~7.4 s for 26 s): every part keeps a full half-timeout of margin. (It used
//! to pet at half the nominal, 13 s, which a 56 kHz part's ~15 s timeout left under 2 s of slack.)
//! At 256 prescale the nominal ceiling is 4096 × 256 / 32 kHz = 32.8 s; [`enable`] clamps `timeout`
//! below it so an over-long request can't trip embassy's prescaler assert.
//!
//! **Liveness gating.** The feeder alone only proves the *executor* runs; a single wedged task
//! (the 2026-07-13 gateway: its main loop parked forever on console backpressure while the feeder
//! kept petting) never trips it. An app whose one main loop IS the product opts in with
//! [`require_checkin`] + [`checkin`]: the feeder then skips the pet once check-ins go stale, so a
//! parked main loop becomes a bounded IWDG reset instead of a device that needs a manual NRST.
//! Battery nodes that legitimately STOP for minutes simply don't opt in; [`enable`] alone keeps
//! the old behaviour. A withheld pet also leaves a reset-surviving **crash breadcrumb**
//! ([`crashlog`](crate::crashlog), kind `watchdog`), reported on the next boot like a panic.
//!
//! **Long synchronous EEPROM work.** A factory wipe or the in-line fallback flip programs the
//! data EEPROM for up to ~5.2 s with the executor (and so the feeder) monopolized. The storage
//! layer reloads the IWDG between programs via [`pet_during_stall`] (bounded, so a genuine hang
//! still resets), which keeps short timeouts (the 8 s of the example below) safe across them.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_stm32::Peri;
use embassy_stm32::peripherals::IWDG;
use embassy_stm32::wdg::IndependentWatchdog;
use embassy_time::{Duration, Instant, Timer};

/// Ceiling for the requested (nominal) IWDG timeout, with margin below the 32.8 s that a 12-bit
/// reload at the 256 prescaler reaches in embassy's 32 kHz arithmetic, so its prescaler
/// computation never asserts.
const MAX_TIMEOUT_US: u32 = 26_000_000;
/// The LSI frequency embassy's IWDG driver assumes when it programs the timeout (L0).
const LSI_ASSUMED_HZ: u64 = 32_000;
/// The fastest LSI the STM32L0 datasheets allow (26 kHz min, 38 kHz typ, 56 kHz max): the corner
/// where the real timeout is shortest, `nominal × 32 / 56`.
const LSI_MAX_HZ: u64 = 56_000;

/// The shortest real timeout (µs) a nominal `timeout_us` can give across the LSI's spread.
const fn fast_corner_us(timeout_us: u32) -> u64 {
    timeout_us as u64 * LSI_ASSUMED_HZ / LSI_MAX_HZ
}

/// Max check-in age (ticks) before the feeder withholds the pet; 0 = liveness gating off.
/// Load/store only (M0+ has no CAS). Tick-truncated to u32 and wrapping-compared, same as
/// the console's `LAST_HOST_RX`; the ~36 h wrap at 32 kHz ticks is harmless at these ages.
static CHECKIN_MAX_TICKS: AtomicU32 = AtomicU32::new(0);
/// Tick timestamp (low 32 bits) of the most recent [`checkin`].
static LAST_CHECKIN: AtomicU32 = AtomicU32::new(0);
/// Whether [`enable`] armed the IWDG (it can't be stopped again short of a reset).
static ARMED: AtomicBool = AtomicBool::new(false);
/// Tick timestamp (low 32 bits) of the feeder's most recent pet: the anchor that bounds
/// [`pet_during_stall`].
static LAST_FEED: AtomicU32 = AtomicU32::new(0);
/// The feeder's pet interval in ticks (half the fast-corner timeout), for [`pet_during_stall`]'s bound.
static FEED_INTERVAL_TICKS: AtomicU32 = AtomicU32::new(0);

/// Longest synchronous stall [`pet_during_stall`] will cover past the feeder's schedule:
/// comfortably above the worst legitimate one (a ~5.2 s wipe / in-line flip, docs/storage.md)
/// yet bounded, so code that wedges *inside* an EEPROM loop still gets reset.
const STALL_MAX: Duration = Duration::from_secs(10);

/// Gate the feeder on app liveness: once called, the feeder pets the IWDG only while the
/// latest [`checkin`] is younger than `max_age`; a main loop parked past that lets the
/// hardware reset fire. Call after [`enable`], then [`checkin`] from the loop being
/// guarded. `max_age` should comfortably exceed the loop's longest legitimate iteration
/// (radio slices, EEPROM writes), and the reset lands at most `max_age` + one feed interval +
/// the real timeout after the hang (the real timeout is up to 1.23 × nominal at the slow LSI
/// corner; see the module docs).
pub fn require_checkin(max_age: Duration) {
    checkin(); // arm fresh so the gate can't fire before the first loop iteration
    CHECKIN_MAX_TICKS.store(max_age.as_ticks().max(1) as u32, Ordering::Relaxed);
}

/// Record main-loop liveness (see [`require_checkin`]). Cheap: one atomic store.
pub fn checkin() {
    LAST_CHECKIN.store(Instant::now().as_ticks() as u32, Ordering::Relaxed);
}

/// Reload an armed IWDG from inside a long **synchronous** operation, called by the storage
/// layer after every EEPROM program (see `storage::Storage`'s `Eeprom` impl). While such a loop
/// runs, the executor is monopolized and the feeder task cannot, so a multi-second wipe or
/// fallback flip would otherwise trip a short timeout (an 8 s IWDG + the ~5.2 s worst case).
///
/// Bounded so it cannot mask a real hang: it pets only while the liveness gate (if configured)
/// still passes, and only within `interval + STALL_MAX` of the feeder's last pet; a stall
/// longer than any legitimate EEPROM operation (e.g. a task spinning on writes forever, the
/// executor starved) stops being covered and the IWDG fires as usual. A no-op when no watchdog
/// is armed. Cheap: two atomic loads, a timestamp, and one register write.
pub fn pet_during_stall() {
    if !ARMED.load(Ordering::Relaxed) || !live() {
        return;
    }
    let since_feed = (Instant::now().as_ticks() as u32).wrapping_sub(LAST_FEED.load(Ordering::Relaxed));
    let limit = FEED_INTERVAL_TICKS
        .load(Ordering::Relaxed)
        .saturating_add(STALL_MAX.as_ticks() as u32);
    if since_feed <= limit {
        // Same key write as `IndependentWatchdog::pet` (the feeder owns that handle; a raw KR
        // reload from here is equivalent and race-free on this single core).
        embassy_stm32::pac::IWDG
            .kr()
            .write(|w| w.set_key(embassy_stm32::pac::iwdg::vals::Key::RESET));
    }
}

/// Whether the feeder may pet: no gating configured, or the last check-in is fresh.
fn live() -> bool {
    let max = CHECKIN_MAX_TICKS.load(Ordering::Relaxed);
    if max == 0 {
        return true;
    }
    let age = (Instant::now().as_ticks() as u32).wrapping_sub(LAST_CHECKIN.load(Ordering::Relaxed));
    age <= max
}

/// Arm the independent watchdog with the nominal `timeout`, then spawn a task that pets it at half
/// the shortest real timeout the LSI's spread allows (`timeout × 32/56 / 2`; module docs).
///
/// Call once, early in the app (before any long-running work). After this, the firmware must keep
/// the async executor alive: a hang that stops the feeder triggers a hardware reset after the real
/// timeout, between `0.57 ×` and `1.23 × timeout` depending on the part's LSI. `timeout` is
/// clamped to 26 s nominal.
///
/// ```ignore
/// let b = Board::take(spawner);
/// watchdog::enable(b.iwdg, spawner, Duration::from_secs(8));
/// ```
pub fn enable(iwdg: Peri<'static, IWDG>, spawner: Spawner, timeout: Duration) {
    let us = (timeout.as_micros().min(MAX_TIMEOUT_US as u64)) as u32;
    let mut wdg = IndependentWatchdog::new(iwdg, us);
    wdg.unleash();
    // Pet at half the SHORTEST real timeout (a 56 kHz LSI), on the accurate LSE time base, so a
    // healthy system keeps half a timeout of margin on every part, not just at the nominal LSI.
    let interval = Duration::from_micros(fast_corner_us(us) / 2);
    FEED_INTERVAL_TICKS.store(interval.as_ticks() as u32, Ordering::Relaxed);
    LAST_FEED.store(Instant::now().as_ticks() as u32, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    // Spawn failure here means the feeder pool is already claimed (enable called twice): a
    // programming error, and leaving an unfed armed watchdog would reset the board, so fail loud.
    spawner.spawn(feeder(wdg, interval).unwrap());
}

#[embassy_executor::task]
async fn feeder(mut wdg: IndependentWatchdog<'static, IWDG>, interval: Duration) {
    let mut withheld = false;
    loop {
        if live() {
            wdg.pet();
            LAST_FEED.store(Instant::now().as_ticks() as u32, Ordering::Relaxed);
            if withheld {
                // The loop recovered before the IWDG fired; retract the breadcrumb so a later,
                // unrelated reset isn't blamed on a wedge that never happened.
                crate::crashlog::clear_kind(crate::crashlog::Kind::Watchdog);
                withheld = false;
            }
        } else {
            // Stale check-ins: withhold the pet and let the IWDG reset fire. Say why on the
            // console (USB-attached case) AND in the reset-surviving crash breadcrumb, so a
            // battery node's wedge leaves a trace the next boot reports (the boot reporter only
            // trusts a `watchdog` record when the reset really was the IWDG).
            log::error!(target: "watchdog", "main loop unresponsive; letting the IWDG reset");
            if !withheld {
                crate::crashlog::record_unless_fault(
                    crate::crashlog::Kind::Watchdog,
                    "main loop unresponsive (liveness check-ins stale) - IWDG reset",
                );
                withheld = true;
            }
        }
        Timer::after(interval).await;
    }
}
