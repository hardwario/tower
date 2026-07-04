//! STM32L0 family target constants and helpers for the UART bootloader.
//!
//! These cover the STM32L0x1/L0x2/L0x3 line. The flash *size* varies across the
//! family (16–192 KiB), and the bootloader can't report it (Read Memory of the
//! factory region is rejected), so a full-chip erase walks up to the family
//! maximum and stops when the device NACKs an out-of-range page. The rest of
//! the geometry — 128-byte pages, the 0x0800_0000 base, 8-byte write alignment
//! — is uniform across STM32L0.

/// UART bootloader baud rate. 115200 is the maximum specified by ST for the
/// USART bootloader (AN3155 / AN2606); the auto-baud detection tops out here.
pub const BAUD: u32 = 115_200;

/// Main flash memory base address — where firmware is written.
pub const FLASH_BASE: u32 = 0x0800_0000;

/// Largest flash in the STM32L0 family (the L07x/L08x density, 192 KiB). Used
/// as a coarse firmware bounds check and as the upper bound a full-chip erase
/// walks toward before the device NACKs the first out-of-range page.
pub const MAX_FLASH_SIZE: u32 = 192 * 1024;

/// Maximum payload per Write/Read Memory command.
pub const MAX_BLOCK: usize = 256;

/// Flash write alignment required by the L0/L1 bootloader (AN2606 Table 7).
pub const WRITE_ALIGN: usize = 8;

/// Flash page size on the STM32L0, uniform across the family (used for Extended
/// Erase page numbering).
pub const PAGE_SIZE: usize = 128;

/// Maximum pages per Extended Erase command.
pub const ERASE_CHUNK: usize = 80;

/// Number of 128-byte flash pages spanning `bytes` of flash.
pub const fn pages_in(bytes: u32) -> usize {
    bytes as usize / PAGE_SIZE
}

/// The flash densities shipped across the STM32L0 family, in 128-byte pages
/// (16 / 32 / 64 / 128 / 192 KiB). The UART bootloader can't report the exact
/// flash size, so a full-chip erase walks toward [`MAX_FLASH_SIZE`] and stops
/// when the device NACKs an out-of-range page — but the bootloader returns the
/// *same* payload NACK for a write-protected (WRP) page. A truncated erase can
/// therefore be told apart from a genuine end-of-flash only by checking that the
/// stop point lands on one of these real densities.
pub const FLASH_DENSITIES_PAGES: [usize; 5] = [
    pages_in(16 * 1024),
    pages_in(32 * 1024),
    pages_in(64 * 1024),
    pages_in(128 * 1024),
    pages_in(192 * 1024),
];

/// Whether `pages` equals a real STM32L0 flash density (see
/// [`FLASH_DENSITIES_PAGES`]). A full-chip erase that stops on any other page
/// count was truncated by something other than the end of flash.
pub fn is_valid_density_pages(pages: usize) -> bool {
    FLASH_DENSITIES_PAGES.contains(&pages)
}

/// Maximum flash size in bytes for a recognized STM32L0 product id (`Get ID`),
/// or `None` if the id isn't recognized. The bootloader can't report the flash
/// size, but the product id pins the family, which bounds it: Cat-1 (0x457)
/// 16 KiB, Cat-2 (0x425) 32 KiB, Cat-3 (0x417) 64 KiB, Cat-5 (0x447) 192 KiB.
/// Used to sanity-check a discovered erase boundary against the identified chip.
pub fn max_flash_size(id: u16) -> Option<u32> {
    Some(match id {
        0x457 => 16 * 1024,
        0x425 => 32 * 1024,
        0x417 => 64 * 1024,
        0x447 => 192 * 1024,
        _ => return None,
    })
}

/// Friendly name for an STM32L0 bootloader chip id (`Get ID` value). These are
/// the STM32 product ids sent over UART — unrelated to any USB-bridge PID.
pub fn chip_name(id: u16) -> &'static str {
    match id {
        0x457 => "STM32L01x/L02x",
        0x425 => "STM32L031/L041",
        0x417 => "STM32L05x/L06x",
        0x447 => "STM32L07x/L08x (e.g. STM32L083CZ)",
        _ => "unknown",
    }
}

/// Whether `id` is a recognized STM32L0-family product id (see [`chip_name`]).
pub fn is_known(id: u16) -> bool {
    chip_name(id) != "unknown"
}
