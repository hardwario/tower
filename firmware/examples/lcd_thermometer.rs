//! lcd_thermometer: a battery-friendly thermometer on the TOWER LCD Module.
//!
//! A big readout of the TMP112 temperature, measured every 30 s, with the minimum and maximum
//! since start. Left click switches °C / °F, right hold resets the minimum and maximum.
//!
//! What keeps it frugal (see `docs/hmi.md`, "Low power"):
//! * The memory LCD holds the picture with no refresh, and a redraw sends only the rows whose
//!   pixels changed: a new reading typically rewrites 30 to 60 rows (a few ms of DMA).
//! * Nothing polls: between readings the MCU sleeps in STOP; a button press wakes it through
//!   PA8. The gesture sensor, backlight and LED stay off.
//! * The large digits come from Spleen 32x64's numeric block (4.8 KB of flash, not the 48 KB of
//!   the whole font).
//!
//! Every 10 minutes it logs the display's work: flushes, rows sent and the VCOM-only toggles that
//! keep the panel healthy in between.
//!
//!   just flash example lcd_thermometer

#![no_std]
#![no_main]

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use log::{error, info, warn};
use tower::hmi::{Align, Color, Rect, TextStyle, fonts};
use tower::lcd::{Button, Event, Kind, Rgb};
use tower::tmp112::{self, Tmp112};
use tower::{app, board::Board, i2c::I2cBus, lcd, shell};

static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd", lcd::SHELL)];

const PERIOD: Duration = Duration::from_secs(30);
const REPORT: Duration = Duration::from_secs(600);

const BIG: TextStyle = TextStyle::new(&fonts::SPLEEN_32X64_NUMERIC).align(Align::Center);
const MEDIUM: TextStyle = TextStyle::new(&fonts::SPLEEN_16X32_NUMERIC).align(Align::Center);
const UNIT: TextStyle = TextStyle::new(&fonts::SPLEEN_8X16).align(Align::Center);
const SMALL: TextStyle = TextStyle::new(&fonts::SPLEEN_6X12).align(Align::Center);

async fn run(b: Board) {
    let bus = I2cBus::share(b.tmp112.release());
    let mut tmp = Tmp112::new(bus, tmp112::ADDR_VPLUS);
    let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
    let lcd = match lcd::init(b.spawner, res, bus, lcd::Config::default()).await {
        Ok(lcd) => lcd,
        Err(e) => {
            error!(target: "lcd", "not started ({e:?}): is the LCD Module attached?");
            return;
        }
    };

    let mut fahrenheit = false;
    let mut range: Option<(i32, i32)> = None; // tenths of °C
    let mut now_t: Option<i32> = None;
    let mut next = Instant::now();
    let mut report = Instant::now() + REPORT;
    loop {
        if Instant::now() >= next {
            next += PERIOD;
            match tmp.oneshot().await {
                Ok(raw) => {
                    let t = tmp112::raw_to_millicelsius(raw).div_euclid(100);
                    now_t = Some(t);
                    range = Some(range.map_or((t, t), |(lo, hi)| (lo.min(t), hi.max(t))));
                }
                Err(e) => warn!(target: "tmp112", "read failed (I2C {e:?})"),
            }
            draw(lcd, now_t, range, fahrenheit).await;
        }
        if Instant::now() >= report {
            report += REPORT;
            let s = lcd.stats();
            info!(target: "lcd", "{} flushes, {} rows sent, {} VCOM toggles", s.flushes, s.rows, s.vcom_toggles);
        }

        match select(Timer::at(next.min(report)), lcd.next_event()).await {
            Either::First(()) => {}
            Either::Second(Event::Button(Button::Left, Kind::Click)) => {
                fahrenheit = !fahrenheit;
                draw(lcd, now_t, range, fahrenheit).await;
            }
            Either::Second(Event::Button(Button::Right, Kind::Hold)) => {
                range = now_t.map(|t| (t, t));
                lcd.led().blink(Rgb::GREEN, 60, 0, 1);
                info!(target: "thermometer", "minimum and maximum reset");
                draw(lcd, now_t, range, fahrenheit).await;
            }
            Either::Second(_) => {}
        }
    }
}

async fn draw(lcd: lcd::Lcd, t: Option<i32>, range: Option<(i32, i32)>, fahrenheit: bool) {
    let show = |t: i32| Tenths(if fahrenheit { t * 9 / 5 + 320 } else { t });
    lcd.draw(|c| {
        c.clear(Color::White);
        match t {
            // Four characters fill the 128 pixels at 32 wide; a longer value takes the 16x32 size.
            Some(t) if Tenths::len(show(t).0) <= 4 => {
                c.text_fmt(64, 6, format_args!("{}", show(t)), &BIG);
            }
            Some(t) => {
                c.text_fmt(64, 22, format_args!("{}", show(t)), &MEDIUM);
            }
            None => {
                c.text(64, 22, "--", &MEDIUM);
            }
        }
        c.text(64, 74, if fahrenheit { "°F" } else { "°C" }, &UNIT);
        c.hline(8, 98, 112, Color::Black);
        if let Some((lo, hi)) = range {
            c.text_fmt(
                64,
                104,
                format_args!("min {}  max {}", show(lo), show(hi)),
                &SMALL,
            );
        }
        c.rect(Rect::new(0, 0, 128, 128), Color::Black);
    })
    .await;
}

/// Tenths of a degree, shown with one decimal.
#[derive(Clone, Copy)]
struct Tenths(i32);

impl Tenths {
    /// Characters the value takes.
    fn len(t: i32) -> usize {
        let digits = (t.unsigned_abs() / 10).checked_ilog10().unwrap_or(0) as usize + 1;
        digits + 2 + (t < 0) as usize
    }
}

impl core::fmt::Display for Tenths {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let a = self.0.unsigned_abs();
        write!(f, "{sign}{}.{}", a / 10, a % 10)
    }
}

app!(run, commands: CMDS);
