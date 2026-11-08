//! lcd_games: five small games on the TOWER LCD Module, picked from a menu, each playable three
//! ways, chosen before each round: the two **buttons**, **swipes** over the gesture sensor, or
//! **tilting** the board (the Core Module's accelerometer).
//!
//! | Game | Play |
//! |------|------|
//! | Asteroid Run | steer a rocket through falling asteroids |
//! | Snake | eat, grow, don't bite yourself |
//! | Blockfall | stack falling pieces, clear full rows |
//! | Bounce | paddle and ball against walls and bricks |
//! | Marble Maze | roll a marble through a random maze to the hole |
//!
//! Only the chosen input is live in a round, so nothing else moves the game by surprise; each game
//! file says what each control does. The app opens on the HARDWARIO TOWER logo (3 s), and each
//! game too (1 s, with its name). After the control comes a card explaining it; hold both buttons
//! for 2 seconds to leave a game, whatever the control. The logo (`apps/lcd_games/logo.rs`) is the
//! approved stacked lockup from the HARDWARIO artwork repository, converted by
//! `crates/hmi/tools/image.py`. The best score of each game with each control is kept in EEPROM
//! (`High scores` in the menu). Between games everything sleeps: the menu waits on the buttons in
//! STOP, the gesture sensor and the accelerometer are on only in a round that uses them.
//!
//!   just flash app lcd_games

#![no_std]
#![no_main]

// One file per game, in `apps/lcd_games/` (a crate root looks for modules beside itself).
#[path = "lcd_games/blockfall.rs"]
mod blockfall;
#[path = "lcd_games/bounce.rs"]
mod bounce;
#[path = "lcd_games/logo.rs"]
mod logo;
#[path = "lcd_games/marble.rs"]
mod marble;
#[path = "lcd_games/rocket.rs"]
mod rocket;
#[path = "lcd_games/snake.rs"]
mod snake;

use embassy_time::{Duration, Instant, Timer};
use log::{error, info};
use tower::hmi::menu::{Item, Menu, MenuState, MenuStyle, Nav, Table};
use tower::hmi::{Align, Canvas, Color, Rect, TextStyle, fonts};
use tower::i2c::I2cBus;
use tower::lcd::{self, Button, Event, Gesture, Kind, Lcd, MenuExit, Rgb};
use tower::lis2dh12::{self, Lis2dh12, Odr};
use tower::storage::{NS_APP, Scoped};
use tower::{app, board, board::Board, shell};

// ---- the menu ----------------------------------------------------------------------------------

/// How the player controls a game, chosen before each round: only that input is live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// The two push buttons.
    Buttons,
    /// Swipes over the gesture sensor.
    Swipes,
    /// Tilting the board (the Core Module's accelerometer).
    Tilt,
}

impl Control {
    const ALL: [Control; 3] = [Control::Buttons, Control::Swipes, Control::Tilt];

    fn name(self) -> &'static str {
        match self {
            Control::Buttons => "buttons",
            Control::Swipes => "swipes",
            Control::Tilt => "tilt",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Control::Buttons => Icon::Buttons,
            Control::Swipes => Icon::SwipeAny,
            Control::Tilt => Icon::Tilt,
        }
    }
}

/// A control shown on a game's card: what to do, drawn.
#[derive(Clone, Copy)]
enum Icon {
    /// The two buttons, (L) and (R).
    Buttons,
    /// The left button, (L).
    ButtonL,
    /// The right button, (R).
    ButtonR,
    /// A swipe in one direction.
    Swipe(Gesture),
    /// A swipe left or right.
    SwipeLR,
    /// A swipe in any direction.
    SwipeAny,
    /// Tilting the board.
    Tilt,
    /// No icon, just the words.
    Note,
}

type Rows = &'static [(Icon, &'static str)];

/// A game and how to play it with each control (in [`Control::ALL`] order), shown on its card:
/// an icon and a few words a row.
struct Game {
    name: &'static str,
    controls: [Rows; 3],
}

