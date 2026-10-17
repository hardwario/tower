//! tower-boot: boot control for HARDWARIO TOWER boards around the STM32 ROM UART
//! bootloader (AN3155): flash / erase / verify, chip info, and the NRST/BOOT0
//! reset pulses.
//!
//! A UI-free library (it was the `jolt` flasher before the monorepo; the binary is
//! gone, the engine is unchanged). The `tower` CLI (`tower flash` / `erase` /
//! `reset` / `chip`) and the HIL harness drive it directly:
//!
//! ```no_run
//! use tower_boot::flash::{self, FlashOptions, Progress};
//! use tower_boot::port::Port;
//!
//! let mut port = Port::open("/dev/ttyUSB0")?;
//! let firmware = tower_boot::firmware::load("app.bin".as_ref())?;
//! let opts = FlashOptions::default(); // erase + verify + run, no `go`
//! // The engine is UI-free: it emits `Progress` events through this callback
//! // instead of printing, so callers render progress however they like.
//! flash::flash(&mut port, &firmware, &opts, &mut |p: Progress| {
//!     if let Progress::Write { bytes_done, bytes_total } = p {
//!         eprintln!("{bytes_done}/{bytes_total}");
//!     }
//! })?;
//! // Or discard progress entirely: `flash::flash(&mut port, &firmware, &opts, &mut flash::no_progress)`.
//! # Ok::<(), tower_boot::error::Error>(())
//! ```
//!
//! The high-level entry points are [`flash::flash`], [`flash::erase`],
//! [`chip::info`] (read-only: chip id, bootloader version, supported commands,
//! readout-protection state), the opt-in production lock [`rdp::protect`] /
//! [`rdp::unprotect`] (RDP level 1 and back, the latter mass-erasing flash and data
//! EEPROM; level 2 is out of reach by design), and the
//! [`Port::reset_into_app`](port::Port::reset_into_app) /
//! [`Port::reset_into_bootloader`](port::Port::reset_into_bootloader) reset
//! pulses. The lower layers ([`bootloader`], [`port`]) are public for callers
//! that need finer control. Every library entry point returns
//! [`error::Error`]; there is no `anyhow` in the public surface, and nothing
//! here prints.

pub mod bootloader;
pub mod chip;
pub mod error;
pub mod firmware;
pub mod flash;
pub mod port;
pub mod rdp;
pub mod target;
