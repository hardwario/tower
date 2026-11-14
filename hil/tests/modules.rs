//! HIL modules group: the Core Module alone, carrying a **Climate Module**, a **CO2 Module** and
//! a **Relay Module** (docs/firmware/modules.md). No radio peer: the apps run unpaired or out of
//! their gateway's range, and everything is checked over the console.
//!
//! 1. `radio_climate` finds every sensor, reads plausible values from each (the shell's `climate
//!    print`) and gets a CO2 reading out of the LP8: the capacitor's first charge, the UART
//!    bridge, the frames and the sensor's own status all have to be right for that.
//! 2. `radio_control` finds the Relay Module, pulses its relay to the boot state and switches it
//!    with the Core button (a simulated click), back and forth.
//!
//! The climate test shortens the node's `report-period` and `co2-period` for its run and puts
//! them back (the bench board keeps its EEPROM).
//!
//! `#[ignore]`d: needs the bench with the three modules on the [core] board; run with
//! `just bench modules_`.

use std::time::{Duration, Instant};

use tower_hil::{Console, Role, bench_or_fail_core, build_image, frame_text};

/// One shell command on the console; its text when the device answered with result 0.
fn shell(console: &mut Console, cmd_id: u16, line: &str) -> String {
    let (result, text) = console
        .shell_roundtrip(cmd_id, line, Duration::from_secs(7))
        .unwrap_or_else(|e| panic!("`{line}`: {e}"));
    assert_eq!(result, 0, "`{line}` answered {result}: {text}");
    text
}