static GAMES: [Game; 5] = [
    Game {
        name: "Asteroid Run",
        controls: [
            &[(Icon::Buttons, "hold: steer"), (Icon::Note, "dodge the rocks")],
            &[(Icon::SwipeLR, "change lane"), (Icon::Note, "dodge the rocks")],
            &[(Icon::Tilt, "lean: steer"), (Icon::Note, "dodge the rocks")],
        ],
    },
    Game {
        name: "Snake",
        controls: [
            &[(Icon::Buttons, "turn"), (Icon::Note, "eat, don't bite")],
            &[(Icon::SwipeAny, "go that way"), (Icon::Note, "eat, don't bite")],
            &[(Icon::Tilt, "lean that way"), (Icon::Note, "eat, don't bite")],
        ],
    },
    Game {
        name: "Blockfall",
        controls: [
            &[
                (Icon::Buttons, "click: move"),
                (Icon::ButtonL, "hold: rotate"),
                (Icon::ButtonR, "hold: drop"),
            ],
            &[
                (Icon::SwipeLR, "move"),
                (Icon::Swipe(Gesture::Up), "rotate"),
                (Icon::Swipe(Gesture::Down), "drop"),
            ],
            &[
                (Icon::Tilt, "lean: move"),
                (Icon::Note, "tilt away: rotate"),
                (Icon::Note, "tilt to you: faster"),
            ],
        ],
    },
    Game {
        name: "Bounce",
        controls: [
            &[(Icon::Buttons, "hold: move"), (Icon::Note, "clear the bricks")],
            &[(Icon::SwipeLR, "glide"), (Icon::Swipe(Gesture::Up), "stop")],
            &[(Icon::Tilt, "lean: move"), (Icon::Note, "clear the bricks")],
        ],
    },
    Game {
        name: "Marble Maze",
        controls: [
            &[(Icon::Buttons, "turn gravity"), (Icon::Note, "roll to the hole")],
            &[(Icon::SwipeAny, "set gravity"), (Icon::Note, "roll to the hole")],
            &[(Icon::Tilt, "roll marble"), (Icon::Note, "roll to the hole")],
        ],
    },
];

/// A tilting board on its pivot.
const TILT: tower::hmi::Image = tower::hmi::image!(
    "......................###",
    "...................######",
    "................######...",
    ".............######......",
    "..........######.........",
    ".......######............",
    "....######...............",
    ".######..................",
    "####........#............",
    "##.........###...........",
    "..........#####..........",
    ".........#######.........",
);

/// Arrows left and right (drawn white on a black tile).
const LEFT_RIGHT: tower::hmi::Image = tower::hmi::image!(
    ".................",
    ".................",
    ".................",
    ".................",
    "..#...........#..",
    ".##...........##.",
    "#################",
    ".##...........##.",
    "..#...........#..",
    ".................",
    ".................",
    ".................",
    ".................",
);

/// Arrows out in four directions (drawn white on a black tile).
const CROSS: tower::hmi::Image = tower::hmi::image!(
    "........#........",
    ".......###.......",
    "......#####......",
    "........#........",
    "..#.....#.....#..",
    ".##.....#.....##.",
    "#################",
    ".##.....#.....##.",
    "..#.....#.....#..",
    "........#........",
    "......#####......",
    ".......###.......",
    "........#........",
);

/// A push button: a black disc with its letter in white.
pub fn button_glyph(c: &mut Canvas<'_>, cx: i32, cy: i32, letter: &str) {
    c.fill_circle(cx, cy, 7, Color::Black);
    c.text(
        cx + 1,
        cy - 6,
        letter,
        &TEXT.align(Align::Center).color(Color::White),
    );
}

/// An arrow centered on `(cx, cy)`, 2 pixels thick, 11 long.
fn arrow(c: &mut Canvas<'_>, cx: i32, cy: i32, dir: Gesture, color: Color) {
    // Drawn pointing up, then turned: (dx, dy) -> screen offset.
    let turn = |dx: i32, dy: i32| match dir {
        Gesture::Up => (dx, dy),
        Gesture::Down => (-dx, -dy),
        Gesture::Left => (dy, -dx),
        Gesture::Right => (-dy, dx),
    };
    for t in [0, 1] {
        for (a, b) in [((t, 5), (t, -5)), ((t, -5), (t - 4, -1)), ((t, -5), (t + 4, -1))] {
            let (p, q) = (turn(a.0, a.1), turn(b.0, b.1));
            c.line(cx + p.0, cy + p.1, cx + q.0, cy + q.1, color);
        }
    }
}

