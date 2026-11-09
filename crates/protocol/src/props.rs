//! The TOWER application property schema (taf, the TOWER Application Framework).
//!
//! A standard TOWER app does not invent radio messages: it declares **properties**, and the
//! framework carries their values in three generic messages
//! ([`NodeMsg::Report`](crate::radio::NodeMsg::Report), [`NodeMsg::Event`](crate::radio::NodeMsg::Event),
//! [`NodeCmd::Set`](crate::radio::NodeCmd::Set)). Every value on the wire is an `i32`; the
//! property's [`Type`] says what it means (a flag, a fixed-point quantity with a unit, an index
//! into names). The tables below are the contract between a node and the host: the firmware's
//! runtime derives its shell settings from them, the bridge derives its MQTT topics and validates
//! commands against them, so an app is added here once and neither end needs code per property.
//!
//! Each app is a [`Kind`] (named after its firmware, `radio_<kind>`), with its own properties
//! (ids [`APP_BASE`] and up) on top of the [`COMMON`] ones every standard app has (ids below
//! [`APP_BASE`]). A fingerprint of every table is folded into
//! [`RADIO_TAG`](crate::radio::RADIO_TAG), so a node and a host built from different tables refuse
//! each other's envelopes like any other schema change.

use core::fmt;

/// The first id of an app's own properties; [`COMMON`] uses the ids below.
pub const APP_BASE: u8 = 16;

/// The shell settings key of a [`Access::Param`] / [`Access::LocalParam`] property: `0x20 + id`
/// (the shell reserves `0x00..=0x0F`; `0x10..=0x1F` stay free for non-taf apps). Derived, so two
/// properties can never alias one stored setting.
pub const fn setting_key(id: u8) -> u8 {
    0x20 + id
}

/// What a property's `i32` value means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    /// `0` or `1`.
    Bool,
    /// A quantity: the wire value is the quantity times 10^`decimals`, within `min..=max` (both
    /// in wire units), in `unit` (empty when dimensionless).
    Number {
        min: i32,
        max: i32,
        decimals: u8,
        unit: &'static str,
    },
    /// An index into `names`.
    Enum(&'static [&'static str]),
}

/// Who writes a property and where it lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Measured or derived by the node; reported, retained on MQTT.
    State,
    /// A state the host may also set (a relay, a setpoint). Reported like a state; the app
    /// decides whether it survives a reboot.
    Control,
    /// Something that happened; the value is its detail. Not retained.
    Event,
    /// A setting kept in the node's EEPROM; settable over the radio and the console.
    Param,
    /// A setting kept in the node's EEPROM; settable over the USB console only (changing it over
    /// the radio could cut the link, like the band).
    LocalParam,
}

/// One property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prop {
    pub id: u8,
    /// The MQTT topic leaf and the shell setting name: lowercase, `-` between words.
    pub name: &'static str,
    pub ty: Type,
    pub access: Access,
    /// A setting's value until one is stored, as the shell parses it (`""` for non-settings).
    pub default: &'static str,
}

/// How a node uses the radio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Sleeps between uplinks: a downlink waits on the gateway for the node's next uplink.
    Sleeping,
    /// Keeps its receiver on (mains powered): a downlink is sent at once.
    Listening,
}

/// A standard TOWER app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kind {
    /// The MQTT kind; the firmware is `radio_<name>`.
    pub name: &'static str,
    pub class: Class,
    /// The app needs wall-clock time (the bridge syncs it over the radio).
    pub time: bool,
    /// The app's own properties, ids from [`APP_BASE`].
    pub props: &'static [Prop],
}

impl Kind {
    /// The property `id`: a [`COMMON`] one or the app's own.
    pub fn prop(&self, id: u8) -> Option<&'static Prop> {
        COMMON.iter().chain(self.props).find(|p| p.id == id)
    }

    /// The property called `name`.
    pub fn prop_named(&self, name: &str) -> Option<&'static Prop> {
        COMMON.iter().chain(self.props).find(|p| p.name == name)
    }

    /// Every property of the kind, [`COMMON`] first.
    pub fn all(&self) -> impl Iterator<Item = &'static Prop> {
        COMMON.iter().chain(self.props)
    }
}

const fn bool_prop(id: u8, name: &'static str, access: Access, default: &'static str) -> Prop {
    Prop {
        id,
        name,
        ty: Type::Bool,
        access,
        default,
    }
}

const fn number(min: i32, max: i32, decimals: u8, unit: &'static str) -> Type {
    Type::Number {
        min,
        max,
        decimals,
        unit,
    }
}

/// Common property ids.
pub mod common {
    pub const TEMPERATURE: u8 = 0;
    pub const HEARTBEAT: u8 = 1;
    pub const REPORT_PERIOD: u8 = 2;
    pub const REPORT_DELTA: u8 = 3;
    pub const LED: u8 = 4;
    pub const BAND: u8 = 5;
    pub const HOLD_TIME: u8 = 6;
}

