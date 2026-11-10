//! Hardware-free cores of the TOWER module drivers.
//!
//! The firmware drivers (in the SDK: `tower::sht20`, `tower::sht30`, `tower::opt3001`,
//! `tower::mpl3115a2`, `tower::co2_module`, `tower::relay_module`) do the bus traffic and the
//! timing; what they compute lives here, unit-tested on the host against datasheet vectors:
//!
//! - [`sht20`], [`sht30`]: the humidity sensors of the Climate Module (first and second
//!   revision): the CRC and the conversions.
//! - [`opt3001`]: the ambient light sensor: its configuration words and the lux conversion.
//! - [`mpl3115a2`]: the barometer: the pressure and temperature conversions.
//! - [`lp8`]: the CO2 Module's Senseair LP8: the Modbus-like frames of one measurement, the
//!   reply's values and what its status bits mean for the reading.
//! - [`relay`]: the Relay Module's latching relay: the expander pin patterns of its H-bridge.
//!
//! Integer only (the Cortex-M0+ has no FPU), in the units of the property tables in
//! `tower_protocol::props`: humidity in tenths of a percent, illuminance in tenths of a lux,
//! pressure in tenths of a hectopascal, temperature in hundredths of a degree Celsius.

#![no_std]

pub mod crc;
pub mod lp8;
pub mod mpl3115a2;
pub mod opt3001;
pub mod relay;
pub mod sht20;
pub mod sht30;

/// Data read from a part that cannot be what the part sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// The checksum does not match.
    Crc,
    /// Well-formed, but not the answer that was asked for.
    Unexpected,
    /// Fewer bytes than the answer has.
    Short,
}

impl core::fmt::Display for Invalid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Invalid::Crc => "CRC mismatch",
            Invalid::Unexpected => "unexpected data",
            Invalid::Short => "short data",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Invalid;

    /// The texts a driver's error ends in (a node logs `CO2 not measured: LP8 frame: CRC
    /// mismatch`).
    #[test]
    fn invalid_data_says_what_is_wrong() {
        extern crate std;
        use std::string::ToString;
        assert_eq!(Invalid::Crc.to_string(), "CRC mismatch");
        assert_eq!(Invalid::Unexpected.to_string(), "unexpected data");
        assert_eq!(Invalid::Short.to_string(), "short data");
    }
}
