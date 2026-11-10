//! NXP SC16IS740 driver: a UART behind I²C with 64-byte FIFOs (on the TOWER CO2 Module, at
//! `0x4D`, talking to the LP8 at 9600 baud, 8N2). HAL-independent over
//! [`embedded_hal::i2c::I2c`].
//!
//! Polled, no interrupt line: the caller asks for the receive level and takes the bytes once
//! they are all there. Registers are addressed as `register << 3`. The module's crystal is
//! 13.56 MHz, so a divisor of 88 gives 9630 baud, within 0.4 % of 9600.
//!
//! **Sleep.** Awake, the part's oscillator runs (milliamps); [`sleep`](Sc16is740::sleep) stops it
//! (30 µA at most by the datasheet) once the lines are idle. A sleeping UART wakes on a start
//! bit but can garble that first byte while its oscillator starts, so the part sleeps only
//! between exchanges: [`init`](Sc16is740::init) leaves it awake, the caller puts it to sleep
//! when the peer is done.

use embedded_hal::i2c::I2c;

/// The address on the CO2 Module (A1 and A0 to ground).
pub const ADDR: u8 = 0x4D;
/// The depth of each FIFO.
pub const FIFO: usize = 64;
/// The longest block [`Sc16is740::send`] takes in one call.
pub const MAX_SEND: usize = 40;

const RHR_THR: u8 = 0x00;
const IER: u8 = 0x01 << 3;
const FCR: u8 = 0x02 << 3;
const LCR: u8 = 0x03 << 3;
const SPR: u8 = 0x07 << 3;
const TXLVL: u8 = 0x08 << 3;
const RXLVL: u8 = 0x09 << 3;
/// With `LCR` = 0x80: the divisor latch.
const DLL: u8 = 0x00;
const DLH: u8 = 0x01 << 3;
/// With `LCR` = 0xBF: the enhanced feature register.
const EFR: u8 = 0x02 << 3;

/// 13.56 MHz / 16 / 88 = 9630 baud.
const DIVISOR_9600: u8 = 88;
/// `IER` bit 4: sleep mode (writable once `EFR` bit 4 is set).
const IER_SLEEP: u8 = 0x10;

/// A block that does not fit the transmit FIFO or [`MAX_SEND`], or a bus error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError<E> {
    Bus(E),
    /// The block is longer than [`MAX_SEND`] or than the free room in the transmit FIFO.
    Full,
}

/// An SC16IS740 on an I²C bus.
pub struct Sc16is740<I2C> {
    i2c: I2C,
    addr: u8,
}

impl<I2C: I2c> Sc16is740<I2C> {
    pub fn new(i2c: I2C, addr: u8) -> Self {
        Self { i2c, addr }
    }

    /// Whether an SC16IS7xx answers: its scratchpad register keeps what is written to it.
    pub fn present(&mut self) -> bool {
        [0x5A, 0xA5]
            .into_iter()
            .all(|pattern| self.write(SPR, pattern).is_ok() && self.read(SPR).is_ok_and(|v| v == pattern))
    }

    /// 9600 baud, 8 data bits, no parity, 2 stop bits; the FIFOs on and empty; no interrupts;
    /// awake. Enhanced functions are enabled, so [`sleep`](Self::sleep) works afterwards.
    pub fn init(&mut self) -> Result<(), I2C::Error> {
        // Out of sleep first: the divisor must not be written while the part sleeps. (`LCR` to a
        // normal value before that, so the write lands on `IER` whatever an earlier, interrupted
        // set-up left selected.)
        self.write(LCR, 0x07)?;
        self.write(IER, 0x00)?;
        self.write(LCR, 0x80)?;
        self.write(DLL, DIVISOR_9600)?;
        self.write(DLH, 0x00)?;
        self.write(LCR, 0xBF)?;
        self.write(EFR, 0x10)?;
        self.write(LCR, 0x07)?;
        self.write(FCR, 0x07)
    }

    /// Let the part stop its oscillator while the lines are idle (`true`), or keep it awake for
    /// an exchange (`false`; allow a few milliseconds for the oscillator before sending).
    pub fn sleep(&mut self, on: bool) -> Result<(), I2C::Error> {
        self.write(IER, if on { IER_SLEEP } else { 0x00 })
    }

    /// Throw away anything received so far.
    pub fn clear_rx(&mut self) -> Result<(), I2C::Error> {
        self.write(FCR, 0x03)
    }

    /// How many received bytes wait in the FIFO.
    pub fn rx_level(&mut self) -> Result<usize, I2C::Error> {
        Ok(usize::from(self.read(RXLVL)?))
    }

    /// Queue `data` for sending.
    pub fn send(&mut self, data: &[u8]) -> Result<(), SendError<I2C::Error>> {
        if data.len() > MAX_SEND || usize::from(self.read(TXLVL).map_err(SendError::Bus)?) < data.len() {
            return Err(SendError::Full);
        }
        let mut frame = [0u8; 1 + MAX_SEND];
        frame[0] = RHR_THR;
        frame[1..=data.len()].copy_from_slice(data);
        self.i2c
            .write(self.addr, &frame[..=data.len()])
            .map_err(SendError::Bus)
    }

    /// Take `buf.len()` received bytes (check [`rx_level`](Self::rx_level) first).
    pub fn receive(&mut self, buf: &mut [u8]) -> Result<(), I2C::Error> {
        self.i2c.write_read(self.addr, &[RHR_THR], buf)
    }

    /// Release the bus.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn read(&mut self, reg: u8) -> Result<u8, I2C::Error> {
        let mut v = [0u8];
        self.i2c.write_read(self.addr, &[reg], &mut v)?;
        Ok(v[0])
    }

    fn write(&mut self, reg: u8, value: u8) -> Result<(), I2C::Error> {
        self.i2c.write(self.addr, &[reg, value])
    }
}
