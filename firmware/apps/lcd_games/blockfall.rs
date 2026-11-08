//! Blockfall: pieces of two to five cells fall into a well nine cells wide; fill a row to clear
//! it. The well speeds up every eight rows.
//!
//! - Buttons: a click moves the piece, holding the left button rotates it, holding the right one
//!   drops it.
//! - Swipes: left and right move it, up rotates it, down drops it.
//! - Tilt: leaning the board slides it (faster the further it leans), tilting the board away
//!   rotates it (once per tilt), tilting it towards the player makes it fall faster.

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use tower::hmi::{Align, Color, Rect};
use tower::lcd::{Button, Event, Gesture, Kind, Rgb};

use crate::{Control, Ctx, End, HUD, Pad, TEXT};

const W: i32 = 9;
const H: i32 = 16;
const CELL: i32 = 7;
const X0: i32 = 4;
const Y0: i32 = 12;
const PANEL_X: i32 = X0 + W * CELL + 6;
/// How often tilt is looked at.
const TILT_POLL: Duration = Duration::from_millis(50);

/// The piece set: a domino, two triominoes, two tetrominoes and two pentominoes, as cell offsets.
const PIECES: [&[(i32, i32)]; 7] = [
    &[(0, 0), (1, 0)],
    &[(0, 0), (1, 0), (2, 0)],
    &[(0, 0), (0, 1), (1, 1)],
    &[(0, 0), (1, 0), (0, 1), (1, 1)],
    &[(0, 0), (1, 0), (2, 0), (1, 1)],
    &[(0, 0), (1, 0), (0, 1), (1, 1), (0, 2)],
    &[(1, 0), (0, 1), (1, 1), (2, 1), (1, 2)],
];

#[derive(Clone, Copy)]
struct Piece {
    cells: [(i32, i32); 5],
    n: usize,
    x: i32,
    y: i32,
}

impl Piece {
    fn new(kind: usize) -> Self {
        let mut cells = [(0, 0); 5];
        cells[..PIECES[kind].len()].copy_from_slice(PIECES[kind]);
        let w = cells[..PIECES[kind].len()].iter().map(|c| c.0).max().unwrap_or(0) + 1;
        Self {
            cells,
            n: PIECES[kind].len(),
            x: (W - w) / 2,
            y: 0,
        }
    }

    fn cells(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.cells[..self.n]
            .iter()
            .map(|&(cx, cy)| (self.x + cx, self.y + cy))
    }

    /// Turned a quarter clockwise, kept at the top left of its box.
    fn rotated(&self) -> Self {
        let mut p = *self;
        for c in &mut p.cells[..p.n] {
            *c = (-c.1, c.0);
        }
        let (mx, my) = p.cells[..p.n]
            .iter()
            .fold((i32::MAX, i32::MAX), |m, c| (m.0.min(c.0), m.1.min(c.1)));
        for c in &mut p.cells[..p.n] {
            *c = (c.0 - mx, c.1 - my);
        }
        p
    }

    fn moved(&self, dx: i32, dy: i32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            ..*self
        }
    }
}

/// The well, a row of bits per row.
struct Well([u16; H as usize]);

impl Well {
    fn fits(&self, p: &Piece) -> bool {
        p.cells()
            .all(|(x, y)| (0..W).contains(&x) && (0..H).contains(&y) && self.0[y as usize] & (1 << x) == 0)
    }

    fn lock(&mut self, p: &Piece) {
        for (x, y) in p.cells() {
            self.0[y as usize] |= 1 << x;
        }
    }

    /// The full rows, as a bit mask.
    fn full(&self) -> u32 {
        (0..H)
            .filter(|&y| self.0[y as usize] == (1 << W) - 1)
            .fold(0, |m, y| m | 1 << y)
    }

    fn clear(&mut self, rows: u32) {
        let mut out = H - 1;
        for y in (0..H).rev() {
            if rows & (1 << y) == 0 {
                self.0[out as usize] = self.0[y as usize];
                out -= 1;
            }
        }
        for y in 0..=out {
            self.0[y as usize] = 0;
        }
    }
}

/// What the player asked of the piece.
enum Move {
    Side(i32),
    Rotate,
    Drop,
}

