//! strip: a tour of the LED strip effects (PA1).
//!
//! Demonstrates [`strip`](tower::strip) (the [`tower_strip`] effects core on the
//! [`ws2812`](tower::ws2812) driver). Set up for the TOWER LED strip, 144 SK6812 RGBW
//! pixels; for another strip edit `NUM` and `KIND` (`LedKind::Rgb` for a WS2812B). Each
//! effect runs for `EFFECT_MS` and its name is logged:
//!
//! * `channel test`: red, green, blue and white quarters from the input end. Any other
//!   order means the strip's byte order differs from G-R-B(-W).
//! * the moving effects: `rainbow`, `palette`, `fire`, `twinkle`, `candle`, `plasma`,
//!   `scanner`, `balls`, `meteor`, `sparkle`, `running lights`, `curtain`, `breathe`;
//! * `white`: the whole strip on the white LEDs (on RGBW the white part of every colour
//!   goes to the W LED).
//!
//! The animation steps come from the clock (one step per `STEP_MS`), not from counting
//! frames, so the speed does not depend on how long a frame takes to send.
//!
//! Power: 144 pixels at full white draw several amperes at 5 V, and even dark they idle at
//! about 1 mA each. `BRIGHTNESS` (30 %, about 9 % duty after gamma) keeps the tour low,
//! and `POWER_LIMIT_MA` caps every frame on top (the limiter scales a frame that would draw
//! more); raise them only on a supply that can feed the strip.
//!
//!   just flash example strip

#![no_std]
#![no_main]

use embassy_time::{Duration, Instant, Ticker};
use log::info;
use tower::strip::palette::{LAVA, PARTY};
use tower::strip::{
    BLUE, Balls, CYAN, Fire, GREEN, LedKind, MAGENTA, ORANGE, PURPLE, RED, Rng, Segment, Strip, WARM_WHITE,
    WHITE, YELLOW,
};
use tower::{app, board, board::Board, ws2812};

/// Pixels on the strip.
const NUM: usize = 144;
/// Wire format: SK6812 RGBW (`LedKind::Rgb` for a WS2812B).
const KIND: LedKind = LedKind::Rgbw;
/// Global brightness, 0 to 100 % (perceptual).
const BRIGHTNESS: u8 = 30;
/// The current cap for every frame, in mA (idle draw included).
const POWER_LIMIT_MA: u32 = 1500;
/// One animation step.
const STEP_MS: u64 = 20;
/// How long each effect runs.
const EFFECT_MS: u64 = 7000;
/// Frame period: a frame renders in well under this, so the ticker keeps a steady 50 fps.
const FRAME_MS: u64 = 20;

const EFFECTS: [&str; 16] = [
    "channel test",
    "rainbow",
    "palette",
    "fire",
    "twinkle",
    "candle",
    "plasma",
    "scanner",
    "balls",
    "meteor",
    "sparkle",
    "running lights",
    "curtain",
    "breathe",
    "white",
    "colour wipe",
];

async fn run(b: Board) {
    let mut strip = Strip::<NUM, { ws2812::rgbw_buf_len(NUM) }>::new(
        b.strip_tim,
        b.strip_data,
        b.strip_dma,
        KIND,
        BRIGHTNESS,
    );
    strip.set_power_limit(POWER_LIMIT_MA);
    let mut rng = Rng::new(board::rand_u32()); // TRNG seed: a different sparkle every boot
    let mut fire = Fire::<NUM>::new();
    let mut balls = Balls::new(NUM, 1400, [RED, YELLOW, CYAN, MAGENTA]);
    let kind = if KIND == LedKind::Rgbw { "RGBW" } else { "RGB" };
    info!(target: "strip", "{} {} pixels on PA1, brightness {} %, limit {} mA", NUM, kind, BRIGHTNESS, POWER_LIMIT_MA);

    let quarter = NUM / 4;
    let channels = [
        Segment::new(quarter, RED),
        Segment::new(quarter, GREEN),
        Segment::new(quarter, BLUE),
        Segment::new(NUM - 3 * quarter, WHITE),
    ];

    let mut ticker = Ticker::every(Duration::from_millis(FRAME_MS));
    loop {
        for (n, name) in EFFECTS.iter().enumerate() {
            info!(target: "strip", "{}", name);
            let start = Instant::now();
            let mut last = start;
            strip.clear();
            while start.elapsed().as_millis() < EFFECT_MS {
                let now = Instant::now();
                let t = (now.duration_since(start).as_millis() / STEP_MS) as u32;
                let dt = now.duration_since(last).as_millis() as u32;
                last = now;
                match n {
                    0 => strip.segments(&channels),
                    1 => strip.rainbow(t),
                    2 => strip.palette_scroll(&PARTY, t),
                    3 => {
                        fire.step(&mut rng);
                        fire.draw(&mut strip);
                    }
                    4 => strip.twinkle(WARM_WHITE, t, 48),
                    5 => strip.candle(t),
                    6 => strip.plasma(&LAVA, t),
                    7 => strip.scanner(RED, t, 16),
                    8 => {
                        balls.step(dt);
                        balls.draw(&mut strip);
                    }
                    9 => strip.meteor(PURPLE, t, 4, &mut rng),
                    10 => strip.sparkle(WHITE, &mut rng),
                    11 => strip.running_lights(ORANGE, t),
                    12 => strip.curtain(CYAN, t / 2),
                    13 => strip.breathe(MAGENTA, t),
                    14 => strip.fill(WHITE),
                    _ => strip.color_wipe(GREEN, t),
                }
                strip.show().await;
                ticker.next().await;
            }
        }
    }
}

app!(run);
