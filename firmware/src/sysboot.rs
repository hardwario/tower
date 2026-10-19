//! The STM32's ROM bootloader (the system memory) and the application: getting into it from the
//! application on the rev 1.3 board, and starting cleanly when it jumps back to us.
//!
//! **Into the bootloader (rev 1.3).** The Core Module r2 enters the ROM bootloader through its
//! USB-UART bridge: the host pulses NRST with BOOT0 held high (`tower flash`,
//! `crates/flasher/src/port.rs`). The rev 1.3 board has no bridge, so its console reproduces that
//! pulse in firmware (`console::usb`, "reset emulation") and comes here for the BOOT0-high case.
//! Neither of the two obvious ways works on its own there, both seen on a rev 1.3 board:
//! * raising BOOT0 from the firmware: it is reachable (the BOOT button's net is tied to PA8
//!   through R3), but a software reset holds the chip in reset for milliseconds (NRST recharges
//!   C1, 100 nF, through the internal pull-up), and with PA8 floating from the start of the reset,
//!   BOOT0 is long low when it is latched;
//! * jumping to the bootloader (AN2606, "jump to system memory"): the STM32L07x/L08x bootloader
//!   first runs its dual-bank boot check and jumps straight back to valid code in bank 1 (it
//!   left the bus within ~100 ms and the application came back). That check is AN2606's (dual
//!   bank boot of the STM32L07xxx/08xxx), and the community's working method is to make bank 1
//!   invalid first.
//!
//! So `reboot_into_bootloader` (rev 1.3 builds only) files a request in reset-surviving RAM (the
//! [`bootguard`](crate::bootguard) pattern) and resets, and the next boot, from `on_boot`
//! before any clock or peripheral is set up, **erases the first page of each flash bank** (so
//! neither starts with a valid stack pointer) and jumps to the bootloader. That costs the
//! application: until it is flashed again, the board boots nowhere but the bootloader. It is only
//! used on the way to a flash (`tower flash`, `tower erase`, `tower reset --bootloader`), whose
//! erase covers those pages anyway, and the flasher writes the vector table last, so an
//! interrupted flash leaves the same state. The jump: booting from system memory maps it at
//! address 0 (`SYSCFG_CFGR1.MEM_MODE = 01`, RM0367 "Boot configuration"), VTOR goes back to 0 (our
//! reset handler pointed it at our table, see the `set-vtor` feature in Cargo.toml), and the
//! bootloader's initial stack pointer and reset vector come from `0x1FF0_0000`.
//!
//! **Started by a bootloader jump.** The other way round, the ROM bootloader starts a freshly
//! flashed application by *jumping* to it, not through a reset: the USB DFU "leave" request
//! (AN3156 rev 19, 5.5 "Leave DFU mode") and the UART bootloader's `Go` (AN3155). AN3156 5.5
//! warns that after a jump from the bootloader's USB code the application "must disable all
//! pending USB interrupts and reset the core before enabling interrupts", or an interrupt the
//! bootloader left pending interferes with it; and the jump only works with the vector table set
//! to the application (the `set-vtor` feature in Cargo.toml). So the very first code after the
//! reset vector, `__pre_init` below (cortex-m-rt calls it right after setting VTOR, before RAM is
//! initialized), masks interrupts and looks for traces only the bootloader leaves: the system
//! clock off MSI, the USB or a USART clock already enabled (all at their reset values after any
//! reset). If it finds one, it resets: BOOT0 is low again by then, so the application restarts
//! from a clean reset. It runs that early because an interrupt the bootloader left enabled, taken
//! before any later check, would land in cortex-m-rt's default handler, which spins forever. It
//! is assembly because no Rust may run before RAM is initialized. This is the same on both boards
//! (the r2 board's `tower flash --go` is the AN3155 jump).

#[cfg(feature = "core-module-r1")]
use core::ptr::addr_of_mut;

#[cfg(feature = "core-module-r1")]
use cortex_m::peripheral::SCB;

/// Base of the STM32L0 system memory (the ROM bootloader): its vector table.
#[cfg(feature = "core-module-r1")]
const SYSTEM_MEMORY: u32 = 0x1FF0_0000;
/// `SYSCFG_CFGR1.MEM_MODE` value that maps the system memory at address 0 (RM0367).
#[cfg(feature = "core-module-r1")]
const MEM_MODE_SYSTEM_FLASH: u8 = 0b01;
/// First pages of the two flash banks of the STM32L083CZ (192 KiB: bank 1 at `0x0800_0000`,
/// bank 2 at `0x0801_8000`), where the dual-bank boot check looks for a stack pointer.
#[cfg(feature = "core-module-r1")]
const BANK_STARTS: [u32; 2] = [0x0800_0000, 0x0801_8000];
/// Marks [`REQUEST`] as a live request (vs cold-boot RAM garbage).
#[cfg(feature = "core-module-r1")]
const MAGIC: u32 = 0xB007_10AD;

/// The request word, in `.uninit` so it survives the warm reset of [`reboot_into_bootloader`]
/// (the initializer is never applied: the section is NOLOAD).
#[cfg(feature = "core-module-r1")]
#[unsafe(link_section = ".uninit.TOWER_SYSBOOT")]
static mut REQUEST: u32 = 0;

