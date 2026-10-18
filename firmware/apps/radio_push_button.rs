//! radio_push_button (TOWER IoT Kit product firmware): a wireless push button with
//! thermometer + accelerometer, built as a **sleeping node**.
//!
//! Every button press / release / click / hold sends a secured radio message carrying
//! that event's running count; temperature is measured on a settable period and sent
//! on-change (threshold) or at latest every heartbeat; the accelerometer's tilt
//! interrupt wakes the node for motion/orientation events. Between events the MCU
//! STOPs (µA) and the SPIRIT1 sleeps; downlinks reach the node via the gateway's
//! queue: each confirmed uplink's ACK carries a *pending* flag, and when set the node
//! holds a short RX window, executes the delivered remote-shell line in the standard
//! shell dispatcher ([`shell::stream_line`]) and streams the response back chunk-by-chunk
//! (`RadioSink`), so a full `export` is never truncated at a fixed buffer. No tab
//! completion over the air: completion is a per-transport feature the radio doesn't offer.
//!
//! Pairing:
//! * **OTA**: hold the button for `hold-time` (1 s by default) while unprovisioned: the node pairs (an X25519 key
//!   agreement, both ends log the same `pairing: SAS`) with any gateway that has a window open.
//!   The new key is **provisional** until the gateway acknowledges an uplink under it (the
//!   commit round: the `NodeInfo`, a few attempts over ~15 s); only then is `(gw, key)`
//!   persisted. A key the gateway never acknowledges (a lost JOIN_CONFIRM, a full registry) is
//!   discarded and the node stays unpaired, so another hold can retry.
//! * **Re-pair** (the recovery gesture): hold the button ≥ [`REPAIR_HOLD`] (12 s) while
//!   paired: the node runs the same OTA pairing; the old pairing stays in force (RAM and
//!   EEPROM) unless the new one commits. For a node whose gateway forgot it, or a move to
//!   another gateway. OTA pairing never re-keys an address a gateway still has registered, so
//!   remove the node on its gateway first (`tower nodes remove`), then open a window and hold.
//! * **Cable**: on USB, the node serves the management channel: `Describe` (role
//!   probe), `Provision` (host-minted credentials; key never rides the shell), and
//!   `JoinOpen` (host-initiated OTA join, with the same commit round).
//!
//! Each of the four button events has a **master enable**: a disabled event is ignored
//! entirely: neither reported over radio nor shown on the LED. The event *recognition*
//! timing is configurable (debounce, click-timeout, hold-time: what the physical input
//! means); the LED feedback is a fixed indicator (press/release/hold a single pulse, a
//! click a double-blink; distinct shapes because a quick tap fires Press on the down
//! edge and Release+Click together on the up edge, so a shared shape would blend). The
//! default is the coherent **gesture** scheme: click + hold on, press + release off;
//! enable press/release for raw-edge reporting.
//!
//! All boards also play the common power-on signature first (500 ms off → 2 s on →
//! 500 ms off, [`Board::boot_indicator`](tower::board::Board::boot_indicator)); this
//! app's own behaviour begins after that.
//!
//! Settings (remote-shell reconfigurable, `system settings set …`): `therm-period`,
//! `therm-delta` (centi-°C, 0 = always send), `accel` (off/low/medium/high, applied at
//! boot), `heartbeat`; per-event `{press,release,click,hold}` (on/off master enable);
//! button timing `debounce-press`, `debounce-release`, `click-timeout`, `hold-time`
//! (ms, applied at boot). The `button simulate <ms>` command injects a synthetic press
//! of that length through the *real* recognition machine (debounce/click/hold) and on
//! into reporting: the console "finger" for testing without a physical press, also
//! used by the HIL bench (console only: refused over the radio).
//!
//!   just build app radio_push_button
//!   just run   app radio_push_button   (then press the button)

#![no_std]
#![no_main]

use core::pin::{Pin, pin};
use core::sync::atomic::{AtomicU8, Ordering};

use embassy_futures::select::{Either, Either4, select, select4};
use embassy_stm32::exti::ExtiInput;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::mode::Async;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use tower::board::{GuardedI2c, UniqueId};
use tower::lis2dh12::{self, Lis2dh12, Sensitivity, TiltConfig};
use tower::radio::Spirit1;
use tower::radio::config::Band;
use tower::radio::net::{
    COMMIT_SETTLE_MS, CommitStep, Net, NetConfig, PAIRING_WINDOW, SendResult, commit_step, is_reserved_addr,
    peer_addr_valid,
};
use tower::shell::ShellSink;
use tower::storage::{NS_APP, NS_SHELL, Nv};
use tower::tmp112::{self, Tmp112};
use tower::{app, board::Board, button, console, led, shell, watchdog};
use tower_protocol::mgmt::{self, DeviceInfo, DeviceRole, Joined, MgmtOp, ProvisionAck};
use tower_protocol::msg::MgmtRequest;
use tower_protocol::radio::{
    AccelKind, ButtonKind, MAX_RADIO_PAYLOAD, NodeCmd, NodeInfo, NodeMsg, NodeShellChunk, RADIO_SHELL_CHUNK,
    RADIO_TAG, decode_node_cmd, encode_node_msg,
};
use tower_protocol::{MsgType, decode_frame};

// --- persistence (NS_APP) --------------------------------------------------------

/// Provision record: `(gw_addr, key, band, channel)` (postcard). The node's own radio
/// address is the SDK `addr` base setting (`shell::radio_addr`), not stored here.
const KEY_PROVISION: u8 = 0x00;

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
struct Provisioned {
    gw_addr: u32,
    key: [u8; 16],
    band: u8,
    channel: u8,
}

// --- settings (NS_SHELL locals) ----------------------------------------------------

const SET_THERM_PERIOD: u8 = 0x10;
const SET_THERM_DELTA: u8 = 0x11;
const SET_ACCEL: u8 = 0x12;
const SET_HEARTBEAT: u8 = 0x13;

// Per-button-event master enable: a disabled event is ignored entirely: NOT reported
// over radio and no LED. Each of the four events is independently switchable.
const SET_PRESS: u8 = 0x20;
const SET_RELEASE: u8 = 0x22;
const SET_CLICK: u8 = 0x24;
const SET_HOLD: u8 = 0x26;

// Button *recognition* timing (ms) → `button::Config`. These shape what the physical
// input means: how long the level must hold to count as a press/release, the longest
// press+release that still reads as a click, and the hold-to-trigger duration. Read
// once at boot (a change needs a reboot, like `accel`).
const SET_DEBOUNCE_PRESS: u8 = 0x30;
const SET_DEBOUNCE_RELEASE: u8 = 0x31;
const SET_CLICK_TIMEOUT: u8 = 0x32;
const SET_HOLD_TIME: u8 = 0x33;

// Fixed LED-feedback shapes/timings (not user-configurable; the LED is just an
// indicator). press/release/hold flash a single pulse; a click is a double-blink of
// two `LED_CLICK_MS` blinks so it reads apart from an adjacent press on the one LED.
const LED_PRESS_MS: u32 = 250;
const LED_RELEASE_MS: u32 = 250;
const LED_CLICK_MS: u32 = 100;
const LED_HOLD_MS: u32 = 1000;