/// The value of `name` in `climate print`'s output (`name: 36.2 %`), if it is there.
fn reading(print: &str, name: &str) -> Option<f64> {
    print.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?.strip_prefix(": ")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// Wait until every one of `needles` has appeared in a log line (in any order).
fn wait_for_all(console: &mut Console, timeout: Duration, needles: &[&str]) {
    let mut seen = vec![false; needles.len()];
    let done = console
        .wait_for(timeout, |f| {
            if let Some(text) = frame_text(f) {
                for (i, needle) in needles.iter().enumerate() {
                    seen[i] |= text.contains(needle);
                }
            }
            seen.iter().all(|s| *s)
        })
        .expect("console read");
    let missing: Vec<_> = needles
        .iter()
        .zip(&seen)
        .filter(|(_, s)| !**s)
        .map(|(n, _)| n)
        .collect();
    assert!(done.is_some(), "not logged within {timeout:?}: {missing:?}");
}

#[test]
#[ignore = "requires the HIL bench (Core Module with the Climate and CO2 Modules)"]
fn modules_climate_reads_every_sensor() {
    let bench = bench_or_fail_core();
    let bin = build_image("app", "radio_climate", "").expect("build radio_climate");
    bench.flash(Role::Core, &bin).expect("flash the Core");
    let mut console = Console::open(&bench.core.serial).expect("open the Core console");
    console.reset_into_app().expect("reset the Core into the app");

    // The first round (right after the 3 s boot indication) finds the parts.
    wait_for_all(
        &mut console,
        Duration::from_secs(15),
        &["sensors: TMP112, SHT20, OPT3001, MPL3115A2", "CO2 Module found"],
    );

    // Short periods for the run: a round every 5 s, CO2 as often as the LP8 allows. The first
    // CO2 measurement still waits for the capacitor's first charge (a minute).
    shell(&mut console, 1, "system settings set report-period 5");
    shell(&mut console, 2, "system settings set co2-period 16");

    let started = Instant::now();
    let mut print = String::new();
    let mut cmd_id = 10;
    while started.elapsed() < Duration::from_secs(100) {
        print = shell(&mut console, cmd_id, "climate print");
        cmd_id += 1;
        if reading(&print, "co2").is_some() {
            break;
        }
        // Keep decoding meanwhile, so a warning the app logs is seen below.
        let warned = console
            .wait_for(Duration::from_secs(5), |f| {
                frame_text(f).is_some_and(|t| t.contains("CO2 not measured") || t.contains("not valid"))
            })
            .expect("console read");
        assert!(
            warned.is_none(),
            "the CO2 measurement failed: {:?}",
            warned.as_ref().and_then(frame_text)
        );
    }
    let co2_after = started.elapsed();

    // Put the node's settings back before judging, so a failed assertion leaves the bench as it was.
    shell(&mut console, 90, "system settings set report-period 60");
    shell(&mut console, 91, "system settings set co2-period 300");

    eprintln!("HIL: climate print after {co2_after:.0?}:\n{print}");
    assert!(
        print.contains("sensors: TMP112, SHT20, OPT3001, MPL3115A2, CO2 Module"),
        "{print}"
    );
    let value = |name: &str| reading(&print, name).unwrap_or_else(|| panic!("no {name} in:\n{print}"));
    // An indoor bench: generous bounds that still catch a wrong conversion or byte order.
    // The room's temperature is the Climate Module's thermometer.
    let (t, h, lx, p, co2) = (
        value("temperature/climate"),
        value("humidity"),
        value("illuminance"),
        value("pressure"),
        value("co2"),
    );
    assert!((5.0..=45.0).contains(&t), "temperature/climate {t} °C");
    assert!((5.0..=95.0).contains(&h), "humidity {h} %");
    assert!((0.0..=83865.6).contains(&lx), "illuminance {lx} lx");
    assert!((850.0..=1085.0).contains(&p), "pressure {p} hPa");
    assert!((300.0..=5000.0).contains(&co2), "CO2 {co2} ppm");
    // Every other thermometer of the stack, each under its own name in the group
    // `temperature/`: within a few degrees of the module's (the Core Module's runs warm on
    // USB, the barometer's die reads low). No plain `temperature`: a node has none.
    for source in ["system", "hygrometer", "barometer", "co2"] {
        let v = value(&format!("temperature/{source}"));
        assert!((v - t).abs() <= 4.0, "temperature/{source} {v} °C against {t} °C");
    }
    assert!(
        reading(&print, "temperature").is_none(),
        "a plain temperature in:\n{print}"
    );
    // The first charge is a minute; the measurement runs in the first round after it.
    assert!(
        co2_after >= Duration::from_secs(40),
        "a CO2 value after {co2_after:?}: the capacitor's first charge was skipped"
    );

    // The stack the sensors task reached, against the app's budget.
    let stack = shell(&mut console, 92, "system stack print");
    eprintln!("HIL: {}", stack.replace('\n', "; "));
    assert_eq!(
        console.decode_errors(),
        0,
        "corrupt console frames during the run"
    );
}

#[test]
#[ignore = "requires the HIL bench (Core Module with the Relay Module)"]
fn modules_relay_switches_with_the_button() {
    let bench = bench_or_fail_core();
    let bin = build_image("app", "radio_control", "").expect("build radio_control");
    bench.flash(Role::Core, &bin).expect("flash the Core");
    let mut console = Console::open(&bench.core.serial).expect("open the Core console");
    console.reset_into_app().expect("reset the Core into the app");

    // Found at start and pulsed to its boot state (`relay-boot` off unless the board says else).
    wait_for_all(
        &mut console,
        Duration::from_secs(15),
        &["Relay Module found", "signal-relay o"],
    );
    let boot = shell(&mut console, 1, "system settings get relay-boot");
    assert!(
        boot.contains("= off"),
        "the bench board's relay-boot is not the default: {boot}"
    );

    // Past the power-up gesture's 3 s window (a press inside it would start the gesture).
    let _ = console.collect_for(Duration::from_secs(4)).expect("console read");

    // A click switches both relays: the Relay Module's follows the Power Module's.
    for (round, state) in [(0u16, "on"), (1, "off"), (2, "on"), (3, "off")] {
        shell(&mut console, 10 + round, "button simulate 100");
        wait_for_all(
            &mut console,
            Duration::from_secs(5),
            &[&format!("power-relay {state}"), &format!("signal-relay {state}")],
        );
        eprintln!("HIL: click {round}: signal-relay {state}");
    }
    assert_eq!(
        console.decode_errors(),
        0,
        "corrupt console frames during the run"
    );
}
