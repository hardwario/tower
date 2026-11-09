//! The Core Module's sensors, the same in every taf node app: the TMP112's temperature (the
//! common `temperature` state, sampled every `report-period`) and the LIS2DH12's tilt interrupt
//! (the common `tilt` event, armed per the `accel` setting). The runtime spawns [`task`]; an app
//! that works with the temperature reads it with [`temperature`](super::temperature) or waits for
//! the next sample with [`temperature_sampled`](super::temperature_sampled).
//!
//! Both chips share I²C2 through one [`I2cBus`] handle (each transaction takes the bus), so an app
//! driver on the same bus (the base's `i2c`) needs nothing from this task.

use embassy_futures::select::{Either3, select3};
use embassy_stm32::exti::ExtiInput;
use embassy_stm32::mode::Async;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use tower_protocol::props::{self, common};

use crate::i2c::I2cBus;
use crate::lis2dh12::{self, Lis2dh12, Sensitivity, TiltConfig};
use crate::tmp112::{self, Tmp112};

/// A setting changed: the task looks at `accel` again (raised by
/// [`setting_changed`](super::setting_changed)).
pub(super) static SETTINGS_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Backoff of the accelerometer interrupt wait after its source register could not be read (an
/// I²C error leaves the latch set, so the line stays high): first 1 s, doubling to 5 min.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// The pause before the next interrupt is taken when the line is high again right after a good
/// read: still moving, no fault; the pause only keeps a line stuck high from spinning the loop.
const REARM: Duration = Duration::from_millis(100);
/// Good reads in a row with nothing to report while the line stayed high: a line stuck high, not
/// motion (real motion yields an event at least every 5 reads); it backs off like a failed read.
const STUCK_READS: u8 = 10;
/// Tilt uplinks go out at most once per this long: continuous motion raises about two a second,
/// ~29 ms/s of airtime against the EU duty budget's 10 ms/s refill. Motion inside the interval is
/// coalesced and its latest face goes when the interval ends.
const UPLINK_MIN: Duration = Duration::from_secs(10);
/// How long the board settles after a tilt before its face up is read.
const SETTLE: Duration = Duration::from_millis(300);

/// What one accelerometer interrupt turned out to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tilt {
    /// Motion to report; the face up after it settled (0 = unknown).
    Event(u8),
    /// Nothing: a spurious edge, or rate-limited by the driver's `min_interval`.
    Nothing,
    /// The interrupt source could not be read (I²C): the latch is still set.
    ReadFailed,
}

/// The tilt threshold of an `accel` setting value; `None` for `off`.
fn sensitivity(accel: i32) -> Option<Sensitivity> {
    match accel {
        0 => None,
        1 => Some(Sensitivity::Low),
        3 => Some(Sensitivity::High),
        _ => Some(Sensitivity::Medium),
    }
}

/// Arm the tilt interrupt for the `accel` setting value, or power the accelerometer down for
/// `off` (its register state persists while powered, so this runs at start and on a change).
fn arm(accel: &mut Lis2dh12<I2cBus>, setting: i32) {
    let name = props::SENSITIVITY.get(setting as usize).copied().unwrap_or("?");
    match sensitivity(setting) {
        Some(s) => {
            let _ = accel.init();
            match accel.enable_tilt(TiltConfig::new(s)) {
                Ok(()) => info!(target: "accel", "tilt detection armed, {}", name),
                Err(_) => warn!(target: "accel", "tilt detection not armed (I2C error)"),
            }
        }
        None => {
            let _ = accel.disable_tilt();
            let _ = accel.power_down();
            info!(target: "accel", "tilt detection off");
        }
    }
}

/// Send a tilt event now and start the uplink interval.
fn send_tilt(face: u8, hold: &mut Option<Instant>) {
    super::event(common::TILT, i32::from(face));
    *hold = Some(Instant::now() + UPLINK_MIN);
}

