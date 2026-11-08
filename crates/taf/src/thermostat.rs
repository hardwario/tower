//! The thermostat's decisions: which setpoint applies, and whether to call for heat.
//!
//! Modes (the `mode` property): **auto** follows the weekly program; a manual change of the
//! setpoint in auto mode (the LCD buttons, or a remote `set`) holds until the program's next
//! switch point, then the program takes over again. **manual** holds its own setpoint. **off**
//! never calls for heat.
//!
//! Demand uses a hysteresis band centred on the setpoint: heat is called for below
//! `setpoint - hysteresis / 2` and released above `setpoint + hysteresis / 2`; in between, the
//! last decision stands, so the boiler is not cycled by sensor noise.

/// The `mode` property's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto = 0,
    Manual = 1,
    Off = 2,
}

impl Mode {
    /// The mode at index `value` (the property's wire value); `None` out of range.
    pub fn from_index(value: i32) -> Option<Self> {
        match value {
            0 => Some(Mode::Auto),
            1 => Some(Mode::Manual),
            2 => Some(Mode::Off),
            _ => None,
        }
    }
}

/// Heating demand with hysteresis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Demand {
    on: bool,
}

impl Demand {
    pub const fn new() -> Self {
        Self { on: false }
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Update with a temperature (`centi`, hundredths of a degree), the setpoint and the
    /// hysteresis (`deci`, tenths); `None` (mode off, or no setpoint) releases the demand.
    pub fn update(
        &mut self,
        temperature_centi: i32,
        setpoint_deci: Option<i16>,
        hysteresis_deci: i16,
    ) -> bool {
        let Some(setpoint) = setpoint_deci else {
            self.on = false;
            return false;
        };
        let setpoint = i32::from(setpoint) * 10;
        let half = i32::from(hysteresis_deci.max(0)) * 10 / 2;
        if temperature_centi < setpoint - half {
            self.on = true;
        } else if temperature_centi > setpoint + half {
            self.on = false;
        }
        self.on
    }
}

/// A manual setpoint that holds until a moment (in auto mode, the program's next switch point).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Override {
    /// The setpoint (tenths) and the local second it ends, if active.
    active: Option<(i16, i64)>,
}

impl Override {
    pub const fn new() -> Self {
        Self { active: None }
    }

    /// Hold `deci` until local second `until`.
    pub fn set(&mut self, deci: i16, until: i64) {
        self.active = Some((deci, until));
    }

    pub fn clear(&mut self) {
        self.active = None;
    }

    /// The override's setpoint at local second `now`, if it still holds (an expired one clears).
    pub fn at(&mut self, now: i64) -> Option<i16> {
        match self.active {
            Some((deci, until)) if now < until => Some(deci),
            Some(_) => {
                self.active = None;
                None
            }
            None => None,
        }
    }
}
