//! Error types for the bootloader / serial layers.
//!
//! Every layer returns this typed `Error` (no `anyhow` in the library), so retry
//! logic can tell a NACK from a timeout and a front end can add its own context.

use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Which phase of a two-step command handshake produced a NACK. AN3155 command
/// frames are acknowledged in two stages (first the command byte + complement,
/// then the command's payload), and the two mean very different things (a
/// command-byte NACK is usually line corruption, or a command the active readout
/// protection forbids; a payload NACK is the device rejecting the request, e.g. an
/// out-of-range Extended Erase page list). Retry and boundary-detection logic must
/// be able to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NackStage {
    /// NACK to the command byte + complement frame (before any payload).
    Command,
    /// NACK to the command's payload (address, length, page list, data).
    Payload,
    /// NACK in place of the second ACK of Readout Protect / Readout Unprotect, which the
    /// bootloader sends only after carrying out the option-byte update (AN3155): the update
    /// failed, so the protection level did not change.
    Operation,
}

impl std::fmt::Display for NackStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NackStage::Command => f.write_str("command"),
            NackStage::Payload => f.write_str("payload"),
            NackStage::Operation => f.write_str("operation"),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serial port error: {0}")]
    Serial(#[from] serialport::Error),

    #[error("bootloader replied NACK (0x1F) to {context} ({stage} stage)")]
    Nack { context: String, stage: NackStage },

    #[error("timed out waiting for {context}")]
    Timeout { context: String },

    #[error("unexpected byte while reading {context}: got 0x{got:02X}, expected 0x{expected:02X}")]
    UnexpectedByte { context: String, got: u8, expected: u8 },

    #[error(
        "could not enter the bootloader after {attempts} attempt(s) (no ACK to 0x7F init); check the --device and the cable"
    )]
    BootloaderInit { attempts: u32 },

    #[error("verify mismatch at 0x{address:08X}: flash=0x{got:02X}, firmware=0x{expected:02X}")]
    VerifyMismatch { address: u32, got: u8, expected: u8 },

    #[error(
        "erase truncated after {erased} page(s) ({kib} KiB) on chip 0x{id:03X}: not a valid STM32L0 flash density; the remaining pages are likely write-protected, so the device is only partly erased"
    )]
    PartialErase { erased: usize, kib: usize, id: u16 },

    #[error("invalid argument: {context}")]
    InvalidArgument { context: &'static str },

    /// The image exceeds a flash bound: either the STM32L0 family maximum
    /// ([`target::MAX_FLASH_SIZE`](crate::target::MAX_FLASH_SIZE), checked before connecting)
    /// or the identified chip's flash ([`target::max_flash_size`](crate::target::max_flash_size),
    /// checked right after `Get ID`, before any page is erased).
    #[error(
        "firmware is {size} bytes, exceeding the {max} byte ({max_kib} KiB) {}",
        flash_limit_desc(.max)
    )]
    FirmwareTooLarge { size: usize, max: u32, max_kib: u32 },

    /// The file is an ELF or Intel HEX image (detected by extension *or* by content), not
    /// the raw binary written at the flash base.
    #[error(
        "{path} looks like a .{ext} file; only raw binaries can be flashed. \
         Convert it first, e.g.: arm-none-eabi-objcopy -O binary in.{ext} out.bin \
         (from a Cargo project: cargo objcopy --release -- -O binary out.bin)"
    )]
    FirmwareFormat { path: PathBuf, ext: String },

    #[error("firmware file {path} is empty")]
    FirmwareEmpty { path: PathBuf },

    #[error("serial port disconnected while {context}")]
    Disconnected { context: &'static str },

    /// A USB control transfer to the DFU bootloader ([`dfu`](crate::dfu)) failed below the
    /// protocol: the device went away, a timeout, an OS error (a Linux user without access to
    /// the device gets `PermissionDenied` here). A request the bootloader refuses is
    /// [`UsbStall`](Self::UsbStall) on the wire and [`Dfu`](Self::Dfu) once its status is read.
    #[error("USB error ({context}): {source}")]
    Usb {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// The DFU bootloader stalled `request`: its way of refusing one, whose reason the next
    /// `DFU_GETSTATUS` tells. The [`dfu`](crate::dfu) engine reads it and reports
    /// [`Dfu`](Self::Dfu) instead, so this surfaces only from a transport.
    #[error("the DFU bootloader stalled {request}")]
    UsbStall { request: &'static str },

    /// The DFU bootloader refused `context` or reported it failed: `status` and `state` are the
    /// DFU 1.1 `bStatus` / `bState` codes of its `DFU_GETSTATUS` answer.
    #[error(
        "the DFU bootloader refused {context}: {} (state {})",
        crate::dfu::status_name(*.status),
        crate::dfu::state_name(*.state)
    )]
    Dfu {
        context: &'static str,
        status: u8,
        state: u8,
    },

    /// No usable STM32 DFU bootloader: none attached, several, or one whose interface or memory
    /// layout is not the STM32L0 main flash this crate writes.
    #[error("{reason}")]
    DfuDevice { reason: String },

    /// The chip has readout protection (RDP level 1) active, found by the probe in
    /// [`rdp`](crate::rdp) right after `Get ID` and before anything was modified. At level 1
    /// the ROM bootloader NACKs Read Memory, Write Memory, Go and both Erase commands
    /// (AN3155), so `operation` cannot work; reported as its own variant instead of the generic
    /// command NACK the first refused command would produce. The only way back is Readout
    /// Unprotect ([`rdp::unprotect`](crate::rdp::unprotect)), which mass-erases the flash AND
    /// the data EEPROM.
    #[error(
        "the chip is readout-protected (RDP level 1): the bootloader refuses to read, write, erase or \
         start its flash, so {operation} is impossible until the protection is removed (which \
         mass-erases the flash and the data EEPROM)"
    )]
    ReadoutProtected { operation: &'static str },

    /// [`rdp::protect`](crate::rdp::protect) / [`rdp::unprotect`](crate::rdp::unprotect): the
    /// bootloader acknowledged the command twice, but a new bootloader session still reads the
    /// old protection state. The usual cause is a debugger (SWD) attached since the last
    /// power-on: RM0367 3.4 then requires a power-on reset for the new option bytes to load (a
    /// system reset is not enough). `protect` is the requested direction.
    #[error(
        "the bootloader acknowledged Readout {}, but a new bootloader session still reads the chip as {}; \
         power-cycle it (USB and any SWD probe) and read the state again",
        if *.protect { "Protect" } else { "Unprotect" },
        if *.protect { "not protected" } else { "protected" }
    )]
    ReadoutUnchanged { protect: bool },

    /// [`rdp::protect`](crate::rdp::protect) / [`rdp::unprotect`](crate::rdp::unprotect): the
    /// bootloader accepted the command (its first ACK), but the second ACK, which confirms the
    /// option-byte update (and for Unprotect the mass erase), did not come: `error` is what came
    /// instead (a timeout, a stray byte, an I/O error). The outcome was then read back in a new
    /// bootloader session: `readback` is the protection state found, or why it could not be read.
    /// For Unprotect this is what tells the operator whether the flash and the data EEPROM were
    /// erased.
    #[error(
        "the bootloader accepted Readout {} but did not confirm it ({error}); {}",
        if *.protect { "Protect" } else { "Unprotect" },
        unconfirmed_outcome(*.protect, .readback)
    )]
    ReadoutUnconfirmed {
        protect: bool,
        error: Box<Error>,
        readback: std::result::Result<bool, Box<Error>>,
    },

    /// `Get ID` returned a product id the crate does not know
    /// ([`target::max_flash_size`](crate::target::max_flash_size) has no size for it): its flash
    /// size and layout are unknown, so [`flash`](crate::flash::flash) and
    /// [`erase`](crate::flash::erase) refuse it before anything is erased or written. Only
    /// [`chip::info`](crate::chip::info) reports such an id.
    #[error(
        "chip id 0x{id:03X} is not a recognized STM32L0 product id; refusing to {operation} it, since its \
         flash size and layout are unknown"
    )]
    UnknownChip { id: u16, operation: &'static str },
}

