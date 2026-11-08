//! Button gesture recognition: debounce, click, double click, hold, auto-repeat and chords
//! (several buttons pressed together), as a pure state machine.
//!
//! The driver samples the raw state of its buttons as a bit mask (bit `n` set = button `n` down)
//! and feeds it to [`Recognizer::update`] with the current time, whenever an input edge wakes
//! it, every scan interval while [`Recognizer::needs_polling`] says buttons are down, and at
//! [`Recognizer::next_deadline`]. Between gestures the recognizer needs nothing, so a driver with
//! an edge interrupt sleeps until the next press.
//!
//! The rules, per *session* (from the first press until every button is up again):
//!
//! 1. **Press** / **Release** for each button as its debounced state changes.
//! 2. **Hold**, at most once per session: when the buttons down have been held for
//!    [`Config::hold_ms`] without another one joining. It reports the buttons down at that moment,
//!    so holding two together is a chord hold.
//! 3. **Repeat**, with [`Config::repeat_ms`] set: after the Hold, every `repeat_ms` while the same
//!    buttons stay down (scrolling a value).
//! 4. **Click**, when the session ends without a Hold and lasted at most [`Config::click_ms`]. It
//!    reports every button pressed during the session: a quick press of two together is a chord
//!    click.
//! 5. **DoubleClick**, with [`Config::double_click_ms`] set: a second click of the same buttons
//!    within that time replaces the two Clicks. Each Click is then held back until that time has
//!    passed without a second one, so only enable it where it is used.

/// A debounced button event: `buttons` is the mask of the buttons involved (one bit for a single
/// button, several for a chord).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ButtonEvent {
    pub buttons: u8,
    pub kind: Kind,
}

/// What happened (see the [module docs](self) for the rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Press,
    Release,
    Click,
    DoubleClick,
    Hold,
    Repeat,
}

impl core::fmt::Display for Kind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Kind::Press => "press",
            Kind::Release => "release",
            Kind::Click => "click",
            Kind::DoubleClick => "double click",
            Kind::Hold => "hold",
            Kind::Repeat => "repeat",
        })
    }
}

/// Recognizer timing, in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// A change must be stable this long to count.
    pub debounce_ms: u32,
    /// Longest session that still ends in a Click.
    pub click_ms: u32,
    /// Hold time before a Hold.
    pub hold_ms: u32,
    /// Repeat period after a Hold (`None`: no repeats).
    pub repeat_ms: Option<u32>,
    /// Window for a second click to make a DoubleClick (`None`: no double clicks, no delay).
    pub double_click_ms: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            debounce_ms: 20,
            click_ms: 500,
            hold_ms: 800,
            repeat_ms: None,
            double_click_ms: None,
        }
    }
}

/// The recognizer (see the [module docs](self)).
pub struct Recognizer {
    cfg: Config,
    /// Debounced state.
    stable: u8,
    /// Last raw sample, and since when it differs from `stable`.
    raw: u8,
    raw_since: u64,
    /// Every button pressed in this session.
    session: u8,
    session_start: u64,
    /// When the set of buttons down last grew (the hold timer).
    grew_at: u64,
    hold_fired: bool,
    hold_mask: u8,
    next_repeat: u64,
    /// A click held back for a possible double click: its buttons and deadline, and whether the
    /// current session is the second press of it.
    pending: Option<(u8, u64)>,
}

