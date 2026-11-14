//! radio_control (TOWER IoT Kit product firmware): the Power Module's relay and LED strip and the
//! Relay Module's relay on the radio, a **listening** taf node (`docs/firmware/taf.md`): it keeps
//! its receiver on, so a command from MQTT takes effect within about one radio round trip.
//!
//! Properties (`nodes/{addr}/state/...`, set with `nodes/{addr}/set/...`): `power-relay` (on/off,
//! the Power Module's), `signal-relay` (on/off, the Relay Module's latching relay), `strip` (on/off),
//! `effect` (one of the [`strip_player`] effects), `brightness` (%), `color` (`0xRRGGBB` as a
//! number), `speed` (%); the setting `relay-boot` says what both relays do after a power-up (off,
//! on, or their last state). The `strip ...` shell commands work as in `led_strip`, locally and
//! over the radio's remote shell, and what they change is reported too. Changes are kept in
//! EEPROM 10 s after the last one, so a power cut restores the strip (and the relays, with
//! `relay-boot last`). The button: a click switches the relays (the Relay Module's follows the
//! Power Module's), a hold the strip (on top of the `button` events every taf app reports). The
//! strip's clock effect shows the wall-clock time the gateway host syncs. The temperature and
//! the accelerometer's `tilt` events are the taf runtime's, as in every taf app.
//!
//! **The Relay Module does not have to be fitted.** The app looks for it at start and every 10 s
//! while it is missing; `signal-relay` is reported only while the module answers. Its relay is
//! latching: it is pulsed to its `relay-boot` state when the module is found (a latching relay
//! cannot be read back, and it keeps whatever position it had through a power cut), and its LED
//! shows the position while the `led` setting is on.
//!
//! Hardware: Core Module + Power Module (relay on P0) + the 144-pixel RGBW strip on PA1, and/or
//! the Relay Module (I2C). The Power Module is mains powered, which is what a listening node
//! needs.
//!
//!   just run app radio_control

#![no_std]
#![no_main]

// The Core Module r1 build (its USB console's stack) leaves this app 6.8 KB short of flash: it is
// built for the r2 board, which the Power Module setup uses. The r1 build is a stub that says so.
#[cfg(not(feature = "core-module-r1"))]
mod control {

    use embassy_futures::select::{Either3, select3};
    use embassy_stm32::peripherals::{DMA1_CH3, PA1, TIM2};
    use embassy_stm32::{Peri, bind_interrupts};
    use embassy_time::{Duration, Instant, Timer};
    use log::{info, warn};
    use tower::i2c::I2cBus;
    use tower::power_module::Relay;
    use tower::relay_module::{self, RelayModule};
    use tower::storage::{NS_APP, Nv};
    use tower::strip::{LedKind, Strip};
    use tower::{board::Board, button, shell, strip_player, taf, ws2812};
    use tower_protocol::props::{self, common, control};

    // The strip driver's refill interrupt (see `tower::ws2812::Refill`).
    bind_interrupts!(struct Irqs {
        DMA1_CHANNEL2_3 => ws2812::Refill;
    });

    const KIND: props::Kind = control::KIND;
    pub(super) static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
    pub(super) static COMMANDS: &[shell::Entry] =
        &[taf::NODE_COMMANDS, taf::BUTTON_COMMANDS, strip_player::MENU];

    /// Pixels on the strip.
    const NUM: usize = 144;
    /// Wire format: SK6812 RGBW (`LedKind::Rgb` for a WS2812B).
    const STRIP_KIND: LedKind = LedKind::Rgbw;
    type TowerStrip = Strip<'static, NUM, { ws2812::rgbw_buf_len(NUM) }>;

    /// `NS_APP` local of the relay's last state (for `relay-boot last`), next to the strip record
    /// (0x40) and clear of the taf runtime's (0x00).
    const KEY_RELAY: u8 = 0x41;
    /// `NS_APP` local of the Relay Module's last state, likewise.
    const KEY_RELAY_MODULE: u8 = 0x42;
    /// A missing Relay Module is looked for every this many turns of the loop (10 s of `POLL`):
    /// four addresses that do not answer.
    const PROBE_POLLS: u8 = 40;
    /// Changes are stored this long after the last one (EEPROM wear: a dimmer dragged over MQTT is one
    /// write, not fifty).
    const SAVE_AFTER: Duration = Duration::from_secs(10);
    /// How often the reporter looks for changes (the shell's `strip` commands change the player behind
    /// the runtime's back).
    const POLL: Duration = Duration::from_millis(250);

    /// Built here, not passed in: a task's argument is stored twice in its future (the argument, and
    /// the local it moves into), 1 KB more for a strip.
    #[embassy_executor::task]
    async fn strip_task(tim: Peri<'static, TIM2>, data: Peri<'static, PA1>, dma: Peri<'static, DMA1_CH3>) {
        let mut strip: TowerStrip = Strip::new(tim, data, dma, Irqs, STRIP_KIND, strip_player::brightness());
        strip_player::play(&mut strip).await
    }

    /// A relay's state after a power-up, from `relay-boot`; `key` is where its last state is kept.
    fn relay_at_boot(kv: Nv, key: u8) -> bool {
        match taf::param(control::RELAY_BOOT) {
            1 => true,
            2 => kv.scope(NS_APP).get::<bool>(key).ok().flatten().unwrap_or(false),
            _ => false,
        }
    }

