//! Error types for the bootloader / serial layers.
//!
//! Command handlers use `anyhow` for context-rich errors; the lower layers
//! return this typed `Error` so retry logic can tell a NACK from a timeout.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serial port error: {0}")]
    Serial(#[from] serialport::Error),

    #[error("bootloader replied NACK (0x1F) to {context}")]
    Nack { context: String },

    #[error("timed out waiting for {context}")]
    Timeout { context: String },

    #[error("unexpected byte while reading {context}: got 0x{got:02X}, expected 0x{expected:02X}")]
    UnexpectedByte {
        context: String,
        got: u8,
        expected: u8,
    },

    #[error(
        "could not enter the bootloader after {attempts} attempt(s) (no ACK to 0x7F init) — check the --port and the cable"
    )]
    BootloaderInit { attempts: u32 },

    #[error("verify mismatch at 0x{address:08X}: flash=0x{got:02X}, firmware=0x{expected:02X}")]
    VerifyMismatch { address: u32, got: u8, expected: u8 },
}
