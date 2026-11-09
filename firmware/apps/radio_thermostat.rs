//! radio_thermostat (TOWER IoT Kit product firmware): a room thermostat with a weekly program, a
//! sleeping taf node (`docs/firmware/taf.md`) that reports its **heating demand**; a `radio_control` relay
//! (or any MQTT automation) switches the boiler on `state/demand`.
//!
//! Modes (`mode`): **auto** follows the weekly program; a setpoint set by hand
//! (`nodes/{addr}/set/setpoint`) holds until the program's next switch point. **manual** holds its
//! own setpoint. **off** never calls for heat. Demand has a hysteresis band of `hysteresis` centred
//! on the setpoint (`tower_taf::thermostat`). The program needs the wall-clock time, which the
//! gateway host syncs; until then auto mode holds the manual setpoint. The weekly program:
//! `thermostat program` prints it; `thermostat program <day|workdays|weekend|all> <hh:mm> <°C> ...`
//! replaces a day's switch points (up to six); `thermostat point <days> <hh:mm> <°C|->` sets or
//! removes one, which is what fits a remote shell line (`tower nodes shell <node> "thermostat
//! point workdays 06:00 21"`).
//!
//! The room temperature is the taf runtime's reading of the Core Module's TMP112. With an **LCD
//! Module** attached (Core Module r2) the thermostat has a screen: the room temperature, the
//! setpoint, the mode, the day and time, a flame while it heats. The left and right buttons step the
//! setpoint by 0.5 °C (applied 3 s after the last press: one save, one report); holding either
//! button opens the menu (the mode, pairing, the node's address, the factory reset). The LCD driver
//! runs in page mode (`tower::lcd::init_paged`): a frame would not fit next to the radio
//! (`docs/firmware/taf.md`, "RAM"). The module's buttons share the Core button's line, so the
//! driver takes the Core button too: the power-up gesture is the menu's pairing and reset then.
//! Without the module, and always on the Core Module r1 (no room for the screen in its RAM), the
//! thermostat runs headless: settings and state go over MQTT.
//!
//!   just run app radio_thermostat

#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_futures::select::{Either, Either4, select, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use tower::shell::{self, Args, Ctx, Entry, Outcome};
use tower::storage::{NS_APP, Nv};
use tower::{app, board::Board, taf};
use tower_protocol::props::{self, thermostat};
use tower_taf::program::{
    DAY_NAMES, Day, MAX_DECI, MIN_DECI, WEEK_BYTES, Week, minutes_until, parse_minute, parse_setpoint,
};
use tower_taf::thermostat::{Demand, Mode, Override};

const KIND: props::Kind = thermostat::KIND;
static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
#[cfg(not(feature = "core-module-r1"))]
static COMMANDS: &[Entry] = &[
    taf::NODE_COMMANDS,
    taf::BUTTON_COMMANDS,
    Entry::menu("thermostat", THERMOSTAT_COMMANDS),
    Entry::menu("lcd", tower::lcd::SHELL),
];
#[cfg(feature = "core-module-r1")]
static COMMANDS: &[Entry] = &[
    taf::NODE_COMMANDS,
    taf::BUTTON_COMMANDS,
    Entry::menu("thermostat", THERMOSTAT_COMMANDS),
];

/// `NS_APP` locals: the weekly program, and the mode with the manual setpoint (clear of the taf
/// runtime's 0x00).
const KEY_PROGRAM: u8 = 0x10;
const KEY_STATE: u8 = 0x11;

/// The program changed (a `thermostat program` command): the regulator reloads it.
static PROGRAM_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The mode and the manual setpoint, kept across a reboot.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
struct Saved {
    mode: u8,
    manual: i16,
}

/// Everything the demand logic works from.
struct State {
    temperature: Option<i32>, // centi °C
    mode: Mode,
    manual: i16, // deci °C
    over: Override,
    week: Week,
    demand: Demand,
}

