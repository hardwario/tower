//! The two checksums the module parts use.

/// Sensirion CRC-8 (polynomial x^8 + x^5 + x^4 + 1, 0x31). The SHT2x starts from `0x00`, the
/// SHT3x from `0xFF`.
pub fn crc8(data: &[u8], init: u8) -> u8 {
    let mut crc = init;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x31
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Modbus CRC-16 (start 0xFFFF, reflected polynomial 0xA001); on the wire low byte first. A frame
/// with its CRC appended checks to zero.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensirion_datasheet_vectors() {
        // SHT3x datasheet: 0xBEEF -> 0x92. SHT2x application note: 0x683A -> 0x7C.
        assert_eq!(crc8(&[0xBE, 0xEF], 0xFF), 0x92);
        assert_eq!(crc8(&[0x68, 0x3A], 0x00), 0x7C);
    }

    #[test]
    fn modbus_vectors() {
        // The LP8 guideline's frames: FE 41 00 80 01 10 28 7E, and the write acknowledgement.
        assert_eq!(
            crc16(&[0xFE, 0x41, 0x00, 0x80, 0x01, 0x10]).to_le_bytes(),
            [0x28, 0x7E]
        );
        assert_eq!(crc16(&[0xFE, 0x41]).to_le_bytes(), [0x81, 0xE0]);
        assert_eq!(crc16(&[0xFE, 0x41, 0x81, 0xE0]), 0);
    }
}
