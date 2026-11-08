//! The TOWER PIR Module: a digital pyroelectric motion detector, the Excelitas "LowPower
//! DigiPyro" PYQ 1648-7053 (its 1:1 successor, the PYQ 1548-7659, works the same way).
//!
//! The detector carries its own ADC and DSP. It filters the pyroelectric signal (a 0.44 to 7 Hz
//! band pass), counts the pulses that cross a threshold and raises its DIRECT LINK line when they
//! meet the configured criteria (the "wake-up operation mode"), drawing about 3 µA meanwhile. The
//! MCU stays in STOP until then: DIRECT LINK is an EXTI input, so a motion event costs one wake-up
//! and nothing else.
//!
//! ```ignore
//! let mut pir = Pir::new(b.pir);
//! pir.configure(&pir::Config::default()).await?;   // writes the register and reads it back
//! loop {
//!     pir.wait_motion().await;                     // STOP until the detector fires
//!     info!(target: "pir", "motion");
//! }
//! ```
//!
//! # Wiring
//!
//! PIR Module rev 1.5: the detector's SERIN input on header P8 (PB0) and its DIRECT LINK on P9
//! (PB2, EXTI line 2), each through 470 Ω with 47 pF to ground at the detector; VDD through 10 Ω
//! and 470 nF. The module has no other parts. [`Board::pir`](crate::board::Board::pir) hands out
//! the two pins and the EXTI channel.
//!
//! # The two interfaces
//!
//! From the Excelitas device specification "PYQ 1648 / 7053" (rev. 27.06.2016) and the
//! "LowPower DigiPyro 2nd Gen." application note (rev. 0.61, 16.05.2020, for the PYQ 15xx). Where
//! the generations differ, this driver keeps to both.
//!
//! **SERIN** takes the 25-bit configuration register, most significant bit first: per bit a low
//! to high clock edge (each level 200 ns to 2 µs), then the bit's value held for t_SHD (at least
//! 72 µs on the PYQ 1648, 80 to 150 µs on the PYQ 1548). SERIN held low for t_SLT (over 580 µs,
//! over 650 µs on the PYQ 1548) latches the last 25 bits. The register is volatile and undefined
//! after power-up, so [`Pir::configure`] always writes it. The 1548 wants DIRECT LINK low while
//! SERIN is written. This driver: clock levels of about 0.9 µs, bits of 100 µs, all 25 in one
//! critical section (2.5 ms, so no interrupt can stretch a bit past 150 µs or pause the
//! transfer long enough to latch half a register), SERIN low at all other times.
//!
//! **DIRECT LINK** is both the motion output and a serial output the host clocks. In wake-up mode
//! the detector drives it high on motion and keeps it high until the host pulls it low (at least
//! one detector clock on the PYQ 1648, 160 µs on the PYQ 1548): the acknowledgement, which also
//! starts the blind time. A readout starts with the host holding the line high for t_DS (110 to
//! 150 µs on the PYQ 1648, at least 120 µs on the PYQ 1548); then per bit the host clocks a low
//! to high edge (each level 200 ns to 2 µs), releases the line and samples it once the detector
//! has settled it. The line may sit low for at most 22 µs per bit and a whole frame must take
//! under about 800 µs (32 detector clocks), so a readout runs in a critical section (this driver:
//! a 130 µs setup pulse, then about 7 µs per bit, 0.4 ms for all 40 bits). The host ends it by
//! holding the line low for t_RA (160 µs) and releasing it. The 40-bit frame: bit 39 (reserved on
//! the PYQ 1648, an active-low out-of-range flag on the PYQ 1548), bits 38 to 25 the 14-bit signal
//! of the selected source (two's complement for the band pass), bits 24 to 0 the configuration
//! register.
//!
//! [`Pir::configure`] reads the frame back once after every write (the 1548 note allows exactly
//! one readout in wake-up mode, after both lines sat low for 2.4 ms): the register must come back
//! as written, which also tells a missing module ([`Error::NotDetected`]: an undriven line reads
//! the same level on every bit) from a disturbed write ([`Error::Readback`]).
//!
//! # Detection criteria
//!
//! A pulse is the band-pass signal crossing ±[`threshold`](Config::threshold) (in ADC counts of
//! 6.5 µV); the next pulse counts only once the signal crossed the other way. A motion event is
//! [`pulses`](Config::pulses) pulses inside one [`window`](Config::window). A person walking
//! across the lens's zones swings the signal both ways, so two pulses reject a single thermal
//! step (a draught, sunlight) that one pulse would report. Measure the signal with
//! [`Mode::Signal`] (the `pir_signal` example logs its peaks) to choose a threshold.
//!
//! # Warm-up and settling
//!
//! A pyroelectric detector needs time after power-up before its filtered signal is quiet. When the
//! MCU itself came up from a power-on reset (the detector shares its supply, so it powered up
//! too), [`Pir::wait_motion`] swallows events for [`WARM_UP`] after boot. After any other reset
//! (a reset pin, a flash, the watchdog) the detector has been powered all along, but the first
//! write after the MCU was away swings its band pass all the same (measured: up to 2300 counts
//! after a one-minute flash, 1150 after an 8 s reset, back under 32 within 4 s; it fired a false
//! event 1.85 s after the write), so events count only [`SETTLE`] after the first successful
//! [`Pir::configure`]. Later writes don't disturb the filters. [`Pir::warming_up`] tells how long
//! is left.

