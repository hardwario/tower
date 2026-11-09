//! radio_climate (TOWER IoT Kit product firmware): a climate sensor on the radio, a sleeping taf
//! node (`docs/firmware/taf.md`).
//!
//! Today it reports what every taf node reports: the Core Module's temperature
//! (`nodes/{addr}/state/temperature`, sampled every `report-period` and sent when it moves past
//! `report-delta` or on a heartbeat) and the accelerometer's `tilt` events, both the taf runtime's
//! like pairing, settings, the remote shell and the heartbeat. The Climate Module's sensors
//! (humidity, illuminance, pressure) join as properties of this kind once their drivers exist.
//!
//!   just run app radio_climate

#![no_std]
#![no_main]

use tower::{app, board::Board, shell, taf};
use tower_protocol::props::{self, climate};

const KIND: props::Kind = climate::KIND;
static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
static COMMANDS: &[shell::Entry] = &[taf::NODE_COMMANDS, taf::BUTTON_COMMANDS];

async fn run(b: Board) {
    taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
}

app!(run, commands: COMMANDS, settings: &SETTINGS);
