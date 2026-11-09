//! The thermostat's screen on the LCD Module, in page mode: [`render`] draws the whole screen from
//! [`SCREEN`] (what the regulator last showed, and the page the user is on) whenever the driver
//! asks, band by band; [`Ui`] is the regulator task's side: it updates [`SCREEN`], shows it, and
//! turns button presses into setpoint steps and menu choices.
//!
//! The main page (128 x 128, buttons below):
//!
//! ```text
//!  mon 14:32               (flame)    the day and time ("not paired", "--:--"); a flame: heating
//!       2 1   °C                       the room temperature, 16x32 digits drawn twice their size
//!       2 1  .5                        and a 16x32 tenth on their baseline
//!  ---------------------------------
//!  auto            21.0                the mode; the setpoint (inverted while being chosen)
//!  to 22:00                            how long a setpoint set by hand holds (auto mode)
//! ```
//!
//! Flash is short on this node (`docs/firmware/taf.md`, "RAM"): two fonts, the 16x32 digits (also
//! drawn at twice their size, for a fifth of the 32x64 font's flash) and the 8x16 ASCII letters;
//! the degree sign is drawn.

use core::cell::RefCell;
use core::fmt::{self, Write as _};

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_time::{Duration, Instant};
use log::{info, warn};
use tower::hmi::fonts::{SPLEEN_16X32_NUMERIC, spleen_8x16};
use tower::hmi::menu::{Item, Menu, MenuState, MenuStyle, Outcome, Values};
use tower::hmi::{Align, Canvas, Color, Font, Image, Rect, TextStyle, image};
use tower::i2c::I2cBus;
use tower::lcd::{self, Button, Event, Kind, Lcd, Paged};
use tower::storage::Nv;
use tower::taf;
use tower_protocol::props::thermostat;
use tower_taf::program::{DAY_NAMES, MAX_DECI, MIN_DECI};
use tower_taf::thermostat::Mode;

use super::State;

/// What a click of the left or right button changes the setpoint by: half a degree.
const STEP: i16 = 5;
/// A setpoint chosen with the buttons is applied this long after the last press: one save, one
/// report, however many presses it took.
const COMMIT: Duration = Duration::from_secs(3);
/// The menu and the info page go back to the main page after this long without input.
const IDLE: Duration = Duration::from_secs(30);
/// How long a notice stays.
const NOTICE: Duration = Duration::from_secs(4);

// ---- what the screen shows ----------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Menu,
    Info,
    Notice(&'static str),
}

/// The regulator's state as the screen shows it, and the page.
#[derive(Clone, Copy)]
struct View {
    page: Page,
    /// The room temperature, centi °C.
    temperature: Option<i32>,
    /// The setpoint in force, deci °C (`None`: off).
    setpoint: Option<i16>,
    /// A setpoint being chosen with the buttons, not applied yet.
    editing: Option<i16>,
    mode: Mode,
    demand: bool,
    /// The weekday and the minute of the day, once the clock is synced.
    now: Option<(u8, u16)>,
    /// The minute of the day a setpoint set by hand holds until (auto mode).
    until: Option<u16>,
    paired: bool,
}

struct Screen {
    view: View,
    menu: MenuState,
    /// When the setpoint being chosen is applied.
    commit_at: Option<Instant>,
    /// When the page goes back to the main page.
    page_until: Option<Instant>,
}

/// What [`render`] draws. Changed by the regulator's task between shows, never across an await.
static SCREEN: Mutex<ThreadModeRawMutex, RefCell<Screen>> = Mutex::new(RefCell::new(Screen {
    view: View {
        page: Page::Main,
        temperature: None,
        setpoint: None,
        editing: None,
        mode: Mode::Auto,
        demand: false,
        now: None,
        until: None,
        paired: false,
    },
    menu: MenuState::new(&MENU),
    commit_at: None,
    page_until: None,
}));

// ---- the menu -----------------------------------------------------------------------------------

const MODE: u16 = 1;
const PAIR: u16 = 2;
const INFO: u16 = 3;
const RESET: u16 = 4;

static RESET_MENU: Menu = Menu::new(
    "Factory reset",
    &[Item::back("Cancel"), Item::action("Reset now", RESET)],
);
static MENU: Menu = Menu::new(
    "Thermostat",
    &[
        Item::choice("Mode", MODE, thermostat::MODES),
        Item::action("Pair", PAIR),
        Item::action("Info", INFO),
        Item::submenu("Reset", &RESET_MENU),
        Item::back("Back"),
    ],
);
const MENU_STYLE: MenuStyle = MenuStyle::new(&LETTERS);

/// The menu's one value, the mode (a choice applied by the regulator's task on `Changed`).
struct ModeValue(Mode);

