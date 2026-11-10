//! Texas Instruments OPT3001 ambient light sensor (the Climate Module, the Lux Meter Tag): its
//! configuration words and the lux conversion.

/// The device ID register's value.
pub const DEVICE_ID: u16 = 0x3001;

/// Configuration: automatic full-scale range, 800 ms conversion, shutdown, latched comparison
/// (the reset value, `0xC810`). The part draws 0.3 µA typical in shutdown.
pub const CONFIG_SHUTDOWN: u16 = 0xC810;
/// The same with one conversion started: the part converts once (1.8 µA typical meanwhile) and
/// returns to shutdown by itself.
pub const CONFIG_SINGLE_SHOT: u16 = 0xCA10;

/// The conversion time: a 10 ms range assessment and the 800 ms conversion, with margin.
pub const CONVERSION_MS: u32 = 880;

/// Whether a single-shot conversion has finished: the mode field is back at shutdown and the
/// conversion-ready flag is set (reading the configuration register clears the flag).
pub const fn ready(config: u16) -> bool {
    config & 0x0600 == 0 && config & 0x0080 != 0
}

/// Illuminance in tenths of a lux from the result register: `0.01 * 2^E * M` lux, with the
/// exponent `E` in the top four bits (0..=11) and the mantissa `M` in the low twelve. The full
/// range is 0.01 to 83865.6 lux; the two lowest ranges resolve less than a tenth, rounded here.
pub fn deci_lux(result: u16) -> u32 {
    let exponent = u32::from(result >> 12);
    let mantissa = u32::from(result & 0x0FFF);
    ((mantissa << exponent) + 5) / 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datasheet_vectors() {
        // 0x6542: 861.44 lux; 0x0FFF: 40.95 lux; 0xBFFF: 83865.6 lux, the top of the range.
        assert_eq!(deci_lux(0x6542), 8614);
        assert_eq!(deci_lux(0x0FFF), 410);
        assert_eq!(deci_lux(0xBFFF), 838_656);
        assert_eq!(deci_lux(0x0000), 0);
        // 0.04 lux rounds to nothing, 0.05 to a tenth.
        assert_eq!(deci_lux(0x0004), 0);
        assert_eq!(deci_lux(0x0005), 1);
    }

    #[test]
    fn ready_needs_shutdown_and_the_flag() {
        assert!(!ready(CONFIG_SINGLE_SHOT), "still converting");
        assert!(!ready(CONFIG_SHUTDOWN), "shut down, nothing converted");
        assert!(ready(CONFIG_SHUTDOWN | 0x0080));
        assert!(!ready(CONFIG_SINGLE_SHOT | 0x0080));
    }
}
