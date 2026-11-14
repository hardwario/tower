//! The TOWER Climate Module: humidity, illuminance and pressure, measured on demand.
//!
//! The module carries an SHT20 (first revision; an SHT30 on the second), an OPT3001, an
//! MPL3115A2 and, on the first revision, a TMP112 of its own, all on the board's I²C bus. The
//! sensor tags carry the same parts at the same addresses, so a Humidity, Lux Meter or Barometer
//! Tag is found and read like the module's part.
//!
//! **Low power.** Nothing runs between measurements: every part converts once when asked and
//! falls back to its sleep by itself (SHT20 0.15 µA, OPT3001 0.3 µA, MPL3115A2 2 µA standby).
//! The module's TMP112 powers up converting four times a second (about 10 µA); it is shut down
//! when found (1 µA) and converts once per round like the others.
//! [`measure`](ClimateModule::measure) starts all conversions together and collects each when
//! it is due, the MCU asleep in between: about 0.9 s per round (the light sensor's 800 ms
//! conversion), a few milliseconds of it awake.
//!
//! **Temperatures.** [`Readings::temperature`] is the module's thermometer: its TMP112 on the
//! first revision, the SHT30's on the second. It sits away from the Core Module's own heat (on
//! a USB powered bench the Core Module's TMP112 read 0.5 °C above it), so it is the one to
//! report as the room's. The humidity sensor and the barometer measure a temperature of their
//! own as well: the barometer's comes with every pressure, the SHT20's costs a second
//! conversion (85 ms, about 25 µC) and is taken only when asked for.
//!
//! **Nothing has to be fitted.** [`probe`](ClimateModule::probe) looks for the parts that are not
//! known yet (a few NACKed addresses when there are none); a part that fails three rounds in a
//! row counts as gone and is looked for again.
//!
//! ```ignore
//! let mut climate = ClimateModule::new(bus);
//! loop {
//!     climate.probe().await;
//!     let r = climate.measure(false).await; // r.temperature, r.humidity, r.illuminance, r.pressure
//!     Timer::after_secs(60).await;
//! }
//! ```

use core::fmt;

use embassy_time::{Duration, Instant, Timer};

use crate::i2c::I2cBus;
use crate::mpl3115a2::{self, Mpl3115a2};
use crate::opt3001::{self, Opt3001};
use crate::sht20::{self, Sht20};
use crate::sht30::{self, Sht30};
use crate::tmp112::{self, Tmp112};

/// The humidity sensor found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Humidity {
    None,
    /// The first revision's (and the Humidity Tag's).
    Sht20,
    /// The second revision's.
    Sht30,
}

/// Which parts answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parts {
    /// The module's TMP112 (first revision; also a Temperature Tag at its address).
    pub thermometer: bool,
    pub humidity: Humidity,
    /// The OPT3001.
    pub light: bool,
    /// The MPL3115A2.
    pub barometer: bool,
}

impl Parts {
    const NONE: Parts = Parts {
        thermometer: false,
        humidity: Humidity::None,
        light: false,
        barometer: false,
    };

    pub fn is_empty(&self) -> bool {
        *self == Self::NONE
    }
}

/// The part names, comma separated (`TMP112, SHT20, OPT3001, MPL3115A2`), or `none`.
impl fmt::Display for Parts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [
            if self.thermometer { "TMP112" } else { "" },
            match self.humidity {
                Humidity::None => "",
                Humidity::Sht20 => "SHT20",
                Humidity::Sht30 => "SHT30",
            },
            if self.light { "OPT3001" } else { "" },
            if self.barometer { "MPL3115A2" } else { "" },
        ];
        let mut first = true;
        for name in names.into_iter().filter(|n| !n.is_empty()) {
            if !first {
                f.write_str(", ")?;
            }
            f.write_str(name)?;
            first = false;
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// One round's values; `None` where the part is absent or did not answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Readings {
    /// The module's thermometer, hundredths of a degree Celsius: its TMP112 (first revision),
    /// else the SHT30's reading (second revision).
    pub temperature: Option<i16>,
    /// Relative humidity, tenths of a percent.
    pub humidity: Option<u16>,
    /// Illuminance, tenths of a lux.
    pub illuminance: Option<u32>,
    /// Pressure, tenths of a hectopascal (10 Pa).
    pub pressure: Option<u16>,
    /// The humidity sensor's own temperature, hundredths of a degree: with every SHT30 reading;
    /// from an SHT20 only when [`measure`](ClimateModule::measure) is asked for every one.
    pub hygrometer_temperature: Option<i16>,
    /// The barometer's die temperature, hundredths of a degree (it comes with the pressure).
    pub barometer_temperature: Option<i16>,
}

