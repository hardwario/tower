//! lcd_hello: first light on the TOWER LCD Module ([`lcd`](tower::lcd) driver, [`hmi`](tower::hmi)
//! framework).
//!
//! Draws a greeting and an uptime counter that changes once a second. Each second the counter
//! area is cleared and drawn again, yet only the rows whose pixels changed go to the panel:
//! `lcd: …` lines report the rows sent. Clicking either button inverts the screen.
//!
//! The `lcd` shell commands are mounted: `tower exec "lcd stats"`, `tower exec "lcd dump"`,
//! `tower exec "lcd press left"`.
//!
//!   just flash example lcd_hello

#![no_std]
#![no_main]

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Ticker};
use log::{error, info};
use tower::hmi::{Align, Color, Rect, TextStyle, fonts};
use tower::{app, board::Board, i2c::I2cBus, lcd, shell};

const TITLE: TextStyle = TextStyle::new(&fonts::SPLEEN_12X24).align(Align::Center);
const LABEL: TextStyle = TextStyle::new(&fonts::SPLEEN_8X16).align(Align::Center);
const COUNTER: TextStyle = TextStyle::new(&fonts::SPLEEN_16X32).align(Align::Center);

static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd", lcd::SHELL)];

async fn run(b: Board) {
    let bus = I2cBus::share(b.tmp112.release());
    let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
    let lcd = match lcd::init(b.spawner, res, bus, lcd::Config::default()).await {
        Ok(lcd) => lcd,
        Err(e) => {
            error!(target: "lcd", "not started ({e:?}): is the LCD Module attached?");
            return;
        }
    };
    info!(target: "lcd", "started");

    lcd.draw(|c| {
        c.round_rect(Rect::new(0, 0, 128, 128), 10, Color::Black);
        c.text(64, 10, "Hello,", &TITLE);
        c.text(64, 36, "TOWER!", &TITLE);
        c.hline(16, 68, 96, Color::Black);
        c.text(64, 74, "uptime", &LABEL);
    })
    .await;

    let start = Instant::now();
    let mut ticker = Ticker::every(Duration::from_secs(1));
    let mut inverted = false;
    loop {
        match select(ticker.next(), lcd.next_event()).await {
            Either::First(()) => {
                let s = start.elapsed().as_secs();
                let before = lcd.stats();
                lcd.draw(|c| {
                    // Immediate mode: clear the area and draw it again; the driver sends only
                    // the rows that came out different.
                    let area = Rect::new(4, 92, 120, 32);
                    c.fill_rect(area, if inverted { Color::Black } else { Color::White });
                    let style = if inverted {
                        COUNTER.color(Color::White)
                    } else {
                        COUNTER
                    };
                    c.text_fmt(64, 92, format_args!("{}:{:02}", s / 60, s % 60), &style);
                })
                .await;
                let after = lcd.stats();
                if s.is_multiple_of(10) {
                    info!(target: "lcd", "{} s, this update sent {} rows", s, after.rows - before.rows);
                }
            }
            Either::Second(event) => {
                info!(target: "lcd", "{event}");
                if let lcd::Event::Button(_, lcd::Kind::Click) = event {
                    inverted = !inverted;
                    lcd.draw(|c| c.invert_rect(c.bounds())).await;
                }
            }
        }
    }
}

app!(run, commands: CMDS);
