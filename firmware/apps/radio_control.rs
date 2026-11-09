//! radio_control (TOWER IoT Kit product firmware): the Power Module's relay and LED strip on the
//! radio, a **listening** taf node (`docs/firmware/taf.md`): it keeps its receiver on, so a command from
//! MQTT takes effect within about one radio round trip.
//!
//! Properties (`nodes/{addr}/state/...`, set with `nodes/{addr}/set/...`): `relay` (on/off),
//! `strip` (on/off), `effect` (one of the [`strip_player`] effects), `brightness` (%), `color`
//! (`0xRRGGBB` as a number), `speed` (%); the setting `relay-boot` says what the relay does after a
//! power-up (off, on, or its last state). The `strip ...` shell commands work as in `led_strip`,
//! locally and over the radio's remote shell, and what they change is reported too. Changes are
//! kept in EEPROM 10 s after the last one, so a power cut restores the strip (and the relay, with
//! `relay-boot last`). The button: a click switches the relay, a hold the strip. The strip's clock
//! effect shows the wall-clock time the gateway host syncs. The temperature and the
//! accelerometer's `tilt` events are the taf runtime's, as in every taf app.
//!
//! Hardware: Core Module + Power Module (relay on P0) + the 144-pixel RGBW strip on PA1. The
//! Power Module is mains powered, which is what a listening node needs.
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
    use tower::power_module::Relay;
    use tower::storage::{NS_APP, Nv};
    use tower::strip::{LedKind, Strip};
    use tower::{board::Board, button, shell, strip_player, taf, ws2812};
    use tower_protocol::props::{self, control};

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

    /// The relay's state after a power-up, from `relay-boot`.
    fn relay_at_boot(kv: Nv) -> bool {
        match taf::param(control::RELAY_BOOT) {
            1 => true,
            2 => kv
                .scope(NS_APP)
                .get::<bool>(KEY_RELAY)
                .ok()
                .flatten()
                .unwrap_or(false),
            _ => false,
        }
    }

    /// The values reported, in property order.
    fn snapshot(relay: &Relay) -> [(u8, i32); 6] {
        [
            (control::RELAY, i32::from(relay.is_on())),
            (control::STRIP, i32::from(strip_player::is_on())),
            (control::EFFECT, strip_player::effect() as i32),
            (control::BRIGHTNESS, i32::from(strip_player::brightness())),
            (control::COLOR, strip_player::color() as i32),
            (control::SPEED, i32::from(strip_player::speed())),
        ]
    }

    #[embassy_executor::task]
    async fn control_task(mut relay: Relay, kv: Nv) {
        // Settings read from here on: the task runs once the runtime has started.
        relay.set(relay_at_boot(kv));
        let mut last = snapshot(&relay);
        for (id, value) in last {
            taf::report(id, value);
        }
        let mut save_at: Option<Instant> = None;
        loop {
            match select3(taf::next_set(), taf::next_button(), Timer::after(POLL)).await {
                Either3::First((id, value)) => match id {
                    control::RELAY => relay.set(value != 0),
                    control::STRIP => strip_player::set_on(value != 0),
                    control::EFFECT => strip_player::set_effect(value.max(0) as usize),
                    control::BRIGHTNESS => strip_player::set_brightness(value.clamp(0, 100) as u8),
                    control::COLOR => strip_player::set_color(value as u32),
                    control::SPEED => strip_player::set_speed(value.clamp(10, 1000) as u16),
                    _ => {} // a setting (relay-boot): read when used
                },
                Either3::Second(button::Event::Click) => relay.set(!relay.is_on()),
                Either3::Second(button::Event::Hold) => strip_player::set_on(!strip_player::is_on()),
                Either3::Second(_) | Either3::Third(()) => {}
            }
            let now = snapshot(&relay);
            if now != last {
                if now[0] != last[0] {
                    info!(target: "control", "relay {}", if relay.is_on() { "on" } else { "off" });
                }
                for (id, value) in now {
                    taf::report(id, value);
                }
                last = now;
                save_at = Some(Instant::now() + SAVE_AFTER);
            }
            if save_at.is_some_and(|t| Instant::now() >= t) {
                save_at = None;
                let relay_ok = kv.scope(NS_APP).set(KEY_RELAY, &relay.is_on()).is_ok();
                if strip_player::save(kv).is_err() || !relay_ok {
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
        b.spawner.spawn(control_task(relay, b.kv).unwrap());
        taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
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