/// Rounds in a row without an answer after which a part counts as gone.
const GIVE_UP: u8 = 3;
/// Margin on a part's conversion time before the first read.
const MARGIN: Duration = Duration::from_millis(5);
/// A read that came too early is tried again this much later, a few times.
const RETRY: Duration = Duration::from_millis(10);
const RETRIES: usize = 4;
/// The TMP112's longest one-shot conversion (26 ms typical).
const THERMOMETER: Duration = Duration::from_millis(35);
/// The light sensor restarts its conversion when the light changed range meanwhile: asked again
/// this often, up to [`LIGHT_POLLS`] times (another conversion's worth).
const LIGHT_POLL: Duration = Duration::from_millis(100);
const LIGHT_POLLS: usize = 9;

/// The Climate Module (or the tags carrying its parts) on the board's I²C bus.
pub struct ClimateModule {
    bus: I2cBus,
    parts: Parts,
    /// Rounds in a row without an answer: humidity, light, barometer, thermometer.
    fails: [u8; 4],
}

impl ClimateModule {
    pub const fn new(bus: I2cBus) -> Self {
        Self {
            bus,
            parts: Parts::NONE,
            fails: [0; 4],
        }
    }

    /// The parts found so far.
    pub fn parts(&self) -> Parts {
        self.parts
    }

    /// Look for the parts that are not known yet, and put what is found into its low-power state.
    /// Whether anything new was found. Cheap when everything is known already (no bus traffic).
    pub async fn probe(&mut self) -> bool {
        let before = self.parts;
        if before.thermometer && before.humidity != Humidity::None && before.light && before.barometer {
            return false;
        }
        let mut parts = self.parts;
        self.bus.raw(|b| {
            if !parts.thermometer {
                // Found by shutting it down: it powers up converting four times a second.
                parts.thermometer = Tmp112::new(&mut *b, tmp112::ADDR_GND).shutdown().is_ok();
            }
            if parts.humidity == Humidity::None {
                if Sht20::new(&mut *b).present() {
                    parts.humidity = Humidity::Sht20;
                } else if Sht30::new(&mut *b, sht30::ADDR).present() {
                    parts.humidity = Humidity::Sht30;
                }
            }
            if !parts.light {
                let mut light = Opt3001::new(&mut *b, opt3001::ADDR);
                parts.light = light.present() && light.shutdown().is_ok();
            }
            if !parts.barometer {
                let mut barometer = Mpl3115a2::new(&mut *b);
                parts.barometer = barometer.present() && barometer.init(mpl3115a2::OVERSAMPLING).is_ok();
            }
        });
        self.bus.rest().await;
        self.parts = parts;
        parts != before
    }