/// The icon of a control, centered on `(cx, cy)`.
fn icon(c: &mut Canvas<'_>, cx: i32, cy: i32, kind: Icon) {
    match kind {
        Icon::Buttons => {
            button_glyph(c, cx - 9, cy, "L");
            button_glyph(c, cx + 9, cy, "R");
        }
        Icon::ButtonL => button_glyph(c, cx - 9, cy, "L"),
        Icon::ButtonR => button_glyph(c, cx + 9, cy, "R"),
        Icon::SwipeLR => {
            c.fill_round_rect(Rect::new(cx - 11, cy - 7, 23, 15), 4, Color::Black);
            c.image(cx - 8, cy - 6, &LEFT_RIGHT, Color::White, None);
        }
        Icon::Swipe(g) => {
            c.fill_round_rect(Rect::new(cx - 11, cy - 7, 23, 15), 4, Color::Black);
            arrow(c, cx, cy, g, Color::White);
        }
        Icon::SwipeAny => {
            c.fill_round_rect(Rect::new(cx - 11, cy - 7, 23, 15), 4, Color::Black);
            c.image(cx - 8, cy - 6, &CROSS, Color::White, None);
        }
        Icon::Tilt => c.image(cx - 12, cy - 6, &TILT, Color::Black, None),
        Icon::Note => {}
    }
}

const SCORES: u16 = 10;

static MAIN: Menu = Menu::new(
    "Arcade",
    &[
        Item::action("Asteroid Run", 1),
        Item::action("Snake", 2),
        Item::action("Blockfall", 3),
        Item::action("Bounce", 4),
        Item::action("Marble Maze", 5),
        Item::action("High scores", SCORES),
    ],
);

const MENU_STYLE: MenuStyle = MenuStyle::new(&fonts::SPLEEN_8X16);
pub const TITLE: TextStyle = TextStyle::new(&fonts::SPLEEN_8X16)
    .align(Align::Center)
    .inverted();
pub const TEXT: TextStyle = TextStyle::new(&fonts::SPLEEN_6X12);
pub const HUD: TextStyle = TextStyle::new(&fonts::SPLEEN_5X8);
const BIG: TextStyle = TextStyle::new(&fonts::SPLEEN_16X32_NUMERIC).align(Align::Center);

static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd", lcd::SHELL)];

/// A new best score: three green flashes.
static RECORD: lcd::Pattern = &[
    lcd::Step::new(Rgb::GREEN, 120),
    lcd::Step::new(Rgb::OFF, 80),
    lcd::Step::new(Rgb::GREEN, 120),
    lcd::Step::new(Rgb::OFF, 80),
    lcd::Step::new(Rgb::GREEN, 300),
];

// ---- what the games share ----------------------------------------------------------------------

/// What a game gets.
pub struct Ctx {
    pub lcd: Lcd,
    pub bus: I2cBus,
    pub rng: Rng,
    /// The control chosen for this round: the game acts on that input only (and on the quit
    /// gesture).
    pub control: Control,
    /// The accelerometer, while the control is [`Control::Tilt`].
    pub tilt: Option<Tilt>,
}

impl Ctx {
    /// Hand the events since the last call to `f`, tracking held buttons and the quit gesture in
    /// `pad` first.
    pub fn drain(&self, pad: &mut Pad, mut f: impl FnMut(&Event)) {
        while let Some(ev) = self.lcd.try_event() {
            pad.feed(&ev);
            f(&ev);
        }
    }

    /// How far the board leans from how it was held when the round started, milli-g (`x` to the
    /// right, `y` down the screen); zero unless the control is tilt.
    pub fn lean(&self) -> (i32, i32) {
        self.tilt.as_ref().map_or((0, 0), Tilt::lean)
    }
}

/// How a game ended.
pub enum End {
    /// Over, with this score.
    Over(u32),
    /// The player left (both buttons held for [`QUIT_HOLD`]).
    Quit,
}

/// Leaving a game: both buttons held this long (long enough not to happen by accident, while
/// steering or pressing them together to rotate a piece).
pub const QUIT_HOLD: Duration = Duration::from_secs(2);
/// While both buttons are down, a game that waits on a timer looks at least this often.
pub const QUIT_CHECK: Duration = Duration::from_millis(100);

/// Which buttons are held, from their press and release events, and the quit gesture.
#[derive(Default)]
pub struct Pad {
    pub left: bool,
    pub right: bool,
    both_since: Option<Instant>,
}