/// The radio bands, by [`common::BAND`] index.
pub const BANDS: &[&str] = &["eu868", "us915"];

/// The properties every standard app has.
pub const COMMON: &[Prop] = &[
    Prop {
        id: common::TEMPERATURE,
        name: "temperature",
        ty: number(-4000, 12500, 2, "°C"),
        access: Access::State,
        default: "",
    },
    Prop {
        id: common::HEARTBEAT,
        name: "heartbeat",
        ty: number(60, 86400, 0, "s"),
        access: Access::Param,
        default: "900",
    },
    Prop {
        id: common::REPORT_PERIOD,
        name: "report-period",
        ty: number(5, 86400, 0, "s"),
        access: Access::Param,
        default: "60",
    },
    Prop {
        id: common::REPORT_DELTA,
        name: "report-delta",
        ty: number(0, 1000, 2, "°C"),
        access: Access::Param,
        default: "0.50",
    },
    bool_prop(common::LED, "led", Access::Param, "on"),
    Prop {
        id: common::BAND,
        name: "band",
        ty: Type::Enum(BANDS),
        access: Access::LocalParam,
        default: "eu868",
    },
    Prop {
        id: common::HOLD_TIME,
        name: "hold-time",
        ty: number(100, 10000, 0, "ms"),
        access: Access::Param,
        default: "1000",
    },
];

/// `radio_button`: a battery push button with a tilt alarm.
pub mod button {
    use super::*;

    pub const BUTTON: u8 = APP_BASE;
    pub const TILT: u8 = APP_BASE + 1;
    pub const PRESS: u8 = APP_BASE + 2;
    pub const RELEASE: u8 = APP_BASE + 3;
    pub const CLICK: u8 = APP_BASE + 4;
    pub const HOLD: u8 = APP_BASE + 5;
    pub const ACCEL: u8 = APP_BASE + 6;
    pub const CLICK_TIMEOUT: u8 = APP_BASE + 7;

    /// [`BUTTON`] event values.
    pub const EVENTS: &[&str] = &["press", "release", "click", "hold"];
    /// [`ACCEL`] (tilt alarm sensitivity) values.
    pub const SENSITIVITY: &[&str] = &["off", "low", "medium", "high"];

    pub const KIND: Kind = Kind {
        name: "button",
        class: Class::Sleeping,
        time: false,
        props: &[
            Prop {
                id: BUTTON,
                name: "button",
                ty: Type::Enum(EVENTS),
                access: Access::Event,
                default: "",
            },
            Prop {
                id: TILT,
                name: "tilt",
                ty: number(0, 6, 0, ""),
                access: Access::Event,
                default: "",
            },
            bool_prop(PRESS, "press", Access::Param, "off"),
            bool_prop(RELEASE, "release", Access::Param, "off"),
            bool_prop(CLICK, "click", Access::Param, "on"),
            bool_prop(HOLD, "hold", Access::Param, "on"),
            Prop {
                id: ACCEL,
                name: "accel",
                ty: Type::Enum(SENSITIVITY),
                access: Access::Param,
                default: "medium",
            },
            Prop {
                id: CLICK_TIMEOUT,
                name: "click-timeout",
                ty: number(50, 5000, 0, "ms"),
                access: Access::Param,
                default: "500",
            },
        ],
    };
}

/// `radio_climate`: a climate sensor (the Core Module's thermometer; the Climate Module's
/// sensors are to come).
pub mod climate {
    use super::*;

    pub const KIND: Kind = Kind {
        name: "climate",
        class: Class::Sleeping,
        time: false,
        props: &[],
    };
}

/// `radio_motion`: a PIR Module motion detector.
pub mod motion {
    use super::*;

    pub const MOTION: u8 = APP_BASE;
    pub const SENSITIVITY: u8 = APP_BASE + 1;
    pub const HOLDOFF: u8 = APP_BASE + 2;

    /// [`SENSITIVITY`] values, least sensitive first.
    pub const LEVELS: &[&str] = &["low", "medium", "high", "very-high"];

    pub const KIND: Kind = Kind {
        name: "motion",
        class: Class::Sleeping,
        time: false,
        props: &[
            // The value is how many detections the event stands for (several within `holdoff`
            // travel as one uplink).
            Prop {
                id: MOTION,
                name: "motion",
                ty: number(1, 65535, 0, ""),
                access: Access::Event,
                default: "",
            },
            Prop {
                id: SENSITIVITY,
                name: "sensitivity",
                ty: Type::Enum(LEVELS),
                access: Access::Param,
                default: "high",
            },
            Prop {
                id: HOLDOFF,
                name: "holdoff",
                ty: number(1, 3600, 0, "s"),
                access: Access::Param,
                default: "10",
            },
        ],
    };
}