static BTN_SETTINGS: &[shell::Setting] = &[
    shell::Setting {
        key: SET_THERM_PERIOD,
        name: "therm-period",
        kind: shell::Kind::Uint { min: 5, max: 86_400 },
        default: "60",
    },
    shell::Setting {
        key: SET_THERM_DELTA,
        name: "therm-delta",
        kind: shell::Kind::Uint { min: 0, max: 10_000 },
        default: "50",
    },
    shell::Setting {
        key: SET_ACCEL,
        name: "accel",
        kind: shell::Kind::Enum(&["off", "low", "medium", "high"]),
        default: "medium",
    },
    shell::Setting {
        key: SET_HEARTBEAT,
        name: "heartbeat",
        kind: shell::Kind::Uint { min: 60, max: 86_400 },
        default: "900",
    },
    // Button events: each a master enable (report over radio + fixed LED feedback); a
    // disabled event is ignored entirely. Default = the coherent **gesture** scheme:
    // click + hold ON, press + release OFF (a click is a quick press+release, so
    // reporting all four is redundant and their LEDs blend on a tap; enable
    // press/release for raw edges).
    shell::Setting {
        key: SET_PRESS,
        name: "press",
        kind: shell::Kind::Bool,
        default: "off",
    },
    shell::Setting {
        key: SET_RELEASE,
        name: "release",
        kind: shell::Kind::Bool,
        default: "off",
    },
    shell::Setting {
        key: SET_CLICK,
        name: "click",
        kind: shell::Kind::Bool,
        default: "on",
    },
    shell::Setting {
        key: SET_HOLD,
        name: "hold",
        kind: shell::Kind::Bool,
        default: "on",
    },
    // Button recognition timing (ms), applied at boot (reboot to change).
    shell::Setting {
        key: SET_DEBOUNCE_PRESS,
        name: "debounce-press",
        kind: shell::Kind::Uint { min: 1, max: 1000 },
        default: "30",
    },
    shell::Setting {
        key: SET_DEBOUNCE_RELEASE,
        name: "debounce-release",
        kind: shell::Kind::Uint { min: 1, max: 1000 },
        default: "30",
    },
    shell::Setting {
        key: SET_CLICK_TIMEOUT,
        name: "click-timeout",
        kind: shell::Kind::Uint { min: 50, max: 5000 },
        default: "500",
    },
    shell::Setting {
        key: SET_HOLD_TIME,
        name: "hold-time",
        kind: shell::Kind::Uint {
            min: 100,
            max: 10_000,
        },
        default: "1000",
    },
];

// --- synthetic button press (the console "finger" / HIL bench) ---------------------

/// Longest press `button simulate` will inject (15 s: past any hold-time and past
/// [`REPAIR_HOLD`], so the bench can drive the re-pair gesture too).
const SIM_MAX_MS: u32 = 15_000;

