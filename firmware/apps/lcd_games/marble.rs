//! Marble Maze: roll a marble through a random maze into the hole in the far corner. The clock
//! runs down from a minute; every maze solved adds 15 seconds and a new maze. The score is the
//! mazes solved.
//!
//! With tilt, the marble rolls the way gravity pulls along the screen (the Core Module's
//! accelerometer, level where the board was held at the start). With the buttons or swipes, the
//! gravity is the game's own, shown by an arrow at the top: the buttons turn it a quarter left or
//! right, a swipe points it that way.

use embassy_time::{Duration, Instant, Ticker};
use tower::hmi::{Align, Color, Rect};
use tower::lcd::{Button, Event, Gesture, Kind, Rgb};

use crate::{Control, Ctx, End, HUD, Pad, Rng};

const N: i32 = 10;
const CELL: i32 = 12;
const X0: i32 = 4;
const Y0: i32 = 8;
/// The marble's radius; its collision box is `2R + 1` pixels.
const R: i32 = 3;
/// Positions and speeds in 1/256 pixel.
const Q: i32 = 256;
/// Fastest roll, pixels per frame: below the 2-pixel walls, so it can't pass through one.
const VMAX: i32 = 3 * Q / 2;
/// The game's own gravity (buttons and swipes), milli-g: as the board leaning about 20 degrees.
const G: i32 = 350;

/// Wall bits: `v[i]` bit `j` is the wall on vertical line `i` beside cell row `j`; `h[j]` bit `i`
/// the wall on horizontal line `j` above cell column `i`.
struct Maze {
    v: [u16; N as usize + 1],
    h: [u16; N as usize + 1],
}

impl Maze {
    /// A perfect maze (one path between any two cells), carved depth first from the start.
    fn new(rng: &mut Rng) -> Self {
        let mut m = Maze {
            v: [(1 << N) - 1; N as usize + 1],
            h: [(1 << N) - 1; N as usize + 1],
        };
        let mut seen = [0u16; N as usize];
        let mut stack = [(0u8, 0u8); (N * N) as usize];
        let mut top = 1;
        seen[0] = 1;
        while top > 0 {
            let (x, y) = (stack[top - 1].0 as i32, stack[top - 1].1 as i32);
            let mut options = [(0, 0); 4];
            let mut n = 0;
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let (nx, ny) = (x + dx, y + dy);
                if (0..N).contains(&nx) && (0..N).contains(&ny) && seen[ny as usize] & (1 << nx) == 0 {
                    options[n] = (dx, dy);
                    n += 1;
                }
            }
            if n == 0 {
                top -= 1;
                continue;
            }
            let (dx, dy) = options[rng.range(0, n as i32) as usize];
            let (nx, ny) = (x + dx, y + dy);
            match (dx, dy) {
                (1, 0) => m.v[nx as usize] &= !(1 << y),
                (-1, 0) => m.v[x as usize] &= !(1 << y),
                (0, 1) => m.h[ny as usize] &= !(1 << x),
                _ => m.h[y as usize] &= !(1 << x),
            }
            seen[ny as usize] |= 1 << nx;
            stack[top] = (nx as u8, ny as u8);
            top += 1;
        }
        m
    }

    /// The wall rectangles (2 pixels thick, overlapping at the corners) near the cell `(cx, cy)`.
    fn walls_near(&self, cx: i32, cy: i32, mut f: impl FnMut(Rect)) {
        for i in (cx - 1).max(0)..=(cx + 2).min(N) {
            for j in (cy - 1).max(0)..=(cy + 1).min(N - 1) {
                if self.v[i as usize] & (1 << j) != 0 {
                    f(Rect::new(X0 + i * CELL - 1, Y0 + j * CELL - 1, 2, CELL + 2));
                }
            }
        }
        for j in (cy - 1).max(0)..=(cy + 2).min(N) {
            for i in (cx - 1).max(0)..=(cx + 1).min(N - 1) {
                if self.h[j as usize] & (1 << i) != 0 {
                    f(Rect::new(X0 + i * CELL - 1, Y0 + j * CELL - 1, CELL + 2, 2));
                }
            }
        }
    }
}

struct Marble {
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
}

impl Marble {
    fn start() -> Self {
        Self {
            x: (X0 + CELL / 2) * Q + Q / 2,
            y: (Y0 + CELL / 2) * Q + Q / 2,
            vx: 0,
            vy: 0,
        }
    }

    fn cell(&self) -> (i32, i32) {
        ((self.x / Q - X0) / CELL, (self.y / Q - Y0) / CELL)
    }