impl Pad {
    /// Track `ev` (every event goes through here, so the quit gesture is seen in every game).
    pub fn feed(&mut self, ev: &Event) {
        match ev {
            Event::Button(Button::Left, Kind::Press) => self.left = true,
            Event::Button(Button::Left, Kind::Release) => self.left = false,
            Event::Button(Button::Right, Kind::Press) => self.right = true,
            Event::Button(Button::Right, Kind::Release) => self.right = false,
            _ => {}
        }
        self.both_since = match (self.both(), self.both_since) {
            (true, None) => Some(Instant::now()),
            (true, since) => since,
            (false, _) => None,
        };
    }

    /// Both buttons are down.
    pub fn both(&self) -> bool {
        self.left && self.right
    }

    /// Both buttons have been down for [`QUIT_HOLD`]: leave the game.
    pub fn quit(&self) -> bool {
        self.both_since.is_some_and(|t| t.elapsed() >= QUIT_HOLD)
    }

    /// -1 (left), 0 or 1 (right); 0 with both down.
    pub fn dir(&self) -> i32 {
        self.right as i32 - self.left as i32
    }

    /// When a game waiting until `at` should look again: sooner while both buttons are down.
    pub fn wake(&self, at: Instant) -> Instant {
        if self.both() {
            at.min(Instant::now() + QUIT_CHECK)
        } else {
            at
        }
    }
}

/// A xorshift32 PRNG, seeded from the TRNG.
pub struct Rng(u32);

impl Rng {
    pub fn next_u32(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    /// Uniform in `lo..hi` (`hi > lo`).
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next_u32() % (hi - lo) as u32) as i32
    }
}

/// The Core Module's accelerometer as a tilt control, level wherever the board was held when it
/// started (a player holds it tilted towards them, not flat).
pub struct Tilt {
    bus: I2cBus,
    neutral: (i32, i32),
}

impl Tilt {
    /// Start sampling at 50 Hz and take the current lean as level.
    pub async fn start(bus: I2cBus) -> Self {
        let _ = bus.raw(|b| Lis2dh12::new(b, lis2dh12::ADDR_DEFAULT).init_rate(Odr::Hz50));
        let mut t = Self { bus, neutral: (0, 0) };
        let (mut sx, mut sy) = (0, 0);
        for _ in 0..4 {
            Timer::after(Duration::from_millis(25)).await;
            let (x, y) = t.gravity();
            sx += x;
            sy += y;
        }
        t.neutral = (sx / 4, sy / 4);
        bus.rest().await;
        t
    }

    /// Gravity in the screen's plane, milli-g: `x` to the right, `y` down as the screen reads (the
    /// way a marble on the screen would roll). The next draw re-sleeps the ATSHA for this read.
    fn gravity(&self) -> (i32, i32) {
        let a = self
            .bus
            .raw(|b| Lis2dh12::new(b, lis2dh12::ADDR_DEFAULT).read())
            .unwrap_or_default();
        // The accelerometer reads the opposite of gravity.
        let (x, y) = lcd::auto_rotate::mounting(a);
        (-x, -y)
    }

    /// The gravity relative to the start: how far the board leans from how it was held then.
    pub fn lean(&self) -> (i32, i32) {
        let (x, y) = self.gravity();
        (x - self.neutral.0, y - self.neutral.1)
    }

    /// Power the accelerometer down.
    pub async fn stop(self) {
        let _ = self
            .bus
            .raw(|b| Lis2dh12::new(b, lis2dh12::ADDR_DEFAULT).power_down());
        self.bus.rest().await;
    }
}

/// A framed box in the middle of the screen (over a game's last frame).
pub fn panel(c: &mut Canvas<'_>, r: Rect) {
    c.fill_round_rect(r, 5, Color::White);
    c.round_rect(r, 5, Color::Black);
    c.round_rect(r.inset(2), 3, Color::Black);
}

// ---- the app -----------------------------------------------------------------------------------