/// `radio_control`: the Power Module's relay and LED strip, a listening node.
pub mod control {
    use super::*;

    pub const RELAY: u8 = APP_BASE;
    pub const RELAY_BOOT: u8 = APP_BASE + 1;
    pub const STRIP: u8 = APP_BASE + 2;
    pub const EFFECT: u8 = APP_BASE + 3;
    pub const BRIGHTNESS: u8 = APP_BASE + 4;
    pub const COLOR: u8 = APP_BASE + 5;
    pub const SPEED: u8 = APP_BASE + 6;

    /// [`RELAY_BOOT`] values: the relay after a power-up.
    pub const RELAY_BOOT_STATES: &[&str] = &["off", "on", "last"];

    /// [`EFFECT`] values (the `led_strip` effect engine's order).
    pub const EFFECTS: &[&str] = &[
        "rainbow", "palette", "fire", "twinkle", "candle", "plasma", "scanner", "balls", "meteor", "sparkle",
        "running", "chase", "wipe", "curtain", "breathe", "solid", "sunrise", "clock",
    ];

    pub const KIND: Kind = Kind {
        name: "control",
        class: Class::Listening,
        time: true,
        props: &[
            bool_prop(RELAY, "relay", Access::Control, ""),
            Prop {
                id: RELAY_BOOT,
                name: "relay-boot",
                ty: Type::Enum(RELAY_BOOT_STATES),
                access: Access::Param,
                default: "off",
            },
            bool_prop(STRIP, "strip", Access::Control, ""),
            Prop {
                id: EFFECT,
                name: "effect",
                ty: Type::Enum(EFFECTS),
                access: Access::Control,
                default: "",
            },
            Prop {
                id: BRIGHTNESS,
                name: "brightness",
                ty: number(0, 100, 0, "%"),
                access: Access::Control,
                default: "",
            },
            // 0xRRGGBB.
            Prop {
                id: COLOR,
                name: "color",
                ty: number(0, 0xFF_FFFF, 0, ""),
                access: Access::Control,
                default: "",
            },
            Prop {
                id: SPEED,
                name: "speed",
                ty: number(10, 1000, 0, "%"),
                access: Access::Control,
                default: "",
            },
        ],
    };
}

/// `radio_thermostat`: an LCD thermostat with a weekly program; reports its heating demand.
pub mod thermostat {
    use super::*;

    pub const SETPOINT: u8 = APP_BASE;
    pub const MODE: u8 = APP_BASE + 1;
    pub const DEMAND: u8 = APP_BASE + 2;
    pub const HYSTERESIS: u8 = APP_BASE + 3;

    /// [`MODE`] values.
    pub const MODES: &[&str] = &["auto", "manual", "off"];

    pub const KIND: Kind = Kind {
        name: "thermostat",
        class: Class::Sleeping,
        time: true,
        props: &[
            Prop {
                id: SETPOINT,
                name: "setpoint",
                ty: number(50, 300, 1, "°C"),
                access: Access::Control,
                default: "",
            },
            Prop {
                id: MODE,
                name: "mode",
                ty: Type::Enum(MODES),
                access: Access::Control,
                default: "",
            },
            bool_prop(DEMAND, "demand", Access::State, ""),
            Prop {
                id: HYSTERESIS,
                name: "hysteresis",
                ty: number(1, 50, 1, "°C"),
                access: Access::Param,
                default: "0.3",
            },
        ],
    };
}

/// Every standard app.
pub const KINDS: &[Kind] = &[
    button::KIND,
    climate::KIND,
    control::KIND,
    motion::KIND,
    thermostat::KIND,
];

/// The kind called `name`.
pub fn kind(name: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.name == name)
}

/// The kind a firmware name stands for: `radio_<kind>`.
pub fn kind_of_firmware(firmware_name: &str) -> Option<&'static Kind> {
    kind(firmware_name.strip_prefix("radio_")?)
}

impl Type {
    /// Whether `value` is valid for this type.
    pub fn accepts(&self, value: i32) -> bool {
        match *self {
            Type::Bool => value == 0 || value == 1,
            Type::Number { min, max, .. } => (min..=max).contains(&value),
            Type::Enum(names) => usize::try_from(value).is_ok_and(|i| i < names.len()),
        }
    }

