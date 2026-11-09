//! radio_motion (TOWER IoT Kit product firmware): a PIR motion detector on the radio, a sleeping
//! taf node (`docs/firmware/taf.md`).
//!
//! The PIR Module's detector judges motion itself and wakes the MCU only for an event
//! (`tower::pir`); each detection is a `motion` event on MQTT (`nodes/{addr}/event/motion`). While
//! someone keeps moving, detections within `holdoff` seconds travel as one uplink whose value is
//! how many they were, so a busy room cannot drain the radio's duty budget. `sensitivity` picks
//! the detector's threshold (low, medium, high, very-high: 128, 64, 32, 16 counts), applied at once
//! when set over the radio. The Core Module's temperature is reported too. Everything else
//! (pairing, settings, the remote shell, the heartbeat) is the taf runtime's.
//!
//!   just run app radio_motion

#![no_std]
#![no_main]

use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Instant, Timer};
use log::{error, info};
use tower::pir::{self, Pir};
use tower::{app, board::Board, shell, taf};
use tower_protocol::props::{self, motion};

const KIND: props::Kind = motion::KIND;
static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
static COMMANDS: &[shell::Entry] = &[taf::NODE_COMMANDS, taf::BUTTON_COMMANDS];

/// The detector threshold of each `sensitivity` (ADC counts of the band-pass signal); the PIR
/// guide's bench numbers: a still room peaks at 7, a hand at 20 cm at 40 to 230.
const THRESHOLDS: [u8; 4] = [128, 64, 32, 16];

fn config() -> pir::Config {
    let level = (taf::param(motion::SENSITIVITY).max(0) as usize).min(THRESHOLDS.len() - 1);
    pir::Config {
        threshold: THRESHOLDS[level],
        ..pir::Config::default()
    }
}

#[embassy_executor::task]
async fn detector(mut pir: Pir) {
    // Without the module there is nothing to detect; the node still pairs and reports.
    while let Err(e) = pir.configure(&config()).await {
        error!(target: "motion", "PIR not configured: {e}, retrying in 60 s");
        Timer::after_secs(60).await;
    }
    info!(target: "motion", "detector ready, {}", config());
    // Detections since the last uplink, and when the holdoff ends (none running: `None`).
    let mut pending: u32 = 0;
    let mut holdoff_end: Option<Instant> = None;
    loop {
        let quiet = async {
            match holdoff_end {
                Some(t) => Timer::at(t).await,
                None => core::future::pending().await,
            }
        };
        match select3(pir.wait_motion(), taf::next_set(), quiet).await {
            Either3::First(()) => {
                if taf::led_on()
                    && let Some(led) = taf::led()
                {
                    led.flash(100);
                }
                if holdoff_end.is_some() {
                    pending += 1;
                } else {
                    info!(target: "motion", "motion");
                    taf::event(motion::MOTION, 1);
                    let holdoff = taf::param(motion::HOLDOFF).clamp(1, 3600) as u64;
                    holdoff_end = Some(Instant::now() + Duration::from_secs(holdoff));
                }
            }
            Either3::Second((id, _)) => {
                if id == motion::SENSITIVITY {
                    match pir.configure(&config()).await {
                        Ok(()) => info!(target: "motion", "sensitivity changed, {}", config()),
                        Err(e) => error!(target: "motion", "PIR not configured: {e}"),
                    }
                }
            }
            Either3::Third(()) => {
                holdoff_end = None;
                if pending > 0 {
                    info!(target: "motion", "motion ({} in the holdoff)", pending);
                    taf::event(motion::MOTION, pending.min(65_535) as i32);
                    pending = 0;
                    // Still moving: the next holdoff starts now.
                    let holdoff = taf::param(motion::HOLDOFF).clamp(1, 3600) as u64;
                    holdoff_end = Some(Instant::now() + Duration::from_secs(holdoff));
                }
            }
        }
    }
}

async fn run(b: Board) {
    b.spawner.spawn(detector(Pir::new(b.pir)).unwrap());
    b.spawner.spawn(taf::thermometer(b.tmp112).unwrap());
    taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
}

app!(run, commands: COMMANDS, settings: &SETTINGS);