async fn run(b: Board) {
    let bus = I2cBus::share(b.tmp112.release());
    let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
    let lcd = match lcd::init(b.spawner, res, bus, lcd::Config::default()).await {
        Ok(lcd) => lcd,
        Err(e) => {
            error!(target: "games", "no LCD Module ({e:?})");
            return;
        }
    };
    let scores = b.kv.scope(NS_APP);
    let mut ctx = Ctx {
        lcd,
        bus,
        rng: Rng(board::rand_u32() | 1),
        control: Control::Buttons,
        tilt: None,
    };
    // The control last chosen for each game: the chooser's starting point.
    let mut last = [Control::Buttons; 5];
    let mut menu = MenuState::new(&MAIN);
    splash(lcd, "", 3000).await;
    loop {
        match lcd
            .menu(&mut menu, &mut Table::<0>::new([]), &MENU_STYLE, None)
            .await
        {
            MenuExit::Action(SCORES) => high_scores(lcd, scores).await,
            MenuExit::Action(id @ 1..=5) => {
                let game = id as usize - 1;
                if let Some(control) = choose(lcd, &GAMES[game], last[game]).await {
                    last[game] = control;
                    play(&mut ctx, scores, game, control).await;
                }
            }
            _ => {}
        }
        lcd.clear_events();
    }
}

async fn play(ctx: &mut Ctx, scores: Scoped, game: usize, control: Control) {
    let g = &GAMES[game];
    let best = best(scores, game, control);
    if !card(ctx.lcd, g, control, best).await {
        return;
    }
    info!(target: "games", "{} started ({})", g.name, control.name());
    splash(ctx.lcd, g.name, 1000).await;
    // Only the chosen input is live: the gesture sensor and the accelerometer draw current, so
    // each runs only for its own control.
    ctx.control = control;
    match control {
        Control::Swipes => ctx.lcd.set_gestures(true),
        Control::Tilt => ctx.tilt = Some(Tilt::start(ctx.bus).await),
        Control::Buttons => {}
    }
    ctx.lcd.clear_events();
    let end = match game {
        0 => rocket::play(ctx).await,
        1 => snake::play(ctx).await,
        2 => blockfall::play(ctx).await,
        3 => bounce::play(ctx).await,
        _ => marble::play(ctx).await,
    };
    // Whatever a game changed, back to the menu's way.
    if let Some(t) = ctx.tilt.take() {
        t.stop().await;
    }
    ctx.lcd.set_buttons(Default::default());
    ctx.lcd.set_gestures(false);
    match end {
        End::Quit => info!(target: "games", "{} left", g.name),
        End::Over(score) => {
            let record = score > best;
            info!(target: "games", "{} over, score {}{}", g.name, score, if record { " (new best)" } else { "" });
            if record {
                let _ = scores.set_bytes(key(game, control), &score.to_le_bytes());
            }
            game_over(ctx.lcd, score, best.max(score), record).await;
        }
    }
}

/// Pick the control for a round of `g`, starting at `current`: `None` to go back.
async fn choose(lcd: Lcd, g: &Game, current: Control) -> Option<Control> {
    let mut i = Control::ALL.iter().position(|c| *c == current).unwrap_or(0);
    lcd.clear_events();
    loop {
        lcd.draw(|c| {
            c.clear(Color::White);
            c.fill_rect(Rect::new(0, 0, 128, 18), Color::Black);
            c.text(64, 1, g.name, &TITLE);
            c.text(64, 21, "play with", &TEXT.align(Align::Center));
            for (k, control) in Control::ALL.iter().enumerate() {
                let cy = 44 + k as i32 * 22;
                let row = Rect::new(4, cy - 10, 120, 21);
                if k == i {
                    c.round_rect(row, 6, Color::Black);
                    c.round_rect(row.inset(1), 5, Color::Black);
                }
                icon(c, 30, cy, control.icon());
                c.text(58, cy - 6, control.name(), &TEXT);
            }
            c.text(64, 107, "L/R move, hold R: pick", &HUD.align(Align::Center));
            c.text(64, 116, "hold L: back", &HUD.align(Align::Center));
        })
        .await;
        match lcd::default_nav(lcd.next_event().await) {
            Some(Nav::Prev) => i = (i + 2) % 3,
            Some(Nav::Next) => i = (i + 1) % 3,
            Some(Nav::Enter) => return Some(Control::ALL[i]),
            Some(Nav::Back) => return None,
            None => {}
        }
    }
}

/// The HARDWARIO TOWER logo in the middle of the screen, `subtitle` under it, for `ms`.
async fn splash(lcd: Lcd, subtitle: &str, ms: u64) {
    lcd.draw(|c| {
        c.clear(Color::White);
        let (w, h) = (logo::LOGO.width as i32, logo::LOGO.height as i32);
        c.image((128 - w) / 2, (128 - h) / 2, &logo::LOGO, Color::Black, None);
        c.text(64, (128 + h) / 2 + 14, subtitle, &TEXT.align(Align::Center));
    })
    .await;
    Timer::after(Duration::from_millis(ms)).await;
}