/// `button simulate <ms>`: inject a synthetic press held `<ms>` milliseconds into the
/// button driver, which derives the event(s) through the *real* recognition machine
/// (debounce / click-timeout / hold-time). So a short press debounces away, a tap
/// yields a click, a long press yields a hold: the same path a physical press takes,
/// exercising the configured button timing (which feeding a finished event would skip).
fn cmd_simulate(ctx: &mut shell::Ctx<'_>, args: &[&str]) -> shell::Outcome {
    use core::fmt::Write as _;
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

/// Console-only: over the radio it would let anyone who can publish a remote shell command
/// inject clicks into the event stream integrations act on, or hold the button 13 s and start the
/// re-pair gesture on every node of the fleet.
static BTN_COMMANDS: &[shell::Entry] = &[shell::Entry::menu(
    "button",
    &[shell::Entry::cmd_console_only(
        "simulate",
        shell::Args::None,
        cmd_simulate,
    )],
)];

// --- LED patterns -------------------------------------------------------------------

static LED_CH: led::LedChannel = led::LedChannel::new();
/// Double-blink background: not paired with any gateway yet (hold the button to pair).
static UNPROVISIONED: led::Pattern = &[
    led::Step::on(60),
    led::Step::off(120),
    led::Step::on(60),
    led::Step::off(1760),
];
/// Fast blink while the OTA join runs.
static JOINING: led::Pattern = &[led::Step::on(100), led::Step::off(150)];
/// How long the unpaired double-blink runs before it stops (it costs LED current on a node
/// that may sit unpaired in a drawer for months); any button press shows it again for as long.
const UNPAIRED_CUE: Duration = Duration::from_secs(180);

/// Backoff of the accelerometer interrupt wait after its source register could not be read (an
/// I²C error leaves the latch set, so the line stays high): first 1 s, doubling to 5 min.
const ACCEL_BACKOFF_MIN: Duration = Duration::from_secs(1);
const ACCEL_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// The pause before the next interrupt is taken when the line is high again right after a
/// successful read: the board is still moving (the read cleared the latch, new motion set it
/// again), which is no fault; the pause only keeps a line stuck high in hardware from spinning
/// the loop.
const ACCEL_REARM: Duration = Duration::from_millis(100);
/// Good reads in a row that found nothing to report while the line stayed high: a line stuck
/// high (a wiring fault, a latch the chip never clears), not motion. Real continuous motion yields
/// an event at least every 5 reads (the driver passes one per 500 ms, read every
/// [`ACCEL_REARM`]), so this never trips on it; a stuck line backs off like an unreadable one
/// instead of costing 10 wake-ups and I2C reads a second forever.
const ACCEL_STUCK_READS: u8 = 10;
/// Motion uplinks go out at most once per this long. Continuous motion (a node carried around)
/// raises about two tilt events a second, ~29 ms/s of airtime against the EU duty budget's
/// 10 ms/s refill (1 %): it emptied the budget in ~30 min, and then every button uplink was
/// refused. Motion inside the interval is coalesced, and its latest face goes out when the
/// interval ends, so the last orientation is never lost.
const ACCEL_UPLINK_MIN: Duration = Duration::from_secs(10);

/// Post-uplink RX window when the gateway's ACK advertises a queued downlink. The
/// gateway transmits ~20 ms after our uplink with one retry (~450 ms worst); 1 s
/// covers it with margin.
const DOWNLINK_WINDOW: Duration = Duration::from_secs(1);
/// Shell-response chunks ride confirmed uplinks with extra reps: the gateway's
/// single-owner loop may be busy retrying a queued downlink when a chunk goes out.
const CHUNK_REPS: u8 = 5;
/// The re-pair gesture: a press held this long on a paired node runs OTA pairing again (the
/// recovery for a node whose gateway forgot it, or a move to another gateway). Past the longest
/// configurable `hold-time` (10 s), so no hold setting can turn an ordinary Hold into it, and far
/// past any accidental press; even a deliberate one only costs a pairing attempt, since the old
/// pairing stays in force unless a new one commits.
const REPAIR_HOLD: Duration = Duration::from_secs(12);
/// Consecutive unacknowledged uplinks after which the node logs a hint that its gateway may have
/// forgotten it (once per streak; the node keeps sending: it may just be out of range).
const UNACKED_HINT: u8 = 8;
/// The current streak of unacknowledged uplinks (main task only; load/store is enough on M0+).
static UNACKED: AtomicU8 = AtomicU8::new(0);

fn read_u32_setting(kv: Nv, local: u8, default: u32) -> u32 {
    let mut b = [0u8; 4];
    match kv.scope(NS_SHELL).get_bytes(local, &mut b) {
        Ok(Some(4)) => u32::from_le_bytes(b),
        _ => default,
    }
}

fn read_bool_setting(kv: Nv, local: u8, default: bool) -> bool {
    let mut b = [0u8; 1];
    match kv.scope(NS_SHELL).get_bytes(local, &mut b) {
        Ok(Some(1)) => b[0] != 0,
        _ => default,
    }
}

/// Whether a button event is enabled (reported over radio + LED). Defaults mirror
/// `BTN_SETTINGS`: the gesture events (click/hold) on, the raw edges (press/release) off.
fn event_enabled(kv: Nv, ev: button::Event) -> bool {
    let (key, default) = match ev {
        button::Event::Press => (SET_PRESS, false),
        button::Event::Release => (SET_RELEASE, false),
        button::Event::Click => (SET_CLICK, true),
        button::Event::Hold => (SET_HOLD, true),
    };
    read_bool_setting(kv, key, default)
}

/// Flash the LED for one (already enabled) button event with its fixed shape:
/// press/release/hold a single pulse, a click a double-blink so it reads apart from an
/// adjacent press even on the one LED. Immediate, fire-and-forget: the LED dispatcher
/// owns the timing; the uplink follows independently.
fn led_feedback(led: &led::Led, ev: button::Event) {
    match ev {
        button::Event::Press => led.flash(LED_PRESS_MS),
        button::Event::Release => led.flash(LED_RELEASE_MS),
        button::Event::Click => led.pulse(LED_CLICK_MS, LED_CLICK_MS, 2),
        button::Event::Hold => led.flash(LED_HOLD_MS),
    }
}

/// Build the button recognition config from the persisted timing settings (read once
/// at boot). `scan_interval` stays a fixed internal poll rate.
fn button_config(kv: Nv) -> button::Config {
    button::Config {
        scan_interval: Duration::from_millis(5),
        debounce_press: Duration::from_millis(read_u32_setting(kv, SET_DEBOUNCE_PRESS, 30) as u64),
        debounce_release: Duration::from_millis(read_u32_setting(kv, SET_DEBOUNCE_RELEASE, 30) as u64),
        click_timeout: Duration::from_millis(read_u32_setting(kv, SET_CLICK_TIMEOUT, 500) as u64),
        hold_time: Duration::from_millis(read_u32_setting(kv, SET_HOLD_TIME, 1000) as u64),
    }
}

fn read_accel_setting(kv: Nv) -> Option<Sensitivity> {
    let mut b = [0u8; 8];
    match kv.scope(NS_SHELL).get_bytes(SET_ACCEL, &mut b) {
        Ok(Some(n)) => match &b[..n] {
            b"off" => None,
            b"low" => Some(Sensitivity::Low),
            b"high" => Some(Sensitivity::High),
            _ => Some(Sensitivity::Medium),
        },
        _ => Some(Sensitivity::Medium),
    }
}

/// The shared I²C2 bus, resident with the accelerometer (its tilt IRQ needs register
/// access most often); `measure_temp` borrows it for each one-shot read. `None` only
/// transiently inside a swap; see `board::Board` for the bus-sharing contract.
type AccelBus = Option<Lis2dh12<GuardedI2c>>;

async fn run(b: Board) {
    // Hardware safety net: a wedged unit resets itself instead of dying in the field. The
    // feeder wakes the low-power executor even from STOP; the longest nominal timeout (26 s,
    // ~15 s on the fastest LSI, so a pet every ~7.4 s) keeps those wakes rare on this
    // battery node.
    watchdog::enable(b.iwdg, b.spawner, Duration::from_secs(26));

    let led = led::init(
        b.spawner,
        Output::new(b.led, Level::Low, Speed::Low),
        &LED_CH,
        led::Polarity::ActiveHigh,
    );
    static BTN_CH: button::ButtonChannel = button::ButtonChannel::new();
    let btn = button::init_exti(
        b.spawner,
        b.button,
        button::Polarity::ActiveHigh,
        button_config(b.kv), // debounce / click-timeout / hold-time from settings
        &BTN_CH,
    );
    let mut accel_int = b.accel_int;

    let kv = b.kv;
    let app_kv = kv.scope(NS_APP);

    // This node's radio address = the `addr` base setting (pinned or UID-derived).
    let addr = shell::radio_addr(kv);
    let mut provision: Option<Provisioned> = app_kv.get::<Provisioned>(KEY_PROVISION).ok().flatten();

    // Accelerometer: reclaim the shared I²C2 bus and arm the tilt interrupt (register
    // state persists while powered, so this is a boot-time configuration; the `accel`
    // setting applies on the next reboot).
    let accel_sens = read_accel_setting(kv);
    let mut bus = {
        let mut accel = Lis2dh12::new(b.tmp112.release(), lis2dh12::ADDR_DEFAULT);
        if let Some(sens) = accel_sens {
            let _ = accel.init();
            match accel.enable_tilt(TiltConfig::new(sens)) {
                Ok(()) => info!(target: "accel", "tilt detection armed"),
                Err(_) => warn!(target: "accel", "tilt detection not armed (I2C error)"),
            }
        }
        Some(accel)
    };

    let radio = Spirit1::new(
        b.radio_spi,
        b.radio_sck,
        b.radio_mosi,
        b.radio_miso,
        b.radio_cs,
        b.radio_sdn,
        b.radio_irq,
    );
    let (band, channel) = match provision {
        Some(p) => (
            if p.band == mgmt::BAND_US915 {
                Band::Us915
            } else {
                Band::Eu868
            },
            p.channel,
        ),
        None => (Band::DEFAULT, 0),
    };
    let mut net = match Net::new(
        radio,
        kv,
        NetConfig {
            addr,
            // NOT the link key: the node runs registered peers only, so the default key never
            // seals or opens anything. The link key here used to be the fallback for a gateway
            // `add_peer` refused (this node's own `addr` set to the gateway's): `send` then
            // sealed under the link key with `src` = the gateway's address, the gateway's own
            // nonces.
            key: [0u8; 16],
            band,
            channel,
            // A provisioning record means this store held a TX watermark before: if it is gone
            // now, fail closed rather than restart the counter under the stored key.
            keys_stored: provision.is_some(),
        },
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            log::error!(target: "node", "radio init failed ({e}), running without radio");
            return;
        }
    };
    // Registered peers only: the node talks to exactly one peer, its gateway (registered below
    // or on a later join). With the default lane on, an unprovisioned node's all-zero default
    // key would be a public key for any frame from an unregistered src; the node never listens
    // unprovisioned today, but there is no reason to keep that door. Also keeps the default
    // lane's EEPROM record unused.
    net.set_default_lane(false);
    // Persist the gateway's replay lane on every accepted downlink. A sleeping node hears a
    // handful per boot, so the lazy default (every 32 accepts) never wrote it at all, and a
    // recorded downlink (a `system reboot`) replayed as fresh after every reboot, while the
    // remote-shell dedup is RAM-only. Downlinks are rare: the wear is negligible.
    net.set_lane_persist_period(1);
    if let Some(p) = provision
        && !net.add_peer(p.gw_addr, &p.key)
    {
        // Refused: the gateway's address is reserved or this node's own (`addr` set to it).
        // Stay off the air rather than talk to a peer we cannot key (docs/gateway.md).
        log::error!(
            target: "node",
            "gateway 0x{:08x} is not a valid peer of 0x{:08x}, radio off: fix addr or re-pair",
            p.gw_addr,
            addr
        );
        provision = None;
    }
    let _ = net.sleep().await; // radio sleeps between events from the start

    match provision {
        Some(p) => info!(target: "node", "0x{:08x} paired with gateway 0x{:08x}", addr, p.gw_addr),
        None => info!(target: "node", "0x{:08x} not paired: hold the button 1 s to pair", addr),
    }
    // The unpaired cue runs for `UNPAIRED_CUE`, then stops; a button press shows it again.
    let mut cue_until: Option<Instant> = None;
    if provision.is_none() {
        led.set_background(Some(UNPROVISIONED));
        cue_until = Some(Instant::now() + UNPAIRED_CUE);
    }

    // Per-kind running counts since boot (RAM: a reset is a reboot, disambiguated
    // host-side by the heartbeat's session_id).
    let mut counts = [0u32; 4];
    let mut last_temp_sent: Option<i32> = None;
    let mut last_temp_tx = Instant::now();
    let mut next_temp = Instant::now(); // measure immediately on boot
    let mut next_heartbeat = Instant::now(); // boot Info doubles as the first heartbeat
    // When the button went down (`None` = up): a paired node re-pairs once it is held for
    // `REPAIR_HOLD`. Taken when the main loop sees the Press, so a busy loop only ever makes the
    // gesture longer, never shorter; a Release queued meanwhile is seen before the timer (the
    // button arm is polled first).
    let mut pressed_at: Option<Instant> = None;
    // Consecutive undelivered uplinks: the periodic temperature uplink backs off on them
    // (`tower_radio::uplink_backoff_s`), so a node whose gateway is gone does not spend a
    // confirmed uplink with every retransmission on every period, forever.
    let mut misses: u8 = 0;
    // The accelerometer interrupt wait resumes at this instant (`None` = at once).
    let mut accel_quiet: Option<Instant> = None;
    let mut accel_backoff = ACCEL_BACKOFF_MIN;
    let mut accel_empty_highs: u8 = 0;
    // Motion uplink rate limit (`ACCEL_UPLINK_MIN`): no motion uplink before this instant, and the
    // face of the motion coalesced meanwhile, sent when it passes.
    let mut accel_hold: Option<Instant> = None;
    let mut accel_pending: Option<u8> = None;

    loop {
        let repair_due = pressed_at
            .filter(|_| provision.is_some())
            .map(|t| t + REPAIR_HOLD);
        let timer_due = next_temp
            .min(next_heartbeat)
            .min(repair_due.unwrap_or(Instant::MAX))
            .min(cue_until.unwrap_or(Instant::MAX))
            .min(match accel_pending {
                Some(_) => accel_hold.unwrap_or(Instant::MIN),
                None => Instant::MAX,
            });
        let accel_wait = async {
            if let Some(t) = accel_quiet {
                Timer::at(t).await;
            }
            accel_int.wait_for_high().await
        };
        match select4(
            btn.next_event(),
            Timer::at(timer_due),
            accel_wait,
            console::mgmt_next(),
        )
        .await
        {
            // --- button (real or simulated, same recognition path) ---
            Either4::First(ev) => {
                let kind = button_kind(ev);
                let idx = kind as usize;
                counts[idx] = counts[idx].wrapping_add(1);
                // The re-pair gesture's clock (every press, whatever the per-event enables).
                match ev {
                    button::Event::Press => pressed_at = Some(Instant::now()),
                    button::Event::Release => pressed_at = None,
                    _ => {}
                }

                // What the node reports (the per-event enables), it also shows on the console,
                // paired or not, so the bench sees every press.
                let enabled = event_enabled(kv, ev);
                if enabled {
                    show_button(kind, counts[idx], provision.map_or(Some(UNPAIRED_NOTE), |_| None)).await;
                }

                // Pairing is a fixed gesture, not a reportable event: while unpaired a
                // long hold always starts the OTA join, regardless of the per-event
                // enables (which govern normal reporting once paired).
                if provision.is_none() {
                    // Someone is at the node: show the unpaired cue again.
                    led.set_background(Some(UNPROVISIONED));
                    cue_until = Some(Instant::now() + UNPAIRED_CUE);
                    if matches!(ev, button::Event::Hold) {
                        pressed_at = None; // this press is spent on the join
                        let attempt = pin!(pair(
                            &mut net,
                            addr,
                            band,
                            channel,
                            kv,
                            &led,
                            PAIRING_WINDOW,
                            None
                        ));
                        if let PairOutcome::Paired { rec, .. } =
                            showing_events(attempt, &btn, &mut accel_int, &mut bus, &mut counts, kv).await
                        {
                            provision = Some(rec);
                            cue_until = None;
                            misses = 0;
                        } else {
                            cue_until = Some(Instant::now() + UNPAIRED_CUE);
                        }
                    }
                    continue; // unpaired: no gateway to report events to
                }
                // Someone is at the node: end any backoff, so the next periodic uplink tries
                // the gateway again on its normal period.
                if misses > 0 {
                    misses = 0;
                    let period = read_u32_setting(kv, SET_THERM_PERIOD, 60).max(5) as u64;
                    next_temp = next_temp.min(Instant::now() + Duration::from_secs(period));
                }

                // The per-event enable is the master switch: a disabled event is
                // ignored entirely: no LED, no radio.
                if !enabled {
                    continue;
                }
                led_feedback(&led, ev); // immediate local feedback (fixed shape)
                // Click/Hold are the semantic events, worth more repetitions than the
                // raw press/release edges around them.
                let reps = match kind {
                    ButtonKind::Click | ButtonKind::Hold => 5,
                    _ => 3,
                };
                let msg = NodeMsg::Button {
                    kind,
                    count: counts[idx],
                };
                note(&mut misses, uplink(&mut net, provision, &msg, reps).await);
            }

            // --- periodic: temperature and/or heartbeat due; the re-pair gesture; the end of the
            // unpaired cue ---
            Either4::Second(()) => {
                if repair_due.is_some() && !btn.is_pressed() {
                    // Released, but the Release was lost to a full button queue while the loop
                    // was busy: no gesture, so no re-pair the user never made.
                    pressed_at = None;
                } else if repair_due.is_some_and(|due| Instant::now() >= due) {
                    // Held REPAIR_HOLD while paired (no Release seen): pair again. The current
                    // pairing stays in force unless the new one commits.
                    pressed_at = None; // one gesture, one attempt
                    info!(target: "node", "re-pairing (button held {} s)", REPAIR_HOLD.as_secs());
                    let attempt = pin!(pair(
                        &mut net,
                        addr,
                        band,
                        channel,
                        kv,
                        &led,
                        PAIRING_WINDOW,
                        provision
                    ));
                    if let PairOutcome::Paired { rec, .. } =
                        showing_events(attempt, &btn, &mut accel_int, &mut bus, &mut counts, kv).await
                    {
                        provision = Some(rec);
                        misses = 0; // a fresh link: no backoff
                    }
                }
                let now = Instant::now();
                if cue_until.is_some_and(|t| now >= t) {
                    cue_until = None;
                    led.set_background(None);
                }
                if now >= next_temp {
                    let period = (read_u32_setting(kv, SET_THERM_PERIOD, 60) as u64).max(5);
                    let heartbeat = (read_u32_setting(kv, SET_HEARTBEAT, 900) as u64).max(60);
                    if let Some(millic) = measure_temp(&mut bus).await {
                        let delta_mc = read_u32_setting(kv, SET_THERM_DELTA, 50) as i32 * 10;
                        let changed = last_temp_sent.is_none_or(|last| (millic - last).abs() >= delta_mc);
                        let stale = last_temp_tx.elapsed() >= Duration::from_secs(heartbeat);
                        if provision.is_some() && (changed || stale) {
                            let msg = NodeMsg::Temperature { millic };
                            let delivered = uplink(&mut net, provision, &msg, 3).await;
                            if delivered {
                                last_temp_sent = Some(millic);
                                last_temp_tx = Instant::now();
                            }
                            note(&mut misses, delivered);
                        }
                    }
                    // The period, doubled per undelivered uplink in a row, capped at the heartbeat.
                    let wait = tower_radio::uplink_backoff_s(period, heartbeat, misses);
                    next_temp = Instant::now() + Duration::from_secs(wait);
                }
                if now >= next_heartbeat {
                    let heartbeat = read_u32_setting(kv, SET_HEARTBEAT, 900) as u64;
                    next_heartbeat = now + Duration::from_secs(heartbeat.max(60));
                    if provision.is_some() {
                        note(&mut misses, send_info(&mut net, provision).await);
                    }
                }
                if accel_hold.is_none_or(|t| Instant::now() >= t)
                    && let Some(face) = accel_pending.take()
                {
                    // The motion coalesced during the last interval: its latest face.
                    let msg = NodeMsg::Accel {
                        kind: AccelKind::Motion,
                        face,
                    };
                    note(&mut misses, uplink(&mut net, provision, &msg, 3).await);
                    accel_hold = Some(Instant::now() + ACCEL_UPLINK_MIN);
                }
            }

            // --- accelerometer tilt interrupt ---
            Either4::Third(()) => {
                accel_quiet = None;
                let tilt = handle_tilt(&mut bus).await;
                if let Tilt::Event(face) = tilt {
                    show_motion(face, provision.map_or(Some(UNPAIRED_NOTE), |_| None)).await;
                }
                if let Tilt::Event(face) = tilt
                    && provision.is_some()
                {
                    if accel_hold.is_some_and(|t| Instant::now() < t) {
                        accel_pending = Some(face); // sent when the interval ends
                    } else {
                        let msg = NodeMsg::Accel {
                            kind: AccelKind::Motion,
                            face,
                        };
                        note(&mut misses, uplink(&mut net, provision, &msg, 3).await);
                        accel_hold = Some(Instant::now() + ACCEL_UPLINK_MIN);
                    }
                }
                // `wait_for_high` returns at once on a high line, and looping on it without an
                // await starved the executor until the watchdog reset the node (a boot loop that
                // rewrote the TX watermark every boot). A failed read leaves the latch set: back
                // off, growing. After a good read a high line is new motion (the read cleared the
                // latch): take it after a short pause, no fault.
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

            // --- management over the cable (USB present by definition) ---
            Either4::Fourth(frame) => {
                let Ok((MsgType::MgmtRequest, _seq, payload)) = decode_frame(&frame) else {
                    continue;
                };
                let Ok(req) = postcard::from_bytes::<MgmtRequest>(payload) else {
                    continue;
                };
                handle_mgmt(req, &mut net, &mut provision, addr, band, channel, kv, &led).await;
                // Someone is at the cable, as at a press, and a `JoinOpen` may have paired anew:
                // either ends the backoff, so the next periodic uplink tries promptly.
                misses = 0;
                if provision.is_some() {
                    led.set_background(None);
                    cue_until = None;
                } else {
                    // Someone is on the cable (a failed JoinOpen too): show the cue again.
                    led.set_background(Some(UNPROVISIONED));
                    cue_until = Some(Instant::now() + UNPAIRED_CUE);
                }
            }
        }
    }
}

/// Count one uplink outcome into the consecutive-miss counter the periodic uplink backs off on.
fn note(misses: &mut u8, delivered: bool) {
    *misses = if delivered { 0 } else { misses.saturating_add(1) };
}

/// How an OTA pairing attempt ([`pair`]) ended.
enum PairOutcome {
    /// Committed: the gateway acknowledged an uplink under the new key. `persisted` is false
    /// when the EEPROM write failed (the pairing then holds until the next reboot).
    Paired {
        rec: Provisioned,
        sas: u32,
        persisted: bool,
    },
    /// No gateway answered within the window.
    NoWindow,
    /// A handshake completed but the gateway never acknowledged the new key: discarded.
    Unconfirmed,
    /// The handshake named a gateway the peer table refuses (a reserved address, or this
    /// node's own): discarded, nothing sent under the key.
    Refused,
}

/// OTA pairing (a button hold, the re-pair gesture or the cable's `JoinOpen`): run the handshake
/// for `window`, then the **commit round**, and persist the credentials only once committed.
///
/// The handshake ends with our JOIN_CONFIRM, which nothing acknowledges: if it is lost (or the
/// gateway's registry refuses the node) the gateway never installs the key. Persisting the key at
/// that point used to leave a node that believed it was paired while the gateway dropped every
/// uplink as an unregistered sender, and since a hold pairs only an unpaired node, nothing short
/// of USB could fix it. So the key stays in the RAM peer table, the `NodeInfo` goes up confirmed
/// under it (also what the host auto-names the node from), and only an ACK, which only a holder
/// of the key can seal, commits it (`tower_net::pairing::commit_step`: a short settle, then up to
/// six attempts 2 s apart, outlasting the gateway's worst post-confirm stall). The gateway closes
/// its window on the commit, so it serves our uplinks again from its next loop turn.
///
/// `current` is the pairing in force before this attempt; it stays in force (in RAM and in
/// EEPROM) unless the new one commits, so a failed re-pair changes nothing. One corner: a new
/// pairing with the *same* gateway address that does not commit restores the old key under that
/// address with a fresh replay lane (`Net::add_peer` resets the lane on every key change); a
/// gateway only answers an address it no longer has registered, so that old key was already
/// dead there.
#[allow(clippy::too_many_arguments)]
async fn pair(
    net: &mut Net,
    addr: u32,
    band: Band,
    channel: u8,
    kv: Nv,
    led: &led::Led,
    window: Duration,
    current: Option<Provisioned>,
) -> PairOutcome {
    info!(target: "node", "pairing: looking for a gateway window, {} s", window.as_secs());
    led.set_background(Some(JOINING));
    let _ = net.wake().await;
    let joined = net.join(addr, window).await;
    let _ = net.sleep().await;
    let idle_led = if current.is_some() {
        None
    } else {
        Some(UNPROVISIONED)
    };
    let Some(peer) = joined else {
        warn!(target: "node", "pairing failed: no gateway window in range");
        led.set_background(idle_led);
        return PairOutcome::NoWindow;
    };
    let sas = peer.sas;
    let rec = Provisioned {
        gw_addr: peer.addr,
        key: peer.key,
        band: if band == Band::Us915 {
            mgmt::BAND_US915
        } else {
            mgmt::BAND_EU868
        },
        channel,
    };

    // The commit round: provisional key in RAM, confirmed NodeInfo uplinks until one is ACKed.
    // The provisional peer is a registered one (the commit uplink is refused for anyone else,
    // registered peers only), and its lane is persisted on every accept like the paired one's.
    // A refusal (a reserved address, or our own) leaves the peer table as it was: never fall
    // back to another key for that address, and persist nothing.
    if !net.add_peer(rec.gw_addr, &rec.key) {
        warn!(target: "node", "pairing: gateway 0x{:08x} is not a valid peer, discarded", rec.gw_addr);
        led.set_background(idle_led);
        return PairOutcome::Refused;
    }
    Timer::after_millis(COMMIT_SETTLE_MS as u64).await;
    let mut attempt = 0u8;
    loop {
        attempt += 1;
        match commit_step(attempt, commit_uplink(net, rec.gw_addr).await) {
            CommitStep::Commit => break,
            CommitStep::Retry { after_ms } => Timer::after_millis(after_ms as u64).await,
            CommitStep::Discard => {
                // Back to the pairing in force before (or to none).
                match current {
                    Some(c) if c.gw_addr == rec.gw_addr => {
                        let _ = net.add_peer(c.gw_addr, &c.key);
                    }
                    _ => {
                        net.remove_peer(rec.gw_addr);
                    }
                }
                warn!(
                    target: "node",
                    "pairing: gateway 0x{:08x} did not confirm (no ACK), key discarded{}",
                    rec.gw_addr,
                    if current.is_some() { ", the previous pairing stays" } else { ", hold the button to retry" }
                );
                led.set_background(idle_led);
                return PairOutcome::Unconfirmed;
            }
        }
    }

    // Committed: the gateway holds the key. Retire a previous gateway (it keeps the node
    // registered until it is removed there), then persist.
    if let Some(c) = current
        && c.gw_addr != rec.gw_addr
    {
        net.remove_peer(c.gw_addr);
    }
    UNACKED.store(0, Ordering::Relaxed); // a fresh link: no unacknowledged streak

    let persisted = kv.scope(NS_APP).set(KEY_PROVISION, &rec).is_ok();
    if persisted {
        // The success line names this unit by its MCU factory ID, so a host log or the operator
        // can tie the pairing (and its SAS) to a device.
        info!(
            target: "node",
            "paired with gateway 0x{:08x}, SAS {:06} (uid {})",
            rec.gw_addr,
            sas,
            UniqueId::read()
        );
    } else {
        warn!(target: "node", "paired with gateway 0x{:08x} but not saved (lost at reboot)", rec.gw_addr);
    }
    led.set_background(None);
    PairOutcome::Paired { rec, sas, persisted }
}

/// One-shot temperature read: take the bus from the accelerometer, measure, hand it
/// back (chip register state, the tilt config, persists across the swap).
async fn measure_temp(bus: &mut AccelBus) -> Option<i32> {
    let mut t = Tmp112::new(bus.take()?.release(), tmp112::ADDR_VPLUS);
    let raw = t.oneshot().await;
    let millic = raw.ok().map(tmp112::raw_to_millicelsius);
    *bus = Some(Lis2dh12::new(t.release(), lis2dh12::ADDR_DEFAULT));
    millic
}

/// Tilt IRQ: clear/validate the latched interrupt, settle, and report the face up
/// (0 = unknown/moving).
/// The uplink payload's kind of a recognized button event.
fn button_kind(ev: button::Event) -> ButtonKind {
    match ev {
        button::Event::Press => ButtonKind::Press,
        button::Event::Release => ButtonKind::Release,
        button::Event::Click => ButtonKind::Click,
        button::Event::Hold => ButtonKind::Hold,
    }
}

/// The event names of [`ButtonKind`] as the console and the uplink payloads spell them.
fn button_name(kind: ButtonKind) -> &'static str {
    match kind {
        ButtonKind::Press => "press",
        ButtonKind::Release => "release",
        ButtonKind::Click => "click",
        ButtonKind::Hold => "hold",
    }
}

