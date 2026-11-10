//! The TOWER Relay Module: a latching (bistable) relay, switched by a 20 ms coil pulse through
//! an H-bridge on a TCA9534A I/O expander.
//!
//! **Low power.** A latching relay holds its position with no current, so the module costs
//! energy only when it switches: one coil pulse from its reservoir capacitor and five short bus
//! transfers. Between switches the bridge is off and the expander idles (1 µA at most); the
//! module's two LEDs stay dark unless asked for ([`Led::State`]), since a lit LED would cost more
//! than the rest of a battery node.
//!
//! **The coil is never left energised.** The bridge is released after the pulse even when the bus
//! fails (tried three times), every switch first writes the idle pattern and the pin
//! directions (the module may have lost its power since the last one), and
//! [`probe`](RelayModule::probe) starts by releasing the bridge, which also ends a pulse that an
//! MCU reset cut short.
//!
//! **Position.** The relay cannot be read back: its position is known once it has been switched
//! ([`state`](RelayModule::state) is `None` until then). "On" is the legacy SDK's `true` (the
//! green LED), "off" its `false` (the red LED). The expander's pin patterns are
//! [`tower_modules::relay`]'s.
//!
//! ```ignore
//! if let Some(mut relay) = RelayModule::probe(bus).await {
//!     relay.set(true, Led::Off).await?;
//! }
//! ```

use embassy_stm32::i2c::Error as BusError;
use embassy_time::{Duration, Instant, Timer};
use tower_modules::relay as pins;

use crate::i2c::{I2cBus, RawI2c};
use crate::tca9534a::Tca9534a;

/// What the module's LEDs show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Led {
    /// Both dark: nothing drawn between switches.
    Off,
    /// The LED of the relay's position stays lit (green for on, red for off): for a mains
    /// powered node.
    State,
}

/// The Relay Module on the board's I²C bus.
pub struct RelayModule {
    bus: I2cBus,
    addr: u8,
    state: Option<bool>,
    /// The LED pattern in force (kept across a switch's idle writes).
    led: u8,
    /// When the last coil pulse ended (the low 32 bits of the uptime in milliseconds; `None`
    /// before the first): the next one waits for the capacitor.
    pulsed: Option<u32>,
}

impl RelayModule {
    /// Look for the module (at each of the expander's addresses a module can have) and make its
    /// pins outputs with the bridge off, without moving the relay. `None` when nothing answers.
    pub async fn probe(bus: I2cBus) -> Option<Self> {
        let addr = bus.raw(|b| {
            pins::ADDRESSES
                .into_iter()
                .find(|&addr| Tca9534a::new(&mut *b, addr).read_config().is_ok() && idle(b, addr, 0).is_ok())
        });
        bus.rest().await;
        Some(Self {
            bus,
            addr: addr?,
            state: None,
            led: 0,
            pulsed: None,
        })
    }

    /// The expander's I²C address.
    pub fn addr(&self) -> u8 {
        self.addr
    }

    /// The relay's position, once it has been switched.
    pub fn state(&self) -> Option<bool> {
        self.state
    }

    /// Switch the relay `on` or off with one coil pulse (also when it already is there: the pulse
    /// is what makes the position known), and show `led`. Takes the pulse's 20 ms, and up to
    /// 300 ms more right after another switch (the capacitor recharges).
    pub async fn set(&mut self, on: bool, led: Led) -> Result<(), BusError> {
        self.apply(on, led, true).await
    }

    /// Change what the LEDs show without switching (after `led` was turned on or off). Nothing
    /// to show while the position is unknown.
    pub async fn show(&mut self, led: Led) -> Result<(), BusError> {
        match self.state {
            Some(on) => self.apply(on, led, false).await,
            None => Ok(()),
        }
    }

    /// [`set`](Self::set) when `pulse`, else [`show`](Self::show) for a relay already at `on`: one
    /// call for a caller that does both from one place (an await site costs code and RAM in an
    /// async task).
    pub async fn apply(&mut self, on: bool, led: Led, pulse: bool) -> Result<(), BusError> {
        let (bus, addr, old_led) = (self.bus, self.addr, self.led);
        let new_led = match led {
            Led::Off => 0,
            Led::State => pins::led(on),
        };
        let mut started = Ok(());
        if pulse {
            if let Some(pulsed) = self.pulsed {
                // Wrapping arithmetic: the uptime's low 32 bits roll over every 49 days.
                let since = (Instant::now().as_millis() as u32).wrapping_sub(pulsed);
                if since < pins::RECHARGE_MS {
                    Timer::after(Duration::from_millis(u64::from(pins::RECHARGE_MS - since))).await;
                }
            }
            started = bus.raw(|b| {
                idle(b, addr, old_led)?;
                Tca9534a::new(&mut *b, addr).write_output(pins::drive(on) | new_led)
            });
            if started.is_ok() {
                Timer::after_millis(u64::from(pins::PULSE_MS)).await;
            }
            self.pulsed = Some(Instant::now().as_millis() as u32);
        }
        // Release the bridge, whatever happened: the expander may have latched the drive before an
        // error came back. (Without a pulse this is the LED change, the bridge staying off.)
        let released = bus.raw(|b| {
            let mut released = idle(b, addr, new_led);
            for _ in 0..2 {
                if released.is_ok() {
                    break;
                }
                released = idle(b, addr, new_led);
            }
            released
        });
        bus.rest().await;
        started?;
        released?;
        if pulse {
            self.state = Some(on);
        }
        self.led = new_led;
        Ok(())
    }
}

/// The bridge off (keeping the LEDs), then every pin an output: the output register first, so
/// the pins drive the idle pattern from the moment they become outputs (the register reads 0xFF
/// after power-up, which would turn both low-side transistors on).
fn idle(b: &mut RawI2c, addr: u8, led: u8) -> Result<(), BusError> {
    let mut expander = Tca9534a::new(b, addr);
    expander.write_output(pins::IDLE | led)?;
    expander.write_config(0x00)
}