    fn on_off(on: bool) -> &'static str {
        if on { "on" } else { "off" }
    }

    /// What the loop does with the Relay Module this turn.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Module {
        Nothing,
        /// Pulse the relay to its wanted position.
        Switch,
        /// Only the LEDs (the `led` setting changed).
        Show,
    }

    /// The values reported, in property order.
    fn snapshot(relay: &Relay) -> [(u8, i32); 6] {
        [
            (control::POWER_RELAY, i32::from(relay.is_on())),
            (control::STRIP, i32::from(strip_player::is_on())),
            (control::EFFECT, strip_player::effect() as i32),
            (control::BRIGHTNESS, i32::from(strip_player::brightness())),
            (control::COLOR, strip_player::color() as i32),
            (control::SPEED, i32::from(strip_player::speed())),
        ]
    }

    #[embassy_executor::task]
    async fn control_task(mut relay: Relay, kv: Nv, bus: I2cBus) {
        // Settings read from here on: the task runs once the runtime has started.
        relay.set(relay_at_boot(kv, KEY_RELAY));
        let mut last = snapshot(&relay);
        for (id, value) in last {
            taf::report(id, value);
        }
        // The Relay Module, once found, and the position its relay should have. A missing module
        // is looked for every `PROBE_POLLS` turns of the loop.
        let mut module: Option<RelayModule> = None;
        let mut module_on = relay_at_boot(kv, KEY_RELAY_MODULE);
        let mut probe_in: u8 = 0;
        let mut save_at: Option<Instant> = None;
        loop {
            let was_on = module_on;
            let mut act = Module::Nothing;
            match select3(taf::next_set(), taf::next_button(), Timer::after(POLL)).await {
                Either3::First((id, value)) => match id {
                    control::POWER_RELAY => relay.set(value != 0),
                    control::SIGNAL_RELAY => {
                        module_on = value != 0;
                        act = Module::Switch;
                        if module.is_none() {
                            warn!(target: "control", "signal-relay set, but no Relay Module");
                        }
                    }
                    control::STRIP => strip_player::set_on(value != 0),
                    control::EFFECT => strip_player::set_effect(value.max(0) as usize),
                    control::BRIGHTNESS => strip_player::set_brightness(value.clamp(0, 100) as u8),
                    control::COLOR => strip_player::set_color(value as u32),
                    control::SPEED => strip_player::set_speed(value.clamp(10, 1000) as u16),
                    common::LED => act = Module::Show,
                    _ => {} // another setting (relay-boot): read when used
                },
                Either3::Second(button::Event::Click) => {
                    relay.set(!relay.is_on());
                    module_on = relay.is_on();
                    act = Module::Switch;
                }
                Either3::Second(button::Event::Hold) => strip_player::set_on(!strip_player::is_on()),
                Either3::Second(_) | Either3::Third(()) => {}
            }
            if module.is_none() {
                if probe_in == 0 {
                    probe_in = PROBE_POLLS;
                    module = RelayModule::probe(bus).await;
                    if module.is_some() {
                        info!(target: "control", "Relay Module found");
                        // Its relay's position is not known until it is pulsed.
                        act = Module::Switch;
                    }
                } else {
                    probe_in -= 1;
                }
            }
            // The one place the module is driven from (each await site is code and RAM in this
            // task's future). A module that does not answer is dropped, and looked for again.
            if act != Module::Nothing
                && let Some(m) = &mut module
            {
                let led = if taf::led_on() {
                    relay_module::Led::State
                } else {
                    relay_module::Led::Off
                };
                match m.apply(module_on, led, act == Module::Switch).await {
                    Ok(()) if act == Module::Switch => {
                        info!(target: "control", "signal-relay {}", on_off(module_on));
                        taf::report(control::SIGNAL_RELAY, i32::from(module_on));
                    }
                    Ok(()) => {}
                    Err(_) => {
                        warn!(target: "control", "Relay Module gone");
                        module = None;
                        taf::forget(control::SIGNAL_RELAY);
                    }
                }
            }
            let now = snapshot(&relay);
            if now != last {
                if now[0] != last[0] {
                    info!(target: "control", "power-relay {}", on_off(relay.is_on()));
                }
                for (id, value) in now {
                    taf::report(id, value);
                }
                last = now;
                save_at = Some(Instant::now() + SAVE_AFTER);
            }
            if module_on != was_on {
                save_at = Some(Instant::now() + SAVE_AFTER);
            }
            if save_at.is_some_and(|t| Instant::now() >= t) {
                save_at = None;
                let app = kv.scope(NS_APP);
                let relays_ok = app.set(KEY_RELAY, &relay.is_on()).is_ok()
                    && app.set(KEY_RELAY_MODULE, &module_on).is_ok();
                if strip_player::save(kv).is_err() || !relays_ok {
                    warn!(target: "control", "state not saved");
                }
            }
        }
    }

    pub(super) async fn run(b: Board) {
        strip_player::load(b.kv);
        b.spawner
            .spawn(strip_task(b.strip_tim, b.strip_data, b.strip_dma).unwrap());
        // Off until the task applies `relay-boot` (settings read 0 before the runtime starts).
        let relay = Relay::new(b.header_p0, false);
        let base = tower::taf_base!(b);
        base.spawner
            .spawn(control_task(relay, base.kv, base.i2c).unwrap());
        taf::run(base, &KIND, &SETTINGS).await
    }
}

#[cfg(not(feature = "core-module-r1"))]
use control::{COMMANDS, SETTINGS, run};
#[cfg(not(feature = "core-module-r1"))]
use tower::app;
#[cfg(not(feature = "core-module-r1"))]
app!(run, commands: COMMANDS, settings: &SETTINGS);

#[cfg(feature = "core-module-r1")]
async fn run(_b: tower::board::Board) {
    log::error!(target: "control", "radio_control does not fit a Core Module r1 (flash); use an r2");
}
#[cfg(feature = "core-module-r1")]
tower::app!(run);
