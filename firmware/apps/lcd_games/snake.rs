//! Snake: each meal grows the snake and speeds it up; the walls and its own body end it. The
//! buttons turn it left and right (from where it heads); a swipe over the gesture sensor sends it
//! that way (up, down, left, right); with tilt, it heads the way the board leans.

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use heapless::Deque;
use tower::hmi::{Align, Color, Rect};
use tower::lcd::{Button, Event, Gesture, Kind, Rgb};

use crate::{Control, Ctx, End, HUD, Pad};

const COLS: i32 = 30;
const ROWS: i32 = 28;
const CELL: i32 = 4;
const X0: i32 = 4;
const Y0: i32 = 14;
/// Every cell of the board: the longest a snake can grow.
const CELLS: usize = (COLS * ROWS) as usize;

/// The body as its tail cell and a ring of 2-bit steps (right, down, left, up) to the head, plus
/// an occupancy bitmap for O(1) self-collision: about 330 bytes for a snake as long as the board,
/// against 1.7 KB as a list of cells.
struct Body {
    steps: [u8; CELLS / 4],
    first: usize,
    len: usize,
    tail: (i32, i32),
    head: (i32, i32),
    taken: [u32; ROWS as usize],
}

const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

impl Body {
    fn new(at: (i32, i32)) -> Self {
        let mut b = Body {
            steps: [0; CELLS / 4],
            first: 0,
            len: 1,
            tail: at,
            head: at,
            taken: [0; ROWS as usize],
        };
        b.mark(at, true);
        b
    }

    fn step(&self, i: usize) -> (i32, i32) {
        let i = (self.first + i) % CELLS;
        DIRS[(self.steps[i / 4] >> (i % 4 * 2)) as usize & 3]
    }

    fn mark(&mut self, (x, y): (i32, i32), on: bool) {
        if on {
            self.taken[y as usize] |= 1 << x;
        } else {
            self.taken[y as usize] &= !(1 << x);
        }
    }

    fn taken(&self, (x, y): (i32, i32)) -> bool {
        self.taken[y as usize] & (1 << x) != 0
    }

    /// Grow at the head, into a neighbouring cell.
    fn push(&mut self, to: (i32, i32)) {
        let d = DIRS
            .iter()
            .position(|&d| d == (to.0 - self.head.0, to.1 - self.head.1))
            .unwrap_or(0);
        let i = (self.first + self.len - 1) % CELLS;
        self.steps[i / 4] = (self.steps[i / 4] & !(3 << (i % 4 * 2))) | (d as u8) << (i % 4 * 2);
        self.len += 1;
        self.head = to;
        self.mark(to, true);
    }

    fn pop_tail(&mut self) {
        let (dx, dy) = self.step(0);
        self.mark(self.tail, false);
        self.tail = (self.tail.0 + dx, self.tail.1 + dy);
        self.first = (self.first + 1) % CELLS;
        self.len -= 1;
    }

    /// The cells from the tail to the head.
    fn cells(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        let mut p = self.tail;
        (0..self.len).map(move |i| {
            let here = p;
            if i + 1 < self.len {
                let (dx, dy) = self.step(i);
                p = (p.0 + dx, p.1 + dy);
            }
            here
        })
    }
}

