//! radio_climate (TOWER IoT Kit product firmware): a climate sensor on the radio, a sleeping taf
//! node (`docs/firmware/taf.md`).
//!
//! It reports the Climate Module's humidity, illuminance and pressure and the CO2 Module's CO2
//! concentration (`nodes/{addr}/state/humidity`, `illuminance`, `pressure`, `co2`), next to what
//! every taf node reports (the Core Module's temperature, the accelerometer's `tilt` events).
//! Neither module has to be fitted: the app looks for each sensor with every round, reports the
//! ones it finds and stops reporting one that went away (a CO2 Module that failed three
//! measurements in a row is looked for again ten minutes later). The sensor tags carry the Climate
//! Module's parts, so a Humidity, Lux Meter or Barometer Tag works the same.
//!
//! **One uplink per round.** The sensors are read in the runtime's sampling round (every
//! `report-period`, with the temperature) and the round's news travels in one uplink. A value is
//! news once it moved by its delta since it was last sent (`humidity-delta`, `pressure-delta`,
//! `co2-delta`; `illuminance-delta` is a share of the reading, 1 lx at least), and everything
//! goes again with the heartbeat.
//!
//! **CO2 on its own period.** A CO2 measurement costs the LP8's lamp pulse (3.6 mC, as much as
//! a few hundred rounds of the other sensors), so it runs every `co2-period` (300 s by default)
//! in the first round after that: the module's capacitor is charged for the 5 s before that round
//! (a minute before the first measurement), everything on the module is off otherwise, and the
//! reading is compensated with the barometer's pressure when there is one.
//!
//! `climate print` (on the USB console and over the remote shell) shows the sensors found and
//! their last readings.
//!
//!   just run app radio_climate

#![no_std]
#![no_main]

use core::cell::Cell;
use core::fmt::Write as _;

use critical_section::Mutex;
use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use tower::climate_module::{ClimateModule, Humidity, Parts, Readings};
use tower::co2_module::{self, Co2Module};
use tower::i2c::I2cBus;
use tower::{app, board::Board, shell, taf};
use tower_protocol::props::{self, climate, common};

const KIND: props::Kind = climate::KIND;
static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
static COMMANDS: &[shell::Entry] = &[
    taf::NODE_COMMANDS,
    taf::BUTTON_COMMANDS,
    shell::Entry::menu(
        "climate",
        &[shell::Entry::cmd("print", shell::Args::None, cmd_print)],
    ),
];

/// The least illuminance change that is news, in tenths of a lux: below it `illuminance-delta`'s
/// share of a dark room's reading would be the sensor's noise.
const LUX_DELTA_MIN: i32 = 10;
/// The charging starts this much before it has to be done: the round it is for may come early.
const CHARGE_MARGIN: Duration = Duration::from_secs(1);
/// A failed CO2 measurement is tried again after this long (and a full charge).
const CO2_RETRY: Duration = Duration::from_secs(60);
/// A CO2 Module that stopped answering is looked for again after this long, not with the next
/// round: one whose sensor is broken answers the probe, and would be charged for a minute and
/// fail three measurements over and over.
const CO2_LOOK_AGAIN: Duration = Duration::from_secs(600);

/// What `climate print` shows: the sensors found and the last value of each.
#[derive(Clone, Copy)]
struct Last {
    parts: Option<Parts>,
    readings: Readings,
    co2_fitted: bool,
    co2: Option<u16>,
}

static LAST: Mutex<Cell<Last>> = Mutex::new(Cell::new(Last {
    parts: None,
    readings: Readings {
        humidity: None,
        illuminance: None,
        pressure: None,
    },
    co2_fitted: false,
    co2: None,
}));

fn update(f: impl FnOnce(&mut Last)) {
    critical_section::with(|cs| {
        let cell = LAST.borrow(cs);
        let mut last = cell.get();
        f(&mut last);
        cell.set(last);
    });
}

