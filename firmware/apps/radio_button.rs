//! radio_button (TOWER IoT Kit product firmware): a wireless push button, a sleeping taf node
//! (`docs/firmware/taf.md`).
//!
//! Everything it does is the taf runtime's, as in every taf app: each gesture of the Core button
//! (press, release, click, hold) is a `button` event (`nodes/{addr}/event/button`, the value
//! `press`/`release`/`click`/`hold` with its running count) when its setting is on (`press`,
//! `release` off and `click`, `hold` on by default), answered on the LED when `led` is on; the
//! temperature and the accelerometer's `tilt` events; the power-up gesture that pairs or resets
//! the node; the cable, the remote shell, settings over the radio and the heartbeat. `button
//! simulate <ms>` (console only) presses the button through the real recognition, for tests and
//! the HIL bench.
//!
//!   just run app radio_button

#![no_std]
#![no_main]

use tower::{app, board::Board, shell, taf};
use tower_protocol::props::{self, button};

const KIND: props::Kind = button::KIND;
static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
static COMMANDS: &[shell::Entry] = &[taf::NODE_COMMANDS, taf::BUTTON_COMMANDS];

async fn run(b: Board) {
    taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
}

app!(run, commands: COMMANDS, settings: &SETTINGS);