/// Wording for [`Error::ReadoutUnconfirmed`]: the state read back, and what it means.
fn unconfirmed_outcome(protect: bool, readback: &std::result::Result<bool, Box<Error>>) -> String {
    match (protect, readback) {
        (true, Ok(true)) => {
            "a new bootloader session reads the chip as protected (RDP level 1): the protection is set".into()
        }
        (true, Ok(false)) => {
            "a new bootloader session reads the chip as not protected: the protection was not set".into()
        }
        (false, Ok(false)) => "a new bootloader session reads the chip as not protected: the protection was \
                               removed, and the flash and the data EEPROM were mass-erased"
            .into(),
        (false, Ok(true)) => "a new bootloader session still reads the chip as protected (RDP level 1): the \
                              protection was not removed (whether the flash was erased cannot be read while \
                              it is protected)"
            .into(),
        (protect, Err(e)) => format!(
            "the protection state could not be read back ({e}){}; power-cycle the board and read it again",
            if protect {
                ""
            } else {
                ", so the flash and the data EEPROM may have been erased"
            }
        ),
    }
}

/// Wording for [`Error::FirmwareTooLarge`]: the family maximum vs a smaller, identified part.
fn flash_limit_desc(max: &u32) -> &'static str {
    if *max == crate::target::MAX_FLASH_SIZE {
        "maximum for any STM32L0 device"
    } else {
        "flash of the connected chip"
    }
}
