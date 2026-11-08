//! The board's one I²C2 bus, shared by every driver that needs it.
//!
//! [`Board::take`](crate::board::Board::take) hands the bus out inside the TMP112 driver; an app
//! that has more than one user of the bus (the TMP112 and the LCD Module, say) turns it into an
//! [`I2cBus`]: a cheap `Copy` handle over the one bus parked in a `static`, like the EEPROM's
//! [`Nv`](crate::storage::Nv). Every transaction locks the bus for its duration only, and no lock
//! is held across an `.await`, so tasks share it freely.
//!
//! ```ignore
//! let bus = I2cBus::share(b.tmp112.release());
//! let mut tmp112 = Tmp112::new(bus, tmp112::ADDR_VPLUS); // a generic driver gets a copy
//! let lcd = lcd::init(b.spawner, lcd::Resources::new(b.header_spi, b.button, b.header_int), bus, cfg).await?;
//! ```
//!
//! **The ATSHA204A.** Any traffic on the bus can wake the board's ATSHA204A into hundreds of µA
//! (see [`AtshaGuard`](crate::board::AtshaGuard)). Through the [`embedded_hal`] interface an
//! `I2cBus` behaves like the guarded bus `Board::take` built: each transaction is followed by a
//! Sleep, with its blocking ~3 ms retry. A driver that makes a burst of transactions uses
//! [`raw`](I2cBus::raw) instead, which skips the Sleep, then one [`rest`](I2cBus::rest) at the end
//! of the burst, which waits out the chip's wake delay with an async timer (the MCU in STOP)
//! instead of busy-waiting.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_stm32::i2c::{Error, I2c, Master};
use embassy_stm32::mode::Blocking;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embedded_hal::i2c::{ErrorType, I2c as I2cTrait, Operation};

use crate::board::{self, GuardedI2c};

/// The raw (unguarded) blocking bus.
pub type RawI2c = I2c<'static, Blocking, Master>;

type Shared = Mutex<ThreadModeRawMutex, RefCell<GuardedI2c>>;

/// Raw traffic happened since the ATSHA was last put back to sleep.
static ATSHA_MAY_BE_AWAKE: AtomicBool = AtomicBool::new(false);

/// A shared handle to the board's I²C2 bus (see the [module docs](self)).
#[derive(Clone, Copy)]
pub struct I2cBus(&'static Shared);

impl I2cBus {
    /// Park the bus in a process-wide `static` and return the handle. Call once (a second call
    /// panics); copy the handle wherever it is needed.
    pub fn share(bus: GuardedI2c) -> Self {
        let cell: &'static Shared = cortex_m::singleton!(: Shared = Mutex::new(RefCell::new(bus)))
            .expect("I2cBus::share called more than once");
        Self(cell)
    }

    /// Run `f` on the raw bus under the lock, without the per-transaction ATSHA Sleep. Follow a
    /// burst with [`rest`](Self::rest). `f` must not use this bus handle itself (the lock is not
    /// reentrant: that panics).
    pub fn raw<R>(&self, f: impl FnOnce(&mut RawI2c) -> R) -> R {
        let r = self.0.lock(|c| f(c.borrow_mut().inner_mut()));
        ATSHA_MAY_BE_AWAKE.store(true, Ordering::Relaxed);
        r
    }

    /// Put the ATSHA204A back to sleep after [`raw`](Self::raw) traffic: a Sleep, and if the chip
    /// NACKs it (it may have just been woken and is deaf for its wake delay), another one after
    /// that delay, awaited rather than busy-waited. Nothing to do when no raw traffic happened
    /// since the last call, or the board has no ATSHA.
    pub async fn rest(&self) {
        // Thread mode only (no atomic swap on the M0+): a load and a store suffice.
        if !ATSHA_MAY_BE_AWAKE.load(Ordering::Relaxed) || !board::atsha_fitted() {
            return;
        }
        ATSHA_MAY_BE_AWAKE.store(false, Ordering::Relaxed);
        let lock = |f: fn(&mut RawI2c) -> bool| self.0.lock(|c| f(c.borrow_mut().inner_mut()));
        if !lock(board::atsha_sleep_once) {
            embassy_time::Timer::after(board::ATSHA_WAKE_DELAY).await;
            lock(board::atsha_sleep_once);
        }
    }
}

impl ErrorType for I2cBus {
    type Error = Error;
}

impl I2cTrait for I2cBus {
    fn transaction(&mut self, address: u8, operations: &mut [Operation<'_>]) -> Result<(), Self::Error> {
        self.0.lock(|c| c.borrow_mut().transaction(address, operations))
    }
}