/// The note on an event the node shows but cannot send: unpaired, or a pairing attempt runs.
const UNPAIRED_NOTE: &str = "not paired";
const PAIRING_NOTE: &str = "pairing";

/// Show a button event on the console: a log line and a structured event (`tower events`).
/// `note` says why it goes nowhere, if it does. Dropped while USB is unplugged.
async fn show_button(kind: ButtonKind, count: u32, note: Option<&str>) {
    let name = button_name(kind);
    match note {
        None => info!(target: "button", "{} #{}", name, count),
        Some(note) => info!(target: "button", "{} #{} ({})", name, count, note),
    }
    use core::fmt::Write as _;
    let mut n: heapless::String<10> = heapless::String::new();
    let _ = write!(n, "{count}");
    console::event("button", &[("kind", name), ("count", n.as_str())]).await;
}

/// Show an accelerometer motion event on the console (see [`show_button`]).
async fn show_motion(face: u8, note: Option<&str>) {
    match note {
        None => info!(target: "accel", "motion, face {}", face),
        Some(note) => info!(target: "accel", "motion, face {} ({})", face, note),
    }
    use core::fmt::Write as _;
    let mut f: heapless::String<3> = heapless::String::new();
    let _ = write!(f, "{face}");
    console::event("accel", &[("kind", "motion"), ("face", f.as_str())]).await;
}