impl State {
    /// The setpoint in force (tenths), `None` when off.
    fn setpoint(&mut self) -> Option<i16> {
        match self.mode {
            Mode::Off => None,
            Mode::Manual => Some(self.manual),
            Mode::Auto => match taf::local_time() {
                Some(t) => self
                    .over
                    .at(t.seconds)
                    .or_else(|| self.week.setpoint(t.weekday, t.minute))
                    .or(Some(self.manual)),
                // No wall clock yet: hold the manual setpoint.
                None => Some(self.manual),
            },
        }
    }

    /// A setpoint chosen by hand: in auto mode it holds until the next switch point.
    fn set_by_hand(&mut self, deci: i16) {
        let deci = deci.clamp(MIN_DECI, MAX_DECI);
        self.manual = deci;
        if self.mode == Mode::Auto
            && let Some(t) = taf::local_time()
            && let Some((day, minute)) = self.week.next_switch(t.weekday, t.minute)
        {
            let until = t.seconds - i64::from(t.second)
                + 60 * i64::from(minutes_until(t.weekday, t.minute, day, minute));
            self.over.set(deci, until);
        }
    }

    /// A new mode: what a hand-set setpoint held is dropped.
    fn set_mode(&mut self, mode: Mode, kv: Nv) {
        self.mode = mode;
        self.over.clear();
        info!(target: "thermostat", "mode {}", thermostat::MODES[mode as usize]);
        self.save(kv);
    }

    /// Recompute the demand and report the state.
    fn update(&mut self) {
        let setpoint = self.setpoint();
        let hysteresis = taf::param(thermostat::HYSTERESIS) as i16;
        if let Some(t) = self.temperature {
            let was = self.demand.is_on();
            let on = self.demand.update(t, setpoint, hysteresis);
            if on != was {
                info!(target: "thermostat", "demand {}", if on { "on" } else { "off" });
            }
        }
        taf::report(thermostat::DEMAND, i32::from(self.demand.is_on()));
        taf::report(thermostat::MODE, self.mode as i32);
        taf::report(thermostat::SETPOINT, i32::from(setpoint.unwrap_or(self.manual)));
    }

    fn save(&self, kv: Nv) {
        let saved = Saved {
            mode: self.mode as u8,
            manual: self.manual,
        };
        if kv.scope(NS_APP).set(KEY_STATE, &saved).is_err() {
            warn!(target: "thermostat", "state not saved");
        }
    }
}

/// The thermostat: regulation on the runtime's temperature samples, the reports, and the screen
/// when there is one (one task: the screen draws from the regulator's state).
#[embassy_executor::task]
async fn regulator(kv: Nv, ui: Option<screen::Ui>) {
    let mut ui = ui;
    let saved = kv.scope(NS_APP).get::<Saved>(KEY_STATE).ok().flatten();
    let mut s = State {
        temperature: taf::temperature(),
        mode: saved
            .and_then(|s| Mode::from_index(i32::from(s.mode)))
            .unwrap_or(Mode::Auto),
        manual: saved.map_or(210, |s| s.manual.clamp(MIN_DECI, MAX_DECI)),
        over: Override::new(),
        week: load_week(kv),
        demand: Demand::new(),
    };
    loop {
        s.update();
        if let Some(ui) = &mut ui {
            ui.show(&mut s).await;
        }
        // Wake at the runtime's next temperature sample (every `report-period`), at the next
        // minute (a switch point, the clock on the screen), or when the screen has something due.
        let mut wake = match taf::local_time() {
            Some(t) => Instant::now() + Duration::from_secs(60 - u64::from(t.second)),
            None => Instant::now() + Duration::from_secs(60),
        };
        if let Some(t) = ui.as_ref().and_then(screen::Ui::deadline) {
            wake = wake.min(t);
        }
        let input = async {
            match &ui {
                Some(ui) => ui.next_input().await,
                None => core::future::pending().await,
            }
        };
        match select4(
            taf::next_set(),
            select(taf::temperature_sampled(), Timer::at(wake)),
            // The wall clock arrived (or moved): the program's setpoint is looked up again.
            select(PROGRAM_CHANGED.wait(), taf::clock_synced()),
            input,
        )
        .await
        {
            Either4::First((id, value)) => match id {
                thermostat::SETPOINT => {
                    s.set_by_hand(value as i16);
                    s.save(kv);
                }
                thermostat::MODE => {
                    if let Some(m) = Mode::from_index(value) {
                        s.set_mode(m, kv);
                    }
                }
                _ => {} // hysteresis and the common settings: read when used
            },
            Either4::Second(Either::First(centi)) => s.temperature = Some(centi),
            Either4::Second(Either::Second(())) => {
                if let Some(ui) = &mut ui {
                    ui.on_timer(&mut s, kv);
                }
            }
            Either4::Third(Either::First(())) => {
                s.week = load_week(kv);
                s.over.clear();
                info!(target: "thermostat", "weekly program changed");
            }
            Either4::Third(Either::Second(())) => {}
            Either4::Fourth(input) => {
                if let Some(ui) = &mut ui {
                    ui.on_input(input, &mut s, kv).await;
                }
            }
        }
    }
}