use core::fmt;

use embassy_stm32::exti::ExtiInput;
use embassy_stm32::gpio::{Level, Output, Pull, Speed};
use embassy_stm32::mode::Async;
use embassy_stm32::pac::GPIOB;
use embassy_stm32::pac::gpio::vals::{Idr, Moder, Pupdr};
use embassy_time::{Duration, Instant, Timer};

use crate::board::PirPins;
use crate::clock::delay_us;

/// How long [`Pir::wait_motion`] ignores events after a power-on reset (see the module docs). The
/// STICKER firmware, the same detector in a product, waits the same 30 s.
pub const WARM_UP: Duration = Duration::from_secs(30);

/// How long [`Pir::wait_motion`] ignores events after the first successful [`Pir::configure`]
/// following any reset (see the module docs).
pub const SETTLE: Duration = Duration::from_secs(5);

/// DIRECT LINK: PB2, header P9. (SERIN, PB0, is driven through its `Output`.)
const DL: usize = 2;

/// Each level of a clock edge, SERIN and DIRECT LINK alike (200 ns to 2 µs): `asm::delay(2)` is
/// 12 cycles (see [`delay_us`]), with the register write after it about 0.9 µs.
const CLOCK_CYCLES: u32 = 2;
/// A SERIN bit's value held after its clock edge (t_SHD: 72 µs on the 1648, 80 to 150 µs on the 1548).
const BIT_US: u32 = 100;
/// SERIN low to latch the register (t_SLT, 650 µs on the 1548) and, before the readback, both lines
/// low (2.4 ms, 1548 note 2.1.8).
const LATCH: Duration = Duration::from_millis(5);
/// DIRECT LINK held high to start a readout (t_DS: 110 to 150 µs on the 1648, at least 120 µs on
/// the 1548).
const SETUP_US: u32 = 130;
/// From releasing DIRECT LINK to sampling a bit: the detector's weak driver discharges the module's
/// 47 pF in about 2.4 µs (the 1548 note's 20 µF/s at 3 V); the line must not sit low past 22 µs.
const SETTLE_US: u32 = 4;
/// DIRECT LINK held low to end a readout (t_RA, 160 µs) or to acknowledge an event (one detector
/// clock on the 1648, 160 µs on the 1548).
const LOW_US: u32 = 200;
/// From one readout's start to the next: t_SMPL (2.0 ms on the 1648, 2.4 ms on the 1548), which also
/// covers t_UP (1250 µs from the release).
const FRAME_INTERVAL: Duration = Duration::from_micros(2500);

/// The register's operation modes (bits 8:7).
const OP_FORCED: u32 = 0;
const OP_WAKE_UP: u32 = 2;
/// The register's low five bits: "reserved, must be 16" on the 1648; on the 1548 the same value
/// selects factory bits 0b10, the 0.4 Hz high pass and pulses that must change sign.
const RESERVED: u32 = 0x10;
/// The 25 register bits.
const REGISTER_MASK: u32 = (1 << 25) - 1;

/// What the detector does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Wake-up operation: the detector applies the detection criteria itself and raises DIRECT
    /// LINK on motion; await it with [`Pir::wait_motion`]. The default.
    Motion,
    /// Forced readout: no motion events; the host reads the signal of a [`Source`] with
    /// [`Pir::sample`] instead.
    Signal(Source),
}

