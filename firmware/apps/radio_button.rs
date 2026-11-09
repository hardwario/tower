//! radio_button (TOWER IoT Kit product firmware): a wireless push button, a sleeping taf node
//! (`docs/firmware/taf.md`).
//!
//! Every button press, release, click and hold is a `button` event (`nodes/{addr}/event/button`,
//! the value `press`/`release`/`click`/`hold` with its running count), each switchable by its
//! setting (`press`, `release` off and `click`, `hold` on by default: the gesture scheme). The
//! LED answers each reported event with a fixed shape (press, release and hold a single pulse, a
//! click a double blink), when the common `led` setting is on.
//!
//! The pairing gestures (hold 1 s while unpaired, 12 s to re-pair), the cable, the remote shell,
//! settings over the radio, the heartbeat, the temperature and the accelerometer's `tilt` events
//! are the taf runtime's, as in every taf app. `button simulate <ms>` (console
//! only) presses the button through the real recognition, for tests and the HIL bench.
//!
//!   just run app radio_button

#![no_std]
#![no_main]

use log::info;
use tower::{app, board::Board, button, console, shell, taf};
use tower_protocol::props::{self, button as props_button};

const KIND: props::Kind = props_button::KIND;
static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
static COMMANDS: &[shell::Entry] = &[taf::NODE_COMMANDS, taf::BUTTON_COMMANDS];

/// LED feedback (fixed shapes: the LED is an indicator, not a setting). A click is a double blink
/// so it reads apart from the press before it on the one LED.
const LED_PULSE_MS: u32 = 250;
const LED_CLICK_MS: u32 = 100;
const LED_HOLD_MS: u32 = 1000;

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

async fn run(b: Board) {
    b.spawner.spawn(buttons().unwrap());
    taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
}

app!(run, commands: COMMANDS, settings: &SETTINGS);
