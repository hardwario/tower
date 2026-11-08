//! APDS-9960 proximity and gesture sensor driver, gesture engine only (HAL-independent, over
//! [`embedded_hal::i2c::I2c`]).
//!
//! The sensor pulses its IR LED and watches the reflection on four photodiodes (up, down, left,
//! right). With the gesture engine enabled it runs proximity checks, a wait between each; when a
//! hand comes closer than [`GestureConfig::enter`] it switches to gesture mode, records datasets
//! of the four channels into a 32-entry FIFO, raises its interrupt (active low, open drain) as
//! the FIFO fills, and leaves gesture mode when every channel falls under
//! [`GestureConfig::exit`]. The host reads the datasets and decides the direction
//! ([`tower_hmi::gesture::Decoder`]).
//!
//! **Power.** Powered down (the state after power-on, and what [`Apds9960::power_down`] restores)
//! it draws about 1 µA. With the gesture engine on it never sleeps: the wait state alone is
//! ~40 µA, and every proximity check pulses the LED (8 pulses of 16 µs at 100 mA here, ~13 µC),
//! so at the default 50 ms wait it averages roughly 0.3 mA. Enable it only while someone is
//! interacting, and power it down after.
//!
//! The register values follow the SparkFun APDS-9960 library's gesture defaults, except a
//! longer proximity wait (SparkFun: 2.78 ms), no LED boost (SparkFun: 300 %, a current spike a
//! coin cell or two AAA cells take badly) and half the gesture gain (saturation, measured on the
//! LCD Module).

use embedded_hal::i2c::I2c;
use tower_hmi::gesture::Sample;

/// The fixed I²C address.
pub const ADDR: u8 = 0x39;

const ENABLE: u8 = 0x80;
const WTIME: u8 = 0x83;
const PPULSE: u8 = 0x8E;
const CONTROL: u8 = 0x8F;
const CONFIG1: u8 = 0x8D;
const CONFIG2: u8 = 0x90;
pub(crate) const ID: u8 = 0x92;
const CONFIG3: u8 = 0x9F;
const GPENTH: u8 = 0xA0;
const GEXTH: u8 = 0xA1;
const GCONF1: u8 = 0xA2;
const GCONF2: u8 = 0xA3;
const GOFFSET_U: u8 = 0xA4;
const GOFFSET_D: u8 = 0xA5;
const GPULSE: u8 = 0xA6;
const GOFFSET_L: u8 = 0xA7;
const GOFFSET_R: u8 = 0xA9;
const GCONF3: u8 = 0xAA;
const GCONF4: u8 = 0xAB;
const GFLVL: u8 = 0xAE;
const AICLEAR: u8 = 0xE7;
const GFIFO_U: u8 = 0xFC;

// ENABLE bits.
const PON: u8 = 0x01;
const PEN: u8 = 0x04;
const WEN: u8 = 0x08;
const GEN: u8 = 0x40;
// GCONF4 bits.
const GMODE: u8 = 0x01;
const GIEN: u8 = 0x02;
const GFIFO_CLR: u8 = 0x04;

/// Whether `id` (the ID register) is an APDS-9960's.
pub fn is_id(id: u8) -> bool {
    matches!(id, 0xAB | 0xA8 | 0x9C)
}

/// Gesture engine settings.
#[derive(Clone, Copy, Debug)]
pub struct GestureConfig {
    /// Wait between proximity checks while no hand is near, in milliseconds (2.78 ms steps,
    /// up to 711 ms): how fast a hand is noticed, against the current drawn meanwhile.
    pub wait_ms: u16,
    /// Proximity level (0 to 255) that starts the gesture engine.
    pub enter: u8,
    /// Level under which, on every photodiode, the gesture ends.
    pub exit: u8,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            wait_ms: 50,
            enter: 40,
            exit: 30,
        }
    }
}

/// An APDS-9960 on an I²C bus.
pub struct Apds9960<I2C> {
    i2c: I2C,
}

impl<I2C: I2c> Apds9960<I2C> {
    pub fn new(i2c: I2C) -> Self {
        Self { i2c }
    }