/// Tilt interrupt: clear the latched source, let the board settle, read the face up.
async fn read_tilt(accel: &mut Lis2dh12<I2cBus>) -> Tilt {
    match accel.tilt_triggered() {
        Ok(true) => {}
        Ok(false) => return Tilt::Nothing,
        Err(_) => return Tilt::ReadFailed,
    }
    Timer::after(SETTLE).await;
    let face = accel.read().ok().and_then(|s| s.dice()).unwrap_or(0);
    Tilt::Event(face)
}

/// The sensors: the temperature every `report-period`, tilt events as they come.
#[embassy_executor::task]
pub(super) async fn task(mut int: ExtiInput<'static, Async>, bus: I2cBus) {
    let mut thermometer = Tmp112::new(bus, tmp112::ADDR_VPLUS);
    let mut accel = Lis2dh12::new(bus, lis2dh12::ADDR_DEFAULT);
    let mut armed = super::param(common::ACCEL);
    arm(&mut accel, armed);
    let mut next_temp = Instant::now();
    let mut quiet: Option<Instant> = None;
    let mut backoff = BACKOFF_MIN;
    let mut empty_highs: u8 = 0;
    // The tilt uplink rate limit: none before `hold`, and the face coalesced meanwhile.
    let mut hold: Option<Instant> = None;
    let mut pending: Option<u8> = None;
    loop {
        let due = match pending {
            Some(_) => next_temp.min(hold.unwrap_or(Instant::MIN)),
            None => next_temp,
        };
        let tilt_wait = async {
            if let Some(t) = quiet {
                Timer::at(t).await;
            }
            int.wait_for_high().await
        };
        match select3(Timer::at(due), tilt_wait, SETTINGS_CHANGED.wait()).await {
            Either3::First(()) => {
                let now = Instant::now();
                if now >= next_temp {
                    match thermometer.oneshot().await {
                        Ok(raw) => super::sampled(tmp112::raw_to_millicelsius(raw) / 10),
                        Err(_) => warn!(target: "node", "thermometer not read (I2C)"),
                    }
                    let period = super::param(common::REPORT_PERIOD).clamp(5, 86_400) as u64;
                    next_temp = Instant::now() + Duration::from_secs(period);
                }
                if hold.is_none_or(|t| now >= t)
                    && let Some(face) = pending.take()
                {
                    send_tilt(face, &mut hold);
                }
            }
            Either3::Second(()) => {
                quiet = None;
                let tilt = read_tilt(&mut accel).await;
                if let Tilt::Event(face) = tilt {
                    info!(target: "accel", "motion, face {}", face);
                    if hold.is_some_and(|t| Instant::now() < t) {
                        pending = Some(face);
                    } else {
                        send_tilt(face, &mut hold);
                    }
                }
                // `wait_for_high` returns at once on a high line: a failed read (latch still set)
                // backs off, growing; after a good read a high line is new motion: a short pause.
                let high = int.is_high();
                empty_highs = match tilt {
                    Tilt::Nothing if high => empty_highs.saturating_add(1),
                    _ => 0,
                };
                let stuck = empty_highs >= STUCK_READS;
                if tilt == Tilt::ReadFailed || stuck {
                    if backoff == BACKOFF_MIN {
                        let why = if stuck {
                            "line stuck high (reads succeed)"
                        } else {
                            "source unreadable (I2C)"
                        };
                        warn!(target: "accel", "interrupt {}, backing off", why);
                    }
                    quiet = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                } else {
                    backoff = BACKOFF_MIN;
                    if high {
                        quiet = Some(Instant::now() + REARM);
                    }
                }
            }
            // A setting changed: `accel` takes effect at once; the others are read where used.
            Either3::Third(()) => {
                let setting = super::param(common::ACCEL);
                if setting != armed {
                    armed = setting;
                    arm(&mut accel, setting);
                    quiet = None;
                    backoff = BACKOFF_MIN;
                    empty_highs = 0;
                }
            }
        }
    }
}
