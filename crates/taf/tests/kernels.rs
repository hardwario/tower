//! The taf kernels: report policy, wall clock, weekly program, thermostat demand.

use tower_taf::clock::{Local, WallClock};
use tower_taf::program::{Day, ParseError, Week, minutes_until};
use tower_taf::report::Reporter;
use tower_taf::thermostat::{Demand, Mode, Override};

#[test]
fn reporter_sends_first_on_delta_and_on_heartbeat() {
    let mut r = Reporter::new();
    assert!(r.due(2150, 0, 50, 900), "the first value always goes");
    r.sent(2150, 0);
    assert!(!r.due(2190, 60, 50, 900), "under the delta");
    assert!(r.due(2200, 60, 50, 900), "at the delta");
    assert!(r.due(2100, 60, 50, 900), "down by the delta");
    assert!(!r.due(2150, 899, 50, 900), "unchanged, before the heartbeat");
    assert!(r.due(2150, 900, 50, 900), "unchanged, at the heartbeat");
    assert!(!r.due(2150, 60, 0, 900), "a zero delta sends changes only");
    assert!(r.due(2151, 60, 0, 900));
    r.reset();
    assert!(r.due(2150, 61, 50, 900));
    assert_eq!(Reporter::new().last(), None);
}

#[test]
fn local_time_fields() {
    // 2026-11-09 00:30:15 UTC was a Monday.
    let t = Local::from_seconds(1_794_184_215);
    assert_eq!(
        (t.weekday, t.hour(), t.minute_of_hour(), t.second),
        (0, 0, 30, 15)
    );
    // 1970-01-01 00:00 was a Thursday.
    assert_eq!(Local::from_seconds(0).weekday, 3);
    // Before the epoch still works (floored).
    let t = Local::from_seconds(-1);
    assert_eq!((t.weekday, t.minute, t.second), (2, 1439, 59));
}

#[test]
fn wall_clock_follows_the_monotonic_clock_and_asks_for_syncs() {
    let mut c = WallClock::new();
    assert!(!c.is_set());
    assert_eq!(c.local(5_000), None);
    assert!(c.wants_sync(5_000, 3_600_000));
    // Synced at monotonic 10 s: 2026-11-09 00:30:15 UTC, UTC+1.
    c.sync(1_794_184_215, 60, 10_000);
    let t = c.local(10_000).unwrap();
    assert_eq!(
        (t.weekday, t.hour(), t.minute_of_hour(), t.second),
        (0, 1, 30, 15)
    );
    // 90 minutes later on the crystal clock.
    let t = c.local(10_000 + 90 * 60_000).unwrap();
    assert_eq!((t.hour(), t.minute_of_hour()), (3, 0));
    assert!(!c.wants_sync(10_000 + 1000, 3_600_000));
    assert!(c.wants_sync(10_000 + 3_600_000, 3_600_000));
    // A sync with another offset (a daylight-saving change) moves local time only.
    c.sync(1_794_184_215, 120, 10_000);
    assert_eq!(c.local(10_000).unwrap().hour(), 2);
}

#[test]
fn day_parses_prints_and_rejects() {
    let d = Day::parse("06:00 21 08:00 18.5").unwrap();
    assert_eq!(d.to_string(), "06:00 21.0 08:00 18.5");
    assert_eq!(Day::parse("").unwrap().to_string(), "-");
    assert_eq!(Day::parse("6:00 21").unwrap().to_string(), "06:00 21.0");
    assert_eq!(Day::parse("24:00 21"), Err(ParseError::Time));
    assert_eq!(Day::parse("06:60 21"), Err(ParseError::Time));
    assert_eq!(Day::parse("06:00"), Err(ParseError::Count));
    assert_eq!(Day::parse("06:00 40"), Err(ParseError::Setpoint), "above 30.0");
    assert_eq!(
        Day::parse("06:00 21.25"),
        Err(ParseError::Setpoint),
        "tenths only"
    );
    assert_eq!(Day::parse("08:00 21 06:00 18"), Err(ParseError::Order));
    assert_eq!(Day::parse("08:00 21 08:00 18"), Err(ParseError::Order));
    let seven = "01:00 20 02:00 20 03:00 20 04:00 20 05:00 20 06:00 20 07:00 20";
    assert_eq!(Day::parse(seven), Err(ParseError::Count));
}