/// `climate print`: the sensors found and their last readings.
fn cmd_print(ctx: &mut shell::Ctx<'_>, _args: &[&str]) -> shell::Outcome {
    let last = critical_section::with(|cs| LAST.borrow(cs).get());
    let Some(parts) = last.parts else {
        let _ = write!(ctx, "no round yet");
        return shell::Outcome::ok();
    };
    let co2 = if last.co2_fitted { ", CO2 Module" } else { "" };
    let _ = writeln!(ctx, "sensors: {parts}{co2}");
    let show = |ctx: &mut shell::Ctx<'_>, id: u8, value: Option<i32>| {
        if let (Some(prop), Some(value)) = (KIND.prop(id), value) {
            let unit = match prop.ty {
                props::Type::Number { unit, .. } => unit,
                _ => "",
            };
            let _ = writeln!(ctx, "{}: {} {}", prop.name, prop.ty.display(value), unit);
        }
    };
    show(ctx, common::TEMPERATURE, taf::temperature());
    show(ctx, climate::HUMIDITY, last.readings.humidity.map(i32::from));
    show(
        ctx,
        climate::ILLUMINANCE,
        last.readings.illuminance.map(|v| v as i32),
    );
    show(ctx, climate::PRESSURE, last.readings.pressure.map(i32::from));
    show(ctx, climate::CO2, last.co2.map(i32::from));
    shell::Outcome::ok()
}

fn seconds(id: u8, min: i32) -> Duration {
    Duration::from_secs(taf::param(id).max(min) as u64)
}

/// A delta setting's value as [`taf::report_delta`] takes it.
fn delta(id: u8) -> u16 {
    taf::param(id).clamp(0, i32::from(u16::MAX)) as u16
}

/// Report a round's readings, each with its delta.
fn report(r: &Readings) {
    if let Some(humidity) = r.humidity {
        taf::report_delta(
            climate::HUMIDITY,
            i32::from(humidity),
            delta(climate::HUMIDITY_DELTA),
        );
    }
    if let Some(lux) = r.illuminance {
        // A share of the reading, 1 lx at least; above 32768 lx the delta stays at 6553.5 lx
        // (what a report slot holds), a smaller share of a reading that large.
        let lux = lux as i32;
        let share =
            (lux / 100 * taf::param(climate::ILLUMINANCE_DELTA)).clamp(LUX_DELTA_MIN, i32::from(u16::MAX));
        taf::report_delta(climate::ILLUMINANCE, lux, share as u16);
    }
    if let Some(pressure) = r.pressure.filter(|p| (2000..=11000).contains(p)) {
        taf::report_delta(
            climate::PRESSURE,
            i32::from(pressure),
            delta(climate::PRESSURE_DELTA),
        );
    }
}

/// The sensors found changed: say so, and stop reporting what went away.
fn parts_changed(before: Parts, now: Parts) {
    info!(target: "climate", "sensors: {}", now);
    if before.humidity != Humidity::None && now.humidity == Humidity::None {
        taf::forget(climate::HUMIDITY);
    }
    if before.light && !now.light {
        taf::forget(climate::ILLUMINANCE);
    }
    if before.barometer && !now.barometer {
        taf::forget(climate::PRESSURE);
    }
}