    /// Measure once with every part found: start all conversions, collect each when it is due.
    /// Takes about 0.9 s with a light sensor (its conversion), 0.15 s without; the MCU sleeps
    /// through the waits. With `every_thermometer`, an SHT20's own temperature is measured too
    /// (a second conversion, 85 ms, about 25 µC). Returns at once when no part is known
    /// ([`probe`](Self::probe) first).
    pub async fn measure(&mut self, every_thermometer: bool) -> Readings {
        let mut out = Readings::default();
        let parts = self.parts;
        if parts.is_empty() {
            return out;
        }
        let bus = self.bus;
        let start = Instant::now();
        // A part that does not take its trigger is not waited for.
        let (thermometer, humidity, light, barometer) = bus.raw(|b| {
            (
                parts.thermometer && Tmp112::new(&mut *b, tmp112::ADDR_GND).trigger_oneshot().is_ok(),
                match parts.humidity {
                    Humidity::None => false,
                    Humidity::Sht20 => Sht20::new(&mut *b).trigger_humidity().is_ok(),
                    Humidity::Sht30 => Sht30::new(&mut *b, sht30::ADDR).trigger().is_ok(),
                },
                parts.light && Opt3001::new(&mut *b, opt3001::ADDR).trigger().is_ok(),
                parts.barometer && Mpl3115a2::new(&mut *b).trigger(mpl3115a2::OVERSAMPLING).is_ok(),
            )
        });
        bus.rest().await;

        // When the SHT20's temperature conversion (started after its humidity is read) is done.
        let mut hygrometer_at: Option<Instant> = None;
        if humidity {
            let ms = match parts.humidity {
                Humidity::Sht30 => sht30::MEASURE_MS,
                _ => sht20::HUMIDITY_MS,
            };
            Timer::at(start + Duration::from_millis(ms) + MARGIN).await;
            for _ in 0..RETRIES {
                // The sensor NACKs its address while it still converts: that read is tried again.
                let read = bus.raw(|b| match parts.humidity {
                    Humidity::Sht30 => match Sht30::new(&mut *b, sht30::ADDR).read() {
                        Ok((temperature, humidity)) => Some(Some((humidity, Some(temperature)))),
                        Err(sht30::Error::Bus(_)) => None,
                        Err(sht30::Error::Data(_)) => Some(None),
                    },
                    _ => match Sht20::new(&mut *b).read_humidity() {
                        Ok(humidity) => Some(Some((humidity, None))),
                        Err(sht20::Error::Bus(_)) => None,
                        Err(sht20::Error::Data(_)) => Some(None),
                    },
                });
                if let Some(value) = read {
                    if let Some((humidity, temperature)) = value {
                        out.humidity = Some(humidity);
                        out.hygrometer_temperature = temperature;
                    }
                    break;
                }
                Timer::after(RETRY).await;
            }
            // The SHT20 measures one thing at a time: its temperature after its humidity.
            if every_thermometer
                && parts.humidity == Humidity::Sht20
                && out.humidity.is_some()
                && bus.raw(|b| Sht20::new(b).trigger_temperature().is_ok())
            {
                hygrometer_at = Some(Instant::now() + Duration::from_millis(sht20::TEMPERATURE_MS) + MARGIN);
            }
        }
        if thermometer {
            Timer::at(start + THERMOMETER + MARGIN).await;
            for _ in 0..RETRIES {
                let read = bus.raw(|b| {
                    let mut thermometer = Tmp112::new(b, tmp112::ADDR_GND);
                    match thermometer.conversion_ready() {
                        Ok(true) => Some(thermometer.read_raw().ok()),
                        Ok(false) => None,
                        Err(_) => Some(None),
                    }
                });
                if let Some(raw) = read {
                    out.temperature = raw.map(|raw| (tmp112::raw_to_millicelsius(raw) / 10) as i16);
                    break;
                }
                Timer::after(RETRY).await;
            }
        }
        if barometer {
            let ms = u64::from(mpl3115a2::OVERSAMPLING.conversion_ms());
            Timer::at(start + Duration::from_millis(ms) + MARGIN).await;
            for _ in 0..RETRIES {
                let read = bus.raw(|b| {
                    let mut barometer = Mpl3115a2::new(&mut *b);
                    match barometer.ready() {
                        Ok(true) => Some(barometer.read().ok()),
                        Ok(false) => None,
                        Err(_) => Some(None),
                    }
                });
                if let Some(value) = read {
                    if let Some((pressure, temperature)) = value {
                        out.pressure = Some(pressure);
                        out.barometer_temperature = Some(temperature);
                    }
                    break;
                }
                Timer::after(RETRY).await;
            }
        }
        if let Some(at) = hygrometer_at {
            Timer::at(at).await;
            for _ in 0..RETRIES {
                let read = bus.raw(|b| match Sht20::new(b).read_temperature() {
                    Ok(temperature) => Some(Some(temperature)),
                    Err(sht20::Error::Bus(_)) => None,
                    Err(sht20::Error::Data(_)) => Some(None),
                });
                if let Some(value) = read {
                    out.hygrometer_temperature = value;
                    break;
                }
                Timer::after(RETRY).await;
            }
        }
        if light {
            Timer::at(start + Duration::from_millis(opt3001::CONVERSION_MS)).await;
            for _ in 0..LIGHT_POLLS {
                let read = bus.raw(|b| {
                    let mut light = Opt3001::new(&mut *b, opt3001::ADDR);
                    match light.ready() {
                        Ok(true) => Some(light.illuminance().ok()),
                        Ok(false) => None,
                        Err(_) => Some(None),
                    }
                });
                if let Some(value) = read {
                    out.illuminance = value;
                    break;
                }
                Timer::after(LIGHT_POLL).await;
            }
        }
        bus.rest().await;
        let thermometer_answered = out.temperature.is_some();
        // Without a TMP112 (the second revision), the module's thermometer is the SHT30's.
        if out.temperature.is_none() && parts.humidity == Humidity::Sht30 {
            out.temperature = out.hygrometer_temperature;
        }

        // A part silent for `GIVE_UP` rounds is forgotten, and looked for again by `probe` (which
        // also sets a barometer up anew: one that lost its power lost its data-ready flags, and
        // shuts a thermometer down again: one that lost its power is converting on its own).
        let answered = [
            (parts.humidity != Humidity::None, out.humidity.is_some()),
            (parts.light, out.illuminance.is_some()),
            (parts.barometer, out.pressure.is_some()),
            (parts.thermometer, thermometer_answered),
        ];
        for (i, (present, ok)) in answered.into_iter().enumerate() {
            self.fails[i] = if present && !ok { self.fails[i] + 1 } else { 0 };
            if self.fails[i] >= GIVE_UP {
                self.fails[i] = 0;
                match i {
                    0 => self.parts.humidity = Humidity::None,
                    1 => self.parts.light = false,
                    2 => self.parts.barometer = false,
                    _ => self.parts.thermometer = false,
                }
            }
        }
        out
    }
}
