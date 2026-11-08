//! strip_tilt: tilt the Core Module to steer the LED strip (PA1), from the LIS2DH12
//! accelerometer. A **click** of the button switches between two modes:
//!
//! * **level**: a spirit level. A bubble sits at the centre of the strip when the board is
//!   level and slides toward the raised end as you tilt it (full scale at 30°); it is green
//!   within 1° of level, yellow within 5°, red beyond.
//! * **ball**: a ball rolls along the strip under the tilt, with friction, bouncing off the
//!   ends, leaving a short trail.
//!
//! The board's X axis maps to the strip (`AXIS_X`; flip `INVERT` if the motion runs the wrong
//! way for how the board sits next to the strip). Set up for 144 RGBW pixels.
//!
//!   just flash example strip_tilt

#![no_std]
#![no_main]

use embassy_futures::select::{Either, select};
use embassy_stm32::bind_interrupts;
use embassy_time::{Duration, Instant, Ticker};
use log::{error, info};
use tower::lis2dh12::{self, Lis2dh12, Odr};
use tower::strip::{GREEN, LedKind, ORANGE, RED, Rgb, Strip, YELLOW};
use tower::{app, board::Board, button, ws2812};

// The strip driver's refill interrupt (see `tower::ws2812::Refill`).
bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2_3 => ws2812::Refill;
});

const NUM: usize = 144;
const KIND: LedKind = LedKind::Rgbw;
const BRIGHTNESS: u8 = 50;
/// Use the board's X axis (`false`: Y).
const AXIS_X: bool = true;
/// Run the motion the other way along the strip.
const INVERT: bool = false;
/// The ball feels this share of gravity (100 = real; a real 1 m strip is fast).
const GRAVITY_PERCENT: i64 = 30;
const FRAME_MS: u64 = 20;

static BUTTON: button::ButtonChannel = button::ButtonChannel::new();

async fn run(b: Board) {
    let btn = button::init_exti(
        b.spawner,
        b.button,
        button::Polarity::ActiveHigh,
        button::Config::default(),
        &BUTTON,
    );
    // The accelerometer shares I²C2 with the TMP112: reclaim the bus from it.
    let mut accel = Lis2dh12::new(b.tmp112.release(), lis2dh12::ADDR_DEFAULT);
    if let Err(e) = accel.init_rate(Odr::Hz100) {
        error!(target: "accel", "init failed (I2C {:?})", e);
    }
    let mut strip = Strip::<NUM, { ws2812::rgbw_buf_len(NUM) }>::new(
        b.strip_tim,
        b.strip_data,
        b.strip_dma,
        Irqs,
        KIND,
        BRIGHTNESS,
    );

    let mut ball_mode = false;
    info!(target: "strip", "tilt: level mode (click for the ball)");
    let mut tilt: i32 = 0; // filtered acceleration along the strip, milli-g
    // The ball: position in 1/65536 pixel, velocity in 1/2^24 pixel per ms (fine enough that a
    // small tilt still accelerates it and friction can bring it to rest).
    let top = (NUM as i64 - 1) << 16;
    let (mut pos, mut vel) = (top / 2, 0i64);
    let mut ticker = Ticker::every(Duration::from_millis(FRAME_MS));
    let mut last = Instant::now();
    loop {
        let now = Instant::now();
        let dt = now.duration_since(last).as_millis().min(100) as i64;
        last = now;

        if let Ok(a) = accel.read() {
            let raw = if AXIS_X { a.x } else { a.y } as i32;
            let raw = if INVERT { -raw } else { raw };
            tilt += (raw - tilt) / 4; // low-pass: about 4 samples
        }

        if ball_mode {
            // a = g sin(angle) = tilt (mg) x 9.81 m/s² / 1000; 144 pixels a metre.
            // In 1/2^24 px per ms²: tilt * 9.81 * 144 * 2^24 / (1000 * 10^6) = tilt * 23.7.
            let acc = tilt as i64 * 237 * GRAVITY_PERCENT / 1000;
            vel += acc * dt;
            vel -= vel / 64; // rolling friction (halves the speed in about 0.9 s)
            pos += (vel * dt) >> 8;
            if pos < 0 {
                pos = -pos;
                vel = -vel * 6 / 10;
            } else if pos > top {
                pos = 2 * top - pos;
                vel = -vel * 6 / 10;
            }
            strip.fade(90); // a short trail
            strip.dot((pos >> 8) as i32 + 128, 512, ORANGE);
        } else {
            // The bubble rises: it goes to the raised end, against the gravity component (the
            // ball above rolls the other way).
            // ±500 mg (30°) spans the strip from the centre.
            let centre = ((NUM as i32 - 1) * 256) / 2 + 128;
            let offset = -tilt.clamp(-500, 500) * ((NUM as i32 - 1) * 256 / 2) / 500;
            let degrees_ish = tilt.abs(); // ~17 mg a degree near level
            let color = if degrees_ish < 17 {
                GREEN
            } else if degrees_ish < 87 {
                YELLOW
            } else {
                RED
            };
            strip.clear();
            strip.dot(centre + offset, 768, color);
            // Faint marks either side of the centre (the bubble sits between them when level).
            for i in [NUM / 2 - 3, NUM / 2 + 2] {
                let p = strip.get(i);
                strip.set(i, p.add(Rgb::new(30, 30, 30)));
            }
        }
        strip.show().await;

        if let Either::Second(button::Event::Click) = select(ticker.next(), btn.next_event()).await {
            ball_mode = !ball_mode;
            if ball_mode {
                pos = top / 2;
                vel = 0;
                strip.clear();
            }
            info!(target: "strip", "tilt: {} mode", if ball_mode { "ball" } else { "level" });
        }
    }
}

app!(run);
