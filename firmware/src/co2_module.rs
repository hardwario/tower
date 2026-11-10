//! The TOWER CO2 Module: a Senseair LP8 behind an SC16IS740 UART bridge (`0x4D`), with its
//! supply, its capacitor's charging, its enable and its RDY line on a TCA9534A (`0x38`).
//!
//! **Low power.** The LP8 is made for batteries: it is powered for one measurement at a time
//! (about 0.6 s; 3.6 mC by its datasheet, nearly all of it the lamp's pulse, drawn from the
//! module's capacitor) and off in between. So is everything else here: between measurements the
//! sensor's supply and the capacitor's charging are off and the UART bridge sleeps (its
//! oscillator stopped). A measurement is two calls with a wait between them, so the caller's
//! task can sleep or do other work meanwhile:
//!
//! 1. [`charge`](Co2Module::charge) switches the charging on and says how long to wait: a minute
//!    for an empty capacitor (the first measurement, or after one that ran short), 5 s otherwise.
//! 2. [`measure`](Co2Module::measure) runs the LP8's cycle (power on, RDY low, write, RDY high,
//!    read) and switches everything off again, whatever happened.
//!
//! The average cost is set by how often that runs: at one measurement in 5 minutes about 12 µA,
//! at the sensor's 16 s minimum about 225 µA.
//!
//! **The sensor state.** The LP8 keeps nothing while it is off: its noise filter and its
//! baseline history live in 23 bytes the host carries from each reply into the next write. The
//! driver keeps them in RAM, so they last until a reset; the first measurement after one starts
//! the filter over (as with the legacy SDK). Every 8 days of uptime it asks the sensor for an
//! automatic baseline correction (the lowest reading since the last one is taken as fresh air,
//! 400 ppm).
//!
//! **The value.** [`Measurement::ppm`] is the filtered, pressure compensated concentration, the
//! one the datasheet specifies the accuracy for (±50 ppm ±3 % of the reading); pass the ambient
//! pressure from a barometer when there is one. The frames are [`tower_modules::lp8`]'s.
//!
//! ```ignore
//! if let Some(mut co2) = Co2Module::probe(bus).await {
//!     loop {
//!         Timer::after(co2.charge().await?).await;
//!         let m = co2.measure(None).await?;
//!         info!("{:?} ppm", m.ppm());
//!         Timer::after_secs(300).await;
//!     }
//! }
//! ```

use core::fmt;

use embassy_time::{Duration, Instant, Timer};
use tower_modules::Invalid;
use tower_modules::lp8::{self, Calculation};
pub use tower_modules::lp8::{MIN_PERIOD_S, Measurement};

use crate::i2c::{I2cBus, RawI2c};
use crate::sc16is740::{self, Sc16is740};
use crate::tca9534a::Tca9534a;

/// The module's I/O expander.
pub const EXPANDER: u8 = 0x38;

// The expander's output latch stays 0: a pin made an output pulls its line low (on), a pin left
// an input lets the line's pull-up switch it off. P0 and P4 are always outputs (the legacy SDK's
// default).
const IDLE: u8 = 0xEE;
/// The capacitor's charging (P1) and the sensor's supply VDD2 (P2) on.
const CHARGE: u8 = IDLE & !0x02 & !0x04;
/// And the sensor enabled (P3).
const ENABLE: u8 = CHARGE & !0x08;
/// The UART bridge's reset (P6, active low).
const UART_RESET: u8 = 0x40;
/// RDY from the LP8 (P7, an input).
const RDY: u8 = 0x80;

/// The charge of a capacitor that may be empty (the legacy SDK's minute).
const FIRST_CHARGE: Duration = Duration::from_secs(60);
/// The top-up before each further measurement.
const TOP_UP: Duration = Duration::from_secs(5);

// One measurement's timing, from the LP8 guideline's table (RDY low 136 to 191 ms after power
// on, the measurement done by 372 ms) with the legacy SDK's margins.
const BOOT: Duration = Duration::from_millis(140);
const RDY_LOW_BY: Duration = Duration::from_millis(500);
const RDY_HIGH_BY: Duration = Duration::from_millis(1500);
/// The 33-byte write takes 38 ms on the wire; the acknowledgement follows it.
const ACK_LEAD: Duration = Duration::from_millis(60);
const ACK_WITHIN: Duration = Duration::from_millis(300);
/// The read takes 8 ms on the wire, the 49-byte reply 56 ms.
const REPLY_LEAD: Duration = Duration::from_millis(70);
const REPLY_WITHIN: Duration = Duration::from_millis(400);
const POLL: Duration = Duration::from_millis(10);

/// Failed measurements in a row after which the module counts as gone.
const GIVE_UP: u8 = 3;

