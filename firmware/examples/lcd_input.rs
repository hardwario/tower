//! lcd_input: every input of the TOWER LCD Module on screen ([`lcd`](tower::lcd) events, the RGB
//! LED).
//!
//! The top half shows the last event in large type, the bottom half the four before it. Each event
//! also flashes the RGB LED: red for the left button, green for the right, blue for both together,
//! white for the Core Module button, cyan for a swipe; a hold blinks three times.
//!
//! The gesture sensor draws ~0.3 mA while on, so this sample switches it on at the first button
//! event and off again after 30 s without input (shown in the status line): the low-power way
//! to offer swipes on a battery device. Double clicks are enabled here (they delay a single
//! click by 300 ms).
//!
//! `tower exec "lcd press both"` and `tower exec "lcd swipe up"` inject input from the console.
//!
//!   just flash example lcd_input

#![no_std]
#![no_main]

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use heapless::Deque;
use log::{error, info};
use tower::hmi::{Align, Color, Rect, TextStyle, fonts};
use tower::lcd::{Button, Event, Gesture, Kind, Rgb};
use tower::{app, board::Board, i2c::I2cBus, lcd, shell};

const BIG: TextStyle = TextStyle::new(&fonts::SPLEEN_12X24).align(Align::Center);
const SMALL: TextStyle = TextStyle::new(&fonts::SPLEEN_6X12);
const STATUS: TextStyle = TextStyle::new(&fonts::SPLEEN_6X12)
    .align(Align::Center)
    .inverted();
/// Gestures stay on this long after the last input.
const GESTURE_IDLE: Duration = Duration::from_secs(30);

static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd", lcd::SHELL)];

async fn run(b: Board) {
    let bus = I2cBus::share(b.tmp112.release());
    let mut cfg = lcd::Config::default();
    cfg.buttons.double_click_ms = Some(300);
    let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
    let lcd = match lcd::init(b.spawner, res, bus, cfg).await {
        Ok(lcd) => lcd,
        Err(e) => {
            error!(target: "lcd", "not started ({e:?}): is the LCD Module attached?");
            return;
        }
    };

    let mut history: Deque<Event, 4> = Deque::new();
    let mut last: Option<Event> = None;
    let mut gestures = false;
    loop {
        draw(lcd, last, &history, gestures).await;
        let event = if gestures {
            match select(lcd.next_event(), Timer::after(GESTURE_IDLE)).await {
                Either::First(e) => e,
                Either::Second(()) => {
                    gestures = false;
                    lcd.set_gestures(false);
                    info!(target: "lcd", "gestures off (idle)");
                    continue;
                }
            }
        } else {
            lcd.next_event().await
        };
        info!(target: "lcd", "{event}");
        if !gestures {
            gestures = true;
            lcd.set_gestures(true);
            info!(target: "lcd", "gestures on for {} s after the last input", GESTURE_IDLE.as_secs());
        }
        feedback(lcd, event);
        if let Some(prev) = last.replace(event) {
            if history.is_full() {
                history.pop_back();
            }
            let _ = history.push_front(prev);
        }
    }
}

fn feedback(lcd: lcd::Lcd, event: Event) {
    let color = match event {
        Event::Button(Button::Left, _) => Rgb::RED,
        Event::Button(Button::Right, _) => Rgb::GREEN,
        Event::Button(Button::Both, _) => Rgb::BLUE,
        Event::Button(Button::Core, _) => Rgb::WHITE,
        Event::Gesture(_) => Rgb::CYAN,
        Event::Rotated(_) => return,
    };
    match event {
        Event::Button(_, Kind::Hold) => lcd.led().blink(color, 80, 80, 3),
        Event::Button(_, Kind::Press | Kind::Release) => {}
        _ => lcd.led().blink(color, 120, 0, 1),
    }
}

async fn draw(lcd: lcd::Lcd, last: Option<Event>, history: &Deque<Event, 4>, gestures: bool) {
    lcd.draw(|c| {
        c.clear(Color::White);
        let status = if gestures { "swipes on" } else { "swipes off" };
        c.fill_rect(Rect::new(0, 0, 128, 13), Color::Black);
        c.text(64, 1, status, &STATUS);
        match last {
            None => {
                c.text(64, 30, "press or", &BIG);
                c.text(64, 54, "swipe", &BIG);
            }
            Some(Event::Button(b, k)) => {
                c.text_fmt(64, 24, format_args!("{b}"), &BIG);
                c.text_fmt(64, 48, format_args!("{k}"), &BIG);
            }
            Some(Event::Rotated(_)) => {
                c.text(64, 36, "rotated", &BIG);
            }
            Some(Event::Gesture(g)) => {
                let arrow = match g {
                    Gesture::Up => "^",
                    Gesture::Down => "v",
                    Gesture::Left => "<",
                    Gesture::Right => ">",
                };
                c.text(64, 24, arrow, &BIG);
                c.text_fmt(64, 48, format_args!("swipe {g}"), &BIG);
            }
        }
        c.hline(0, 76, 128, Color::Black);
        for (i, e) in history.iter().enumerate() {
            c.text_fmt(2, 79 + i as i32 * 12, format_args!("{e}"), &SMALL);
        }
    })
    .await;
}

app!(run, commands: CMDS);
