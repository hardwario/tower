//! Sensirion SHT30 (the Climate Module's second revision): the answer to a single-shot
//! measurement and its conversions.

use crate::Invalid;
use crate::crc::crc8;

/// An answer's temperature (hundredths of a degree Celsius) and relative humidity (tenths of a
/// percent): `[T msb, T lsb, T crc, RH msb, RH lsb, RH crc]`, each word with its own CRC.
pub fn decode(answer: &[u8; 6]) -> Result<(i16, u16), Invalid> {
    if crc8(&answer[..2], 0xFF) != answer[2] || crc8(&answer[3..5], 0xFF) != answer[5] {
        return Err(Invalid::Crc);
    }
    let t = u16::from_be_bytes([answer[0], answer[1]]);
    let rh = u16::from_be_bytes([answer[3], answer[4]]);
    Ok((temperature_centi(t), humidity_deci(rh)))
}

/// Temperature in hundredths of a degree Celsius: `-45 + 175 * raw / (2^16 - 1)`.
pub fn temperature_centi(raw: u16) -> i16 {
    ((17500 * i32::from(raw) + 32767) / 65535 - 4500) as i16
}

/// Relative humidity in tenths of a percent: `100 * raw / (2^16 - 1)`.
pub fn humidity_deci(raw: u16) -> u16 {
    ((1000 * u32::from(raw) + 32767) / 65535) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(t: u16, rh: u16) -> [u8; 6] {
        let (t, rh) = (t.to_be_bytes(), rh.to_be_bytes());
        [t[0], t[1], crc8(&t, 0xFF), rh[0], rh[1], crc8(&rh, 0xFF)]
    }

    #[test]
    fn conversions() {
        assert_eq!(temperature_centi(0x6666), 2500);
        assert_eq!(temperature_centi(0), -4500);
        assert_eq!(temperature_centi(0xFFFF), 13000);
        assert_eq!(humidity_deci(0x8000), 500);
        assert_eq!(humidity_deci(0), 0);
        assert_eq!(humidity_deci(0xFFFF), 1000);
    }

    #[test]
    fn decodes_and_checks_both_crcs() {
        assert_eq!(decode(&answer(0x6666, 0x8000)), Ok((2500, 500)));
        let mut a = answer(0x6666, 0x8000);
        a[4] ^= 1;
        assert_eq!(decode(&a), Err(Invalid::Crc));
        let mut a = answer(0x6666, 0x8000);
        a[0] ^= 1;
        assert_eq!(decode(&a), Err(Invalid::Crc));
    }
}