/// A measurement that failed. The module is switched off again in every case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An I²C transfer failed (the module is not there, or the bus is stuck).
    Bus,
    /// The LP8 did not pull RDY low after power-on.
    NotReady,
    /// The LP8 did not acknowledge the write in time.
    NoAck,
    /// The LP8 did not release RDY: the measurement did not finish.
    NotFinished,
    /// The LP8 did not send its reply in time.
    NoReply,
    /// A frame from the LP8 was not what it should be.
    Frame(Invalid),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bus => f.write_str("I2C error"),
            Error::NotReady => f.write_str("LP8 not ready"),
            Error::NoAck => f.write_str("LP8 did not acknowledge"),
            Error::NotFinished => f.write_str("LP8 did not finish"),
            Error::NoReply => f.write_str("LP8 did not reply"),
            Error::Frame(e) => write!(f, "LP8 frame: {e}"),
        }
    }
}

impl From<embassy_stm32::i2c::Error> for Error {
    fn from(_: embassy_stm32::i2c::Error) -> Self {
        Error::Bus
    }
}

/// The CO2 Module on the board's I²C bus.
pub struct Co2Module {
    bus: I2cBus,
    /// From the previous measurement, for the next one.
    state: Option<[u8; lp8::STATE_LEN]>,
    /// The next charge is a full one (nothing measured yet, or the last measurement ran short).
    empty: bool,
    /// When the next automatic baseline correction is due.
    abc_at: Instant,
    failures: u8,
}

impl Co2Module {
    /// Look for the module (its expander and its UART bridge) and switch everything on it off:
    /// the sensor, the charging, the UART bridge asleep. `None` when it does not answer.
    pub async fn probe(bus: I2cBus) -> Option<Self> {
        let found = bus.raw(|b| {
            Tca9534a::new(&mut *b, EXPANDER).read_config().is_ok()
                && Sc16is740::new(&mut *b, sc16is740::ADDR).present()
                && reset(b).is_ok()
        });
        if found {
            // The UART bridge needs 10 µs after its reset; a whole millisecond costs nothing.
            Timer::after_millis(1).await;
        }
        let found =
            found && bus.raw(|b| Sc16is740::new(&mut *b, sc16is740::ADDR).init().is_ok() && off(b).is_ok());
        bus.rest().await;
        found.then_some(Self {
            bus,
            state: None,
            empty: true,
            abc_at: Instant::MAX,
            failures: 0,
        })
    }

    /// Whether the module failed too many measurements in a row to still be there: drop it and
    /// [`probe`](Self::probe) again.
    pub fn gone(&self) -> bool {
        self.failures >= GIVE_UP
    }

    /// How long the next [`charge`](Self::charge) takes: a minute for a capacitor that may be
    /// empty, 5 s for a top-up.
    pub fn charge_time(&self) -> Duration {
        if self.empty { FIRST_CHARGE } else { TOP_UP }
    }

    /// Switch the capacitor's charging on; [`measure`](Self::measure) after the returned time
    /// (a minute for a capacitor that may be empty, 5 s for a top-up), not before.
    pub async fn charge(&mut self) -> Result<Duration, Error> {
        let bus = self.bus;
        let r = bus.raw(|b| {
            let mut expander = Tca9534a::new(b, EXPANDER);
            expander.write_output(0x00)?;
            expander.write_config(CHARGE)
        });
        bus.rest().await;
        if r.is_err() {
            self.failures = self.failures.saturating_add(1);
        }
        r?;
        Ok(self.charge_time())
    }