/// The stored weekly program (`Week::encode`'s bytes), else the default one.
fn load_week(kv: Nv) -> Week {
    let mut buf = [0u8; WEEK_BYTES];
    match kv.scope(NS_APP).get_bytes(KEY_PROGRAM, &mut buf) {
        Ok(Some(n)) => buf.get(..n).and_then(Week::decode).unwrap_or_default(),
        _ => Week::default(),
    }
}

// ---- the `thermostat` shell commands ----------------------------------------------------------

/// The days a `thermostat` command names: one day (`mon`..`sun`), `workdays`, `weekend` or `all`.
fn days_of(which: &str) -> Option<&'static [usize]> {
    const ALL: [usize; 7] = [0, 1, 2, 3, 4, 5, 6];
    match which {
        "all" => Some(&ALL),
        "workdays" => Some(&ALL[..5]),
        "weekend" => Some(&ALL[5..]),
        d => DAY_NAMES.iter().position(|n| *n == d).map(|i| &ALL[i..=i]),
    }
}

/// Store `week` if it differs from what is stored (the shell pages a long reply by running the
/// command again: the second run finds nothing to store) and tell the regulator.
fn store_week(ctx: &mut Ctx<'_>, week: &Week) -> Result<(), Outcome> {
    let kv = ctx.kv.raw();
    if load_week(kv) == *week {
        return Ok(());
    }
    let mut buf = [0u8; WEEK_BYTES];
    if kv
        .scope(NS_APP)
        .set_bytes(KEY_PROGRAM, week.encode(&mut buf))
        .is_err()
    {
        let _ = write!(ctx, "not saved");
        return Err(Outcome::code(shell::R_STORAGE));
    }
    PROGRAM_CHANGED.signal(());
    Ok(())
}

/// Print the program of `days`, a line each.
fn print_days(ctx: &mut Ctx<'_>, week: &Week, days: &[usize]) -> Outcome {
    for &d in days {
        let _ = writeln!(ctx, "{} {}", DAY_NAMES[d], week.days[d]);
    }
    Outcome::ok()
}

/// `thermostat program`: print the week; `thermostat program <days> <hh:mm> <°C> ...`: replace
/// those days' switch points (as many as one line holds; `thermostat point` sets one at a time).
fn cmd_program(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    let mut week = load_week(ctx.kv.raw());
    let Some((&which, points)) = args.split_first() else {
        return print_days(ctx, &week, &[0, 1, 2, 3, 4, 5, 6]);
    };
    let Some(days) = days_of(which) else {
        let _ = write!(
            ctx,
            "usage: thermostat program [<mon..sun|workdays|weekend|all> <hh:mm> <°C> ...]"
        );
        return Outcome::code(shell::R_BAD_ARG);
    };
    let mut text: heapless::String<96> = heapless::String::new();
    for (i, p) in points.iter().enumerate() {
        let _ = write!(text, "{}{p}", if i > 0 { " " } else { "" });
    }
    let day = match Day::parse(&text) {
        Ok(d) => d,
        Err(e) => {
            let _ = write!(
                ctx,
                "not a program ({e:?}): <hh:mm> <°C> pairs, up to six, in time order"
            );
            return Outcome::code(shell::R_BAD_ARG);
        }
    };
    for &d in days {
        week.days[d] = day;
    }
    if let Err(o) = store_week(ctx, &week) {
        return o;
    }
    print_days(ctx, &week, days)
}