#[test]
fn week_setpoints_look_back_over_days() {
    let w = Week::default();
    // Monday 07:00: the 06:00 point.
    assert_eq!(w.setpoint(0, 7 * 60), Some(210));
    // Monday 05:00: Sunday's last point (22:30, 18.0).
    assert_eq!(w.setpoint(0, 5 * 60), Some(180));
    // Saturday 09:00.
    assert_eq!(w.setpoint(5, 9 * 60), Some(210));
    // Exactly on a switch point.
    assert_eq!(w.setpoint(0, 8 * 60), Some(180));
    // A program with only Wednesday points: Monday falls back to Wednesday's last.
    let mut sparse = Week {
        days: [Day::new(); 7],
    };
    sparse.days[2] = Day::parse("10:00 19 20:00 17").unwrap();
    assert_eq!(sparse.setpoint(0, 12 * 60), Some(170));
    assert_eq!(
        sparse.setpoint(2, 9 * 60),
        Some(170),
        "before today's first: last week's last"
    );
    assert_eq!(sparse.setpoint(2, 11 * 60), Some(190));
    assert_eq!(
        Week {
            days: [Day::new(); 7]
        }
        .setpoint(3, 0),
        None
    );
}

#[test]
fn next_switch_and_minutes_until() {
    let w = Week::default();
    assert_eq!(w.next_switch(0, 7 * 60), Some((0, 8 * 60)));
    assert_eq!(w.next_switch(0, 23 * 60), Some((1, 6 * 60)));
    assert_eq!(w.next_switch(6, 23 * 60), Some((0, 6 * 60)), "round the week");
    assert_eq!(minutes_until(0, 7 * 60, 0, 8 * 60), 60);
    assert_eq!(minutes_until(6, 23 * 60, 0, 6 * 60), 7 * 60);
    assert_eq!(minutes_until(0, 60, 0, 60), 7 * 1440);
    let mut one = Week {
        days: [Day::new(); 7],
    };
    one.days[3] = Day::parse("12:00 20").unwrap();
    assert_eq!(
        one.next_switch(3, 12 * 60),
        Some((3, 12 * 60)),
        "the same point a week on"
    );
    assert_eq!(
        Week {
            days: [Day::new(); 7]
        }
        .next_switch(0, 0),
        None
    );
}

#[test]
fn week_fits_one_eeprom_record_and_roundtrips() {
    let mut full = Week::default();
    let day = Day::parse("01:00 30 02:00 30 03:00 30 04:00 30 05:00 30 23:59 30").unwrap();
    full.days = [day; 7];
    let mut buf = [0u8; 256];
    let bytes = postcard::to_slice(&full, &mut buf).expect("a full week fits 256 bytes");
    assert!(bytes.len() <= 256);
    let back: Week = postcard::from_bytes(bytes).unwrap();
    assert_eq!(back, full);
    let mut buf = [0u8; 256];
    let bytes = postcard::to_slice(&Week::default(), &mut buf).unwrap();
    assert_eq!(postcard::from_bytes::<Week>(bytes).unwrap(), Week::default());
}

#[test]
fn demand_has_hysteresis() {
    let mut d = Demand::new();
    // Setpoint 21.0, hysteresis 0.4: on below 20.80, off above 21.20.
    assert!(d.update(2079, Some(210), 4));
    assert!(d.update(2100, Some(210), 4), "inside the band: holds on");
    assert!(d.update(2120, Some(210), 4), "at the edge: holds");
    assert!(!d.update(2121, Some(210), 4));
    assert!(!d.update(2100, Some(210), 4), "inside the band: holds off");
    assert!(!d.update(2080, Some(210), 4), "at the lower edge: holds");
    assert!(d.update(2079, Some(210), 4));
    assert!(!d.update(1000, None, 4), "off releases at once");
    assert!(!d.is_on());
}

#[test]
fn override_expires() {
    let mut o = Override::new();
    assert_eq!(o.at(0), None);
    o.set(225, 100);
    assert_eq!(o.at(99), Some(225));
    assert_eq!(o.at(100), None);
    assert_eq!(o.at(50), None, "an expired override is gone");
    o.set(200, 10);
    o.clear();
    assert_eq!(o.at(0), None);
    assert_eq!(Mode::from_index(1), Some(Mode::Manual));
    assert_eq!(Mode::from_index(3), None);
}
