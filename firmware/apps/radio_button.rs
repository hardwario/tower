//! radio_button (TOWER IoT Kit product firmware): a wireless push button with a tilt alarm and a
//! thermometer, a sleeping taf node (`docs/firmware/taf.md`).
//!
//! Every button press, release, click and hold is a `button` event (`nodes/{addr}/event/button`,
//! the value `press`/`release`/`click`/`hold` with its running count), each switchable by its
//! setting (`press`, `release` off and `click`, `hold` on by default: the gesture scheme). The
//! LED answers each reported event with a fixed shape (press, release and hold a single pulse, a
//! click a double blink), when the common `led` setting is on. The accelerometer's tilt interrupt
//! raises a `tilt` event whose value is the face up (1..=6, 0 while moving), at most one per 10 s
//! with the latest face coalesced; `accel` picks its sensitivity (off, low, medium, high; applied
//! at boot). The Core Module's temperature goes up when it moves past `report-delta`.
//!
//! The pairing gestures (hold 1 s while unpaired, 12 s to re-pair), the cable, the remote shell,
//! settings over the radio and the heartbeat are the taf runtime's. `button simulate <ms>` (console
//! only) presses the button through the real recognition, for tests and the HIL bench.
//!
//!   just run app radio_button

#![no_std]
#![no_main]

use embassy_futures::select::{Either3, select3};
use embassy_stm32::exti::ExtiInput;
use embassy_stm32::mode::Async;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use tower::board::GuardedI2c;
use tower::lis2dh12::{self, Lis2dh12, Sensitivity, TiltConfig};
use tower::tmp112::{self, Tmp112};
use tower::{app, board::Board, button, console, shell, taf};
use tower_protocol::props::{self, button as props_button, common};

const KIND: props::Kind = props_button::KIND;
static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
static COMMANDS: &[shell::Entry] = &[taf::NODE_COMMANDS, taf::BUTTON_COMMANDS];

/// LED feedback (fixed shapes: the LED is an indicator, not a setting). A click is a double blink
/// so it reads apart from the press before it on the one LED.
const LED_PULSE_MS: u32 = 250;
const LED_CLICK_MS: u32 = 100;
const LED_HOLD_MS: u32 = 1000;

/// Backoff of the accelerometer interrupt wait after its source register could not be read (an
/// I²C error leaves the latch set, so the line stays high): first 1 s, doubling to 5 min.
const ACCEL_BACKOFF_MIN: Duration = Duration::from_secs(1);
const ACCEL_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// The pause before the next interrupt is taken when the line is high again right after a good
/// read: still moving, no fault; the pause only keeps a line stuck high from spinning the loop.
const ACCEL_REARM: Duration = Duration::from_millis(100);
/// Good reads in a row with nothing to report while the line stayed high: a line stuck high, not
/// motion (real motion yields an event at least every 5 reads); it backs off like a failed read.
const ACCEL_STUCK_READS: u8 = 10;
/// Tilt uplinks go out at most once per this long: continuous motion raises about two a second,
/// ~29 ms/s of airtime against the EU duty budget's 10 ms/s refill. Motion inside the interval is
/// coalesced and its latest face goes when the interval ends.
const ACCEL_UPLINK_MIN: Duration = Duration::from_secs(10);

/// The accelerometer with the shared I²C2 bus; `measure_temp` borrows the bus for each read.
type AccelBus = Option<Lis2dh12<GuardedI2c>>;

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

#[embassy_executor::task]
async fn buttons() {
    let mut counts = [0u32; 4];
    loop {
        let ev = taf::next_button().await;
        let (index, setting) = match ev {
            button::Event::Press => (0, props_button::PRESS),
            button::Event::Release => (1, props_button::RELEASE),
            button::Event::Click => (2, props_button::CLICK),
            button::Event::Hold => (3, props_button::HOLD),
        };
        counts[index] = counts[index].wrapping_add(1);
        // A disabled event is ignored entirely: not reported, not shown on the LED.
        if taf::param(setting) == 0 {
            continue;
        }
        let name = props_button::EVENTS[index];
        let note = if taf::paired() { "" } else { " (not paired)" };
        info!(target: "button", "{} #{}{}", name, counts[index], note);
        let mut count = heapless::String::<10>::new();
        let _ = core::fmt::write(&mut count, format_args!("{}", counts[index]));
        console::event_now("button", &[("kind", name), ("count", count.as_str())]);
        if taf::led_on()
            && let Some(led) = taf::led()
        {
            match ev {
                button::Event::Click => led.pulse(LED_CLICK_MS, LED_CLICK_MS, 2),
                button::Event::Hold => led.flash(LED_HOLD_MS),
                _ => led.flash(LED_PULSE_MS),
            }
        }
        taf::event(props_button::BUTTON, index as i32);
    }
}

