//! Texas Instruments OPT3001 ambient light sensor driver (the Climate Module, the Lux Meter
//! Tag), HAL-independent over [`embedded_hal::i2c::I2c`].
//!
//! Single-shot: the part stays in shutdown (0.3 µA typical, its state after power-up), converts
//! once when told to (automatic full-scale range, 800 ms, 1.8 µA typical meanwhile) and shuts
//! down again by itself. The configuration words and the lux arithmetic are
//! [`tower_modules::opt3001`]'s.

use embedded_hal::i2c::I2c;
use tower_modules::opt3001 as codec;

/// The address on the Climate Module (ADDR pin to ground).
pub const ADDR: u8 = 0x44;
/// The Lux Meter Tag's other strapping.
pub const ADDR_ALT: u8 = 0x45;

const REG_RESULT: u8 = 0x00;
const REG_CONFIG: u8 = 0x01;
const REG_DEVICE_ID: u8 = 0x7F;

/// How long after [`Opt3001::trigger`] the conversion is done, in milliseconds.
pub const CONVERSION_MS: u64 = codec::CONVERSION_MS as u64;

/// An OPT3001 on an I²C bus.
pub struct Opt3001<I2C> {
    i2c: I2C,
    addr: u8,
}

impl<I2C: I2c> Opt3001<I2C> {
    pub fn new(i2c: I2C, addr: u8) -> Self {
        Self { i2c, addr }
    }

    /// Whether an OPT3001 answers, by its device ID (an SHT3x can share the address).
    pub fn present(&mut self) -> bool {
        self.read(REG_DEVICE_ID).is_ok_and(|id| id == codec::DEVICE_ID)
    }

    /// Shut the part down (it already is after power-up and after each single-shot conversion).
    pub fn shutdown(&mut self) -> Result<(), I2C::Error> {
        self.write_config(codec::CONFIG_SHUTDOWN)
    }

    /// Start one conversion; the result is ready [`CONVERSION_MS`] later (check with
    /// [`ready`](Self::ready)).
    pub fn trigger(&mut self) -> Result<(), I2C::Error> {
        self.write_config(codec::CONFIG_SINGLE_SHOT)
    }

    /// Whether the conversion started last has finished. Reading clears the part's ready flag,
    /// so read the [`illuminance`](Self::illuminance) once this is `true`, without asking again.
    pub fn ready(&mut self) -> Result<bool, I2C::Error> {
        Ok(codec::ready(self.read(REG_CONFIG)?))
    }

    /// The illuminance of the last conversion, in tenths of a lux.
    pub fn illuminance(&mut self) -> Result<u32, I2C::Error> {
        Ok(codec::deci_lux(self.read(REG_RESULT)?))
    }

    /// Release the bus.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn read(&mut self, reg: u8) -> Result<u16, I2C::Error> {
        let mut b = [0u8; 2];
        self.i2c.write_read(self.addr, &[reg], &mut b)?;
        Ok(u16::from_be_bytes(b))
    }

    fn write_config(&mut self, config: u16) -> Result<(), I2C::Error> {
        let [hi, lo] = config.to_be_bytes();
        self.i2c.write(self.addr, &[REG_CONFIG, hi, lo])
    }
}