/// The local EEPROM key of a game's best score with a control.
fn key(game: usize, control: Control) -> u8 {
    0x50 + (game * 3) as u8 + control as u8
}

fn best(scores: Scoped, game: usize, control: Control) -> u32 {
    let mut b = [0u8; 4];
    match scores.get_bytes(key(game, control), &mut b) {
        Ok(Some(4)) => u32::from_le_bytes(b),
        _ => 0,
    }
}

/// The game's card: its controls and best score. `true` to play, `false` to go back.
async fn card(lcd: Lcd, g: &Game, control: Control, best: u32) -> bool {
    lcd.draw(|c| {
        c.clear(Color::White);
        c.fill_rect(Rect::new(0, 0, 128, 18), Color::Black);
        c.text(64, 1, g.name, &TITLE);
        // One control a row: its icon in a column on the left, the words beside it.
        for (i, (kind, words)) in g.controls[control as usize].iter().enumerate() {
            let cy = 28 + i as i32 * 16;
            icon(c, 22, cy, *kind);
            let x = if matches!(kind, Icon::Note) { 8 } else { 44 };
            c.text(x, cy - 6, words, &TEXT);
        }
        c.text_fmt(64, 71, format_args!("best {best}"), &TEXT.align(Align::Center));
        c.hline(4, 86, 120, Color::Black);
        // Quit: (L) + (R) held 2 s.
        button_glyph(c, 24, 96, "L");
        c.text(37, 90, "+", &TEXT.align(Align::Center));
        button_glyph(c, 50, 96, "R");
        c.text(62, 90, "2 s: quit", &TEXT);
        c.fill_round_rect(Rect::new(10, 108, 108, 18), 9, Color::Black);
        c.text(
            64,
            111,
            "click to play",
            &TEXT.align(Align::Center).color(Color::White),
        );
    })
    .await;
    lcd.clear_events();
    loop {
        match lcd.next_event().await {
            Event::Button(Button::Left, Kind::Hold) => return false,
            Event::Button(_, Kind::Click) | Event::Gesture(_) => return true,
            _ => {}
        }
    }
}

async fn game_over(lcd: Lcd, score: u32, best: u32, record: bool) {
    if record {
        lcd.led().play(RECORD);
    } else {
        lcd.led().blink(Rgb::RED, 300, 0, 1);
    }
    lcd.draw(|c| {
        panel(c, Rect::new(10, 18, 108, 92));
        c.text(64, 24, "GAME OVER", &TEXT.align(Align::Center));
        c.text_fmt(64, 40, format_args!("{score}"), &BIG);
        if record {
            c.text(64, 76, "new best!", &TEXT.align(Align::Center));
        } else {
            c.text_fmt(64, 76, format_args!("best {best}"), &TEXT.align(Align::Center));
        }
        c.text(64, 92, "click", &HUD.align(Align::Center));
    })
    .await;
    // Ignore the presses that ended the game, then wait for a deliberate one.
    Timer::after(Duration::from_millis(700)).await;
    lcd.clear_events();
    while !matches!(
        lcd.next_event().await,
        Event::Button(_, Kind::Click | Kind::Hold) | Event::Gesture(_)
    ) {}
}

async fn high_scores(lcd: Lcd, scores: Scoped) {
    lcd.draw(|c| {
        c.clear(Color::White);
        c.fill_rect(Rect::new(0, 0, 128, 18), Color::Black);
        c.text(64, 1, "High scores", &TITLE);
        // A column per control.
        for (k, head) in ["btn", "swp", "tlt"].iter().enumerate() {
            c.text(84 + k as i32 * 20, 22, head, &HUD.align(Align::Right));
        }
        for (i, g) in GAMES.iter().enumerate() {
            let y = 34 + i as i32 * 16;
            c.text(2, y, g.name, &HUD);
            for (k, control) in Control::ALL.iter().enumerate() {
                let b = best(scores, i, *control);
                c.text_fmt(
                    84 + k as i32 * 20,
                    y,
                    format_args!("{b}"),
                    &HUD.align(Align::Right),
                );
            }
        }
    })
    .await;
    lcd.clear_events();
    while !matches!(
        lcd.next_event().await,
        Event::Button(_, Kind::Click | Kind::Hold) | Event::Gesture(_)
    ) {}
}

app!(run, commands: CMDS);