    /// Measure once and switch the module off again (also after an error). `pressure` is the
    /// ambient pressure in tenths of a hectopascal, for the compensation; without one the sensor
    /// assumes 1012.4 hPa. Takes about 0.6 s, the MCU asleep for most of it. The capacitor must
    /// have been [charged](Self::charge).
    pub async fn measure(&mut self, pressure: Option<u16>) -> Result<Measurement, Error> {
        let calculation = match self.state {
            None => Calculation::Initial,
            Some(_) if Instant::now() >= self.abc_at => Calculation::Abc,
            Some(_) => Calculation::Sequential,
        };
        // The sensor takes 300.0 to 1200.0 hPa; anything else is a barometer gone wrong.
        let pressure = pressure
            .filter(|p| (3000..=12000).contains(p))
            .unwrap_or(lp8::DEFAULT_PRESSURE);
        let result = self.cycle(calculation, pressure).await;
        let bus = self.bus;
        let switched_off = bus.raw(off);
        bus.rest().await;
        let (m, state) = match result.and_then(|ok| Ok(switched_off.map(|()| ok)?)) {
            Ok(ok) => ok,
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                self.empty = true;
                return Err(e);
            }
        };
        self.failures = 0;
        if m.needs_restart() {
            // The sensor reported a fault of its own: its state is not to be written back.
            self.state = None;
        } else {
            self.state = Some(state);
            if calculation != Calculation::Sequential {
                self.abc_at = Instant::now() + Duration::from_secs(u64::from(lp8::ABC_PERIOD_S));
            }
        }
        self.empty = m.vcap_low();
        Ok(m)
    }

    /// The LP8's cycle: power on, RDY low, write, acknowledgement, RDY high, read, reply.
    async fn cycle(
        &mut self,
        calculation: Calculation,
        pressure: u16,
    ) -> Result<(Measurement, [u8; lp8::STATE_LEN]), Error> {
        let bus = self.bus;
        let on = Instant::now();
        bus.raw(|b| {
            Sc16is740::new(&mut *b, sc16is740::ADDR).init()?;
            Tca9534a::new(&mut *b, EXPANDER).write_config(ENABLE)
        })?;
        Timer::at(on + BOOT).await;
        if !self.rdy(false, on + RDY_LOW_BY).await? {
            return Err(Error::NotReady);
        }
        let state = self.state;
        bus.raw(|b| send(b, &lp8::write_frame(calculation, state.as_ref(), pressure)))?;
        Timer::after(ACK_LEAD).await;
        let mut ack = [0u8; lp8::WRITE_ACK.len()];
        if !self.receive(&mut ack, ACK_WITHIN).await? {
            return Err(Error::NoAck);
        }
        lp8::check_ack(&ack).map_err(Error::Frame)?;
        if !self.rdy(true, on + RDY_HIGH_BY).await? {
            return Err(Error::NotFinished);
        }
        bus.raw(|b| send(b, &lp8::READ_FRAME))?;
        Timer::after(REPLY_LEAD).await;
        // Decoded where it is received, so the 49-byte reply is not kept across an await.
        let by = Instant::now() + REPLY_WITHIN;
        loop {
            let decoded = bus.raw(|b| {
                let mut uart = Sc16is740::new(b, sc16is740::ADDR);
                if uart.rx_level()? < lp8::REPLY_LEN {
                    return Ok(None);
                }
                let mut reply = [0u8; lp8::REPLY_LEN];
                uart.receive(&mut reply)?;
                let m = lp8::parse_reply(&reply).map_err(Error::Frame)?;
                Ok::<_, Error>(Some((m, lp8::sensor_state(&reply))))
            })?;
            match decoded {
                Some(done) => return Ok(done),
                None if Instant::now() >= by => return Err(Error::NoReply),
                None => Timer::after(POLL).await,
            }
        }
    }

    /// Wait until RDY is `high` (or low), at most until `by`; whether it got there.
    async fn rdy(&self, high: bool, by: Instant) -> Result<bool, Error> {
        loop {
            let input = self.bus.raw(|b| Tca9534a::new(b, EXPANDER).read_input())?;
            if (input & RDY != 0) == high {
                return Ok(true);
            }
            if Instant::now() >= by {
                return Ok(false);
            }
            Timer::after(POLL).await;
        }
    }

    /// Wait for `buf.len()` bytes from the LP8, at most `within`; whether they came.
    async fn receive(&self, buf: &mut [u8], within: Duration) -> Result<bool, Error> {
        let by = Instant::now() + within;
        loop {
            let got = self.bus.raw(|b| {
                let mut uart = Sc16is740::new(b, sc16is740::ADDR);
                if uart.rx_level()? < buf.len() {
                    return Ok(false);
                }
                uart.receive(buf).map(|()| true)
            })?;
            if got {
                return Ok(true);
            }
            if Instant::now() >= by {
                return Ok(false);
            }
            Timer::after(POLL).await;
        }
    }
}

/// Pulse the UART bridge's reset (longer than its 3 µs by the transfer itself), everything else
/// off.
fn reset(b: &mut RawI2c) -> Result<(), embassy_stm32::i2c::Error> {
    let mut expander = Tca9534a::new(b, EXPANDER);
    expander.write_output(0x00)?;
    expander.write_config(IDLE & !UART_RESET)?;
    expander.write_config(IDLE)
}

/// The sensor and the charging off, the UART bridge (set up before) asleep.
fn off(b: &mut RawI2c) -> Result<(), embassy_stm32::i2c::Error> {
    Tca9534a::new(&mut *b, EXPANDER).write_config(IDLE)?;
    Sc16is740::new(b, sc16is740::ADDR).sleep(true)
}

/// Send one frame to the LP8, past whatever was received before.
fn send(b: &mut RawI2c, frame: &[u8]) -> Result<(), Error> {
    let mut uart = Sc16is740::new(b, sc16is740::ADDR);
    uart.clear_rx()?;
    uart.send(frame).map_err(|_| Error::Bus)
}
