//! strip_thermometer: the Core Module's TMP112 temperature as a bar on the LED strip (PA1).
//!
//! The bar runs from pixel 0 and covers `MIN_C` to `MAX_C` along the strip, coloured by
//! position from blue (cold) to red (warm), so its colour shows the temperature too; faint
//! marks sit at every 5 °C. The bar glides to each new reading (one every 2 s) instead of
//! jumping, its end pixel lit by the fraction it covers. A reading that fails shows a dim
//! red blink at pixel 0.
//!
//! The sensor runs on its own task (a one-shot conversion takes ~30 ms), so the animation
//! never stalls on it. Set up for 144 RGBW pixels (`NUM`, `KIND`).
//!
//!   just flash example strip_thermometer

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, Ordering::Relaxed};

use embassy_stm32::bind_interrupts;
use embassy_time::{Duration, Instant, Ticker, Timer};
use log::{info, warn};
use tower::board::Tmp112Sensor;
use tower::strip::{BLUE, LedKind, RED, Rgb, Strip};
use tower::{app, board::Board, tmp112, ws2812};

// The strip driver's refill interrupt (see `tower::ws2812::Refill`).
bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2_3 => ws2812::Refill;
});

const NUM: usize = 144;
const KIND: LedKind = LedKind::Rgbw;
const BRIGHTNESS: u8 = 40;
/// The temperatures at the two ends of the strip, in °C.
const MIN_C: i32 = 15;
const MAX_C: i32 = 35;
const FRAME_MS: u64 = 20;

/// The latest reading in milli-°C; `i32::MIN` = no valid reading.
static MILLI_C: AtomicI32 = AtomicI32::new(i32::MIN);

#[embassy_executor::task]
async fn sensor(mut tmp112: Tmp112Sensor) {
    let mut failures = 0u32;
    loop {
        match tmp112.oneshot().await {
            Ok(raw) => {
                let mc = tmp112::raw_to_millicelsius(raw);
                // One writer (this task), so load then store is exact (the M0+ has no swap).
                let old = MILLI_C.load(Relaxed);
                MILLI_C.store(mc, Relaxed);
                if old != mc {
                    let sign = if mc < 0 { "-" } else { "" };
                    let centi = (mc.unsigned_abs() + 5) / 10;
                    info!(target: "tmp112", "{}{}.{:02} °C", sign, centi / 100, centi % 100);
                }
                failures = 0;
            }
            Err(e) => {
                MILLI_C.store(i32::MIN, Relaxed);
                failures += 1;
                if failures == 1 || failures.is_power_of_two() {
                    warn!(target: "tmp112", "read failed (I2C {:?}) ({} in a row)", e, failures);
                }
            }
        }
        Timer::after_secs(2).await;
    }
}

/// Milli-°C to the bar level (0..=65535 along the strip), clamped to the range.
fn level_of(mc: i32) -> u16 {
    let span = (MAX_C - MIN_C) * 1000;
    ((mc - MIN_C * 1000).clamp(0, span) as i64 * 65_535 / span as i64) as u16
}

async fn run(b: Board) {
    b.spawner.spawn(sensor(b.tmp112).unwrap());
    let mut strip = Strip::<NUM, { ws2812::rgbw_buf_len(NUM) }>::new(
        b.strip_tim,
        b.strip_data,
        b.strip_dma,
        Irqs,
        KIND,
        BRIGHTNESS,
    );
    info!(target: "strip", "thermometer bar, {} to {} °C along {} pixels", MIN_C, MAX_C, NUM);

    let mut level: i32 = 0; // shown level, gliding toward the reading
    let mut ticker = Ticker::every(Duration::from_millis(FRAME_MS));
    let start = Instant::now();
    loop {
        let mc = MILLI_C.load(Relaxed);
        if mc == i32::MIN {
            // No reading: a dim red blink at pixel 0, once a second.
            strip.clear();
            if start.elapsed().as_millis() % 1000 < 150 {
                strip.set(0, Rgb::new(64, 0, 0));
            }
        } else {
            // Glide: an eighth of the remaining distance per frame (about 0.3 s to settle).
            let target = level_of(mc) as i32;
            level += (target - level) / 8 + (target - level).signum();
            level = level.clamp(0, 65_535);
            strip.bar(level as u16, BLUE, RED);
            // Faint marks every 5 °C.
            let mut c = (MIN_C / 5 + 1) * 5;
            while c < MAX_C {
                let pos = (level_of(c * 1000) as usize * (NUM - 1)) / 65_535;
                let p = strip.get(pos);
                strip.set(pos, p.add(Rgb::new(40, 40, 40)));
                c += 5;
            }
        }
        strip.show().await;
        ticker.next().await;
    }
}

app!(run);
