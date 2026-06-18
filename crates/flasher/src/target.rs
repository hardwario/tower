//! STM32L0 target constants (STM32L083CZ) for the UART bootloader.

/// UART bootloader baud rate. 115200 is the maximum specified by ST for the
/// USART bootloader (AN3155 / AN2606); the auto-baud detection tops out here.
pub const BAUD: u32 = 115_200;

/// Main flash memory base address — where firmware is written.
pub const FLASH_BASE: u32 = 0x0800_0000;

/// Main flash size on the STM32L083CZ (192 KiB).
pub const FLASH_SIZE: u32 = 192 * 1024;

/// Expected bootloader chip id reported by the `Get ID` command for STM32L0x3.
/// This is the STM32 product id sent over UART — unrelated to the FT231X USB PID.
pub const EXPECTED_CHIP_ID: u16 = 0x447;

/// Maximum payload per Write/Read Memory command.
pub const MAX_BLOCK: usize = 256;

/// Flash write alignment required by the L0/L1 bootloader (AN2606 Table 7).
pub const WRITE_ALIGN: usize = 8;

/// Flash page size on the STM32L0 (used for Extended Erase page numbering).
pub const PAGE_SIZE: usize = 128;

/// Total flash pages (192 KiB / 128 B) — full-chip erase covers 0..PAGE_COUNT.
pub const PAGE_COUNT: usize = FLASH_SIZE as usize / PAGE_SIZE;

/// Maximum pages per Extended Erase command.
pub const ERASE_CHUNK: usize = 80;

/// Friendly name for an STM32L0 bootloader chip id (`Get ID` value).
pub fn chip_name(id: u16) -> &'static str {
    match id {
        0x457 => "STM32L01x/L02x",
        0x425 => "STM32L031/L041",
        0x417 => "STM32L05x/L06x",
        0x447 => "STM32L07x/L08x (e.g. STM32L083CZ)",
        _ => "unknown",
    }
}