/// Run a pairing attempt (`attempt`, up to a minute) while still showing the button and the
/// accelerometer on the console: the main loop is parked in the attempt, and without this the
/// events of that minute would vanish. They are shown and counted, not sent (the node is
/// between gateways), and never interrupt the attempt. The attempt comes pinned by the caller:
/// taken by value, the future would be laid out twice in the caller's state (the argument and
/// the `select`), over a kilobyte of RAM this part does not have.
async fn showing_events<F: core::future::Future>(
    attempt: Pin<&mut F>,
    btn: &button::Button,
    accel_int: &mut ExtiInput<'static, Async>,
    bus: &mut AccelBus,
    counts: &mut [u32; 4],
    kv: Nv,
) -> F::Output {
    let watch = async {
        loop {
            match select(btn.next_event(), accel_int.wait_for_high()).await {
                Either::First(ev) => {
                    let kind = button_kind(ev);
                    let idx = kind as usize;
                    counts[idx] = counts[idx].wrapping_add(1);
                    if event_enabled(kv, ev) {
                        show_button(kind, counts[idx], Some(PAIRING_NOTE)).await;
                    }
                }
                Either::Second(()) => {
                    // The same pacing as the main loop (see its accelerometer branch).
                    match handle_tilt(bus).await {
                        Tilt::Event(face) => show_motion(face, Some(PAIRING_NOTE)).await,
                        Tilt::ReadFailed => Timer::after(ACCEL_BACKOFF_MIN).await,
                        Tilt::Nothing => {}
                    }
                    if accel_int.is_high() {
                        Timer::after(ACCEL_REARM).await;
                    }
                }
            }
        }
    };
    match select(attempt, watch).await {
        Either::First(outcome) => outcome,
        Either::Second(never) => never,
    }
}

