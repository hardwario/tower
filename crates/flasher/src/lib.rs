//! jolt — program STM32L0 microcontrollers over the UART bootloader.
//!
//! This crate is both a CLI (`src/main.rs`) and a reusable library. The library
//! exposes the STM32 USART-bootloader engine so other tools can drive
//! flash/erase/reset directly instead of shelling out to the `jolt` binary:
//!
//! ```no_run
//! use jolt::flash::{self, FlashOptions};
//! use jolt::port::Port;
//!
//! let mut port = Port::open("/dev/ttyUSB0")?;
//! let firmware = jolt::firmware::load("app.bin".as_ref())?;
//! let opts = FlashOptions { erase: true, verify: true, run: true, go: false, verbose: false };
//! flash::flash(&mut port, &firmware, &opts)?;
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! The high-level entry points are [`flash::flash`], [`flash::erase`], and the
//! [`Port::reset_into_app`](port::Port::reset_into_app) /
//! [`Port::reset_into_bootloader`](port::Port::reset_into_bootloader) reset
//! pulses. The lower layers ([`bootloader`], [`port`]) are public for callers
//! that need finer control.

pub mod bootloader;
pub mod error;
pub mod firmware;
pub mod flash;
pub mod port;
pub mod target;
