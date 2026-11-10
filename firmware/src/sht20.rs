//! Sensirion SHT20 humidity sensor driver (the Climate Module's first revision, the Humidity
//! Tag), HAL-independent over [`embedded_hal::i2c::I2c`].
//!
//! One measurement at a time, on demand: the sensor converts once ("no hold master" commands, so
//! the bus is free meanwhile) and goes back to sleep by itself (0.15 µA typical; about 300 µA
//! for the 29 ms of a humidity conversion). While it converts it does not acknowledge its
//! address, so a read sent too early fails with a NACK and is simply tried again. The answer's
//! CRC and its kind (humidity or temperature) are checked; the arithmetic is
//! [`tower_modules::sht20`]'s.

use embedded_hal::i2c::I2c;
use tower_modules::Invalid;
use tower_modules::sht20 as codec;

/// The SHT20's only address.
pub const ADDR: u8 = 0x40;

const CMD_TEMPERATURE: u8 = 0xF3;
const CMD_HUMIDITY: u8 = 0xF5;
const CMD_READ_USER: u8 = 0xE7;
const CMD_SOFT_RESET: u8 = 0xFE;

/// The longest humidity conversion at the default 12-bit resolution, in milliseconds.
pub const HUMIDITY_MS: u64 = 29;
/// The longest temperature conversion at the default 14-bit resolution, in milliseconds.
pub const TEMPERATURE_MS: u64 = 85;
/// How long the sensor is deaf after a soft reset, in milliseconds.
pub const RESET_MS: u64 = 15;

/// A measurement that could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// The bus transfer failed (a NACK while the sensor still converts, or no sensor).
    Bus(E),
    /// The sensor answered, but not with a valid measurement of the kind asked for.
    Data(Invalid),
}

/// An SHT20 on an I²C bus.
pub struct Sht20<I2C> {
    i2c: I2C,
}

impl<I2C: I2c> Sht20<I2C> {
    pub fn new(i2c: I2C) -> Self {
        Self { i2c }
    }

    /// Whether a sensor answers: reads its user register, which changes nothing.
    pub fn present(&mut self) -> bool {
        let mut user = [0u8];
        self.i2c.write_read(ADDR, &[CMD_READ_USER], &mut user).is_ok()
    }

    /// Reload the sensor's defaults (12-bit humidity, 14-bit temperature, heater off). It is deaf
    /// for [`RESET_MS`] afterwards.
    pub fn soft_reset(&mut self) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[CMD_SOFT_RESET])
    }

    /// Start a humidity conversion; read it with [`read_humidity`](Self::read_humidity) after
    /// [`HUMIDITY_MS`].
    pub fn trigger_humidity(&mut self) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[CMD_HUMIDITY])
    }

    /// Start a temperature conversion; read it with [`read_temperature`](Self::read_temperature)
    /// after [`TEMPERATURE_MS`].
    pub fn trigger_temperature(&mut self) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[CMD_TEMPERATURE])
    }

    /// The relative humidity of the conversion started last, in tenths of a percent.
    pub fn read_humidity(&mut self) -> Result<u16, Error<I2C::Error>> {
        Ok(codec::humidity_deci(self.read(true)?))
    }

    /// The temperature of the conversion started last, in hundredths of a degree Celsius.
    pub fn read_temperature(&mut self) -> Result<i16, Error<I2C::Error>> {
        Ok(codec::temperature_centi(self.read(false)?))
    }

    /// Release the bus.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn read(&mut self, humidity: bool) -> Result<u16, Error<I2C::Error>> {
        let mut answer = [0u8; 3];
        self.i2c.read(ADDR, &mut answer).map_err(Error::Bus)?;
        codec::decode(&answer, humidity).map_err(Error::Data)
    }
}