/// What one accelerometer interrupt turned out to be ([`handle_tilt`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tilt {
    /// Motion to report; the face up after it settled (0 = unknown).
    Event(u8),
    /// Nothing to report: a spurious edge, or rate-limited by the driver's `min_interval`.
    Nothing,
    /// The interrupt source could not be read (I²C): the latch is still set.
    ReadFailed,
}

async fn handle_tilt(bus: &mut AccelBus) -> Tilt {
    let Some(accel) = bus else {
        return Tilt::Nothing; // bus mid-swap (can't happen: swaps are scoped to measure_temp)
    };
    match accel.tilt_triggered() {
        Ok(true) => {}
        Ok(false) => return Tilt::Nothing,
        Err(_) => return Tilt::ReadFailed,
    }
    // Let the unit settle, then read which face is up for the event's context.
    Timer::after_millis(300).await;
    let face = accel.read().ok().and_then(|s| s.dice()).unwrap_or(0);
    Tilt::Event(face)
}

/// Encode this node's identity/heartbeat `Info` into `buf`; its length.
fn encode_info(buf: &mut [u8; MAX_RADIO_PAYLOAD]) -> Option<usize> {
    let name = console::firmware_name();
    let msg = NodeMsg::Info(NodeInfo {
        firmware_name: name.as_str(),
        firmware_version: console::firmware_version(),
        session_id: console::session_id(),
        sleeping: true,
    });
    encode_node_msg(&msg, buf).ok()
}

/// Send the identity/heartbeat `Info` (also the delivery opportunity for queued
/// downlinks on an otherwise idle node). Returns whether it was delivered.
async fn send_info(net: &mut Net, provision: Option<Provisioned>) -> bool {
    let name = console::firmware_name();
    let msg = NodeMsg::Info(NodeInfo {
        firmware_name: name.as_str(),
        firmware_version: console::firmware_version(),
        session_id: console::session_id(),
        sleeping: true,
    });
    uplink(net, provision, &msg, 3).await
}

/// One commit attempt of [`pair`]: the `Info` (the host auto-names the node from it), confirmed,
/// to the gateway the handshake named; whether it was acknowledged. A plain confirmed send, no
/// downlink window: a node this fresh has nothing queued, and a pending flag on its ACK is
/// served by the next regular uplink. (Kept apart from [`uplink`] also for RAM: the downlink and
/// remote-shell machinery inside it would otherwise be carried by `pair`'s future as well, and
/// `handle_mgmt`'s with it.)
async fn commit_uplink(net: &mut Net, gw_addr: u32) -> bool {
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    let Some(n) = encode_info(&mut buf) else {
        return false;
    };
    let _ = net.wake().await;
    let result = net.send(gw_addr, &buf[..n], true, 3).await;
    let _ = net.sleep().await;
    if !matches!(result, SendResult::Delivered) {
        warn!(target: "node", "pairing: commit uplink {result}");
        return false;
    }
    true
}

