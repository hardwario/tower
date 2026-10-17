//! watchdog: the opt-in independent watchdog (IWDG).
//!
//! Arms an 8 s watchdog, then blinks the LED and logs a heartbeat. The feeder task pets the
//! watchdog every 4 s (half the timeout), so a healthy run never resets, even across STOP-mode
//! idle, since the low-power executor wakes for the feeder's timer. To watch it bite, replace the
//! loop body with `loop {}` (a hang): the feeder stops and the MCU resets after ~8 s.
//!
//! An 8 s timeout is shorter than the worst synchronous EEPROM stall plus a feed interval
//! (a `/system/eeprom wipe` or an in-line compaction flip freezes the executor ~2.6-5.2 s). That
//! is safe because the storage layer reloads an armed IWDG between EEPROM programs
//! (`watchdog::pet_during_stall`, bounded so a real hang still resets). Products use 26 s anyway.
//!
//!   just flash example watchdog     (then: tower logs)

#![no_std]
#![no_main]

use embassy_time::{Duration, Timer};
use log::info;
use tower::{app, board::Board, watchdog};

async fn run(b: Board) {
    // Arm the watchdog: a firmware hang now triggers a hardware reset after 8 s.
    watchdog::enable(b.iwdg, b.spawner, Duration::from_secs(8));

    let mut beat = 0u32;
    loop {
        info!(target: "wdg", "alive: heartbeat {beat} (watchdog fed by the feeder task)");
        beat += 1;
        Timer::after(Duration::from_secs(2)).await;
    }
}

app!(run);
