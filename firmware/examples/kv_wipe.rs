//! kv_wipe: factory-reset the data-EEPROM key-value store: drop every key, leaving an empty
//! store the next firmware boots on.
//!
//! The normal reset is the shell command `system eeprom wipe confirm` (console-only); per-key
//! removal is `Nv::delete` (tombstones, so the live set can shrink). This example is the fallback
//! for a device whose shell is unreachable (e.g. a crash-looping app, or a `no_shell` build):
//!
//!   just flash example kv_wipe     # LED: on = wiping (~3-5 s), slow blink = done, then
//!   just flash example <your app>  # boots on an empty store
//!
//! It runs the same codec operation as the shell ([`storage::factory_wipe`]): a **flip to an
//! empty store**: blank the dead half, commit its superblock, then scrub the old half. A power
//! loss at any point leaves either the complete old store or an empty one (never a half-zeroed
//! log whose tail could resurface), both record halves end up physically zero, and the wear
//! gauge (the superblock generation, `system eeprom print` `flips:`) keeps counting rather
//! than resetting (the wipe costs one flip).
//!
//! ⚠️ Erases EVERYTHING the store holds: pairing keys, peer tables, replay lanes, the TX
//! watermark, settings, session counter. A re-keyed/re-paired network is required after,
//! which is exactly the fail-closed behaviour the security model wants on a wiped store.
//!
//! Deliberately NOT an `app!`: it must run before any `Nv`/`Kv` (and any subsystem holding RAM
//! state derived from the store) exists, so it uses a bare entry with `board::init` + the raw
//! `Storage`.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_stm32::flash::Flash;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_time::Timer;
use tower::storage::{self, Storage};

#[embassy_executor::main(executor = "embassy_stm32::executor::Executor", entry = "cortex_m_rt::entry")]
async fn main(_spawner: Spawner) {
    let p = tower::board::init();
    let mut led = Output::new(p.PH1, Level::Low, Speed::Low);
    let mut storage = Storage::new(Flash::new_blocking(p.FLASH));

    // LED on for the duration: the wipe is one synchronous run of CPU-stalling EEPROM programs
    // (~2.6 s, ~5.2 s if the dead half wasn't pre-blanked), so nothing can blink meanwhile.
    led.set_high();
    let ok = storage::factory_wipe(&mut storage).is_ok();
    led.set_low();

    // Done: slow blink (fast blink = the wipe reported an EEPROM error: re-flash and retry;
    // the store is still consistent, either untouched or empty). Flash the real firmware next.
    let half_period = if ok { 500 } else { 100 };
    loop {
        led.toggle();
        Timer::after_millis(half_period).await;
    }
}