#[embassy_executor::task]
async fn sensors(mut accel_int: ExtiInput<'static, Async>, bus: GuardedI2c) {
    // The tilt interrupt is a boot-time configuration (register state persists while powered).
    let sensitivity = match taf::param(props_button::ACCEL) {
        0 => None,
        1 => Some(Sensitivity::Low),
        3 => Some(Sensitivity::High),
        _ => Some(Sensitivity::Medium),
    };
    let mut bus: AccelBus = {
        let mut accel = Lis2dh12::new(bus, lis2dh12::ADDR_DEFAULT);
        if let Some(s) = sensitivity {
            let _ = accel.init();
            match accel.enable_tilt(TiltConfig::new(s)) {
                Ok(()) => info!(target: "accel", "tilt detection armed"),
                Err(_) => warn!(target: "accel", "tilt detection not armed (I2C error)"),
            }
        }
        Some(accel)
    };
    let mut next_temp = Instant::now();
    let mut accel_quiet: Option<Instant> = None;
    let mut accel_backoff = ACCEL_BACKOFF_MIN;
    let mut accel_empty_highs: u8 = 0;
    // The tilt uplink rate limit: none before `accel_hold`, and the face coalesced meanwhile.
    let mut accel_hold: Option<Instant> = None;
    let mut accel_pending: Option<u8> = None;
    loop {
        let due = match accel_pending {
            Some(_) => next_temp.min(accel_hold.unwrap_or(Instant::MIN)),
            None => next_temp,
        };
        let accel_wait = async {
            if let Some(t) = accel_quiet {
                Timer::at(t).await;
            }
            accel_int.wait_for_high().await
        };
        match select3(Timer::at(due), accel_wait, taf::next_set()).await {
            Either3::First(()) => {
                let now = Instant::now();
                if now >= next_temp {
                    if let Some(millic) = measure_temp(&mut bus).await {
                        taf::report(common::TEMPERATURE, millic / 10);
                    }
                    let period = taf::param(common::REPORT_PERIOD).clamp(5, 86_400) as u64;
                    next_temp = Instant::now() + Duration::from_secs(period);
                }
                if accel_hold.is_none_or(|t| now >= t)
                    && let Some(face) = accel_pending.take()
                {
                    taf::event(props_button::TILT, i32::from(face));
                    accel_hold = Some(Instant::now() + ACCEL_UPLINK_MIN);
                }
            }
            Either3::Second(()) => {
                accel_quiet = None;
                let tilt = handle_tilt(&mut bus).await;
                if let Tilt::Event(face) = tilt {
                    info!(target: "accel", "motion, face {}", face);
                    let mut f = heapless::String::<3>::new();
                    let _ = core::fmt::write(&mut f, format_args!("{face}"));
                    console::event_now("accel", &[("kind", "motion"), ("face", f.as_str())]);
                    if accel_hold.is_some_and(|t| Instant::now() < t) {
                        accel_pending = Some(face);
                    } else {
                        taf::event(props_button::TILT, i32::from(face));
                        accel_hold = Some(Instant::now() + ACCEL_UPLINK_MIN);
                    }
                }
                // `wait_for_high` returns at once on a high line: a failed read (latch still set)
                // backs off, growing; after a good read a high line is new motion: a short pause.
                let high = accel_int.is_high();
                accel_empty_highs = match tilt {
                    Tilt::Nothing if high => accel_empty_highs.saturating_add(1),
                    _ => 0,
                };
                let stuck = accel_empty_highs >= ACCEL_STUCK_READS;
                if tilt == Tilt::ReadFailed || stuck {
                    if accel_backoff == ACCEL_BACKOFF_MIN {
                        if stuck {
                            warn!(target: "accel", "interrupt line stuck high (reads succeed), backing off");
                        } else {
                            warn!(target: "accel", "interrupt source unreadable (I2C), backing off");
                        }
                    }
                    accel_quiet = Some(Instant::now() + accel_backoff);
                    accel_backoff = (accel_backoff * 2).min(ACCEL_BACKOFF_MAX);
                } else {
                    accel_backoff = ACCEL_BACKOFF_MIN;
                    if high {
                        accel_quiet = Some(Instant::now() + ACCEL_REARM);
                    }
                }
            }
            // `accel` and the event switches are read where they are used; nothing to apply now.
            Either3::Third(_) => {}
        }
    }
}

/// One-shot temperature read: take the bus from the accelerometer, measure, hand it back (the
/// chip's tilt configuration persists across the swap).
async fn measure_temp(bus: &mut AccelBus) -> Option<i32> {
    let mut t = Tmp112::new(bus.take()?.release(), tmp112::ADDR_VPLUS);
    let raw = t.oneshot().await;
    *bus = Some(Lis2dh12::new(t.release(), lis2dh12::ADDR_DEFAULT));
    raw.ok().map(tmp112::raw_to_millicelsius)
}

/// Tilt interrupt: clear the latched source, let the board settle, read the face up.
async fn handle_tilt(bus: &mut AccelBus) -> Tilt {
    let Some(accel) = bus else {
        return Tilt::Nothing;
    };
    match accel.tilt_triggered() {
        Ok(true) => {}
        Ok(false) => return Tilt::Nothing,
        Err(_) => return Tilt::ReadFailed,
    }
    Timer::after_millis(300).await;
    let face = accel.read().ok().and_then(|s| s.dice()).unwrap_or(0);
    Tilt::Event(face)
}

async fn run(b: Board) {
    b.spawner.spawn(buttons().unwrap());
    b.spawner.spawn(sensors(b.accel_int, b.tmp112.release()).unwrap());
    taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
}

app!(run, commands: COMMANDS, settings: &SETTINGS);
