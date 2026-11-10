//! Sensirion SHT30 temperature and humidity sensor driver (the Climate Module's second
//! revision), HAL-independent over [`embedded_hal::i2c::I2c`].
//!
//! Single-shot: one command measures both values (high repeatability, no clock stretching,
//! 15.5 ms at most, about 0.8 mA meanwhile) and the sensor idles again by itself (0.2 µA
//! typical). Both words of the answer carry a CRC; the arithmetic is [`tower_modules::sht30`]'s.

use embedded_hal::i2c::I2c;
use tower_modules::Invalid;
use tower_modules::crc::crc8;
use tower_modules::sht30 as codec;

/// The address on the Climate Module (ADDR pin high); the other strapping is `0x44`.
pub const ADDR: u8 = 0x45;

const CMD_MEASURE: [u8; 2] = [0x24, 0x00];
const CMD_STATUS: [u8; 2] = [0xF3, 0x2D];

/// The longest high-repeatability measurement, in milliseconds (15.5 by the datasheet).
pub const MEASURE_MS: u64 = 16;

/// A measurement that could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// The bus transfer failed (a NACK while the sensor still measures, or no sensor).
    Bus(E),
    /// The sensor answered, but a CRC does not match.
    Data(Invalid),
}

/// An SHT30 on an I²C bus.
pub struct Sht30<I2C> {
    i2c: I2C,
    addr: u8,
}

impl<I2C: I2c> Sht30<I2C> {
    pub fn new(i2c: I2C, addr: u8) -> Self {
        Self { i2c, addr }
    }

    /// Whether an SHT3x answers: reads its status register and checks the word's CRC, which
    /// tells it from another part at the same address (an OPT3001 can sit at `0x45` too).
    pub fn present(&mut self) -> bool {
        let mut status = [0u8; 3];
        self.i2c.write_read(self.addr, &CMD_STATUS, &mut status).is_ok()
            && crc8(&status[..2], 0xFF) == status[2]
    }

    /// Start a measurement; read it with [`read`](Self::read) after [`MEASURE_MS`].
    pub fn trigger(&mut self) -> Result<(), I2C::Error> {
        self.i2c.write(self.addr, &CMD_MEASURE)
    }

    /// The measurement started last: the temperature in hundredths of a degree Celsius and the
    /// relative humidity in tenths of a percent.
    pub fn read(&mut self) -> Result<(i16, u16), Error<I2C::Error>> {
        let mut answer = [0u8; 6];
        self.i2c.read(self.addr, &mut answer).map_err(Error::Bus)?;
        codec::decode(&answer).map_err(Error::Data)
    }

    /// Release the bus.
    pub fn release(self) -> I2C {
        self.i2c
    }
}