pub async fn play(ctx: &mut Ctx) -> End {
    let lcd = ctx.lcd;
    if ctx.control == Control::Buttons {
        // A hold sooner than the menu's: it rotates or drops, so it must feel quick. No repeats.
        lcd.set_buttons(tower::hmi::input::Config {
            hold_ms: 350,
            ..Default::default()
        });
    }
    let mut well = Well([0; H as usize]);
    let mut piece = Piece::new(ctx.rng.range(0, 7) as usize);
    let mut next = ctx.rng.range(0, 7) as usize;
    let (mut score, mut rows) = (0u32, 0u32);
    let mut fall_at = Instant::now();
    let mut pad = Pad::default();
    let mut tilt = TiltInput::default();
    loop {
        if pad.quit() {
            return End::Quit;
        }
        let level = rows / 8 + 1;
        draw(ctx, &well, Some(&piece), next, score, rows, level, 0).await;
        let period = Duration::from_millis(700u64.saturating_sub(60 * (level as u64 - 1)).max(110));
        if fall_at <= Instant::now() {
            fall_at = Instant::now() + period;
        }
        // Tilt is polled; buttons and swipes arrive as events.
        let wake = match ctx.control {
            Control::Tilt => fall_at.min(Instant::now() + TILT_POLL),
            _ => fall_at,
        };
        let mut landed = false;
        let mut moves: heapless::Vec<Move, 2> = heapless::Vec::new();
        match select(Timer::at(pad.wake(wake)), lcd.next_event()).await {
            Either::First(()) => {
                if ctx.control == Control::Tilt {
                    let (lx, ly) = ctx.lean();
                    tilt.read(lx, ly, &mut moves);
                    if tilt.fast {
                        // Falling faster: a step every 60 ms at most.
                        fall_at = fall_at.min(Instant::now() + Duration::from_millis(60));
                    }
                }
                if Instant::now() >= fall_at {
                    let down = piece.moved(0, 1);
                    if well.fits(&down) {
                        piece = down;
                    } else {
                        landed = true;
                    }
                    fall_at = Instant::now() + period;
                }
            }
            Either::Second(ev) => {
                pad.feed(&ev);
                let m = match (ctx.control, ev) {
                    (Control::Buttons, Event::Button(Button::Left, Kind::Click)) => Some(Move::Side(-1)),
                    (Control::Buttons, Event::Button(Button::Right, Kind::Click)) => Some(Move::Side(1)),
                    (Control::Buttons, Event::Button(Button::Left, Kind::Hold)) => Some(Move::Rotate),
                    (Control::Buttons, Event::Button(Button::Right, Kind::Hold)) => Some(Move::Drop),
                    (Control::Swipes, Event::Gesture(Gesture::Left)) => Some(Move::Side(-1)),
                    (Control::Swipes, Event::Gesture(Gesture::Right)) => Some(Move::Side(1)),
                    (Control::Swipes, Event::Gesture(Gesture::Up)) => Some(Move::Rotate),
                    (Control::Swipes, Event::Gesture(Gesture::Down)) => Some(Move::Drop),
                    _ => None,
                };
                if let Some(m) = m {
                    let _ = moves.push(m);
                }
            }
        }
        for m in moves {
            match m {
                Move::Side(dx) => {
                    if well.fits(&piece.moved(dx, 0)) {
                        piece = piece.moved(dx, 0);
                    }
                }
                Move::Rotate => {
                    let r = piece.rotated();
                    // Kick it sideways if it doesn't fit where it turned.
                    if let Some(p) = [0, -1, 1, -2, 2]
                        .into_iter()
                        .map(|dx| r.moved(dx, 0))
                        .find(|p| well.fits(p))
                    {
                        piece = p;
                    }
                }
                Move::Drop => {
                    while well.fits(&piece.moved(0, 1)) {
                        piece = piece.moved(0, 1);
                        score += 1;
                    }
                    landed = true;
                }
            }
        }
        if !landed {
            continue;
        }
        well.lock(&piece);
        let full = well.full();
        if full != 0 {
            let n = full.count_ones();
            score += [0, 10, 30, 60, 100, 150][n as usize] * level;
            rows += n;
            lcd.led()
                .blink(if n > 1 { Rgb::MAGENTA } else { Rgb::BLUE }, 60, 0, 1);
            // Flash the full rows, then let the rest fall.
            for k in 0..4 {
                draw(
                    ctx,
                    &well,
                    None,
                    next,
                    score,
                    rows,
                    level,
                    if k % 2 == 0 { full } else { 0 },
                )
                .await;
                Timer::after(Duration::from_millis(70)).await;
            }
            well.clear(full);
        }
        piece = Piece::new(next);
        next = ctx.rng.range(0, 7) as usize;
        fall_at = Instant::now() + period;
        if !well.fits(&piece) {
            lcd.led().blink(Rgb::RED, 80, 60, 3);
            draw(ctx, &well, Some(&piece), next, score, rows, level, 0).await;
            return End::Over(score);
        }
    }
}