/// The signal a [`Mode::Signal`] readout carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The band-pass filtered pyroelectric signal, the one the detection criteria judge: signed
    /// counts of 6.5 µV around 0.
    BandPass = 0,
    /// The low-pass filtered pyroelectric signal: 0 to 16383 counts, about 8000 at rest.
    LowPass = 1,
    /// The detector's temperature sensor: 0 to 16383 counts, 80 counts per kelvin; its offset spreads
    /// by ±1700 counts from part to part, so it tells changes, not degrees.
    Temperature = 3,
}

/// The detector's configuration: the detection criteria and the [`Mode`]. The [`Default`] is the
/// STICKER firmware's tested setting: threshold 32, two pulses within 2 s, a blind time of 1 s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// The detection threshold in ADC counts (6.5 µV each) of the band-pass signal, 0 to 255:
    /// lower is more sensitive.
    pub threshold: u8,
    /// Threshold crossings, each of the opposite sign to the one before, that make a motion event:
    /// 1 to 4 (clamped).
    pub pulses: u8,
    /// The time window the pulses must fall in: 2, 4, 6 or 8 s (rounded down to one of them).
    pub window: Duration,
    /// After an event is acknowledged, the detector ignores motion this long: 0.5 to 8 s in 0.5 s
    /// steps (rounded down to one of them).
    pub blind_time: Duration,
    /// What the detector does.
    pub mode: Mode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: 32,
            pulses: 2,
            window: Duration::from_secs(2),
            blind_time: Duration::from_secs(1),
            mode: Mode::Motion,
        }
    }
}

impl Config {
    /// The 25-bit configuration register: threshold (bits 24:17), blind time (16:13), pulse
    /// count (12:11), window (10:9), operation mode (8:7), filter source (6:5), the reserved 16
    /// (4:0).
    #[must_use]
    pub const fn register(&self) -> u32 {
        let pulses = clamp(self.pulses as u64, 1, 4) - 1;
        let window = (clamp(self.window.as_secs(), 2, 8) - 2) / 2;
        let blind = (clamp(self.blind_time.as_millis(), 500, 8000) - 500) / 500;
        let (op, source) = match self.mode {
            Mode::Motion => (OP_WAKE_UP, Source::BandPass as u32),
            Mode::Signal(source) => (OP_FORCED, source as u32),
        };
        ((self.threshold as u32) << 17)
            | ((blind as u32) << 13)
            | ((pulses as u32) << 11)
            | ((window as u32) << 9)
            | (op << 7)
            | (source << 5)
            | RESERVED
    }
}

const fn clamp(v: u64, lo: u64, hi: u64) -> u64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

// Known answers: the 1548 application note's example registers ("Example function calls"), and
// the STICKER firmware's setting, this driver's default.
const _: () = assert!(
    Config {
        threshold: 24,
        pulses: 2,
        window: Duration::from_secs(6),
        blind_time: Duration::from_millis(1500),
        mode: Mode::Motion,
    }
    .register()
        == 0x0030_4D10
);
const _: () = assert!(
    Config {
        threshold: 96,
        pulses: 3,
        window: Duration::from_secs(8),
        blind_time: Duration::from_millis(500),
        mode: Mode::Motion,
    }
    .register()
        == 0x00C0_1710
);
const _: () = assert!(
    Config {
        threshold: 0,
        pulses: 1,
        window: Duration::from_secs(2),
        blind_time: Duration::from_millis(500),
        mode: Mode::Signal(Source::LowPass),
    }
    .register()
        == 0x0000_0030
);
const _: () = assert!(
    Config {
        threshold: 32,
        pulses: 2,
        window: Duration::from_secs(2),
        blind_time: Duration::from_secs(1),
        mode: Mode::Motion,
    }
    .register()
        == 0x0040_2910
);

