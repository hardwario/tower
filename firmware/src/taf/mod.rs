//! **taf**, the TOWER Application Framework: the plumbing every standard TOWER radio app shares,
//! so an app is its business logic and nothing else.
//!
//! An app is a `Kind` from the property tables in
//! `tower_protocol::props` (its properties: what it measures, what can be set, its settings) plus
//! a task that does its job and talks to the runtime through four calls:
//!
//! - [`report`]`(id, value)`: a property's current value. The runtime decides when it is worth an
//!   uplink (a change past `report-delta` for the temperature, any change for the rest, all of them
//!   on a heartbeat) and batches what is due into one `Report`.
//! - [`event`]`(id, value)`: something happened; one `Event` uplink with its running count.
//! - [`next_set`]: a `set` from the host (MQTT `nodes/{addr}/set/{prop}`) for one of the app's
//!   controls; a setting (`Param`) is stored by the runtime before the app hears of it.
//! - [`param`]`(id)`: a setting's current value (from EEPROM, the shell's `system settings`).
//!
//! The runtime ([`run`], which never returns) owns the radio and gives every app the same
//! behaviour: OTA pairing (Core button held while unpaired; a 12 s hold re-pairs), cable
//! provisioning over USB, the unpaired LED cue, the heartbeat `Info` + full report, the remote
//! shell, settings over the radio, wall-clock time from the host ([`local_time`]), the watchdog.
//! Two radio classes: a **sleeping** node wakes the radio for an uplink and hears downlinks right
//! after it; a **listening** node (mains powered) keeps its receiver on, so a `set` takes effect at
//! once. The guide is `docs/taf.md`.
//!
//! ```ignore
//! const KIND: Kind = props::motion::KIND;
//! static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
//!
//! async fn run(b: Board) {
//!     b.spawner.spawn(detector(Pir::new(b.pir)).unwrap());
//!     taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
//! }
//! app!(run, commands: COMMANDS, settings: &SETTINGS);
//! ```

mod node;

use core::cell::RefCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use critical_section::Mutex;
use embassy_executor::Spawner;
use embassy_stm32::Peri;
use embassy_stm32::exti::ExtiInput;
use embassy_stm32::mode::Async;
use embassy_stm32::peripherals::{IWDG, PA15, PB3, PB4, PB5, PB7, PH1, SPI1};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use tower_protocol::props::{self, Access, Kind, Type};
use tower_protocol::radio::MAX_REPORT;
use tower_taf::clock::{Local, WallClock};

use crate::shell::{self, Setting};
use crate::storage::Nv;
use crate::{button, led};

pub use node::run;

/// The SPIRIT1's lines, as [`Board`](crate::board::Board) hands them out.
pub struct RadioPins {
    pub sdn: Peri<'static, PB7>,
    pub cs: Peri<'static, PA15>,
    pub spi: Peri<'static, SPI1>,
    pub sck: Peri<'static, PB3>,
    pub mosi: Peri<'static, PB5>,
    pub miso: Peri<'static, PB4>,
    pub irq: ExtiInput<'static, Async>,
}

/// What the runtime takes from the board; build it with [`taf_base!`](crate::taf_base).
pub struct Base {
    pub spawner: Spawner,
    pub kv: Nv,
    pub iwdg: Peri<'static, IWDG>,
    pub led: Peri<'static, PH1>,
    /// The Core Module button, for the pairing gestures and the app's button events; `None` when
    /// the app gives it to a module driver (the LCD Module's buttons share its line): pair with
    /// [`request_pairing`] then.
    pub button: Option<ExtiInput<'static, Async>>,
    pub radio: RadioPins,
}

