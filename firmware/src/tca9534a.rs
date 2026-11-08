//! TCA9534A 8-bit I²C I/O expander driver (HAL-independent, over [`embedded_hal::i2c::I2c`]).
//!
//! Four registers: the input port (the pin levels), the output port (the levels driven on the
//! pins configured as outputs), polarity inversion (of the input port) and configuration (a set
//! bit makes the pin an input, the power-on default for every pin). The A variant answers at
//! `0x38 + A2A1A0`; the TOWER LCD Module straps it to `0x3C`.

use embedded_hal::i2c::I2c;

/// Base address (A2 = A1 = A0 = 0) of the TCA9534**A**; the plain TCA9534 starts at `0x20`.
pub const ADDR_BASE: u8 = 0x38;

const REG_INPUT: u8 = 0x00;
const REG_OUTPUT: u8 = 0x01;
const REG_POLARITY: u8 = 0x02;
const REG_CONFIG: u8 = 0x03;

/// A TCA9534A on an I²C bus.
pub struct Tca9534a<I2C> {
    i2c: I2C,
    addr: u8,
}

impl<I2C: I2c> Tca9534a<I2C> {
    pub fn new(i2c: I2C, addr: u8) -> Self {
        Self { i2c, addr }
    }

    /// The levels on all eight pins (bit n = pin Pn), inverted where polarity inversion is set.
    pub fn read_input(&mut self) -> Result<u8, I2C::Error> {
        self.read(REG_INPUT)
    }

    /// Set the levels the output pins drive.
    pub fn write_output(&mut self, value: u8) -> Result<(), I2C::Error> {
        self.write(REG_OUTPUT, value)
    }

    pub fn read_output(&mut self) -> Result<u8, I2C::Error> {
        self.read(REG_OUTPUT)
    }

    /// Set pin directions: a set bit is an input (high impedance), a clear bit an output. Write
    /// the output port first, so a pin turned into an output drives the intended level at once.
    pub fn write_config(&mut self, inputs: u8) -> Result<(), I2C::Error> {
        self.write(REG_CONFIG, inputs)
    }

    pub fn read_config(&mut self) -> Result<u8, I2C::Error> {
        self.read(REG_CONFIG)
    }

    /// Invert the input port reading of the set pins.
    pub fn write_polarity(&mut self, inverted: u8) -> Result<(), I2C::Error> {
        self.write(REG_POLARITY, inverted)
    }

    pub fn read_polarity(&mut self) -> Result<u8, I2C::Error> {
        self.read(REG_POLARITY)
    }

    /// Release the bus.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn read(&mut self, reg: u8) -> Result<u8, I2C::Error> {
        let mut v = [0u8];
        self.i2c.write_read(self.addr, &[reg], &mut v)?;
        Ok(v[0])
    }

    fn write(&mut self, reg: u8, value: u8) -> Result<(), I2C::Error> {
        self.i2c.write(self.addr, &[reg, value])
    }
}
