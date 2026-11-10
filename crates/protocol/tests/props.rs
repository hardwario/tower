//! The taf property tables (`src/props.rs`): the invariants the firmware runtime and the bridge
//! rely on, and the value text forms both ends parse and print.

use std::collections::HashSet;

use tower_protocol::props::*;

/// Ids and names are unique within each kind (common ones included), app ids start at
/// `APP_BASE`, names are MQTT- and shell-safe, and every setting has a default its type accepts.
#[test]
fn tables_are_consistent() {
    assert!(COMMON.iter().all(|p| p.id < APP_BASE));
    let mut kinds = HashSet::new();
    let mut kind_ids = HashSet::new();
    for kind in KINDS {
        assert!(kinds.insert(kind.name), "kind {} twice", kind.name);
        assert_ne!(kind.id, KIND_UNKNOWN, "{}: id 0 means an unknown kind", kind.name);
        assert!(
            kind_ids.insert(kind.id),
            "{}: kind id {} twice",
            kind.name,
            kind.id
        );
        assert!(
            kind.props.iter().all(|p| p.id >= APP_BASE),
            "{}: app id below APP_BASE",
            kind.name
        );
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for p in kind.all() {
            assert!(ids.insert(p.id), "{}: id {} twice", kind.name, p.id);
            assert!(names.insert(p.name), "{}: name {} twice", kind.name, p.name);
            assert!(
                !p.name.is_empty()
                    && p.name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{}: name {:?}",
                kind.name,
                p.name
            );
            // The shell settings key must not reach past one byte or into the SDK's range.
            assert!(p.id < 0xE0);
            let setting = matches!(p.access, Access::Param | Access::LocalParam);
            if setting {
                assert!(
                    p.ty.parse(p.default).is_some(),
                    "{}: default {:?} of {}",
                    kind.name,
                    p.default,
                    p.name
                );
            } else {
                assert_eq!(
                    p.default, "",
                    "{}: {} has a default but is not a setting",
                    kind.name, p.name
                );
            }
            if let Type::Number { min, max, .. } = p.ty {
                assert!(min <= max);
            }
        }
    }
}

/// Every kind is found by its name, its firmware name and its id.
#[test]
fn kinds_resolve_by_name_and_firmware() {
    for k in KINDS {
        assert_eq!(kind(k.name), Some(k));
        assert_eq!(kind_of_firmware(&format!("radio_{}", k.name)), Some(k));
        assert_eq!(kind_by_id(k.id), Some(k));
    }
    assert_eq!(kind_by_id(KIND_UNKNOWN), None);
    assert_eq!(kind_by_id(0xFF), None);
    assert_eq!(kind_of_firmware("radio_gateway"), None);
    assert_eq!(kind_of_firmware("button"), None);
    let b = kind("button").unwrap();
    assert_eq!(b.prop(common::TEMPERATURE).unwrap().name, "temperature");
    assert_eq!(b.prop_named("click").unwrap().id, common::CLICK);
    assert_eq!(kind("control").unwrap().class, Class::Listening);
}

/// The kind ids are stored on gateways (the registry's kind byte): pinned, so a renumbering is a
/// decision, not an accident. A new kind takes the next free number.
#[test]
fn kind_ids_are_pinned() {
    let ids: Vec<(&str, u8)> = KINDS.iter().map(|k| (k.name, k.id)).collect();
    assert_eq!(
        ids,
        [
            ("button", 1),
            ("climate", 2),
            ("control", 4),
            ("motion", 3),
            ("thermostat", 5)
        ]
    );
}