/// Move the runtime's resources out of a [`Board`](crate::board::Board) (the app keeps the rest):
/// `taf_base!(b)`, or `taf_base!(b, no_button)` to leave the Core button to the app.
#[macro_export]
macro_rules! taf_base {
    ($b:ident) => {
        $crate::taf_base!(@ $b, Some($b.button))
    };
    ($b:ident, no_button) => {
        $crate::taf_base!(@ $b, None)
    };
    (@ $b:ident, $button:expr) => {
        $crate::taf::Base {
            spawner: $b.spawner,
            kv: $b.kv,
            iwdg: $b.iwdg,
            led: $b.led,
            button: $button,
            radio: $crate::taf::RadioPins {
                sdn: $b.radio_sdn,
                cs: $b.radio_cs,
                spi: $b.radio_spi,
                sck: $b.radio_sck,
                mosi: $b.radio_mosi,
                miso: $b.radio_miso,
                irq: $b.radio_irq,
            },
        }
    };
}

// ---- settings derived from the property tables ------------------------------------------------

const fn is_setting(p: &props::Prop) -> bool {
    matches!(p.access, Access::Param | Access::LocalParam)
}

/// How many shell settings `kind` has (its own and the common ones): the length of the array
/// [`settings`] builds.
pub const fn settings_len(kind: &Kind) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < props::COMMON.len() {
        if is_setting(&props::COMMON[i]) {
            n += 1;
        }
        i += 1;
    }
    let mut i = 0;
    while i < kind.props.len() {
        if is_setting(&kind.props[i]) {
            n += 1;
        }
        i += 1;
    }
    n
}

const fn setting_of(p: &props::Prop) -> Setting {
    let kind = match p.ty {
        Type::Bool => shell::Kind::Bool,
        Type::Number {
            min, max, decimals, ..
        } if decimals > 0 => shell::Kind::Fixed { min, max, decimals },
        Type::Number { min, max, .. } if min >= 0 => shell::Kind::Uint {
            min: min as u32,
            max: max as u32,
        },
        Type::Number { min, max, .. } => shell::Kind::Int { min, max },
        Type::Enum(names) => shell::Kind::Enum(names),
    };
    Setting {
        key: props::setting_key(p.id),
        name: p.name,
        kind,
        default: p.default,
    }
}

/// The shell settings of `kind` (every `Param` and `LocalParam`, common ones first), at the keys
/// [`props::setting_key`] derives from the property ids: pass them to `app!` as `settings:`.
pub const fn settings<const N: usize>(kind: &Kind) -> [Setting; N] {
    const EMPTY: Setting = Setting {
        key: 0,
        name: "",
        kind: shell::Kind::Bool,
        default: "",
    };
    let mut out = [EMPTY; N];
    let mut n = 0;
    let mut i = 0;
    while i < props::COMMON.len() {
        if is_setting(&props::COMMON[i]) {
            out[n] = setting_of(&props::COMMON[i]);
            n += 1;
        }
        i += 1;
    }
    let mut i = 0;
    while i < kind.props.len() {
        if is_setting(&kind.props[i]) {
            out[n] = setting_of(&kind.props[i]);
            n += 1;
        }
        i += 1;
    }
    assert!(n == N, "settings: N must be settings_len(kind)");
    out
}

// ---- the state the app and the runtime share --------------------------------------------------

/// One reported property: its latest value, and what the runtime last sent of it.
#[derive(Clone, Copy)]
struct Slot {
    id: u8,
    value: i32,
    sent: tower_taf::report::Reporter,
}

/// Reported properties the runtime keeps (an app reports a handful: the most, `radio_control`,
/// seven).
const SLOTS: usize = 10;

struct Shared {
    kind: Option<&'static Kind>,
    settings: &'static [Setting],
    kv: Option<Nv>,
    led: Option<led::Led>,
    clock: WallClock,
}

static SHARED: Mutex<RefCell<Shared>> = Mutex::new(RefCell::new(Shared {
    kind: None,
    settings: &[],
    kv: None,
    led: None,
    clock: WallClock::new(),
}));
static SLOT_TABLE: Mutex<RefCell<heapless::Vec<Slot, SLOTS>>> =
    Mutex::new(RefCell::new(heapless::Vec::new()));
