//! pir_signal: the PIR Module's raw signal, to see what the detection threshold is compared
//! against ([`pir`](tower::pir) block).
//!
//! Puts the detector in forced readout and samples it at 100 Hz. Every half second it logs the
//! band-pass signal's peak (the detector counts a pulse where the signal crosses ±threshold; the
//! driver's default threshold is 32), so walking past shows how far motion swings it above the
//! noise of an empty room. A click on the board's button switches the source: band pass, low pass
//! (the signal before the high pass, about 8000 counts at rest), temperature (80 counts per
//! kelvin, offset per part).
//!
//!   just run example pir_signal

#![no_std]
#![no_main]

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Ticker};
use log::{error, info};
use tower::pir::{self, Mode, Pir, Source};
use tower::{app, board::Board, button};

/// The sampling period: the band pass ends at 7 Hz, the detector allows a frame every 2.4 ms.
const PERIOD: Duration = Duration::from_millis(10);
/// How often a line is logged.
const REPORT: Duration = Duration::from_millis(500);
/// The threshold the band-pass peak is compared against in the log (the driver's default).
const THRESHOLD: i16 = 32;
/// The PIR ADC's sensitivity, 6.5 µV per count (PYQ 1648 specification), as tenths of a µV.
const DECI_UV_PER_COUNT: i32 = 65;

const SOURCES: [(Source, &str); 3] = [
    (Source::BandPass, "band pass"),
    (Source::LowPass, "low pass"),
    (Source::Temperature, "temperature"),
];

static BTN_CH: button::ButtonChannel = button::ButtonChannel::new();

async fn run(b: Board) {
    let btn = button::init_exti(
        b.spawner,
        b.button,
        button::Polarity::ActiveHigh,
        button::Config::default(),
        &BTN_CH,
    );
    let mut pir = Pir::new(b.pir);
    let mut source = 0;
    configure(&mut pir, source).await;
    info!(target: "pir", "click the button to switch the source");

    let mut ticker = Ticker::every(PERIOD);
    let (mut lo, mut hi, mut sum, mut n) = (i16::MAX, i16::MIN, 0i32, 0i32);
    let mut report_at = Instant::now() + REPORT;
    loop {
        match select(ticker.next(), btn.next_event()).await {
            Either::First(()) => {
                // `None` while the detector isn't configured (no module).
                if let Some(v) = pir.sample().await {
                    (lo, hi, sum, n) = (lo.min(v), hi.max(v), sum + i32::from(v), n + 1);
                }
            }
            Either::Second(button::Event::Click) => {
                source = (source + 1) % SOURCES.len();
                configure(&mut pir, source).await;
                (lo, hi, sum, n) = (i16::MAX, i16::MIN, 0, 0);
                report_at = Instant::now() + REPORT;
                continue;
            }
            Either::Second(_) => continue,
        }
        if Instant::now() < report_at || n == 0 {
            continue;
        }
        report_at += REPORT;
        match SOURCES[source].0 {
            Source::BandPass => {
                let peak = hi.max(lo.saturating_neg());
                let uv = i32::from(peak) * DECI_UV_PER_COUNT / 10;
                let over = if peak > THRESHOLD { ", over threshold" } else { "" };
                info!(target: "pir", "peak {peak} ({uv} µV), min {lo}, max {hi}{over}");
            }
            _ => info!(target: "pir", "mean {}, min {lo}, max {hi}", sum / n),
        }
        (lo, hi, sum, n) = (i16::MAX, i16::MIN, 0, 0);
    }
}

/// Switch the detector to readout of `SOURCES[index]`.
async fn configure(pir: &mut Pir, index: usize) {
    let (source, name) = SOURCES[index];
    let config = pir::Config {
        mode: Mode::Signal(source),
        ..pir::Config::default()
    };
    match pir.configure(&config).await {
        Ok(()) => info!(target: "pir", "signal: {name}"),
        Err(e) => error!(target: "pir", "not configured: {e} (is the PIR Module plugged in?)"),
    }
}

app!(run);