/// Reset into the ROM bootloader, its USB DFU device (`0483:df11`) on the rev 1.3 board, the one
/// `tower flash` then programs. **Erases the start of the application** on the way (see the
/// module docs). Does not return.
#[cfg(feature = "core-module-r1")]
pub fn reboot_into_bootloader() -> ! {
    // SAFETY: a plain volatile store to a static word nothing else holds a reference to.
    unsafe { addr_of_mut!(REQUEST).write_volatile(MAGIC) };
    SCB::sys_reset()
}

/// First thing [`board::init`](crate::board::init) does on the rev 1.3 board: if the previous
/// boot asked for the bootloader ([`reboot_into_bootloader`]), make both flash banks invalid and
/// jump to it, from the reset state (no clock, peripheral or interrupt of ours exists yet).
#[cfg(feature = "core-module-r1")]
pub(crate) fn on_boot() {
    use embassy_stm32::pac::{RCC, SYSCFG};

    // SAFETY: volatile accesses to a static word nothing else holds a reference to.
    if unsafe { addr_of_mut!(REQUEST).read_volatile() } != MAGIC {
        return;
    }
    // Cleared first: whatever happens next (the bootloader's own resets, its jump into a freshly
    // flashed application) must start the application, not come back here.
    unsafe { addr_of_mut!(REQUEST).write_volatile(0) };
    for page in BANK_STARTS {
        erase_page(page);
    }
    RCC.apb2enr().modify(|w| w.set_syscfgen(true));
    SYSCFG.cfgr1().modify(|w| w.set_mem_mode(MEM_MODE_SYSTEM_FLASH));
    // SAFETY: nothing but this boot path runs yet (no interrupt is enabled), and the jump never
    // returns: VTOR goes back to 0 (the bootloader's table through the mapping above, the state a
    // BOOT0 reset leaves), then `bootload` loads the initial stack pointer and the reset vector
    // from that table and branches.
    unsafe {
        (*SCB::PTR).vtor.write(0);
        cortex_m::asm::bootload(SYSTEM_MEMORY as *const u32)
    }
}

/// Erase the flash page at `addr` (RM0367, "Page erase"): unlock the PECR and the program memory
/// with their key pairs, set ERASE + PROG, write a word into the page, wait out BSY, then lock
/// again. Runs before the clocks are set up (the flash interface's clock is on after reset).
#[cfg(feature = "core-module-r1")]
fn erase_page(addr: u32) {
    use embassy_stm32::pac::FLASH;

    if FLASH.pecr().read().pelock() {
        FLASH.pekeyr().write_value(0x89AB_CDEF);
        FLASH.pekeyr().write_value(0x0203_0405);
    }
    if FLASH.pecr().read().prglock() {
        FLASH.prgkeyr().write_value(0x8C9D_AEBF);
        FLASH.prgkeyr().write_value(0x1314_1516);
    }
    FLASH.pecr().modify(|w| {
        w.set_erase(true);
        w.set_prog(true);
    });
    // SAFETY: a word write into the page, which with ERASE + PROG set starts its erase.
    unsafe { (addr as *mut u32).write_volatile(0) };
    while FLASH.sr().read().bsy() {}
    FLASH.pecr().modify(|w| {
        w.set_erase(false);
        w.set_prog(false);
    });
    FLASH.pecr().modify(|w| w.set_pelock(true));
}

// `__pre_init`: restart through a reset when a ROM bootloader jumped here (see the module docs).
// After any reset the system clock is MSI and every peripheral clock is off; the bootloader runs
// on HSI16 and enables the clock of the interface it served (USB for DFU, USART1 or USART2 for
// AN3155). RM0367 (STM32L0x3) RCC registers, base 0x4002_1000: RCC_CFGR at +0x0C (SWS, the system
// clock switch status, bits 3:2, 00 = MSI), RCC_APB2ENR at +0x34 (USART1EN bit 14), RCC_APB1ENR
// at +0x38 (USART2EN bit 17, USBEN bit 23); the reset is SCB AIRCR = VECTKEY | SYSRESETREQ.
// Interrupts are masked first (the bootloader's may still be enabled and pending) and unmasked
// again only on the reset path, where PRIMASK is 0 as the hardware left it.
core::arch::global_asm!(
    ".section .text.__pre_init,\"ax\",%progbits",
    ".global __pre_init",
    ".type __pre_init,%function",
    ".thumb_func",
    "__pre_init:",
    "cpsid i",
    "ldr r0, =0x40021000",
    "ldr r1, [r0, #0x0C]",
    "movs r2, #0x0C",
    "tst r1, r2",
    "bne 1f",
    "ldr r1, [r0, #0x38]",
    "ldr r2, =0x00820000",
    "tst r1, r2",
    "bne 1f",
    "ldr r1, [r0, #0x34]",
    "ldr r2, =0x00004000",
    "tst r1, r2",
    "bne 1f",
    "cpsie i",
    "bx lr",
    "1:",
    "dsb",
    "ldr r0, =0xE000ED0C",
    "ldr r1, =0x05FA0004",
    "str r1, [r0]",
    "dsb",
    "2:",
    "b 2b",
    ".ltorg",
    ".size __pre_init, . - __pre_init",
);
