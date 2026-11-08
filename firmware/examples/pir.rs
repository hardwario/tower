//! pir: motion events from the TOWER PIR Module ([`pir`](tower::pir) block).
//!
//! The detector judges motion itself and wakes the MCU (out of STOP) only for an event: each one is
//! logged as `motion #n` and flashed on the LED. A click on the board's button steps the
//! sensitivity (the detection threshold) through four levels; `pir_signal` shows the signal the
//! threshold is compared against. Events count 30 s after a power-on (the detector warms up) and
//! 5 s after any other reset (its filters settle).
//!
//!   just run example pir

#![no_std]
#![no_main]

use embassy_futures::select::{Either, select};
use embassy_stm32::gpio::{Level, Output, Speed};
use log::{error, info};
use tower::pir::{self, Pir};
use tower::{app, board::Board, button, led};

/// The click-selectable sensitivities: a name and the threshold (ADC counts of the band-pass
/// signal; lower is more sensitive). "high" is the driver's default.
const LEVELS: [(&str, u8); 4] = [("low", 128), ("medium", 64), ("high", 32), ("very high", 16)];
const START: usize = 2;

static BTN_CH: button::ButtonChannel = button::ButtonChannel::new();
static LED_CH: led::LedChannel = led::LedChannel::new();

async fn run(b: Board) {
    let led = led::init(
        b.spawner,
        Output::new(b.led, Level::Low, Speed::Low),
        &LED_CH,
        led::Polarity::ActiveHigh,
    );
    let btn = button::init_exti(
        b.spawner,
        b.button,
        button::Polarity::ActiveHigh,
        button::Config::default(),
        &BTN_CH,
    );

    let mut pir = Pir::new(b.pir);
    let mut level = START;
    let mut config = pir::Config {
        threshold: LEVELS[level].1,
        ..pir::Config::default()
    };
    if let Err(e) = pir.configure(&config).await {
        // Without the module there is nothing to do; the board idles in STOP.
        error!(target: "pir", "not configured: {e} (is the PIR Module plugged in?)");
        core::future::pending::<()>().await;
    }
    info!(target: "pir", "ready, {config}");
    if let Some(left) = pir.warming_up() {
        info!(target: "pir", "settling, events count in {} s", left.as_millis().div_ceil(1000));
    }
    info!(target: "pir", "click the button to change the sensitivity");

    let mut count = 0u32;
    loop {
        match select(pir.wait_motion(), btn.next_event()).await {
            Either::First(()) => {
                count += 1;
                info!(target: "pir", "motion #{count}");
                led.flash(200);
            }
            Either::Second(button::Event::Click) => {
                level = (level + 1) % LEVELS.len();
                config.threshold = LEVELS[level].1;
                match pir.configure(&config).await {
                    Ok(()) => info!(target: "pir", "sensitivity {}, {config}", LEVELS[level].0),
                    Err(e) => error!(target: "pir", "not configured: {e}"),
                }
            }
            Either::Second(_) => {}
        }
    }
}

app!(run);
