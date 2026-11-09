//! The TOWER Power Module: a mains relay on header P0 (PA0, active high), and the LED strip's 5 V
//! supply with a level shifter for its data line (PA1, driven by [`ws2812`](crate::ws2812)).
//!
//! The relay is a plain output: the legacy SDK's `twr_module_power` drives P0 high for on and keeps
//! no other state. Its coil draws its current from the module's own supply, not the MCU.

use embassy_stm32::Peri;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::peripherals::PA0;

/// The Power Module's relay.
pub struct Relay {
    pin: Output<'static>,
}

impl Relay {
    /// Take P0 ([`Board::header_p0`](crate::board::Board::header_p0)) with the relay `on` or off.
    pub fn new(pin: Peri<'static, PA0>, on: bool) -> Self {
        let level = if on { Level::High } else { Level::Low };
        Self {
            pin: Output::new(pin, level, Speed::Low),
        }
    }

    pub fn set(&mut self, on: bool) {
        if on {
            self.pin.set_high();
        } else {
            self.pin.set_low();
        }
    }

    /// The relay's state, read back from the pin's output latch.
    pub fn is_on(&self) -> bool {
        self.pin.is_set_high()
    }
}
