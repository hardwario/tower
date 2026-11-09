//! A weekly heating program: per day, up to [`POINTS`] switch points, each a time of day and the
//! setpoint from then on.
//!
//! The setpoint at any moment is that of the last switch point at or before it, looking back over
//! earlier days (and round the week) when today has none yet. The whole program is one record of
//! at most [`WEEK_BYTES`] bytes ([`Week::encode`]), well inside the node's 256-byte EEPROM value.

use core::fmt;

/// Switch points per day.
pub const POINTS: usize = 6;

/// One switch point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    /// Minute of the day, 0..=1439.
    pub minute: u16,
    /// Setpoint in tenths of a degree Celsius.
    pub deci: i16,
}

/// One day's switch points, in time order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Day {
    len: u8,
    points: [Option<Point>; POINTS],
}

/// Why a day's text does not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// A time that is not `hh:mm` within the day.
    Time,
    /// A setpoint that is not a temperature in tenths (`21` or `21.5`) within range.
    Setpoint,
    /// More than [`POINTS`] switch points, or a time without a setpoint.
    Count,
    /// Switch points not in time order.
    Order,
}

/// The lowest and highest setpoint a program may hold (tenths of a degree Celsius), the
/// thermostat's `setpoint` property range.
pub const MIN_DECI: i16 = 50;
pub const MAX_DECI: i16 = 300;

impl Day {
    pub const fn new() -> Self {
        Self {
            len: 0,
            points: [None; POINTS],
        }
    }

    /// The switch points, in time order.
    pub fn points(&self) -> impl Iterator<Item = Point> + '_ {
        self.points[..self.len as usize].iter().flatten().copied()
    }

    /// Parse `hh:mm setpoint` pairs (`06:00 21 08:00 18.5`); an empty text is a day without
    /// switch points.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut day = Self::new();
        let mut words = text.split_ascii_whitespace();
        while let Some(time) = words.next() {
            let deci = words.next().ok_or(ParseError::Count)?;
            let minute = parse_minute(time).ok_or(ParseError::Time)?;
            let deci = parse_setpoint(deci).ok_or(ParseError::Setpoint)?;
            if day.len as usize == POINTS {
                return Err(ParseError::Count);
            }
            if day.points().last().is_some_and(|p| p.minute >= minute) {
                return Err(ParseError::Order);
            }
            day.points[day.len as usize] = Some(Point { minute, deci });
            day.len += 1;
        }
        Ok(day)
    }

    /// Set the switch point at `minute` to `deci`, adding it in time order, or remove it (`None`;
    /// removing one that is not there changes nothing). `Err(Count)` when adding to a full day.
    /// One point at a time is what a remote shell line can carry (the radio's 56-byte line holds
    /// a day of two or three points at most).
    pub fn set_point(&mut self, minute: u16, deci: Option<i16>) -> Result<(), ParseError> {
        let mut points: [Option<Point>; POINTS] = [None; POINTS];
        let mut len = 0;
        let mut placed = deci.is_none();
        for p in self.points() {
            if p.minute == minute {
                continue; // replaced or removed
            }
            if !placed && p.minute > minute {
                if len == POINTS {
                    return Err(ParseError::Count);
                }
                points[len] = deci.map(|deci| Point { minute, deci });
                len += 1;
                placed = true;
            }
            if len == POINTS {
                return Err(ParseError::Count);
            }
            points[len] = Some(p);
            len += 1;
        }
        if !placed {
            if len == POINTS {
                return Err(ParseError::Count);
            }
            points[len] = deci.map(|deci| Point { minute, deci });
            len += 1;
        }
        self.points = points;
        self.len = len as u8;
        Ok(())
    }

    /// The setpoint of the last switch point at or before `minute`.
    fn at(&self, minute: u16) -> Option<i16> {
        self.points()
            .filter(|p| p.minute <= minute)
            .last()
            .map(|p| p.deci)
    }

    fn last(&self) -> Option<i16> {
        self.points().last().map(|p| p.deci)
    }
}

impl fmt::Display for Day {
    /// `06:00 21.0 08:00 18.0` (the text [`Day::parse`] reads); `-` for a day without switch
    /// points.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for p in self.points() {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(
                f,
                "{:02}:{:02} {}.{}",
                p.minute / 60,
                p.minute % 60,
                p.deci / 10,
                (p.deci % 10).abs()
            )?;
        }
        if first {
            f.write_str("-")?;
        }
        Ok(())
    }
}