    /// Parse the text form (`on`/`off`/`true`/`false`/`1`/`0`, a decimal like `21.5`, a whole
    /// number in hex like `0x00ff00`, an enum name) into a wire value; `None` when it is not valid
    /// for the type.
    pub fn parse(&self, text: &str) -> Option<i32> {
        let value = match *self {
            Type::Bool => match text {
                "on" | "true" | "1" => 1,
                "off" | "false" | "0" => 0,
                _ => return None,
            },
            // A whole number may also be hex (`0x00ff00`: a colour reads best that way).
            Type::Number { decimals: 0, .. } if text.starts_with("0x") || text.starts_with("0X") => {
                i32::try_from(u32::from_str_radix(&text[2..], 16).ok()?).ok()?
            }
            Type::Number { decimals, .. } => parse_fixed(text, decimals)?,
            Type::Enum(names) => names.iter().position(|n| *n == text)? as i32,
        };
        self.accepts(value).then_some(value)
    }

    /// The text form of a wire value: `on`/`off`, `21.5`, an enum name (an out-of-range enum
    /// index prints as the number).
    pub fn display(&self, value: i32) -> Display {
        Display { ty: *self, value }
    }
}

/// The text form of a property value; see [`Type::display`].
pub struct Display {
    ty: Type,
    value: i32,
}

impl fmt::Display for Display {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty {
            Type::Bool => f.write_str(if self.value != 0 { "on" } else { "off" }),
            Type::Number { decimals, .. } => write_fixed(f, self.value, decimals),
            Type::Enum(names) => match usize::try_from(self.value).ok().and_then(|i| names.get(i)) {
                Some(name) => f.write_str(name),
                None => write!(f, "{}", self.value),
            },
        }
    }
}

/// Parse a decimal with at most `decimals` fraction digits into the integer `value *
/// 10^decimals` (`"21.5"`, 1 -> 215; `"-0.25"`, 2 -> -25; `"7"`, 1 -> 70).
pub fn parse_fixed(text: &str, decimals: u8) -> Option<i32> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int, frac) = match digits.split_once('.') {
        Some((i, f)) => (i, f),
        None => (digits, ""),
    };
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if frac.len() > decimals as usize || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: i64 = 0;
    for b in int.bytes().chain(frac.bytes()) {
        value = value.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
    }
    for _ in frac.len()..decimals as usize {
        value = value.checked_mul(10)?;
    }
    let value = if negative { -value } else { value };
    i32::try_from(value).ok()
}

/// Write `value / 10^decimals` with exactly `decimals` fraction digits.
pub fn write_fixed(f: &mut impl fmt::Write, value: i32, decimals: u8) -> fmt::Result {
    if decimals == 0 {
        return write!(f, "{value}");
    }
    let scale = 10i64.pow(u32::from(decimals));
    let v = i64::from(value);
    let sign = if v < 0 { "-" } else { "" };
    let a = v.abs();
    write!(
        f,
        "{sign}{}.{:0width$}",
        a / scale,
        a % scale,
        width = decimals as usize
    )
}

// ---- the fingerprint folded into RADIO_TAG -----------------------------------------------------

const fn feed(mut state: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        state ^= bytes[i] as u64;
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    state
}

const fn feed_str(state: u64, s: &str) -> u64 {
    feed(feed(state, &(s.len() as u32).to_le_bytes()), s.as_bytes())
}

const fn feed_i32(state: u64, v: i32) -> u64 {
    feed(state, &v.to_le_bytes())
}

const fn hash_prop(mut state: u64, p: &Prop) -> u64 {
    state = feed(state, &[p.id, p.access as u8]);
    state = feed_str(state, p.name);
    state = feed_str(state, p.default);
    match p.ty {
        Type::Bool => feed(state, &[0]),
        Type::Number {
            min,
            max,
            decimals,
            unit,
        } => {
            state = feed(state, &[1, decimals]);
            state = feed_i32(feed_i32(state, min), max);
            feed_str(state, unit)
        }
        Type::Enum(names) => {
            state = feed(state, &[2, names.len() as u8]);
            let mut i = 0;
            while i < names.len() {
                state = feed_str(state, names[i]);
                i += 1;
            }
            state
        }
    }
}

const fn hash_props(mut state: u64, props: &[Prop]) -> u64 {
    state = feed(state, &[props.len() as u8]);
    let mut i = 0;
    while i < props.len() {
        state = hash_prop(state, &props[i]);
        i += 1;
    }
    state
}

const fn tables_hash() -> u64 {
    let mut state = hash_props(0xcbf2_9ce4_8422_2325, COMMON);
    let mut i = 0;
    while i < KINDS.len() {
        let k = &KINDS[i];
        state = feed_str(state, k.name);
        state = feed(state, &[k.class as u8, k.time as u8]);
        state = hash_props(state, k.props);
        i += 1;
    }
    state
}

/// A 64-bit fingerprint of [`COMMON`] and every [`KINDS`] table (ids, names, types, access,
/// defaults, classes), folded into [`RADIO_TAG`](crate::radio::RADIO_TAG).
pub const PROPS_HASH: u64 = tables_hash();