/// Events from the app, sent in order.
static EVENTS: Channel<CriticalSectionRawMutex, (u8, i32), 4> = Channel::new();
/// `set`s for the app's controls (and notices of settings changed over the radio).
static INBOX: Channel<CriticalSectionRawMutex, (u8, i32), 2> = Channel::new();
/// The Core button's events, past the runtime's pairing gestures.
static BUTTON: Channel<CriticalSectionRawMutex, button::Event, 4> = Channel::new();
/// Something for the runtime to look at (a report, an event, a pairing request).
static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Raised by every clock sync, for [`clock_synced`].
static CLOCK_SYNCED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static PAIR_REQUEST: AtomicBool = AtomicBool::new(false);
/// The gateway this node is paired with (0 = none), for [`paired`] and `node status`.
static GATEWAY: AtomicU32 = AtomicU32::new(0);
/// This node's radio address, for `node status`.
static ADDR: AtomicU32 = AtomicU32::new(0);

/// Report the current value of property `id` (a state, a control, or the temperature). Cheap
/// and non-blocking: the runtime sends it when its policy says so.
pub fn report(id: u8, value: i32) {
    critical_section::with(|cs| {
        let mut table = SLOT_TABLE.borrow_ref_mut(cs);
        match table.iter_mut().find(|s| s.id == id) {
            Some(s) => s.value = value,
            None => {
                let _ = table.push(Slot {
                    id,
                    value,
                    sent: tower_taf::report::Reporter::new(),
                });
            }
        }
    });
    WAKE.signal(());
}

/// Report event `id` with its detail `value`; it goes up as soon as the radio allows. Up to 4 wait
/// for the radio; more are dropped (rate-limit bursts in the app).
pub fn event(id: u8, value: i32) {
    if EVENTS.try_send((id, value)).is_err() {
        log::warn!(target: "node", "event {} dropped (queue full)", id);
    }
    WAKE.signal(());
}

/// The next `set` for one of the app's controls, `(id, value)` (validated against its type),
/// or the notice that a setting changed over the radio (already stored; re-read it with [`param`]).
/// Report a control's new value once applied: that confirms it to the host.
pub async fn next_set() -> (u8, i32) {
    INBOX.receive().await
}

/// The next Core button event the runtime passed on (not those spent on pairing).
pub async fn next_button() -> button::Event {
    BUTTON.receive().await
}

/// The current value of setting `id` (a `Param` or `LocalParam` of the app's kind or the common
/// table), as its type's wire value; its default when unset. 0 before [`run`] starts.
pub fn param(id: u8) -> i32 {
    let (kv, settings) = critical_section::with(|cs| {
        let s = SHARED.borrow_ref(cs);
        (s.kv, s.settings)
    });
    let Some(kv) = kv else { return 0 };
    use tower_hmi::menu::Values as _;
    shell::SettingsValues::new(kv, settings).get(u16::from(props::setting_key(id)))
}

/// Store setting `id` (validated by the shell's rules); whether it was stored.
pub(crate) fn set_param(id: u8, value: i32) -> bool {
    let (kv, settings) = critical_section::with(|cs| {
        let s = SHARED.borrow_ref(cs);
        (s.kv, s.settings)
    });
    let Some(kv) = kv else { return false };
    use tower_hmi::menu::Values as _;
    let mut values = shell::SettingsValues::new(kv, settings);
    values.set(u16::from(props::setting_key(id)), value);
    values.get(u16::from(props::setting_key(id))) == value
}

/// The board LED (the runtime shows its pairing cues on it; an app may flash it for its events,
/// when the `led` setting is on: see [`led_on`]).
pub fn led() -> Option<led::Led> {
    critical_section::with(|cs| SHARED.borrow_ref(cs).led)
}

/// Whether event feedback on the LED is on (the common `led` setting).
pub fn led_on() -> bool {
    param(props::common::LED) != 0
}

/// Local wall-clock time, once the host has synced it (an app of a kind with `time` asks for it
/// at boot and every 12 hours).
pub fn local_time() -> Option<Local> {
    let now = crate::clock::now_ms();
    critical_section::with(|cs| SHARED.borrow_ref(cs).clock.local(now))
}