impl Values for ModeValue {
    fn get(&self, id: u16) -> i32 {
        if id == MODE { self.0 as i32 } else { 0 }
    }

    fn set(&mut self, _id: u16, _value: i32) {}
}

// ---- drawing ------------------------------------------------------------------------------------

/// The 8x16 letters, ASCII only (no `°`: [`degrees`] draws it).
static LETTERS: Font = spleen_8x16::with(&[&spleen_8x16::ASCII]);
const SMALL: TextStyle = TextStyle::new(&LETTERS);
const DIGITS: TextStyle = TextStyle::new(&SPLEEN_16X32_NUMERIC);

/// Shown in the status line while the thermostat calls for heat.
const FLAME: Image = image!(
    "......#......",
    "......##.....",
    ".....###.....",
    ".....####....",
    "....#####....",
    "....#####.#..",
    "...######.##.",
    "...#########.",
    "..##########.",
    "..####.#####.",
    ".####...####.",
    ".###.....###.",
    ".###.....###.",
    ".####...####.",
    "..#########..",
    "....#####....",
);

/// The page-mode renderer: the whole screen, from [`SCREEN`].
pub fn render(c: &mut Canvas<'_>) {
    SCREEN.lock(|s| {
        let mut s = s.borrow_mut();
        let v = s.view;
        match v.page {
            Page::Main => main_page(c, &v),
            Page::Menu => s.menu.render(c, &ModeValue(v.mode), &MENU_STYLE),
            Page::Info => info_page(c, &v),
            Page::Notice(text) => {
                c.clear(Color::White);
                c.text(64, 56, text, &SMALL.align(Align::Center));
            }
        }
    })
}

/// The left margin of text (the 8x16 letters have no blank column on their left).
const LEFT: i32 = 2;

fn main_page(c: &mut Canvas<'_>, v: &View) {
    c.clear(Color::White);
    match (v.paired, v.now) {
        (false, _) => {
            c.text(LEFT, 0, "not paired", &SMALL);
        }
        (true, Some((day, minute))) => {
            let day = DAY_NAMES[usize::from(day) % 7];
            let time = format_args!("{day} {:02}:{:02}", minute / 60, minute % 60);
            c.text_fmt(LEFT, 0, time, &SMALL);
        }
        (true, None) => {
            c.text(LEFT, 0, "--:--", &SMALL);
        }
    }
    if v.demand {
        c.image(128 - i32::from(FLAME.width), 0, &FLAME, Color::Black, None);
    }
    temperature(c, v.temperature);
    c.hline(0, 76, 128, Color::Black);
    c.text(LEFT, 84, thermostat::MODES[v.mode as usize], &SMALL);
    if let Some(minute) = v.until {
        let until = format_args!("to {:02}:{:02}", minute / 60, minute % 60);
        c.text_fmt(LEFT, 102, until, &SMALL);
    } else if v.mode == Mode::Auto && v.now.is_none() {
        c.text(LEFT, 102, "no clock", &SMALL);
    }
    // The setpoint (none when off), inverted while being chosen.
    if let Some(deci) = v.editing.or(v.setpoint) {
        let mut style = DIGITS;
        if v.editing.is_some() {
            c.fill_round_rect(Rect::new(68, 86, 60, 36), 4, Color::Black);
            style = style.color(Color::White);
        }
        setpoint(c, 125, 88, deci, &style);
    }
}

/// A setpoint (tenths) in the 16x32 digits, right-aligned at `right`, the point's cell narrowed
/// (it is mostly blank): it fits beside `to 16:00`.
fn setpoint(c: &mut Canvas<'_>, right: i32, y: i32, deci: i16, style: &TextStyle) {
    let whole = style.align(Align::Right);
    c.text_fmt(right - 22, y, format_args!("{}", deci / 10), &whole);
    c.text(right - 26, y, ".", style);
    c.text_fmt(right - 16, y, format_args!("{}", deci % 10), style);
}

/// Text in the 16x32 digits at twice their size (each pixel a 2 x 2 block), drawn as it is
/// formatted: the room temperature's whole degrees.
struct Big<'p, 'a> {
    c: &'p mut Canvas<'a>,
    x: i32,
    y: i32,
}

