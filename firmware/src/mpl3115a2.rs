//! NXP MPL3115A2 barometer driver (the Climate Module, the Barometer Tag), HAL-independent over
//! [`embedded_hal::i2c::I2c`].
//!
//! One-shot: the part stays in standby (2 µA, its state after power-up), converts once when told
//! to and returns to standby by itself. The oversampling ([`Oversampling`]) trades noise for
//! charge: [`OVERSAMPLING`] is the SDK's choice. The control values and the arithmetic are
//! [`tower_modules::mpl3115a2`]'s.

use embedded_hal::i2c::I2c;
use tower_modules::mpl3115a2 as codec;
pub use tower_modules::mpl3115a2::Oversampling;

/// The MPL3115A2's only address.
pub const ADDR: u8 = 0x60;

/// 16 samples per conversion: about 5 Pa RMS of noise (the reported unit is 10 Pa) for 66 ms and
/// about 38 µC, a seventh of the charge of the part's best resolution.
pub const OVERSAMPLING: Oversampling = Oversampling::X16;

const REG_STATUS: u8 = 0x00;
const REG_OUT_P_MSB: u8 = 0x01;
const REG_WHO_AM_I: u8 = 0x0C;
const REG_PT_DATA_CFG: u8 = 0x13;
const REG_CTRL_REG1: u8 = 0x26;

/// Raise the data-ready flags for pressure and temperature.
const PT_DATA_CFG_ALL: u8 = 0x07;
/// `STATUS`: new pressure data.
const STATUS_PDR: u8 = 0x04;

/// An MPL3115A2 on an I²C bus.
pub struct Mpl3115a2<I2C> {
    i2c: I2C,
}

impl<I2C: I2c> Mpl3115a2<I2C> {
    pub fn new(i2c: I2C) -> Self {
        Self { i2c }
    }

    /// Whether an MPL3115A2 answers, by its `WHO_AM_I`.
    pub fn present(&mut self) -> bool {
        let mut id = [0u8];
        self.i2c.write_read(ADDR, &[REG_WHO_AM_I], &mut id).is_ok() && id[0] == codec::WHO_AM_I
    }

    /// Standby in barometer mode with the data-ready flags on. Once after power-up; the part
    /// keeps it while powered.
    pub fn init(&mut self, oversampling: Oversampling) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[REG_CTRL_REG1, oversampling.standby()])?;
        self.i2c.write(ADDR, &[REG_PT_DATA_CFG, PT_DATA_CFG_ALL])
    }

    /// Start one conversion; it takes [`Oversampling::conversion_ms`] (check with
    /// [`ready`](Self::ready)).
    pub fn trigger(&mut self, oversampling: Oversampling) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[REG_CTRL_REG1, oversampling.one_shot()])
    }

    /// Whether the conversion started last has new pressure data.
    pub fn ready(&mut self) -> Result<bool, I2C::Error> {
        let mut status = [0u8];
        self.i2c.write_read(ADDR, &[REG_STATUS], &mut status)?;
        Ok(status[0] & STATUS_PDR != 0)
    }

    /// The last conversion: the pressure in tenths of a hectopascal and the part's temperature in
    /// hundredths of a degree Celsius. Reading clears the data-ready flags.
    pub fn read(&mut self) -> Result<(u16, i16), I2C::Error> {
        let mut out = [0u8; 5];
        self.i2c.write_read(ADDR, &[REG_OUT_P_MSB], &mut out)?;
        Ok((
            codec::pressure_deci_hpa([out[0], out[1], out[2]]),
            codec::temperature_centi([out[3], out[4]]),
        ))
    }

    /// Release the bus.
    pub fn release(self) -> I2C {
        self.i2c
    }
}