/// One uplink cycle: wake the radio, send confirmed, honour the ACK's pending flag
/// (downlink window + remote shell), then sleep the radio again.
async fn uplink(net: &mut Net, provision: Option<Provisioned>, msg: &NodeMsg<'_>, reps: u8) -> bool {
    let Some(p) = provision else {
        return false;
    };
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    let Ok(n) = encode_node_msg(msg, &mut buf) else {
        warn!(target: "node", "uplink not encoded");
        return false;
    };
    let _ = net.wake().await;
    let result = net.send(p.gw_addr, &buf[..n], true, reps).await;
    let delivered = matches!(result, SendResult::Delivered);
    if delivered {
        UNACKED.store(0, Ordering::Relaxed);
    } else {
        warn!(target: "node", "uplink {result}");
        // Refused by the duty budget: nothing went out, so it says nothing about the gateway.
        let streak = match result {
            SendResult::DutyLimited => UNACKED.load(Ordering::Relaxed),
            _ => UNACKED.load(Ordering::Relaxed).saturating_add(1),
        };
        UNACKED.store(streak, Ordering::Relaxed);
        if streak == UNACKED_HINT && !matches!(result, SendResult::DutyLimited) {
            // Most likely out of range or switched off; but a gateway that no longer knows this
            // node drops everything it sends, and only a re-pair gets it back.
            warn!(
                target: "node",
                "gateway 0x{:08x}: {} uplinks unacknowledged; if it forgot this node, re-pair (hold {} s)",
                p.gw_addr,
                streak,
                REPAIR_HOLD.as_secs()
            );
        }
    }
    while net.last_ack().is_some_and(|m| m.pending) {
        if !downlink_window(net, p.gw_addr).await {
            break;
        }
        // The last response chunk's ACK re-advertises pending, chaining queued
        // commands one per window without any polling.
    }
    let _ = net.sleep().await;
    delivered
}

/// Last remote-shell command executed, for at-least-once dedup: bit 24 = valid,
/// bits 23..16 = result, bits 15..0 = cmd_id. Written only by the main task (M0+
/// load/store is enough). RAM-only: survives the delivery retries that matter
/// (same boot); a reboot-command redelivery after the reboot re-executes, bounded
/// by the item's TTL and ended by the first delivery ACK that gets through.
static LAST_REMOTE_CMD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// The host-process `epoch` of the last remote command (paired with `LAST_REMOTE_CMD`).
/// The dedup keys on `(epoch, cmd_id)`, so a fresh host that reuses a low `cmd_id`
/// can't be mistaken for a re-delivery this node already ran. Two u32s, not one u64:
/// thumbv6m has no 64-bit atomic, and single-task access needs no cross-word atomicity.
static LAST_REMOTE_EPOCH: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Hold RX open for one queued downlink; execute it and stream the response. Returns
/// whether a downlink actually arrived (false = window expired, stop chaining).
async fn downlink_window(net: &mut Net, gw_addr: u32) -> bool {
    let deadline = Instant::now() + DOWNLINK_WINDOW;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.as_ticks() == 0 {
            return false;
        }
        // `None` is a timeout, but also a frame for another node, a non-DATA frame, a CRC error
        // or a replay: none of them ends the window early (a busy star would otherwise push our
        // queued command back to the next uplink, up to a heartbeat away). The check above does.
        let Some(rx) = net.recv(remaining).await else {
            continue;
        };
        if rx.src != gw_addr {
            continue; // another node's traffic, not our downlink
        }
        match decode_node_cmd(rx.data()) {
            Ok(NodeCmd::Shell { epoch, cmd_id, line }) => {
                let mut sink = RadioSink::new(net, gw_addr, cmd_id);
                // The gateway's queue is at-least-once: if our delivery ACK is lost it
                // re-sends the same command under a fresh counter (the replay lane
                // can't help). Executing twice is wrong for anything non-idempotent
                // (`system reboot`, a toggling set), so dedup on `(epoch, cmd_id)`: on a
                // repeat, skip execution and just re-complete the protocol with the cached
                // result (the full text already went up with the first execution). The
                // host stamps a random `epoch` per process, so a fresh host reusing a low
                // `cmd_id` is never mistaken for a re-delivery this node already ran.
                use core::sync::atomic::Ordering::Relaxed;
                let last = LAST_REMOTE_CMD.load(Relaxed);
                if last & (1 << 24) != 0
                    && (last & 0xFFFF) as u16 == cmd_id
                    && LAST_REMOTE_EPOCH.load(Relaxed) == epoch
                {
                    let result = ((last >> 16) & 0xFF) as u8;
                    info!(target: "node", "remote shell: command {} repeated, not run again", cmd_id);
                    sink.done(result).await; // empty reply, cached result
                    return true;
                }
                info!(target: "node", "remote shell: {}", line);
                // Stream the response chunk-by-chunk: no fixed response buffer, so a full
                // `export` or `settings print` is never truncated (the dispatcher owns the
                // unbounded producers; `RadioSink` owns the radio framing).
                let (result, reboot) = shell::stream_line(line, &mut sink).await;
                // Epoch first, then the valid-flagged CMD word: a reader that sees the
                // valid bit has already seen this epoch (single task, but keep the order).
                LAST_REMOTE_EPOCH.store(epoch, Relaxed);
                LAST_REMOTE_CMD.store((1 << 24) | ((result as u32) << 16) | cmd_id as u32, Relaxed);
                if reboot {
                    // The response is on the air; the reboot request came over radio, so
                    // there is no console to flush; reset now.
                    cortex_m::peripheral::SCB::sys_reset();
                }
                return true;
            }
            Err(tower_protocol::Error::SchemaMismatch { got }) => {
                warn!(
                    target: "node",
                    "downlink schema {:#06x}, expected {:#06x}: build host and node from one checkout",
                    got,
                    RADIO_TAG
                );
                // Nothing exchanged that would clear the pending flag; stop chaining
                // rather than burn another full RX window on the same bad item.
                return false;
            }
            Err(_) => {
                warn!(target: "node", "downlink malformed, dropped");
                return false;
            }
        }
    }
}

/// Streams a remote-shell response back as `NodeShellChunk` uplinks (≤ RADIO_SHELL_CHUNK
/// text bytes each, confirmed). Owns chunk numbering + the `last` flag, so the shell
/// dispatcher ([`shell::stream_line`]) can emit an unbounded response through a single
/// [`RADIO_SHELL_CHUNK`]-sized buffer: no `RESP_CAP` ceiling on `export` & friends.
struct RadioSink<'a> {
    net: &'a mut Net,
    gw_addr: u32,
    cmd_id: u16,
    chunk: u16,
    buf: heapless::String<RADIO_SHELL_CHUNK>,
    ok: bool,
}

impl<'a> RadioSink<'a> {
    fn new(net: &'a mut Net, gw_addr: u32, cmd_id: u16) -> Self {
        Self {
            net,
            gw_addr,
            cmd_id,
            chunk: 0,
            buf: heapless::String::new(),
            ok: true,
        }
    }