/// `thermostat point <days> <hh:mm> <°C>`: set one switch point of those days (added, or the one
/// at that time replaced); `... <hh:mm> -` removes it. Short enough for a remote shell line.
fn cmd_point(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    let &[which, time, value] = args else {
        let _ = write!(
            ctx,
            "usage: thermostat point <mon..sun|workdays|weekend|all> <hh:mm> <°C|->"
        );
        return Outcome::code(shell::R_BAD_ARG);
    };
    let Some(days) = days_of(which) else {
        let _ = write!(ctx, "not a day: mon..sun, workdays, weekend or all");
        return Outcome::code(shell::R_BAD_ARG);
    };
    let Some(minute) = parse_minute(time) else {
        let _ = write!(ctx, "not a time of day (hh:mm)");
        return Outcome::code(shell::R_BAD_ARG);
    };
    let deci = match value {
        "-" => None,
        v => match parse_setpoint(v) {
            Some(d) => Some(d),
            None => {
                let _ = write!(ctx, "not a setpoint (5.0 to 30.0 °C, one decimal)");
                return Outcome::code(shell::R_BAD_ARG);
            }
        },
    };
    let mut week = load_week(ctx.kv.raw());
    for &d in days {
        if week.days[d].set_point(minute, deci).is_err() {
            let _ = write!(ctx, "{} is full (six switch points)", DAY_NAMES[d]);
            return Outcome::code(shell::R_BAD_ARG);
        }
    }
    if let Err(o) = store_week(ctx, &week) {
        return o;
    }
    print_days(ctx, &week, days)
}

static THERMOSTAT_COMMANDS: &[Entry] = &[
    Entry::cmd("program", Args::None, cmd_program),
    Entry::cmd("point", Args::None, cmd_point),
];

#[cfg(not(feature = "core-module-r1"))]
async fn run(b: Board) {
    // The LCD Module's buttons share the Core button's line: with the module, its driver takes the
    // button and the runtime goes without (pairing and the reset are in the menu then).
    let mut base = tower::taf_base!(b, no_button);
    let ui = if tower::lcd::probe(base.i2c).await {
        let res = tower::lcd::Resources::new(b.header_spi, b.button, b.header_int);
        screen::Ui::start(b.spawner, res, base.i2c).await
    } else {
        info!(target: "thermostat", "no LCD Module: headless");
        base.button = Some(b.button);
        None
    };
    b.spawner.spawn(regulator(b.kv, ui).unwrap());
    taf::run(base, &KIND, &SETTINGS).await
}

#[cfg(feature = "core-module-r1")]
async fn run(b: Board) {
    b.spawner.spawn(regulator(b.kv, None).unwrap());
    taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
}

/// The screen, on the LCD Module (Core Module r2).
#[cfg(not(feature = "core-module-r1"))]
#[path = "radio_thermostat/screen.rs"]
mod screen;

/// The Core Module r1 has no room for the screen (RAM; `docs/firmware/taf.md`, "RAM"): headless.
#[cfg(feature = "core-module-r1")]
mod screen {
    use super::{Nv, State};

    pub enum Ui {}
    pub enum Input {}

    impl Ui {
        pub async fn show(&mut self, _: &mut State) {}
        pub fn deadline(&self) -> Option<embassy_time::Instant> {
            None
        }
        pub async fn next_input(&self) -> Input {
            match *self {}
        }
        pub fn on_timer(&mut self, _: &mut State, _: Nv) {}
        pub async fn on_input(&mut self, _: Input, _: &mut State, _: Nv) {}
    }
}

app!(run, commands: COMMANDS, settings: &SETTINGS);