impl fmt::Write for Big<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let font = &SPLEEN_16X32_NUMERIC;
        let (w, h) = (i32::from(font.width), i32::from(font.height));
        let clip = self.c.clip();
        for ch in s.chars() {
            let cell = Rect::new(self.x, self.y, 2 * w, 2 * h);
            if let Some(bits) = font.glyph(ch).filter(|_| !cell.intersect(&clip).is_empty()) {
                let stride = font.stride();
                for row in 0..h {
                    let y = self.y + 2 * row;
                    // A band of a page-mode screen holds a few rows: skip the others early.
                    if y + 2 <= clip.y || y >= clip.bottom() {
                        continue;
                    }
                    let bits = &bits[row as usize * stride..(row as usize + 1) * stride];
                    let ink = |x: i32| bits[x as usize / 8] & (0x80 >> (x % 8)) != 0;
                    // Runs of ink, one rectangle each.
                    let mut x = 0;
                    while x < w {
                        let start = x;
                        while x < w && ink(x) {
                            x += 1;
                        }
                        if x > start {
                            self.c.fill_rect(
                                Rect::new(self.x + 2 * start, y, 2 * (x - start), 2),
                                Color::Black,
                            );
                        }
                        x += 1;
                    }
                }
            }
            self.x += 2 * w;
        }
        Ok(())
    }
}

/// `°C` from `(x, y)` (an 8x16 cell's top left): the ring drawn, the `C` a letter; 15 pixels wide.
fn degrees(c: &mut Canvas<'_>, x: i32, y: i32) {
    c.circle(x + 3, y + 4, 2, Color::Black);
    c.text(x + 7, y, "C", &SMALL);
}

/// The room temperature, centered: whole degrees in twice-size digits (their ink rows 24 to 63),
/// the tenth in 16x32 ones on the same baseline, `°C` level with the top.
fn temperature(c: &mut Canvas<'_>, centi: Option<i32>) {
    const Y: i32 = 12;
    // The tenth's cell top, so its baseline meets the twice-size one.
    let tenth_y = Y + i32::from(SPLEEN_16X32_NUMERIC.baseline);
    let Some(t) = centi else {
        let _ = write!(Big { c, x: 16, y: Y }, "--");
        c.text(80, tenth_y, ".-", &DIGITS);
        return;
    };
    // Tenths, rounded half away from zero.
    let deci = if t >= 0 { (t + 5) / 10 } else { (t - 5) / 10 };
    let (whole, tenth) = (deci / 10, (deci % 10).abs());
    let sign = if deci < 0 && whole == 0 { "-" } else { "" };
    let mut cells = sign.len() as i32 + i32::from(whole < 0) + 1;
    let mut rest = whole.abs();
    while rest >= 10 {
        rest /= 10;
        cells += 1;
    }
    let x = ((128 - cells * 32 - 32) / 2).max(0);
    let _ = write!(Big { c, x, y: Y }, "{sign}{whole}");
    let x = x + cells * 32;
    c.text_fmt(x, tenth_y, format_args!(".{tenth}"), &DIGITS);
    degrees(c, x + 9, Y + 12);
}

fn info_page(c: &mut Canvas<'_>, v: &View) {
    c.clear(Color::White);
    c.fill_rect(Rect::new(0, 0, 128, 18), Color::Black);
    c.text(64, 1, "Info", &SMALL.inverted().align(Align::Center));
    c.text_fmt(LEFT, 24, format_args!("addr {:08x}", taf::addr()), &SMALL);
    match taf::gateway() {
        Some(gw) => c.text_fmt(LEFT, 42, format_args!("gw   {gw:08x}"), &SMALL),
        None => c.text(LEFT, 42, "not paired", &SMALL),
    };
    if let Some(t) = v.temperature {
        let sign = if t < 0 { "-" } else { "" };
        let t = t.unsigned_abs();
        let r = c.text_fmt(
            LEFT,
            60,
            format_args!("room {sign}{}.{:02}", t / 100, t % 100),
            &SMALL,
        );
        degrees(c, r.right() + 4, 60);
    }
    let h = taf::param(thermostat::HYSTERESIS);
    let r = c.text_fmt(LEFT, 78, format_args!("hyst {}.{}", h / 10, h % 10), &SMALL);
    degrees(c, r.right() + 4, 78);
}

// ---- the regulator task's side ------------------------------------------------------------------

/// The screen as the regulator's task drives it (its state is in [`SCREEN`]: a task argument is
/// stored twice in the task's future).
pub struct Ui {
    lcd: Lcd<Paged>,
}

impl Ui {
    /// Start the LCD driver in page mode (`None`, logged, if it does not start).
    pub async fn start(spawner: Spawner, res: lcd::Resources, bus: I2cBus) -> Option<Self> {
        match lcd::init_paged(spawner, res, bus, lcd::Config::default(), render).await {
            Ok(lcd) => Some(Self { lcd }),
            Err(e) => {
                warn!(target: "lcd", "not started ({e:?}): the thermostat runs headless");
                None
            }
        }
    }