    /// Transmit the buffered text as one chunk (confirmed). `last` marks the final frame;
    /// intermediate chunks carry `result = 0` (the host reads `result` only on `last`).
    async fn flush(&mut self, last: bool, result: u8) -> bool {
        let msg = NodeMsg::Shell(NodeShellChunk {
            cmd_id: self.cmd_id,
            result,
            chunk: self.chunk,
            last,
            text: self.buf.as_str(),
        });
        let mut wire = [0u8; MAX_RADIO_PAYLOAD];
        let Ok(n) = encode_node_msg(&msg, &mut wire) else {
            self.ok = false;
            return false;
        };
        let delivered = matches!(
            self.net.send(self.gw_addr, &wire[..n], true, CHUNK_REPS).await,
            SendResult::Delivered
        );
        self.buf.clear();
        self.chunk = self.chunk.wrapping_add(1);
        if !delivered {
            // A lost chunk shows up host-side as a chunk gap (incomplete response);
            // aborting beats streaming the tail of a response the host can't anchor.
            warn!(target: "node", "remote shell: response chunk {} not delivered, rest dropped", self.chunk - 1);
            self.ok = false;
        }
        delivered
    }
}

impl shell::ShellSink for RadioSink<'_> {
    async fn push(&mut self, text: &str) -> bool {
        if !self.ok {
            return false;
        }
        let mut rest = text;
        while !rest.is_empty() {
            let space = RADIO_SHELL_CHUNK - self.buf.len();
            let mut take = rest.len().min(space);
            while take > 0 && !rest.is_char_boundary(take) {
                take -= 1;
            }
            if take == 0 {
                // A multi-byte char won't fit the remaining space; flush and retry.
                if !self.flush(false, 0).await {
                    return false;
                }
                continue;
            }
            let _ = self.buf.push_str(&rest[..take]);
            rest = &rest[take..];
            if self.buf.len() == RADIO_SHELL_CHUNK && !self.flush(false, 0).await {
                return false;
            }
        }
        true
    }

    async fn done(&mut self, result: u8) -> bool {
        // Always send a final `last` frame (empty if nothing buffered) so the host
        // completes the command.
        if !self.ok {
            return false;
        }
        self.flush(true, result).await
    }
}

/// Cable-side management: `Describe` / `Provision` / `JoinOpen` (USB present, since
/// the console is USB-gated: exactly the cable-pairing situation).
#[allow(clippy::too_many_arguments)]
async fn handle_mgmt(
    req: MgmtRequest<'_>,
    net: &mut Net,
    provision: &mut Option<Provisioned>,
    addr: u32,
    band: Band,
    channel: u8,
    kv: Nv,
    led: &led::Led,
) {
    let req_id = req.req_id;
    match req.op {
        MgmtOp::Describe => {
            let name = console::firmware_name();
            respond_record(
                req_id,
                &DeviceInfo {
                    role: DeviceRole::Node,
                    addr,
                    band: match band {
                        Band::Eu868 => mgmt::BAND_EU868,
                        Band::Us915 => mgmt::BAND_US915,
                    },
                    channel,
                    node_capacity: 0,
                    node_count: 0,
                    provisioned: provision.is_some(),
                    gw_addr: provision.map(|p| p.gw_addr).unwrap_or(0),
                    firmware_name: name.as_str(),
                    // The same version the boot `Hello` carries, so a host that attached after
                    // boot (missed the Hello) still shows it.
                    firmware_version: console::firmware_version(),
                },
            )
            .await;
        }
        MgmtOp::Provision(p) => {
            // The node's address after this provision: an explicit override (0 = keep), else ours.
            let override_addr = p.addr.filter(|&a| a != 0);
            let effective_addr = override_addr.unwrap_or(addr);
            // Same address rules as pairing / `Net::add_peer`: neither end reserved (0 /
            // broadcast), and the gateway never at our own address (shared `src` ⇒ colliding
            // CCM nonces under the link key; `add_peer` would refuse it and the node go mute).
            if p.band > mgmt::BAND_US915
                || is_reserved_addr(effective_addr)
                || !peer_addr_valid(p.gw_addr, effective_addr)
            {
                respond_empty(req_id, mgmt::MGMT_BAD_ARG).await;
                return;
            }
            let app_kv = kv.scope(NS_APP);
            // The key must be minted for this provision (the host's cable pairing mints one each
            // time): it rides this board's TX counter from wherever it stands now, and an old
            // key on a counter that restarted (a replacement board pinned to the old node's
            // address, given the key the gateway revealed) would repeat that key's nonces. Only
            // the node that used a key knows its counter history, and keys never leave a node,
            // so nothing could carry a floor for it here (docs/security-model.md, 4.1).
            let rec = Provisioned {
                gw_addr: p.gw_addr,
                key: p.key,
                band: p.band,
                channel: p.channel,
            };
            // An optional address override pins the `addr` base setting (the same
            // one `system settings set addr` edits), so the node comes up under it after
            // reboot. Written FIRST: the provision record below is the single commit point,
            // so a storage failure can't leave the node durably provisioned while the
            // host was told MGMT_STORAGE (a dangling address pin, by contrast, has no
            // radio effect until a provision succeeds).
            if let Some(override_addr) = override_addr
                && kv
                    .scope(NS_SHELL)
                    .set_bytes(shell::ADDR_KEY, &override_addr.to_le_bytes())
                    .is_err()
            {
                respond_empty(req_id, mgmt::MGMT_STORAGE).await;
                return;
            }
            if app_kv.set(KEY_PROVISION, &rec).is_err() {
                respond_empty(req_id, mgmt::MGMT_STORAGE).await;
                return;
            }
            respond_record(req_id, &ProvisionAck { addr: effective_addr }).await;
            info!(target: "node", "provisioned for gateway 0x{:08x}, rebooting", p.gw_addr);
            // Band/channel/id take effect at Net::new; reboot into the new identity
            // (also bumps session_id, so the host sees the reprovision).
            console::flush().await;
            cortex_m::peripheral::SCB::sys_reset();
        }
        MgmtOp::JoinOpen { window } => {
            if window == 0 {
                respond_empty(req_id, mgmt::MGMT_BAD_ARG).await;
                return;
            }
            // The same pairing as the button's, commit round included: `Joined` means the gateway
            // acknowledged the new key, not merely that a JOIN_CONFIRM went out.
            let window = Duration::from_secs(window as u64);
            match pair(net, addr, band, channel, kv, led, window, *provision).await {
                PairOutcome::Paired { rec, sas, persisted } => {
                    *provision = Some(rec);
                    if persisted {
                        respond_record(
                            req_id,
                            &Joined {
                                gw_addr: rec.gw_addr,
                                sas,
                            },
                        )
                        .await;
                    } else {
                        // Paired for this boot only (the gateway has it; the EEPROM refused it).
                        respond_empty(req_id, mgmt::MGMT_STORAGE).await;
                    }
                }
                PairOutcome::NoWindow | PairOutcome::Unconfirmed => {
                    respond_empty(req_id, mgmt::MGMT_TIMEOUT).await;
                }
                PairOutcome::Refused => respond_empty(req_id, mgmt::MGMT_BAD_ARG).await,
            }
        }
        // Gateway-side ops: this device is a node.
        _ => respond_empty(req_id, mgmt::MGMT_UNSUPPORTED).await,
    }
}

async fn respond_empty(req_id: u16, result: u8) {
    console::mgmt_chunk(req_id, result, 0, true, &[]).await;
}

async fn respond_record<T: serde::Serialize>(req_id: u16, record: &T) {
    let mut buf = [0u8; 192];
    match postcard::to_slice(record, &mut buf) {
        Ok(bytes) => {
            let n = bytes.len();
            console::mgmt_chunk(req_id, mgmt::MGMT_OK, 0, true, &buf[..n]).await;
        }
        Err(_) => respond_empty(req_id, mgmt::MGMT_STORAGE).await,
    }
}

app!(run, commands: BTN_COMMANDS, settings: BTN_SETTINGS);
