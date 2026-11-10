//! NXP MPL3115A2 barometer (the Climate Module, the Barometer Tag): its control values and the
//! pressure and temperature conversions.

/// `WHO_AM_I`'s value.
pub const WHO_AM_I: u8 = 0xC4;

/// How many samples one conversion averages, as the `CTRL_REG1` oversampling field: more samples
/// are less noise for more charge. One-shot conversions from standby (2 µA) cost, per the
/// datasheet's 1 Hz figures, about 6.5 µC at 1, 38 µC at 16 and 263 µC at 128 samples, for a
/// noise of about 19, 5 and 1.5 Pa RMS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Oversampling {
    X1 = 0,
    X2 = 1,
    X4 = 2,
    X8 = 3,
    X16 = 4,
    X32 = 5,
    X64 = 6,
    X128 = 7,
}

impl Oversampling {
    /// `CTRL_REG1` for a one-shot barometer conversion from standby (`OST` set, `SBYB` and `ALT`
    /// clear) at this oversampling.
    pub const fn one_shot(self) -> u8 {
        ((self as u8) << 3) | 0x02
    }

    /// `CTRL_REG1` for standby at this oversampling (what the register reads once the one-shot
    /// bit has cleared itself).
    pub const fn standby(self) -> u8 {
        (self as u8) << 3
    }

    /// The conversion time in milliseconds (datasheet: `2^n * 4 + 2`), without margin.
    pub const fn conversion_ms(self) -> u32 {
        (4 << (self as u32)) + 2
    }
}

/// Pressure in tenths of a hectopascal (10 Pa) from `OUT_P_MSB..OUT_P_LSB`: an unsigned 18.2
/// fixed-point number of pascals in the upper 20 bits.
pub fn pressure_deci_hpa(out_p: [u8; 3]) -> u16 {
    let quarter_pa = (u32::from(out_p[0]) << 16 | u32::from(out_p[1]) << 8 | u32::from(out_p[2])) >> 4;
    ((quarter_pa + 20) / 40) as u16
}

/// Temperature in hundredths of a degree Celsius from `OUT_T_MSB`, `OUT_T_LSB`: a signed 8.4
/// fixed-point number in the upper 12 bits.
pub fn temperature_centi(out_t: [u8; 2]) -> i16 {
    let sixteenths = i32::from(i16::from_be_bytes(out_t) >> 4);
    (sixteenths * 100 / 16) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_vectors() {
        // 0x630580 >> 4 = 405592 quarter pascals = 101398 Pa = 1013.98 hPa.
        assert_eq!(pressure_deci_hpa([0x63, 0x05, 0x80]), 10140);
        // 98 000 Pa.
        let q = (98_000u32 * 4) << 4;
        assert_eq!(
            pressure_deci_hpa([(q >> 16) as u8, (q >> 8) as u8, q as u8]),
            9800
        );
        assert_eq!(pressure_deci_hpa([0, 0, 0]), 0);
        // The top of the 20-bit field, 262143.75 Pa, still fits.
        assert_eq!(pressure_deci_hpa([0xFF, 0xFF, 0xF0]), 26214);
    }

    #[test]
    fn temperature_vectors() {
        assert_eq!(temperature_centi([0x17, 0x80]), 2350);
        assert_eq!(temperature_centi([0xFF, 0xF0]), -6);
        assert_eq!(temperature_centi([0x00, 0x00]), 0);
    }

    #[test]
    fn control_values() {
        assert_eq!(Oversampling::X128.one_shot(), 0x3A);
        assert_eq!(Oversampling::X128.standby(), 0x38);
        assert_eq!(Oversampling::X16.one_shot(), 0x22);
        assert_eq!(Oversampling::X1.conversion_ms(), 6);
        assert_eq!(Oversampling::X16.conversion_ms(), 66);
        assert_eq!(Oversampling::X128.conversion_ms(), 514);
    }
}