    /// Roll along one axis, then push out of any wall it ran into, losing most of its speed.
    fn roll(&mut self, maze: &Maze, horizontal: bool) {
        if horizontal {
            self.x += self.vx
        } else {
            self.y += self.vy
        }
        let (cx, cy) = self.cell();
        let (px, py) = (self.x / Q, self.y / Q);
        let mut hit = None;
        maze.walls_near(cx, cy, |w| {
            if px + R >= w.x && px - R < w.right() && py + R >= w.y && py - R < w.bottom() {
                hit = Some(w);
            }
        });
        if let Some(w) = hit {
            if horizontal {
                self.x = if self.vx > 0 {
                    (w.x - R - 1) * Q
                } else {
                    (w.right() + R) * Q
                } + Q / 2;
                self.vx = -self.vx / 3;
            } else {
                self.y = if self.vy > 0 {
                    (w.y - R - 1) * Q
                } else {
                    (w.bottom() + R) * Q
                } + Q / 2;
                self.vy = -self.vy / 3;
            }
        }
    }
}

pub async fn play(ctx: &mut Ctx) -> End {
    let lcd = ctx.lcd;
    let mut maze = Maze::new(&mut ctx.rng);
    let mut marble = Marble::start();
    let mut solved = 0u32;
    let mut deadline = Instant::now() + Duration::from_secs(60);
    let goal = (X0 + (N - 1) * CELL + CELL / 2, Y0 + (N - 1) * CELL + CELL / 2);
    let mut ticker = Ticker::every(Duration::from_millis(30));
    let mut pad = Pad::default();
    // The game's gravity as a unit step, down the screen to start.
    let mut fall = (0, 1);
    loop {
        let control = ctx.control;
        ctx.drain(&mut pad, |ev| {
            fall = match (control, ev) {
                // A quarter turn, as the screen shows it (y grows downwards).
                (Control::Buttons, Event::Button(Button::Left, Kind::Press)) => (fall.1, -fall.0),
                (Control::Buttons, Event::Button(Button::Right, Kind::Press)) => (-fall.1, fall.0),
                (Control::Swipes, Event::Gesture(g)) => match g {
                    Gesture::Up => (0, -1),
                    Gesture::Down => (0, 1),
                    Gesture::Left => (-1, 0),
                    Gesture::Right => (1, 0),
                },
                _ => fall,
            }
        });
        if pad.quit() {
            return End::Quit;
        }
        // Gravity along the screen accelerates the marble (a small dead band for a board held
        // nearly level), friction slows it.
        let (gx, gy) = match control {
            Control::Tilt => ctx.lean(),
            _ => (fall.0 * G, fall.1 * G),
        };
        let dead = |g: i32| if g.abs() < 40 { 0 } else { g };
        marble.vx = (marble.vx + dead(gx) * 40 / 1000 - marble.vx / 24).clamp(-VMAX, VMAX);
        marble.vy = (marble.vy + dead(gy) * 40 / 1000 - marble.vy / 24).clamp(-VMAX, VMAX);
        marble.roll(&maze, true);
        marble.roll(&maze, false);

        let (px, py) = (marble.x / Q, marble.y / Q);
        if (px - goal.0).pow(2) + (py - goal.1).pow(2) <= 9 {
            solved += 1;
            lcd.led().blink(Rgb::GREEN, 80, 60, 2);
            deadline += Duration::from_secs(15);
            maze = Maze::new(&mut ctx.rng);
            marble = Marble::start();
        }
        let left = deadline.saturating_duration_since(Instant::now()).as_secs();
        if left == 0 {
            lcd.led().blink(Rgb::RED, 300, 0, 1);
            return End::Over(solved);
        }

        lcd.draw(|c| {
            c.clear(Color::White);
            c.text_fmt(2, 0, format_args!("maze {}", solved + 1), &HUD);
            c.text_fmt(
                126,
                0,
                format_args!("{}:{:02}", left / 60, left % 60),
                &HUD.align(Align::Right),
            );
            for i in 0..N {
                for j in 0..=N {
                    if maze.h[j as usize] & (1 << i) != 0 {
                        c.fill_rect(
                            Rect::new(X0 + i * CELL - 1, Y0 + j * CELL - 1, CELL + 2, 2),
                            Color::Black,
                        );
                    }
                    if maze.v[j as usize] & (1 << i) != 0 {
                        c.fill_rect(
                            Rect::new(X0 + j * CELL - 1, Y0 + i * CELL - 1, 2, CELL + 2),
                            Color::Black,
                        );
                    }
                }
            }
            // The game's gravity: a small arrow between the maze number and the clock.
            if control != Control::Tilt {
                let (cx, cy) = (64, 3);
                let (dx, dy) = fall;
                let tip = (cx + 3 * dx, cy + 3 * dy);
                c.line(cx - 3 * dx, cy - 3 * dy, tip.0, tip.1, Color::Black);
                c.line(tip.0, tip.1, cx + dx - 2 * dy, cy + dy + 2 * dx, Color::Black);
                c.line(tip.0, tip.1, cx + dx + 2 * dy, cy + dy - 2 * dx, Color::Black);
            }
            // The hole, and the marble with a glint.
            c.circle(goal.0, goal.1, 4, Color::Black);
            c.fill_circle(goal.0, goal.1, 2, Color::Black);
            c.fill_circle(px, py, R, Color::Black);
            c.pixel(px - 1, py - 1, Color::White);
        })
        .await;
        ticker.next().await;
    }
}