pub async fn play(ctx: &mut Ctx) -> End {
    let lcd = ctx.lcd;
    let mut body = Body::new((8, ROWS / 2));
    for x in 9..12 {
        body.push((x, ROWS / 2));
    }
    let mut dir = (1, 0);
    let mut turns: Deque<(i32, i32), 3> = Deque::new();
    let mut food = place_food(ctx, &body);
    let mut score = 0u32;
    let mut pad = Pad::default();
    loop {
        draw(ctx, &body, food, score).await;
        // Collect turns until the next step.
        let period = Duration::from_millis((230 - 6 * score as u64).max(90));
        let step_at = Instant::now() + period;
        loop {
            if pad.quit() {
                return End::Quit;
            }
            let ev = match select(Timer::at(pad.wake(step_at)), lcd.next_event()).await {
                Either::First(()) if Instant::now() >= step_at => break,
                Either::First(()) => continue,
                Either::Second(ev) => ev,
            };
            pad.feed(&ev);
            // The direction after the turns already queued.
            let last = turns.back().copied().unwrap_or(dir);
            let new = match (ctx.control, ev) {
                (Control::Swipes, Event::Gesture(g)) => match g {
                    Gesture::Up => (0, -1),
                    Gesture::Down => (0, 1),
                    Gesture::Left => (-1, 0),
                    Gesture::Right => (1, 0),
                },
                // Left and right turn relative to where the snake heads (y grows downwards).
                (Control::Buttons, Event::Button(Button::Left, Kind::Press)) => (last.1, -last.0),
                (Control::Buttons, Event::Button(Button::Right, Kind::Press)) => (-last.1, last.0),
                _ => continue,
            };
            // No turning back into itself, no repeats.
            if new != last && new != (-last.0, -last.1) {
                let _ = turns.push_back(new);
            }
        }
        if let Some(t) = turns.pop_front() {
            dir = t;
        }
        // Tilt: the way the board leans furthest, once it leans clearly (about 9 degrees).
        if ctx.control == Control::Tilt {
            let (lx, ly) = ctx.lean();
            let new = if lx.abs() >= ly.abs() {
                (lx.signum(), 0)
            } else {
                (0, ly.signum())
            };
            if lx.abs().max(ly.abs()) >= 150 && new != (-dir.0, -dir.1) {
                dir = new;
            }
        }
        let (hx, hy) = body.head;
        let next = (hx + dir.0, hy + dir.1);
        let eats = next == food;
        if !eats {
            body.pop_tail(); // the tail moves away first, so the head may take its cell
        }
        if next.0 < 0 || next.0 >= COLS || next.1 < 0 || next.1 >= ROWS || body.taken(next) {
            lcd.led().blink(Rgb::RED, 80, 60, 3);
            crash(ctx, next).await;
            return End::Over(score);
        }
        body.push(next);
        if eats {
            score += 1;
            lcd.led().blink(Rgb::GREEN, 40, 0, 1);
            if body.len == CELLS {
                return End::Over(score + 100); // the board is full: a bonus, and the end
            }
            food = place_food(ctx, &body);
        }
    }
}

fn place_food(ctx: &mut Ctx, body: &Body) -> (i32, i32) {
    loop {
        let p = (ctx.rng.range(0, COLS), ctx.rng.range(0, ROWS));
        if !body.taken(p) {
            return p;
        }
    }
}

fn cell(p: (i32, i32)) -> Rect {
    Rect::new(X0 + p.0 * CELL, Y0 + p.1 * CELL, CELL, CELL)
}

async fn draw(ctx: &mut Ctx, body: &Body, food: (i32, i32), score: u32) {
    ctx.lcd
        .draw(|c| {
            c.clear(Color::White);
            c.text(2, 2, "SNAKE", &HUD);
            c.text_fmt(126, 2, format_args!("{score}"), &HUD.align(Align::Right));
            c.rect(
                Rect::new(X0 - 2, Y0 - 2, COLS * CELL + 4, ROWS * CELL + 4),
                Color::Black,
            );
            // Food: a little diamond.
            let f = cell(food);
            c.hline(f.x + 1, f.y, 2, Color::Black);
            c.hline(f.x, f.y + 1, 4, Color::Black);
            c.hline(f.x, f.y + 2, 4, Color::Black);
            c.hline(f.x + 1, f.y + 3, 2, Color::Black);
            for p in body.cells().take(body.len - 1) {
                let r = cell(p);
                c.fill_rect(Rect::new(r.x, r.y, 3, 3), Color::Black);
            }
            // The head, a full cell with an eye.
            let h = cell(body.head);
            c.fill_rect(h, Color::Black);
            c.pixel(h.x + 1, h.y + 1, Color::White);
        })
        .await;
}

/// Flash the cell the snake ran into.
async fn crash(ctx: &mut Ctx, at: (i32, i32)) {
    let r = cell(at).inset(-2);
    for _ in 0..4 {
        ctx.lcd.draw(|c| c.invert_rect(r)).await;
        Timer::after(Duration::from_millis(120)).await;
    }
}
