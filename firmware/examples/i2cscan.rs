//! i2cscan: probe the I2C2 bus (PB10/PB11) and log responding addresses.
//!
//! A maker diagnostic: confirms the bus works and finds device addresses (e.g.
//! which address the TMP112 is strapped to, or which modules are fitted). Watch with `tower logs`.
//!
//! Each address is probed with a register pointer write (one byte, `0x00`: it selects a register
//! and changes nothing on the parts of the TOWER modules) and, when that is not acknowledged,
//! with a one-byte read. Either alone can miss a part: the bare read alone missed the two
//! TCA9534A expanders of a bench stack (2026-11-10), and a part that takes no write would be
//! missed by the write. An SHT20 (`0x40`) answers neither: it takes commands, not register
//! pointers, and has nothing to read until it has measured, so its address is probed with its
//! "read user register" command. Next to each address the scan names the TOWER part that usually
//! sits there.
//!
//! Uses the standard `Board` setup like the other samples, then reclaims the raw
//! I2C2 bus from the (shut-down) TMP112 driver via `release()` to probe it.
//!
//!   just flash example i2cscan

#![no_std]
#![no_main]

use embassy_time::Timer;
use log::info;
use tower::{
    app,
    board::{self, Board},
    console,
};

/// The TOWER part that usually answers at `addr`.
fn usual_part(addr: u8) -> &'static str {
    match addr {
        0x19 => "LIS2DH12, Core Module",
        0x23 | 0x27 => "TCA9534, Relay Module",
        0x38 => "TCA9534A, CO2 Module",
        0x39 => "APDS-9960, LCD Module",
        0x3B | 0x3F => "TCA9534A, Relay Module",
        0x3C => "TCA9534A, LCD Module",
        0x40 => "SHT20, Climate Module or Humidity Tag",
        0x44 => "OPT3001, Climate Module or Lux Meter Tag",
        0x45 => "SHT30, Climate Module",
        0x48 => "TMP112, Climate Module or Temperature Tag",
        0x49 => "TMP112, Core Module",
        0x4D => "SC16IS740, CO2 Module",
        0x60 => "MPL3115A2, Climate Module or Barometer Tag",
        0x64 => "ATSHA204A, Core Module",
        _ => "?",
    }
}

async fn run(b: Board) {
    // Take the *raw* (unguarded) I2C2 bus from the TMP112 driver for address probing:
    // a bus scanner wants to see the real bus, so bypass the AtshaGuard via into_inner().
    let mut i2c = b.tmp112.release().into_inner();

    info!(target: "i2c", "scanning I2C2 at 100 kHz");
    let mut found = 0;
    for addr in 0x08u8..=0x77 {
        // 0xE7 at 0x40: the SHT20's "read user register" (a register pointer for any other part).
        let pointer = if addr == 0x40 { 0xE7 } else { 0x00 };
        let answered =
            i2c.blocking_write(addr, &[pointer]).is_ok() || i2c.blocking_read(addr, &mut [0u8; 1]).is_ok();
        if answered {
            info!(target: "i2c", "device at 0x{:02x} ({})", addr, usual_part(addr));
            found += 1;
            // A full stack answers at a dozen addresses: keep the burst within the console queue.
            console::flush().await;
        }
    }
    info!(target: "i2c", "scan complete, {} device(s)", found);

    // The scan probes every address (incl. the ATSHA204A at 0x64); re-sleep it
    // defensively so a probed/woken crypto part can't hold ~200 µA. Good hygiene after
    // any batch of transactions on the shared I2C bus. See `board::atsha_sleep`.
    board::atsha_sleep(&mut i2c);

    loop {
        Timer::after_secs(60).await;
    }
}

app!(run);
