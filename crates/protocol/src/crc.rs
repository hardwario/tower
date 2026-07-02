//! CRC-32/IEEE (reflected, poly `0xEDB8_8320`) — the single integrity primitive
//! shared by the console frame and the EEPROM key-value store.
//!
//! [`crc32_update`] is the raw running update (no init, no final XOR): fold one or
//! more slices into a running value. [`crc32_ieee`] is the one-shot (init
//! `0xFFFF_FFFF`, finalize `!`) used for a single contiguous buffer.
//!
//! `storage.rs`'s `entry_crc` folds two slices (`hdr ‖ value`) through
//! [`crc32_update`] with that same init/XOR — so moving the primitive here is
//! **byte-for-byte identical** to the firmware's previous private copy, and
//! existing EEPROM records stay valid.

/// One bitwise CRC-32 pass over `data`, continuing from `crc` (no table).
/// This is the raw update — apply the `0xFFFF_FFFF` init and final `!` yourself
/// (or use [`crc32_ieee`] for a one-shot).
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

/// One-shot CRC-32/IEEE over a single contiguous buffer (init `0xFFFF_FFFF`,
/// finalize `!`). Used for the console frame's integrity field.
pub fn crc32_ieee(data: &[u8]) -> u32 {
    !crc32_update(0xFFFF_FFFF, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical CRC-32/ISO-HDLC ("IEEE") check value: the CRC of the ASCII string
    /// "123456789" is 0xCBF43926. Pins the polynomial + reflection + init/XOR against the
    /// standard — a self-consistent round-trip test would pass for the *wrong* CRC too, so this
    /// reference vector is the guard that the primitive is exactly CRC-32/IEEE. `tower-kv`'s
    /// deployed EEPROM records depend on this being byte-for-byte stable.
    #[test]
    fn standard_check_value() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
    }

    /// CRC of empty input is the finalized init: `!0xFFFF_FFFF == 0`.
    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32_ieee(b""), 0x0000_0000);
    }

    /// Folding two slices through `crc32_update` (then finalizing) must equal the one-shot over
    /// the concatenation — the exact contract `storage.rs`'s `entry_crc` relies on to CRC
    /// `hdr ‖ value` as two calls.
    #[test]
    fn multi_slice_fold_matches_contiguous() {
        let a = b"hello, ";
        let b = b"tower";
        let folded = !crc32_update(crc32_update(0xFFFF_FFFF, a), b);
        let contiguous = crc32_ieee(b"hello, tower");
        assert_eq!(folded, contiguous);
    }
}
