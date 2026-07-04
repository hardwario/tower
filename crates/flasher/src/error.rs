//! Error types for the bootloader / serial layers.
//!
//! Command handlers use `anyhow` for context-rich errors; the lower layers
//! return this typed `Error` so retry logic can tell a NACK from a timeout.

use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Which phase of a two-step command handshake produced a NACK. AN3155 command
/// frames are acknowledged in two stages — first the command byte + complement,
/// then the command's payload — and the two mean very different things (a
/// command-byte NACK is usually line corruption; a payload NACK is the device
/// rejecting the request, e.g. an out-of-range Extended Erase page list). Retry
/// and boundary-detection logic must be able to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NackStage {
    /// NACK to the command byte + complement frame (before any payload).
    Command,
    /// NACK to the command's payload (address, length, page list, data).
    Payload,
}

impl std::fmt::Display for NackStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NackStage::Command => f.write_str("command"),
            NackStage::Payload => f.write_str("payload"),
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
    UnexpectedByte {
        context: String,
        got: u8,
        expected: u8,
    },

    #[error(
        "could not enter the bootloader after {attempts} attempt(s) (no ACK to 0x7F init) — check the --device and the cable"
    )]
    BootloaderInit { attempts: u32 },

    #[error("verify mismatch at 0x{address:08X}: flash=0x{got:02X}, firmware=0x{expected:02X}")]
    VerifyMismatch { address: u32, got: u8, expected: u8 },

    #[error(
        "erase truncated after {erased} page(s) ({kib} KiB) on chip 0x{id:03X}: not a valid STM32L0 flash density — the remaining pages are likely write-protected, so the device is only partly erased"
    )]
    PartialErase { erased: usize, kib: usize, id: u16 },

    #[error("invalid argument: {context}")]
    InvalidArgument { context: &'static str },

    #[error(
        "firmware is {size} bytes, exceeding the {max} byte ({max_kib} KiB) maximum for any STM32L0 device"
    )]
    FirmwareTooLarge {
        size: usize,
        max: u32,
        max_kib: u32,
    },

    #[error(
        "{path} looks like a .{ext} file; jolt flashes raw binaries. \
         Convert it first, e.g.: arm-none-eabi-objcopy -O binary in.{ext} out.bin"
    )]
    FirmwareFormat { path: PathBuf, ext: String },

    #[error("firmware file {path} is empty")]
    FirmwareEmpty { path: PathBuf },

    #[error("serial port disconnected while {context}")]
    Disconnected { context: &'static str },
}