/// A `hh:mm` time of day as minutes since midnight (`6:00` too).
pub fn parse_minute(text: &str) -> Option<u16> {
    let (h, m) = text.split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let h: u16 = h.parse().ok()?;
    let m: u16 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// A setpoint in °C with at most one decimal (`21`, `18.5`) as tenths, within
/// [`MIN_DECI`]..=[`MAX_DECI`].
pub fn parse_setpoint(text: &str) -> Option<i16> {
    let (int, frac) = match text.split_once('.') {
        Some((i, f)) => (i, f),
        None => (text, "0"),
    };
    if frac.len() != 1 || !frac.bytes().all(|b| b.is_ascii_digit()) || int.is_empty() {
        return None;
    }
    let int: i16 = int.parse().ok()?;
    let deci = int
        .checked_mul(10)?
        .checked_add(i16::from(frac.as_bytes()[0] - b'0'))?;
    (MIN_DECI..=MAX_DECI).contains(&deci).then_some(deci)
}

/// The day names, Monday first, as the shell takes them.
pub const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// A week of switch points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Week {
    pub days: [Day; 7],
}

/// The most bytes [`Week::encode`] writes: per day a count, then four bytes per switch point.
pub const WEEK_BYTES: usize = 7 * (1 + POINTS * 4);
const _: () = assert!(
    WEEK_BYTES <= 256,
    "a week is one EEPROM value (256 bytes at most)"
);

impl Default for Week {
    /// Workdays 06:00 21.0, 08:00 18.0, 16:00 21.0, 22:00 18.0; the weekend 08:00 21.0, 22:30 18.0.
    fn default() -> Self {
        let workday = Day::parse("06:00 21 08:00 18 16:00 21 22:00 18").unwrap_or_default();
        let weekend = Day::parse("08:00 21 22:30 18").unwrap_or_default();
        Self {
            days: [workday, workday, workday, workday, workday, weekend, weekend],
        }
    }
}

impl Week {
    /// The program as bytes for storage: per day its count of switch points, then each point's
    /// minute (`u16`) and setpoint (`i16`), little-endian. Made by hand: postcard's code for these
    /// arrays of options cost the node about 3.5 KB of flash.
    pub fn encode<'a>(&self, out: &'a mut [u8; WEEK_BYTES]) -> &'a [u8] {
        let mut n = 0;
        for day in &self.days {
            out[n] = day.len;
            n += 1;
            for p in day.points() {
                out[n..n + 2].copy_from_slice(&p.minute.to_le_bytes());
                out[n + 2..n + 4].copy_from_slice(&p.deci.to_le_bytes());
                n += 4;
            }
        }
        &out[..n]
    }

    /// The program [`encode`](Self::encode) wrote; `None` when `bytes` are not one (a count, a
    /// time or a setpoint out of range, switch points out of order, bytes missing or left over).
    pub fn decode(bytes: &[u8]) -> Option<Week> {
        let mut week = Week {
            days: [Day::new(); 7],
        };
        let mut rest = bytes;
        for day in &mut week.days {
            let (&len, tail) = rest.split_first()?;
            rest = tail;
            if usize::from(len) > POINTS {
                return None;
            }
            for i in 0..usize::from(len) {
                let (p, tail) = rest.split_at_checked(4)?;
                rest = tail;
                let minute = u16::from_le_bytes([p[0], p[1]]);
                let deci = i16::from_le_bytes([p[2], p[3]]);
                let after_last = day.points().last().is_none_or(|last| last.minute < minute);
                if minute >= 1440 || !(MIN_DECI..=MAX_DECI).contains(&deci) || !after_last {
                    return None;
                }
                day.points[i] = Some(Point { minute, deci });
                day.len += 1;
            }
        }
        rest.is_empty().then_some(week)
    }

    /// The setpoint at `minute` of `weekday` (0 = Monday): the last switch point at or before it,
    /// this day or earlier ones; `None` for a program without any switch point.
    pub fn setpoint(&self, weekday: u8, minute: u16) -> Option<i16> {
        let today = weekday as usize % 7;
        if let Some(deci) = self.days[today].at(minute) {
            return Some(deci);
        }
        (1..=7).find_map(|back| self.days[(today + 7 - back) % 7].last())
    }

    /// The next switch point strictly after `minute` of `weekday`: `(weekday, minute)`, possibly
    /// on a later day (or the same switch point a week on); `None` without switch points.
    pub fn next_switch(&self, weekday: u8, minute: u16) -> Option<(u8, u16)> {
        let today = weekday as usize % 7;
        if let Some(p) = self.days[today].points().find(|p| p.minute > minute) {
            return Some((today as u8, p.minute));
        }
        (1..=7).find_map(|ahead| {
            let day = (today + ahead) % 7;
            self.days[day].points().next().map(|p| (day as u8, p.minute))
        })
    }
}

/// Minutes from `(weekday, minute)` to `(to_weekday, to_minute)`, going forward round the week
/// (a full week when they are equal).
pub fn minutes_until(weekday: u8, minute: u16, to_weekday: u8, to_minute: u16) -> u32 {
    let from = u32::from(weekday % 7) * 1440 + u32::from(minute);
    let to = u32::from(to_weekday % 7) * 1440 + u32::from(to_minute);
    let week = 7 * 1440;
    match (to + week - from) % week {
        0 => week,
        d => d,
    }
}
