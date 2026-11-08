//! lcd_swipe: the gesture sensor alone, every activation on screen (a test and tuning aid for the
//! LCD Module's APDS-9960).
//!
//! The sensor stays on. Each time it switches to gesture mode (a hand came close), the screen
//! shows what the driver made of it: the direction as a big arrow, or `?` when the samples didn't
//! add up to a swipe, with the numbers behind the decision: how long the sensor saw the hand, how
//! many datasets it recorded, the lags between opposite photodiodes (how much later one saw the
//! hand than the other, in thousandths of the gesture: the larger one past 40 decides) and the
//! brightest reading (255 is saturated). The LED flashes cyan for a swipe, red for none. A counter
//! row tallies the results; a click resets it.
//!
//! Every activation is also logged (`tower logs`), one line each, for comparing runs.
//!
//!   just flash example lcd_swipe

#![no_std]
#![no_main]

use embassy_futures::select::{Either, select};
use log::{error, info};
use tower::hmi::fonts::{self, spleen_16x32};
use tower::hmi::{Align, Color, Font, Rect, TextStyle};
use tower::lcd::{Event, Gesture, Kind, Rgb, SwipeTrace};
use tower::{app, board::Board, i2c::I2cBus, lcd, shell};

static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd", lcd::SHELL)];

/// 16x32 with the arrows.
static ARROWS: Font = spleen_16x32::with(&[&spleen_16x32::ASCII, &spleen_16x32::SYMBOLS]);

const TITLE: TextStyle = TextStyle::new(&fonts::SPLEEN_8X16)
    .align(Align::Center)
    .inverted();
const NAME: TextStyle = TextStyle::new(&fonts::SPLEEN_12X24);
const TEXT: TextStyle = TextStyle::new(&fonts::SPLEEN_6X12);

async fn run(b: Board) {
    let bus = I2cBus::share(b.tmp112.release());
    let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
    let cfg = lcd::Config {
        gestures: true,
        ..lcd::Config::default()
    };
    let lcd = match lcd::init(b.spawner, res, bus, cfg).await {
        Ok(lcd) => lcd,
        Err(e) => {
            error!(target: "lcd", "not started ({e:?}): is the LCD Module attached?");
            return;
        }
    };
    info!(target: "swipe", "gesture sensor on, swipe over the module");

    // Up, down, left, right, none.
    let mut tally = [0u32; 5];
    let mut n = 0u32;
    let mut last: Option<SwipeTrace> = None;
    loop {
        draw(lcd, n, last.as_ref(), &tally).await;
        match select(lcd::swipe_trace(), lcd.next_event()).await {
            Either::First(t) => {
                n += 1;
                let i = match t.gesture {
                    Some(Gesture::Up) => 0,
                    Some(Gesture::Down) => 1,
                    Some(Gesture::Left) => 2,
                    Some(Gesture::Right) => 3,
                    None => 4,
                };
                tally[i] += 1;
                lcd.led()
                    .blink(if t.gesture.is_some() { Rgb::CYAN } else { Rgb::RED }, 80, 0, 1);
                info!(
                    target: "swipe",
                    "#{} {}: {} ms, {} samples, lag ud {} lr {}, swing ud {} lr {}, peak {}{}",
                    n,
                    t.gesture.map_or("none", name),
                    t.ms,
                    t.trace.samples,
                    t.trace.lag_ud,
                    t.trace.lag_lr,
                    t.trace.ud,
                    t.trace.lr,
                    t.trace.peak,
                    if t.ignored { " (ignored: a button was in use)" } else { "" }
                );
                last = Some(t);
            }
            Either::Second(Event::Button(_, Kind::Click)) => {
                tally = [0; 5];
                n = 0;
                last = None;
                info!(target: "swipe", "tally reset");
            }
            Either::Second(_) => {}
        }
    }
}

fn name(g: Gesture) -> &'static str {
    match g {
        Gesture::Up => "up",
        Gesture::Down => "down",
        Gesture::Left => "left",
        Gesture::Right => "right",
    }
}

async fn draw(lcd: lcd::Lcd, n: u32, last: Option<&SwipeTrace>, tally: &[u32; 5]) {
    lcd.draw(|c| {
        c.clear(Color::White);
        c.fill_rect(Rect::new(0, 0, 128, 18), Color::Black);
        c.text(64, 1, "Swipe test", &TITLE);
        match last {
            None => {
                c.text(64, 34, "swipe over", &TEXT.align(Align::Center));
                c.text(64, 48, "the module", &TEXT.align(Align::Center));
            }
            Some(t) => {
                let (arrow, word) = match t.gesture {
                    Some(Gesture::Up) => ("↑", "UP"),
                    Some(Gesture::Down) => ("↓", "DOWN"),
                    Some(Gesture::Left) => ("←", "LEFT"),
                    Some(Gesture::Right) => ("→", "RIGHT"),
                    None => ("?", "NONE"),
                };
                // The arrow inverted in a box, the word beside it.
                c.fill_round_rect(Rect::new(4, 22, 28, 36), 5, Color::Black);
                c.text(
                    18,
                    24,
                    arrow,
                    &TextStyle::new(&ARROWS).align(Align::Center).color(Color::White),
                );
                c.text_fmt(38, 22, format_args!("#{n}"), &TEXT);
                c.text(38, 34, word, &NAME);
                c.text_fmt(
                    4,
                    62,
                    format_args!("{} ms, {} samples", t.ms, t.trace.samples),
                    &TEXT,
                );
                c.text_fmt(
                    4,
                    74,
                    format_args!("lag UD{:+} LR{:+}", t.trace.lag_ud, t.trace.lag_lr),
                    &TEXT,
                );
                c.text_fmt(4, 86, format_args!("peak {}", t.trace.peak), &TEXT);
                if t.ignored {
                    c.text(4, 98, "ignored (button)", &TEXT);
                }
            }
        }
        c.hline(0, 110, 128, Color::Black);
        c.text_fmt(
            64,
            113,
            format_args!(
                "U{} D{} L{} R{} ?{}",
                tally[0], tally[1], tally[2], tally[3], tally[4]
            ),
            &TEXT.align(Align::Center),
        );
    })
    .await;
}

app!(run, commands: CMDS);