/// The app's one task: the Climate Module's sensors in every round of the runtime, the CO2
/// Module's in the first round after each `co2-period`. Built here, not passed in (a task's
/// argument is stored twice in its future).
#[embassy_executor::task]
async fn sensors(bus: I2cBus) {
    let mut climate = ClimateModule::new(bus);
    let mut co2: Option<Co2Module> = None;
    // The CO2 measurement's plan: when the next one is due, and (while the capacitor charges)
    // when it is charged. The next round is expected a `report-period` after the last one.
    let mut co2_due = Instant::now();
    let mut co2_look_at = Instant::MIN;
    let mut charged_at: Option<Instant> = None;
    let mut round_at = Instant::now();
    let mut pressure: Option<u16> = None;
    loop {
        let charge = async {
            match (&co2, charged_at) {
                (Some(module), None) => {
                    // The round the measurement runs in: the first one at or after `co2_due`.
                    let period = seconds(common::REPORT_PERIOD, 5);
                    let mut round = round_at + period;
                    if round < co2_due {
                        let rounds = (co2_due - round).as_ticks().div_ceil(period.as_ticks());
                        round += Duration::from_ticks(rounds * period.as_ticks());
                    }
                    let lead = module.charge_time() + CHARGE_MARGIN;
                    Timer::at(round.checked_sub(lead).unwrap_or(Instant::MIN)).await
                }
                _ => core::future::pending().await,
            }
        };
        match select3(taf::round(), charge, taf::next_set()).await {
            Either3::First(round) => {
                round_at = Instant::now();
                let before = climate.parts();
                climate.probe().await;
                if co2.is_none() && round_at >= co2_look_at {
                    co2 = Co2Module::probe(bus).await;
                    if co2.is_some() {
                        info!(target: "climate", "CO2 Module found");
                        co2_due = round_at;
                        update(|last| last.co2_fitted = true);
                    }
                }
                let readings = climate.measure().await;
                if readings.pressure.is_some() {
                    pressure = readings.pressure;
                }
                report(&readings);
                let parts = climate.parts();
                if parts != before {
                    parts_changed(before, parts);
                }
                update(|last| {
                    last.parts = Some(parts);
                    last.readings = readings;
                });
                if let Some(module) = &mut co2
                    && charged_at.is_some_and(|t| round_at >= t)
                {
                    charged_at = None;
                    match module.measure(pressure).await {
                        Ok(m) => {
                            co2_due =
                                round_at + seconds(climate::CO2_PERIOD, co2_module::MIN_PERIOD_S as i32);
                            match m.ppm() {
                                Some(ppm) => {
                                    taf::report_delta(
                                        climate::CO2,
                                        i32::from(ppm),
                                        delta(climate::CO2_DELTA),
                                    );
                                    update(|last| last.co2 = Some(ppm));
                                }
                                // The sensor withheld the value (its status says why): a short
                                // capacitor is charged in full for the next one.
                                None => warn!(
                                    target: "climate",
                                    "CO2 reading not valid (status {:02x} {:02x}, capacitor {} mV)",
                                    m.status0, m.status1, m.vcap2
                                ),
                            }
                        }
                        Err(e) => {
                            warn!(target: "climate", "CO2 not measured: {}", e);
                            co2_due = round_at + CO2_RETRY;
                        }
                    }
                }
                if co2.as_ref().is_some_and(Co2Module::gone) {
                    co2_gone(&mut co2, &mut charged_at);
                    co2_look_at = Instant::now() + CO2_LOOK_AGAIN;
                }
                // The round's reports may go now, in one uplink.
                drop(round);
            }
            Either3::Second(()) => {
                if let Some(module) = &mut co2 {
                    match module.charge().await {
                        Ok(wait) => charged_at = Some(Instant::now() + wait),
                        Err(e) => {
                            warn!(target: "climate", "CO2 Module not charging: {}", e);
                            co2_due = Instant::now() + CO2_RETRY;
                        }
                    }
                }
                if co2.as_ref().is_some_and(Co2Module::gone) {
                    co2_gone(&mut co2, &mut charged_at);
                    co2_look_at = Instant::now() + CO2_LOOK_AGAIN;
                }
            }
            // A setting changed (a delta, `co2-period`): they are read where they are used, and
            // the loop plans the charging again.
            Either3::Third(_) => {}
        }
    }
}

/// The CO2 Module stopped answering: stop reporting it; the caller says when to look for it again.
fn co2_gone(co2: &mut Option<Co2Module>, charged_at: &mut Option<Instant>) {
    warn!(target: "climate", "CO2 Module gone");
    *co2 = None;
    *charged_at = None;
    taf::forget(climate::CO2);
    update(|last| {
        last.co2_fitted = false;
        last.co2 = None;
    });
}

async fn run(b: Board) {
    let base = tower::taf_base!(b);
    base.spawner.spawn(sensors(base.i2c).unwrap());
    taf::run(base, &KIND, &SETTINGS).await
}

app!(run, commands: COMMANDS, settings: &SETTINGS);