impl fmt::Display for Config {
    /// `threshold 32, 2 pulses in 2 s, blind 1 s` (or `signal band pass`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reg = self.register();
        match self.mode {
            Mode::Motion => {
                let pulses = ((reg >> 11) & 3) + 1;
                let window = 2 + 2 * ((reg >> 9) & 3);
                let blind_halves = 1 + ((reg >> 13) & 0xF);
                write!(f, "threshold {}, {} pulse", self.threshold, pulses)?;
                if pulses > 1 {
                    write!(f, "s in {} s", window)?;
                }
                write!(f, ", blind {}", blind_halves / 2)?;
                if blind_halves % 2 == 1 {
                    f.write_str(".5")?;
                }
                f.write_str(" s")
            }
            Mode::Signal(source) => f.write_str(match source {
                Source::BandPass => "signal band pass",
                Source::LowPass => "signal low pass",
                Source::Temperature => "signal temperature",
            }),
        }
    }
}

/// A failed [`Pir::configure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The register's low five bits, which a detector returns as written (16), came back wrong: no
    /// PIR Module on the header, or a stuck line.
    NotDetected,
    /// The register read back differs from the one written.
    Readback {
        /// The register written.
        written: u32,
        /// The register read back.
        read: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotDetected => f.write_str("no detector answers on P9"),
            Error::Readback { written, read } => {
                write!(f, "register read back 0x{read:07x}, written 0x{written:07x}")
            }
        }
    }
}

/// The PIR Module's detector. Keep it alive: it holds SERIN low, as the detector wants between
/// writes.
pub struct Pir {
    serin: Output<'static>,
    dl: ExtiInput<'static, Async>,
    config: Option<Config>,
    /// Motion events count from here on (see [`WARM_UP`]).
    armed_at: Instant,
    /// The earliest start of the next readout ([`FRAME_INTERVAL`]).
    next_frame: Instant,
    /// No `configure` has succeeded yet: the next one starts the [`SETTLE`] time.
    fresh: bool,
}

impl Pir {
    /// Take the PIR Module's pins: SERIN driven low, DIRECT LINK an input with a pull-down (the
    /// detector drives it both ways; the pull-down keeps an empty header quiet). Nothing is
    /// written to the detector until [`configure`](Self::configure).
    pub fn new(pins: PirPins) -> Self {
        let serin = Output::new(pins.serin, Level::Low, Speed::Low);
        let dl = ExtiInput::new(pins.dl, pins.exti, Pull::Down, crate::board::Irqs);
        let power_on = embassy_stm32::pac::rcc::regs::Csr(crate::board::preinit_csr()).porrstf();
        let boot = Instant::from_ticks(0);
        Self {
            serin,
            dl,
            config: None,
            armed_at: if power_on { boot + WARM_UP } else { boot },
            next_frame: boot,
            fresh: true,
        }
    }

    /// Write `config` to the detector and read it back. Takes about 10 ms; interrupts wait for
    /// 2.5 ms of it (the write) and 0.4 ms (the readback). Acknowledges any pending motion event.
    pub async fn configure(&mut self, config: &Config) -> Result<(), Error> {
        let written = config.register();
        // Unknown until the readback says otherwise.
        self.config = None;
        Timer::at(self.next_frame).await;
        // DIRECT LINK low while SERIN is written (1548 note 2.1.1), acknowledging a pending event;
        // SERIN has been low since `new`, longer than the 16 detector clocks a write wants first.
        dl_drive_low();
        Timer::after_millis(1).await;
        critical_section::with(|_| self.write_register(written));
        // Latch, and both lines low for 2.4 ms before the one readout allowed in wake-up mode.
        Timer::after(LATCH).await;
        let read = (read_frame(40) & u64::from(REGISTER_MASK)) as u32;
        self.next_frame = Instant::now() + FRAME_INTERVAL;
        if read != written {
            return Err(if read & 0x1F != RESERVED {
                Error::NotDetected
            } else {
                Error::Readback { written, read }
            });
        }
        self.config = Some(*config);
        if self.fresh {
            self.fresh = false;
            self.armed_at = self.armed_at.max(Instant::now() + SETTLE);
        }
        Ok(())
    }

    /// The configuration the detector holds: the last [`configure`](Self::configure), if it succeeded.
    pub fn config(&self) -> Option<&Config> {
        self.config.as_ref()
    }

    /// How long motion events are still ignored after a power-on ([`WARM_UP`]) or the first
    /// write after a reset ([`SETTLE`]), or `None` when they count.
    pub fn warming_up(&self) -> Option<Duration> {
        let now = Instant::now();
        (now < self.armed_at).then(|| self.armed_at - now)
    }