/// Tilt as Blockfall moves: leaning sideways slides the piece a column at a time, faster the
/// further the board leans; tilting it away rotates the piece once (tilt back to rotate again);
/// tilting it towards the player makes it fall faster. Milli-g, from where it was held at the start.
#[derive(Default)]
struct TiltInput {
    next_slide: Option<Instant>,
    /// A rotation is taken: the board must come back before the next.
    rotated: bool,
    /// Falling faster.
    fast: bool,
}

impl TiltInput {
    /// About 10 degrees to start sliding, 25 to slide fast.
    const SLIDE: i32 = 170;
    const SLIDE_FAST: i32 = 420;
    /// About 14 degrees away to rotate, back under 7 to arm the next one.
    const ROTATE: i32 = -250;
    const REARM: i32 = -120;
    /// About 14 degrees towards the player to fall faster.
    const FAST: i32 = 250;

    fn read(&mut self, lx: i32, ly: i32, moves: &mut heapless::Vec<Move, 2>) {
        let now = Instant::now();
        if lx.abs() < Self::SLIDE {
            self.next_slide = None;
        } else if self.next_slide.is_none_or(|t| now >= t) {
            let _ = moves.push(Move::Side(lx.signum()));
            let gap = if lx.abs() >= Self::SLIDE_FAST { 90 } else { 220 };
            self.next_slide = Some(now + Duration::from_millis(gap));
        }
        if ly <= Self::ROTATE && !self.rotated {
            let _ = moves.push(Move::Rotate);
            self.rotated = true;
        } else if ly > Self::REARM {
            self.rotated = false;
        }
        self.fast = ly >= Self::FAST;
    }
}

fn block(c: &mut tower::hmi::Canvas<'_>, x: i32, y: i32, solid: bool) {
    let r = Rect::new(x, y, CELL - 1, CELL - 1);
    if solid {
        c.fill_round_rect(r, 1, Color::Black);
        c.pixel(x + 1, y + 1, Color::White);
    } else {
        c.round_rect(r, 1, Color::Black);
        c.fill_rect(Rect::new(x + 2, y + 2, 2, 2), Color::Black);
    }
}

#[allow(clippy::too_many_arguments)]
async fn draw(
    ctx: &mut Ctx,
    well: &Well,
    piece: Option<&Piece>,
    next: usize,
    score: u32,
    rows: u32,
    level: u32,
    flash: u32,
) {
    ctx.lcd
        .draw(|c| {
            c.clear(Color::White);
            // The well: walls and floor.
            c.vline(X0 - 2, Y0, H * CELL + 2, Color::Black);
            c.vline(X0 + W * CELL, Y0, H * CELL + 2, Color::Black);
            c.hline(X0 - 2, Y0 + H * CELL + 1, W * CELL + 3, Color::Black);
            for y in 0..H {
                for x in 0..W {
                    if well.0[y as usize] & (1 << x) != 0 {
                        block(c, X0 + x * CELL, Y0 + y * CELL, true);
                    }
                }
                if flash & (1 << y) != 0 {
                    c.invert_rect(Rect::new(X0, Y0 + y * CELL, W * CELL - 1, CELL - 1));
                }
            }
            if let Some(p) = piece {
                for (x, y) in p.cells() {
                    block(c, X0 + x * CELL, Y0 + y * CELL, false);
                }
            }
            // The side panel: next piece, score, rows, level.
            c.text(PANEL_X, Y0, "NEXT", &HUD);
            for &(x, y) in PIECES[next] {
                block(c, PANEL_X + x * CELL, Y0 + 12 + y * CELL, true);
            }
            let right = TEXT.align(Align::Right);
            c.text(PANEL_X, 48, "SCORE", &HUD);
            c.text_fmt(126, 57, format_args!("{score}"), &right);
            c.text(PANEL_X, 76, "ROWS", &HUD);
            c.text_fmt(126, 85, format_args!("{rows}"), &right);
            c.text(PANEL_X, 104, "LEVEL", &HUD);
            c.text_fmt(126, 113, format_args!("{level}"), &right);
        })
        .await;
}