/// Wait until the host syncs the wall clock (what [`local_time`] reads changed: an app that
/// schedules by it looks again). One waiter.
pub async fn clock_synced() {
    CLOCK_SYNCED.wait().await
}

/// Start OTA pairing (the gesture for an app that gave the Core button away).
pub fn request_pairing() {
    PAIR_REQUEST.store(true, Ordering::Relaxed);
    WAKE.signal(());
}

/// Whether the node is paired with a gateway.
pub fn paired() -> bool {
    GATEWAY.load(Ordering::Relaxed) != 0
}

// ---- the common shell commands ------------------------------------------------------------

/// `node status`: the node's address, kind and class, its gateway, its wall-clock time.
fn cmd_status(ctx: &mut shell::Ctx<'_>, _args: &[&str]) -> shell::Outcome {
    let kind = critical_section::with(|cs| SHARED.borrow_ref(cs).kind);
    let _ = write!(ctx, "addr 0x{:08x}", ADDR.load(Ordering::Relaxed));
    if let Some(k) = kind {
        let class = match k.class {
            props::Class::Sleeping => "sleeping",
            props::Class::Listening => "listening",
        };
        let _ = write!(ctx, ", kind {} ({class})", k.name);
    }
    match GATEWAY.load(Ordering::Relaxed) {
        0 => {
            let _ = write!(ctx, ", not paired");
        }
        gw => {
            let _ = write!(ctx, ", gateway 0x{gw:08x}");
        }
    }
    match local_time() {
        Some(t) => {
            let day = tower_taf::program::DAY_NAMES[t.weekday as usize];
            let _ = write!(
                ctx,
                ", time {day} {:02}:{:02}:{:02}",
                t.hour(),
                t.minute_of_hour(),
                t.second
            );
        }
        None => {
            let _ = write!(ctx, ", time not synced");
        }
    }
    shell::Outcome::ok()
}

/// `node pair`: start OTA pairing, as the button gesture does (console only: over the radio it
/// would let any MQTT publisher move a node to another gateway's window).
fn cmd_pair(ctx: &mut shell::Ctx<'_>, _args: &[&str]) -> shell::Outcome {
    request_pairing();
    let _ = write!(ctx, "pairing requested");
    shell::Outcome::ok()
}

/// The longest press `button simulate` injects (past the 12 s re-pair gesture).
const SIM_MAX_MS: u32 = 15_000;

/// `button simulate <ms>`: a synthetic press of `<ms>` through the real recognition (debounce,
/// click, hold): the console "finger" for tests and the HIL bench. Console only: over the radio
/// it would inject events integrations act on, or start the re-pair gesture fleet-wide.
fn cmd_simulate(ctx: &mut shell::Ctx<'_>, args: &[&str]) -> shell::Outcome {
    match args.first().and_then(|s| s.parse::<u32>().ok()) {
        Some(ms) if (1..=SIM_MAX_MS).contains(&ms) => {
            button::simulate(ms);
            let _ = write!(ctx, "simulating a {ms} ms press");
            shell::Outcome::ok()
        }
        _ => {
            let _ = write!(ctx, "usage: button simulate <ms> (1..={SIM_MAX_MS})");
            shell::Outcome::code(shell::R_BAD_ARG)
        }
    }
}

/// The `node` commands every taf app mounts: `node status`, `node pair`.
pub const NODE_COMMANDS: shell::Entry = shell::Entry::menu(
    "node",
    &[
        shell::Entry::cmd("status", shell::Args::None, cmd_status),
        shell::Entry::cmd_console_only("pair", shell::Args::None, cmd_pair),
    ],
);

/// `button simulate <ms>`, for an app whose runtime owns the Core button.
pub const BUTTON_COMMANDS: shell::Entry = shell::Entry::menu(
    "button",
    &[shell::Entry::cmd_console_only(
        "simulate",
        shell::Args::None,
        cmd_simulate,
    )],
);

// ---- runtime-side access to the shared state ----------------------------------------------