    /// The ID register (see [`is_id`]).
    pub fn id(&mut self) -> Result<u8, I2C::Error> {
        self.read(ID)
    }

    /// Everything off (~1 µA).
    pub fn power_down(&mut self) -> Result<(), I2C::Error> {
        self.write(ENABLE, 0)
    }

    /// Configure and start the gesture engine, its interrupt raised as the FIFO fills.
    pub fn start_gestures(&mut self, cfg: &GestureConfig) -> Result<(), I2C::Error> {
        self.write(ENABLE, 0)?;
        // Wait between proximity checks: (256 - WTIME) x 2.78 ms (CONFIG1.WLONG off).
        let counts = (cfg.wait_ms as u32 * 100 / 278).clamp(1, 256);
        self.write(WTIME, (256 - counts) as u8)?;
        self.write(CONFIG1, 0x60)?; // reserved bits as specified, WLONG off
        self.write(PPULSE, 0x87)?; // proximity: 16 µs pulses, 8 of them
        self.write(CONTROL, 0x09)?; // LED 100 mA, proximity gain 4x, ALS gain 4x
        self.write(CONFIG2, 0x01)?; // LED boost 100 % (bit 0 reserved, set)
        self.write(CONFIG3, 0x00)?; // all four photodiodes
        self.write(GPENTH, cfg.enter)?;
        self.write(GEXTH, cfg.exit)?;
        self.write(GCONF1, 0x40)?; // interrupt at 4 datasets, exit after the first one below
        // Gesture gain 2x (SparkFun: 4x, which saturates all four channels at 255 for most of a
        // swipe on the LCD Module), LED 100 mA, 2.8 ms between datasets.
        self.write(GCONF2, 0x21)?;
        for reg in [GOFFSET_U, GOFFSET_D, GOFFSET_L, GOFFSET_R] {
            self.write(reg, 0)?;
        }
        self.write(GPULSE, 0xC9)?; // gesture: 32 µs pulses, 10 of them
        self.write(GCONF3, 0x00)?; // all dimensions
        self.write(GCONF4, GIEN | GFIFO_CLR)?;
        self.i2c.write(ADDR, &[AICLEAR])?; // clear the other interrupts
        self.write(ENABLE, PON | WEN | PEN | GEN)
    }

    /// Datasets waiting in the FIFO.
    pub fn fifo_level(&mut self) -> Result<u8, I2C::Error> {
        self.read(GFLVL)
    }

    /// Whether the engine is in gesture mode (a hand is over the sensor).
    pub fn gesture_active(&mut self) -> Result<bool, I2C::Error> {
        Ok(self.read(GCONF4)? & GMODE != 0)
    }

    /// Read the FIFO into `out`, up to its length or the FIFO level; returns how many.
    pub fn read_fifo(&mut self, out: &mut [Sample]) -> Result<usize, I2C::Error> {
        let n = (self.fifo_level()? as usize).min(out.len());
        let mut done = 0;
        while done < n {
            // At most 8 datasets (32 bytes) per burst; each read pops what it reads.
            let k = (n - done).min(8);
            let mut b = [0u8; 32];
            self.i2c.write_read(ADDR, &[GFIFO_U], &mut b[..4 * k])?;
            for (s, &[up, down, left, right]) in out[done..done + k].iter_mut().zip(b.as_chunks::<4>().0) {
                *s = Sample {
                    up,
                    down,
                    left,
                    right,
                };
            }
            done += k;
        }
        Ok(n)
    }

    /// Empty the FIFO, which also releases the interrupt line.
    pub fn clear_fifo(&mut self) -> Result<(), I2C::Error> {
        self.write(GCONF4, GIEN | GFIFO_CLR)
    }

    /// Release the bus.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn read(&mut self, reg: u8) -> Result<u8, I2C::Error> {
        let mut v = [0u8];
        self.i2c.write_read(ADDR, &[reg], &mut v)?;
        Ok(v[0])
    }

    fn write(&mut self, reg: u8, value: u8) -> Result<(), I2C::Error> {
        self.i2c.write(ADDR, &[reg, value])
    }
}