/// The property ids are pinned: an id is what both ends put on the air for a value (a report, an
/// event, a set), and `0x20 + id` is the key a node stores the setting under in its EEPROM, so
/// a renumbered property reads another one's stored value. A new property takes the next free
/// id of its table.
#[test]
fn property_ids_are_pinned() {
    let ids = |props: &[Prop]| -> Vec<(&'static str, u8)> { props.iter().map(|p| (p.name, p.id)).collect() };
    assert_eq!(
        ids(COMMON),
        [
            ("temperature", 0),
            ("heartbeat", 1),
            ("report-period", 2),
            ("report-delta", 3),
            ("led", 4),
            ("band", 5),
            ("hold-time", 6),
            ("tilt", 7),
            ("accel", 8),
            ("button", 9),
            ("press", 10),
            ("release", 11),
            ("click", 12),
            ("hold", 13),
            ("click-timeout", 14),
        ]
    );
    assert_eq!(APP_BASE, 16);
    let own = |name: &str| ids(kind(name).unwrap().props);
    assert!(own("button").is_empty());
    assert_eq!(
        own("climate"),
        [
            ("humidity", 16),
            ("illuminance", 17),
            ("pressure", 18),
            ("co2", 19),
            ("humidity-delta", 20),
            ("illuminance-delta", 21),
            ("pressure-delta", 22),
            ("co2-delta", 23),
            ("co2-period", 24),
        ]
    );
    assert_eq!(
        own("motion"),
        [("motion", 16), ("sensitivity", 17), ("holdoff", 18)]
    );
    assert_eq!(
        own("control"),
        [
            ("relay", 16),
            ("relay-boot", 17),
            ("strip", 18),
            ("effect", 19),
            ("brightness", 20),
            ("color", 21),
            ("speed", 22),
            ("relay-module", 23),
        ]
    );
    assert_eq!(
        own("thermostat"),
        [("setpoint", 16), ("mode", 17), ("demand", 18), ("hysteresis", 19)]
    );
    // The id constants the firmware and the bridge use are those ids.
    assert_eq!(
        (motion::MOTION, motion::SENSITIVITY, motion::HOLDOFF),
        (16, 17, 18)
    );
    assert_eq!(
        (
            control::RELAY,
            control::RELAY_BOOT,
            control::STRIP,
            control::EFFECT
        ),
        (16, 17, 18, 19)
    );
    assert_eq!(
        (
            control::BRIGHTNESS,
            control::COLOR,
            control::SPEED,
            control::RELAY_MODULE
        ),
        (20, 21, 22, 23)
    );
    assert_eq!(
        (
            climate::HUMIDITY,
            climate::ILLUMINANCE,
            climate::PRESSURE,
            climate::CO2
        ),
        (16, 17, 18, 19)
    );
    assert_eq!(
        (
            climate::HUMIDITY_DELTA,
            climate::ILLUMINANCE_DELTA,
            climate::PRESSURE_DELTA,
            climate::CO2_DELTA,
            climate::CO2_PERIOD
        ),
        (20, 21, 22, 23, 24)
    );
    assert_eq!(
        (
            thermostat::SETPOINT,
            thermostat::MODE,
            thermostat::DEMAND,
            thermostat::HYSTERESIS
        ),
        (16, 17, 18, 19)
    );
    assert_eq!(
        (common::TEMPERATURE, common::BUTTON, common::CLICK_TIMEOUT),
        (0, 9, 14)
    );
}

/// The settings keys: `0x20 + id`, above the SDK's reserved range.
#[test]
fn setting_keys_are_derived_from_ids() {
    assert_eq!(setting_key(0), 0x20);
    assert_eq!(setting_key(APP_BASE), 0x30);
}

/// The Core Module's thermometer reads below zero: the temperature every kind reports spans the
/// sensor's range, -40 to 125 °C in hundredths.
#[test]
fn the_temperature_spans_the_sensors_range() {
    let temperature = COMMON[common::TEMPERATURE as usize];
    assert_eq!(temperature.name, "temperature");
    let ty = temperature.ty;
    assert!(ty.accepts(-4000) && ty.accepts(-1) && ty.accepts(0) && ty.accepts(12500));
    assert!(!ty.accepts(-4001) && !ty.accepts(12501));
    assert_eq!(ty.parse("-12.5"), Some(-1250));
    assert_eq!(ty.display(-1250).to_string(), "-12.50");
}

#[test]
fn fixed_point_parses_and_prints() {
    assert_eq!(parse_fixed("21.5", 1), Some(215));
    assert_eq!(parse_fixed("21", 1), Some(210));
    assert_eq!(parse_fixed("-0.25", 2), Some(-25));
    assert_eq!(parse_fixed(".5", 1), Some(5));
    assert_eq!(parse_fixed("7", 0), Some(7));
    assert_eq!(parse_fixed("1.25", 1), None, "too many decimals");
    assert_eq!(parse_fixed("1e3", 0), None);
    assert_eq!(parse_fixed("", 0), None);
    assert_eq!(parse_fixed("-", 1), None);
    assert_eq!(parse_fixed("99999999999", 0), None, "overflow");
    let show = |v, d| {
        let mut s = String::new();
        write_fixed(&mut s, v, d).unwrap();
        s
    };
    assert_eq!(show(215, 1), "21.5");
    assert_eq!(show(-25, 2), "-0.25");
    assert_eq!(show(-5, 1), "-0.5");
    assert_eq!(show(7, 0), "7");
    assert_eq!(show(2150, 2), "21.50");
    // Zero has no sign, with or without decimals; the smallest negative step has one.
    assert_eq!(show(0, 0), "0");
    assert_eq!(show(0, 1), "0.0");
    assert_eq!(show(0, 2), "0.00");
    assert_eq!(show(-1, 2), "-0.01");
    assert_eq!(show(i32::MIN, 1), "-214748364.8");
}

#[test]
fn types_parse_validate_and_display() {
    let t = kind("thermostat").unwrap();
    let setpoint = t.prop_named("setpoint").unwrap().ty;
    assert_eq!(setpoint.parse("21.5"), Some(215));
    assert_eq!(setpoint.parse("40"), None, "above max");
    assert_eq!(setpoint.display(215).to_string(), "21.5");
    let mode = t.prop_named("mode").unwrap().ty;
    assert_eq!(mode.parse("manual"), Some(1));
    assert_eq!(mode.parse("eco"), None);
    assert_eq!(mode.display(2).to_string(), "off");
    assert_eq!(mode.display(9).to_string(), "9");
    assert!(!mode.accepts(-1) && !mode.accepts(3) && mode.accepts(0));
    assert_eq!(Type::Bool.parse("on"), Some(1));
    assert_eq!(Type::Bool.parse("false"), Some(0));
    assert_eq!(Type::Bool.parse("2"), None);
    assert_eq!(Type::Bool.display(1).to_string(), "on");
    let color = kind("control").unwrap().prop_named("color").unwrap().ty;
    assert_eq!(color.parse("0x00ff00"), Some(0x00ff00));
    assert_eq!(color.parse("65280"), Some(65280));
    assert_eq!(color.parse("0x1000000"), None, "above 0xffffff");
    assert_eq!(color.parse("0xzz"), None);
    assert_eq!(setpoint.parse("0x10"), None, "hex only for whole numbers");
}

/// The tables are part of the radio tag: the fingerprint is a pure function of them.
#[test]
fn the_tables_feed_the_radio_tag() {
    assert_ne!(PROPS_HASH, 0);
    assert_eq!(
        tower_protocol::radio::RADIO_TAG,
        tower_protocol::fingerprint::fold16(tower_protocol::fingerprint::RADIO_HASH)
    );
}