fn init_shared(kind: &'static Kind, settings: &'static [Setting], kv: Nv, led: led::Led, addr: u32) {
    critical_section::with(|cs| {
        let mut s = SHARED.borrow_ref_mut(cs);
        s.kind = Some(kind);
        s.settings = settings;
        s.kv = Some(kv);
        s.led = Some(led);
    });
    ADDR.store(addr, Ordering::Relaxed);
}

fn sync_clock(unix: u32, utc_offset_min: i16) {
    let now = crate::clock::now_ms();
    critical_section::with(|cs| SHARED.borrow_ref_mut(cs).clock.sync(unix, utc_offset_min, now));
    CLOCK_SYNCED.signal(());
}

/// Whether the node should ask the host for the time (its kind needs it, and the last sync is
/// missing or 12 hours old).
fn wants_time(kind: &Kind) -> bool {
    const MAX_AGE_MS: u64 = 12 * 3600 * 1000;
    let now = crate::clock::now_ms();
    kind.time && critical_section::with(|cs| SHARED.borrow_ref(cs).clock.wants_sync(now, MAX_AGE_MS))
}

/// Collect into `out` (up to its capacity) the reported values due now, from slot `from` on: all of
/// them when `all` (a heartbeat, a `Describe`), else those the policy says are news. Returns the
/// slot after the last one looked at (`None` when the table is done).
fn due_reports(
    all: bool,
    now_s: u64,
    from: usize,
    out: &mut heapless::Vec<(u8, i32), MAX_REPORT>,
) -> Option<usize> {
    let delta = param(props::common::REPORT_DELTA);
    let heartbeat = param(props::common::HEARTBEAT).max(60) as u64;
    critical_section::with(|cs| {
        let table = SLOT_TABLE.borrow_ref(cs);
        for (i, s) in table.iter().enumerate().skip(from) {
            // The temperature moves continuously: only a change past `report-delta` is news.
            // Everything else is discrete: any change is.
            let d = if s.id == props::common::TEMPERATURE {
                delta
            } else {
                0
            };
            if (all || s.sent.due(s.value, now_s, d, heartbeat)) && out.push((s.id, s.value)).is_err() {
                return Some(i);
            }
        }
        None
    })
}

/// Record that `items` went up at `now_s`.
fn mark_sent(items: &[(u8, i32)], now_s: u64) {
    critical_section::with(|cs| {
        let mut table = SLOT_TABLE.borrow_ref_mut(cs);
        for (id, value) in items {
            if let Some(s) = table.iter_mut().find(|s| s.id == *id) {
                s.sent.sent(*value, now_s);
            }
        }
    });
}

/// Make property `id` due at the next send whatever its value (a `set` is confirmed by a report
/// even when it changed nothing).
fn force_report(id: u8) {
    critical_section::with(|cs| {
        if let Some(s) = SLOT_TABLE.borrow_ref_mut(cs).iter_mut().find(|s| s.id == id) {
            s.sent.reset();
        }
    });
}

/// Forget what was sent (a new pairing: the new gateway has seen nothing).
fn forget_sent() {
    critical_section::with(|cs| {
        for s in SLOT_TABLE.borrow_ref_mut(cs).iter_mut() {
            s.sent.reset();
        }
    });
}

// ---- common sensors ---------------------------------------------------------------------------

/// The Core Module's thermometer as the common `temperature` property: a one-shot read every
/// `report-period` (the runtime sends it when it moved past `report-delta`, or on a heartbeat).
/// Spawn it when the app leaves the TMP112 its own bus: `spawner.spawn(taf::thermometer(b.tmp112)?)`.
#[embassy_executor::task]
pub async fn thermometer(mut sensor: crate::board::Tmp112Sensor) {
    loop {
        match sensor.oneshot().await {
            Ok(raw) => report(
                props::common::TEMPERATURE,
                crate::tmp112::raw_to_millicelsius(raw) / 10,
            ),
            Err(_) => log::warn!(target: "node", "thermometer not read (I2C)"),
        }
        let period = param(props::common::REPORT_PERIOD).clamp(5, 86_400) as u64;
        embassy_time::Timer::after_secs(period).await;
    }
}