impl Recognizer {
    pub const fn new(cfg: Config) -> Self {
        Self {
            cfg,
            stable: 0,
            raw: 0,
            raw_since: 0,
            session: 0,
            session_start: 0,
            grew_at: 0,
            hold_fired: false,
            hold_mask: 0,
            next_repeat: 0,
            pending: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The debounced mask of the buttons down.
    pub fn pressed(&self) -> u8 {
        self.stable
    }

    /// Whether the driver must keep sampling at its scan interval: buttons are down, or a change
    /// is settling. When `false`, only an input edge or [`next_deadline`](Self::next_deadline)
    /// can produce an event.
    pub fn needs_polling(&self) -> bool {
        self.stable != 0 || self.raw != self.stable
    }

    /// Whether only a release can produce the next event: buttons are down, their Hold has
    /// fired and no Repeat follows. A driver may then poll slowly, or sleep on a release edge,
    /// instead of sampling at its scan interval for as long as a button stays down.
    pub fn waiting_for_release(&self) -> bool {
        self.stable != 0
            && self.raw == self.stable
            && self.hold_fired
            && self.cfg.repeat_ms.is_none()
            && self.pending.is_none()
    }

    /// When [`update`](Self::update) must run again even if no input changes (a debounce
    /// settling, a Hold, a Repeat, a held-back Click), or `None`.
    pub fn next_deadline(&self) -> Option<u64> {
        let mut d: Option<u64> = None;
        let mut at = |t: u64| d = Some(d.map_or(t, |v: u64| v.min(t)));
        if self.raw != self.stable {
            at(self.raw_since + self.cfg.debounce_ms as u64);
        }
        if self.stable != 0 && !self.hold_fired {
            at(self.grew_at + self.cfg.hold_ms as u64);
        }
        if self.hold_fired
            && self.cfg.repeat_ms.is_some()
            && self.stable == self.hold_mask
            && self.stable != 0
        {
            at(self.next_repeat);
        }
        if let Some((_, t)) = self.pending {
            at(t);
        }
        d
    }

    /// Feed the raw button mask sampled at `now` (milliseconds, monotonic); events go to `emit`.
    pub fn update(&mut self, now: u64, raw: u8, emit: &mut impl FnMut(ButtonEvent)) {
        if raw != self.raw {
            self.raw = raw;
            self.raw_since = now;
        }
        if self.raw != self.stable && now.saturating_sub(self.raw_since) >= self.cfg.debounce_ms as u64 {
            self.transition(now, self.raw, emit);
        }
        self.timers(now, emit);
    }

    fn transition(&mut self, now: u64, new: u8, emit: &mut impl FnMut(ButtonEvent)) {
        let old = self.stable;
        self.stable = new;
        let ev = |buttons, kind| ButtonEvent { buttons, kind };
        if old == 0 {
            // A new session. A held-back click of other buttons is final now.
            if let Some((m, _)) = self.pending
                && m != new
            {
                emit(ev(m, Kind::Click));
                self.pending = None;
            }
            self.session = 0;
            self.session_start = now;
            self.hold_fired = false;
        }
        for bit in (0..8).map(|b| 1u8 << b) {
            if new & bit != 0 && old & bit == 0 {
                emit(ev(bit, Kind::Press));
            }
        }
        for bit in (0..8).map(|b| 1u8 << b) {
            if old & bit != 0 && new & bit == 0 {
                emit(ev(bit, Kind::Release));
            }
        }
        if new & !old != 0 {
            self.session |= new;
            self.grew_at = now;
        }
        if new == 0 {
            // Session over.
            let short = now - self.session_start <= self.cfg.click_ms as u64;
            if !self.hold_fired && short {
                match (self.cfg.double_click_ms, self.pending) {
                    (Some(_), Some((m, _))) if m == self.session => {
                        emit(ev(m, Kind::DoubleClick));
                        self.pending = None;
                    }
                    (Some(window), _) => self.pending = Some((self.session, now + window as u64)),
                    (None, _) => emit(ev(self.session, Kind::Click)),
                }
            } else if let Some((m, _)) = self.pending.take() {
                // The second press was not a click: the first one stands on its own.
                emit(ev(m, Kind::Click));
            }
        }
    }

    fn timers(&mut self, now: u64, emit: &mut impl FnMut(ButtonEvent)) {
        if self.stable != 0 && !self.hold_fired && now >= self.grew_at + self.cfg.hold_ms as u64 {
            if let Some((m, _)) = self.pending.take() {
                emit(ButtonEvent {
                    buttons: m,
                    kind: Kind::Click,
                });
            }
            self.hold_fired = true;
            self.hold_mask = self.stable;
            emit(ButtonEvent {
                buttons: self.stable,
                kind: Kind::Hold,
            });
            if let Some(r) = self.cfg.repeat_ms {
                self.next_repeat = now + r as u64;
            }
        }
        if let Some(r) = self.cfg.repeat_ms
            && self.hold_fired
            && self.stable != 0
            && self.stable == self.hold_mask
            && now >= self.next_repeat
        {
            emit(ButtonEvent {
                buttons: self.stable,
                kind: Kind::Repeat,
            });
            self.next_repeat = now + r as u64;
        }
        if let Some((m, t)) = self.pending
            && now >= t
            && self.stable == 0
        {
            emit(ButtonEvent {
                buttons: m,
                kind: Kind::Click,
            });
            self.pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const L: u8 = 1;
    const R: u8 = 2;

    /// Drive the recognizer with a raw-mask timeline sampled every 5 ms up to `end`.
    fn run(cfg: Config, timeline: &[(u64, u8)], end: u64) -> Vec<(u64, u8, Kind)> {
        let mut r = Recognizer::new(cfg);
        let mut out = Vec::new();
        let mut t = 0;
        while t <= end {
            let raw = timeline
                .iter()
                .rev()
                .find(|(at, _)| *at <= t)
                .map_or(0, |(_, m)| *m);
            r.update(t, raw, &mut |e| out.push((t, e.buttons, e.kind)));
            t += 5;
        }
        out
    }

    fn kinds(ev: &[(u64, u8, Kind)]) -> Vec<(u8, Kind)> {
        ev.iter().map(|e| (e.1, e.2)).collect()
    }

    #[test]
    fn click() {
        let ev = run(Config::default(), &[(0, L), (100, 0)], 1000);
        assert_eq!(
            kinds(&ev),
            [(L, Kind::Press), (L, Kind::Release), (L, Kind::Click)]
        );
        assert_eq!(ev[0].0, 20); // debounced
    }

    #[test]
    fn bounce_is_ignored() {
        let ev = run(
            Config::default(),
            &[(0, L), (5, 0), (10, L), (15, 0), (300, 0)],
            400,
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn hold_then_no_click() {
        let ev = run(Config::default(), &[(0, R), (1500, 0)], 2000);
        assert_eq!(
            kinds(&ev),
            [(R, Kind::Press), (R, Kind::Hold), (R, Kind::Release)]
        );
        assert_eq!(ev[1].0, 820);
    }

    #[test]
    fn repeat_after_hold() {
        let cfg = Config {
            repeat_ms: Some(200),
            ..Config::default()
        };
        let ev = run(cfg, &[(0, L), (1300, 0)], 1500);
        let k = kinds(&ev);
        assert_eq!(k[1], (L, Kind::Hold));
        assert_eq!(k.iter().filter(|e| e.1 == Kind::Repeat).count(), 2); // 1020, 1220
    }

    #[test]
    fn chord_click_and_hold() {
        let ev = run(Config::default(), &[(0, L), (60, L | R), (200, R), (230, 0)], 800);
        assert_eq!(
            kinds(&ev),
            [
                (L, Kind::Press),
                (R, Kind::Press),
                (L, Kind::Release),
                (R, Kind::Release),
                (L | R, Kind::Click)
            ]
        );
        let ev = run(Config::default(), &[(0, L), (60, L | R), (1500, 0)], 1600);
        assert_eq!(kinds(&ev)[2], (L | R, Kind::Hold));
        assert_eq!(ev[2].0, 80 + 800); // the hold timer restarts when R joins
    }

    #[test]
    fn double_click() {
        let cfg = Config {
            double_click_ms: Some(300),
            ..Config::default()
        };
        let ev = run(cfg, &[(0, L), (80, 0), (200, L), (280, 0)], 1000);
        assert_eq!(kinds(&ev).last(), Some(&(L, Kind::DoubleClick)));
        assert!(!kinds(&ev).contains(&(L, Kind::Click)));
        // A single click is delivered once the window has passed.
        let ev = run(cfg, &[(0, L), (80, 0)], 1000);
        assert_eq!(ev.last().map(|e| (e.0, e.1, e.2)), Some((400, L, Kind::Click)));
        // A click of another button flushes the held-back one first.
        let ev = run(cfg, &[(0, L), (80, 0), (150, R), (230, 0)], 1000);
        let k = kinds(&ev);
        assert!(k.contains(&(L, Kind::Click)) && k.contains(&(R, Kind::Click)));
    }

    #[test]
    fn deadlines_and_polling() {
        let mut r = Recognizer::new(Config::default());
        assert!(!r.needs_polling() && r.next_deadline().is_none());
        r.update(0, L, &mut |_| {});
        assert!(r.needs_polling());
        assert_eq!(r.next_deadline(), Some(20));
        r.update(20, L, &mut |_| {});
        assert_eq!(r.next_deadline(), Some(820)); // hold
        assert!(!r.waiting_for_release());
        r.update(820, L, &mut |_| {});
        assert!(r.waiting_for_release()); // held past the Hold
        r.update(900, 0, &mut |_| {});
        r.update(920, 0, &mut |_| {});
        assert!(!r.needs_polling() && r.next_deadline().is_none() && !r.waiting_for_release());
    }
}