    /// Wait for a motion event and acknowledge it (which starts the blind time). Sleeps on the
    /// EXTI interrupt, so the MCU stays in STOP meanwhile. Needs [`Mode::Motion`] (in
    /// [`Mode::Signal`] it never returns). Cancel-safe: dropped before it returns, it loses no
    /// event (the detector holds DIRECT LINK high until acknowledged).
    pub async fn wait_motion(&mut self) {
        // A cancelled readout or `configure` can leave DIRECT LINK driven low.
        dl_release();
        loop {
            self.dl.wait_for_high().await;
            // The detector latches the line high, so an event is still there; a glitch is gone.
            let event = self.dl.is_high();
            dl_drive_low();
            delay_us(LOW_US);
            dl_release();
            if event && Instant::now() >= self.armed_at {
                return;
            }
        }
    }

    /// Read the signal in [`Mode::Signal`]: a band-pass value in signed counts, low-pass and
    /// temperature values in 0..=16383; `None` in [`Mode::Motion`] (or before a successful
    /// [`configure`](Self::configure)), where the detector allows no readout. Readouts are at least
    /// 2.5 ms apart (this waits as needed); interrupts wait for 0.2 ms of each.
    pub async fn sample(&mut self) -> Option<i16> {
        let Some(Mode::Signal(source)) = self.config.map(|c| c.mode) else {
            return None;
        };
        Timer::at(self.next_frame).await;
        let raw = (read_frame(15) & 0x3FFF) as i16;
        self.next_frame = Instant::now() + FRAME_INTERVAL;
        Some(match source {
            Source::BandPass => (raw << 2) >> 2,
            _ => raw,
        })
    }

    /// Clock `register` into SERIN, most significant bit first. Call in a critical section.
    fn write_register(&mut self, register: u32) {
        for bit in (0..25).rev() {
            self.serin.set_low();
            cortex_m::asm::delay(CLOCK_CYCLES);
            self.serin.set_high();
            cortex_m::asm::delay(CLOCK_CYCLES);
            if (register >> bit) & 1 == 0 {
                self.serin.set_low();
            }
            delay_us(BIT_US);
        }
        self.serin.set_low();
    }
}

/// Read the first `bits` bits of a DIRECT LINK frame (bit 39 first), into the low bits of the
/// result, then end the readout (DIRECT LINK low for t_RA, then released).
fn read_frame(bits: u32) -> u64 {
    let frame = critical_section::with(|_| {
        // No pull while the detector drives the bits: its driver is weak.
        GPIOB.pupdr().modify(|w| w.set_pupdr(DL, Pupdr::FLOATING));
        dl_set(true);
        GPIOB.moder().modify(|w| w.set_moder(DL, Moder::OUTPUT));
        delay_us(SETUP_US);
        let mut frame = 0u64;
        for _ in 0..bits {
            dl_set(false);
            GPIOB.moder().modify(|w| w.set_moder(DL, Moder::OUTPUT));
            cortex_m::asm::delay(CLOCK_CYCLES);
            dl_set(true);
            cortex_m::asm::delay(CLOCK_CYCLES);
            GPIOB.moder().modify(|w| w.set_moder(DL, Moder::INPUT));
            delay_us(SETTLE_US);
            frame = (frame << 1) | u64::from(GPIOB.idr().read().idr(DL) == Idr::HIGH);
        }
        dl_set(false);
        GPIOB.moder().modify(|w| w.set_moder(DL, Moder::OUTPUT));
        GPIOB.pupdr().modify(|w| w.set_pupdr(DL, Pupdr::PULL_DOWN));
        frame
    });
    delay_us(LOW_US);
    dl_release();
    frame
}

/// Set DIRECT LINK's output latch (it reaches the pin only while the pin is an output).
fn dl_set(high: bool) {
    GPIOB.bsrr().write(|w| {
        if high {
            w.set_bs(DL, true);
        } else {
            w.set_br(DL, true);
        }
    });
}

/// Drive DIRECT LINK low.
fn dl_drive_low() {
    dl_set(false);
    critical_section::with(|_| GPIOB.moder().modify(|w| w.set_moder(DL, Moder::OUTPUT)));
}

/// Release DIRECT LINK back to the (pulled-down) input the EXTI watches.
fn dl_release() {
    critical_section::with(|_| GPIOB.moder().modify(|w| w.set_moder(DL, Moder::INPUT)));
}