    /// Bring the screen up to date with `s` and send what changed.
    pub async fn show(&mut self, s: &mut State) {
        let setpoint = s.setpoint();
        let now = taf::local_time();
        let until = s
            .over
            .until()
            .filter(|_| s.mode == Mode::Auto)
            .map(|t| (t.rem_euclid(86_400) / 60) as u16);
        SCREEN.lock(|sc| {
            let v = &mut sc.borrow_mut().view;
            v.temperature = s.temperature;
            v.setpoint = setpoint;
            v.mode = s.mode;
            v.demand = s.demand.is_on();
            v.now = now.map(|t| (t.weekday, t.minute));
            v.until = until;
            v.paired = taf::paired();
        });
        self.lcd.show().await;
    }

    /// When [`on_timer`](Self::on_timer) has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        SCREEN.lock(|sc| {
            let sc = sc.borrow();
            match (sc.commit_at, sc.page_until) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        })
    }

    pub async fn next_event(&self) -> Event {
        self.lcd.next_event().await
    }

    /// Apply a setpoint chosen with the buttons, close a page that timed out.
    pub fn on_timer(&mut self, s: &mut State, kv: Nv) {
        let now = Instant::now();
        let (commit, close) = SCREEN.lock(|sc| {
            let sc = sc.borrow();
            (
                sc.commit_at.is_some_and(|t| t <= now),
                sc.page_until.is_some_and(|t| t <= now),
            )
        });
        if commit {
            self.commit(s, kv);
        }
        if close {
            self.open(Page::Main, None);
        }
    }

    /// A button press or a swipe.
    pub async fn on_event(&mut self, ev: Event, s: &mut State, kv: Nv) {
        let page = SCREEN.lock(|sc| sc.borrow().view.page);
        let Event::Button(button, kind) = ev else { return };
        match page {
            Page::Main => match (button, kind) {
                (Button::Left, Kind::Click) => self.step(s, -STEP),
                (Button::Right, Kind::Click) => self.step(s, STEP),
                (_, Kind::Hold) | (Button::Both | Button::Core, Kind::Click) => {
                    self.commit(s, kv);
                    SCREEN.lock(|sc| sc.borrow_mut().menu.reset());
                    self.open(Page::Menu, Some(IDLE));
                }
                _ => {}
            },
            Page::Menu => {
                let Some(nav) = lcd::default_nav(ev) else { return };
                let outcome = SCREEN.lock(|sc| {
                    let mut sc = sc.borrow_mut();
                    sc.page_until = Some(Instant::now() + IDLE);
                    sc.menu.handle(nav, &mut ModeValue(s.mode))
                });
                match outcome {
                    Some(Outcome::Changed { id: MODE, value }) => {
                        if let Some(m) = Mode::from_index(value) {
                            s.set_mode(m, kv);
                        }
                    }
                    Some(Outcome::Action(PAIR)) => {
                        info!(target: "thermostat", "pairing from the menu");
                        taf::request_pairing();
                        self.open(Page::Notice("pairing"), Some(NOTICE));
                    }
                    Some(Outcome::Action(INFO)) => self.open(Page::Info, Some(IDLE)),
                    Some(Outcome::Action(RESET)) => {
                        warn!(target: "node", "factory reset from the menu, rebooting");
                        self.open(Page::Notice("factory reset"), None);
                        self.lcd.show().await;
                        taf::factory_reset().await
                    }
                    Some(Outcome::Exit) => self.open(Page::Main, None),
                    _ => {}
                }
            }
            Page::Info | Page::Notice(_) => {
                if kind == Kind::Click || kind == Kind::Hold {
                    self.open(Page::Main, None);
                }
            }
        }
    }

    /// A click: the setpoint being chosen moves a step (not while off).
    fn step(&mut self, s: &mut State, by: i16) {
        let Some(current) = s.setpoint() else { return };
        SCREEN.lock(|sc| {
            let mut sc = sc.borrow_mut();
            let v = &mut sc.view;
            v.editing = Some(
                v.editing
                    .unwrap_or(current)
                    .saturating_add(by)
                    .clamp(MIN_DECI, MAX_DECI),
            );
            sc.commit_at = Some(Instant::now() + COMMIT);
        });
    }

    /// Apply the setpoint being chosen, if any.
    fn commit(&mut self, s: &mut State, kv: Nv) {
        let editing = SCREEN.lock(|sc| {
            let mut sc = sc.borrow_mut();
            sc.commit_at = None;
            sc.view.editing.take()
        });
        if let Some(deci) = editing {
            info!(target: "thermostat", "setpoint {}.{} °C from the buttons", deci / 10, deci % 10);
            s.set_by_hand(deci);
            s.save(kv);
        }
    }

    fn open(&mut self, page: Page, for_: Option<Duration>) {
        SCREEN.lock(|sc| {
            let mut sc = sc.borrow_mut();
            sc.view.page = page;
            sc.page_until = for_.map(|d| Instant::now() + d);
        });
    }
}
