//! Sensirion SHT20 (the Climate Module's first revision, the Humidity Tag): the answer to a
//! "no hold master" measurement and its conversions.
//!
//! The sensor answers a measurement with two data bytes and a CRC. The two low bits of the data
//! are status: bit 1 says which measurement it is (1 = humidity), so a stale answer of the other
//! kind is recognised.

use crate::Invalid;
use crate::crc::crc8;

/// Check an answer's CRC and that it is the measurement asked for (`humidity` or temperature);
/// the raw value with the status bits cleared.
pub fn decode(answer: &[u8; 3], humidity: bool) -> Result<u16, Invalid> {
    if crc8(&answer[..2], 0x00) != answer[2] {
        return Err(Invalid::Crc);
    }
    if (answer[1] & 0x02 != 0) != humidity {
        return Err(Invalid::Unexpected);
    }
    Ok(u16::from_be_bytes([answer[0], answer[1]]) & !0x03)
}

/// Relative humidity in tenths of a percent: `-6 + 125 * raw / 2^16`, held to 0..=100 % (the
/// formula runs a little past both ends by design).
pub fn humidity_deci(raw: u16) -> u16 {
    let deci = ((1250 * i32::from(raw) + 0x8000) >> 16) - 60;
    deci.clamp(0, 1000) as u16
}

/// Temperature in hundredths of a degree Celsius: `-46.85 + 175.72 * raw / 2^16`.
pub fn temperature_centi(raw: u16) -> i16 {
    (((17572 * i32::from(raw) + 0x8000) >> 16) - 4685) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(data: u16) -> [u8; 3] {
        let b = data.to_be_bytes();
        [b[0], b[1], crc8(&b, 0x00)]
    }

    #[test]
    fn datasheet_humidity_example() {
        // SHT2x datasheet: 0x6350 with the humidity status bit set reads 42.5 %RH.
        let raw = decode(&answer(0x6352), true).unwrap();
        assert_eq!(raw, 0x6350);
        assert_eq!(humidity_deci(raw), 425);
    }

    #[test]
    fn datasheet_temperature_example() {
        // SHT2x datasheet: 0x6650 reads about 23.3 °C (23.38 by the formula).
        let raw = decode(&answer(0x6650), false).unwrap();
        assert_eq!(temperature_centi(raw), 2338);
        assert_eq!(temperature_centi(0), -4685);
    }

    #[test]
    fn the_other_measurement_is_refused() {
        assert_eq!(decode(&answer(0x6350), true), Err(Invalid::Unexpected));
        assert_eq!(decode(&answer(0x6352), false), Err(Invalid::Unexpected));
    }

    #[test]
    fn a_bad_crc_is_refused() {
        let mut a = answer(0x6352);
        a[2] ^= 1;
        assert_eq!(decode(&a, true), Err(Invalid::Crc));
        // An idle bus reads as all ones: not a valid answer either.
        assert_eq!(decode(&[0xFF, 0xFF, 0xFF], true), Err(Invalid::Crc));
    }

    #[test]
    fn humidity_stays_within_0_and_100() {
        assert_eq!(humidity_deci(0), 0);
        assert_eq!(humidity_deci(0xFFFC), 1000);
        // 0x0C4C is the first raw value above 0 %: -6 + 125 * 3148 / 65536 = 0.004.
        assert_eq!(humidity_deci(0x0C4C), 0);
        assert_eq!(humidity_deci(0x8000), 565);
    }
}
