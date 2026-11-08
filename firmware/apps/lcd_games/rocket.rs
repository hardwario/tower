//! Asteroid Run: steer a rocket through falling asteroids. Every asteroid that passes scores a
//! point; they come faster and thicker as time goes on. With the buttons, hold one to steer; with
//! swipes, each swipe left or right moves the rocket a lane over (five lanes); with tilt, the
//! rocket goes where the board leans.

use embassy_time::{Duration, Ticker, Timer};
use heapless::Vec;
use tower::hmi::{Align, Color, Image, image};
use tower::lcd::{Event, Gesture, Rgb};

use crate::{Control, Ctx, End, HUD, Pad};

const ROCKET: Image = image!(
    "....#....",
    "...###...",
    "...#.#...",
    "..##.##..",
    "..#####..",
    "..#####..",
    "..#####..",
    ".#######.",
    "##.###.##",
    "#..###..#",
    "...#.#...",
);
const FLAME: [Image; 2] = [image!("#.#", ".#."), image!(".#.", "#.#")];

/// Rocket sprite top; the flame sits below it.
const ROCKET_Y: i32 = 128 - 16;
const MAX_ROCKS: usize = 12;
/// The swipe lanes' centers, pixels.
const LANES: [i32; 5] = [16, 40, 64, 88, 112];
/// Positions and speeds are in 1/16 pixel.
const Q: i32 = 16;

struct Rock {
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    r: i32,
    /// Where its craters sit.
    seed: u32,
}

pub async fn play(ctx: &mut Ctx) -> End {
    let lcd = ctx.lcd;
    let mut pad = Pad::default();
    let (mut x, mut vx) = (64 * Q, 0);
    let mut rocks: Vec<Rock, MAX_ROCKS> = Vec::new();
    let mut stars = [(0i32, 0i32); 16];
    for s in &mut stars {
        *s = (ctx.rng.range(0, 128), ctx.rng.range(0, 128 * Q));
    }
    let mut score = 0u32;
    let mut frame = 0u32;
    let mut spawn_in = 15;
    let mut lane = 2;
    let mut ticker = Ticker::every(Duration::from_millis(40));
    loop {
        ctx.drain(&mut pad, |ev| match ev {
            Event::Gesture(Gesture::Left) => lane = lane.max(1) - 1,
            Event::Gesture(Gesture::Right) => lane = (lane + 1).min(LANES.len() - 1),
            _ => {}
        });
        if pad.quit() {
            return End::Quit;
        }
        match ctx.control {
            // Steering with a little inertia.
            Control::Buttons => {
                let dir = pad.dir();
                vx = if dir == 0 {
                    vx * 2 / 3
                } else {
                    (vx + dir * 8).clamp(-44, 44)
                };
                x += vx;
            }
            // Sliding over to the lane.
            Control::Swipes => x += (LANES[lane] * Q - x).clamp(-48, 48),
            // Where the board leans: about 20 degrees to either side reaches the edge.
            Control::Tilt => {
                let target = 64 * Q + ctx.lean().0.clamp(-350, 350) * 59 * Q / 350;
                x += (target - x) / 3;
            }
        }
        x = x.clamp(5 * Q, 123 * Q);

        // Harder every 8 seconds: faster rocks, shorter gaps.
        let level = (frame / 200) as i32;
        spawn_in -= 1;
        if spawn_in <= 0 && !rocks.is_full() {
            let r = ctx.rng.range(3, 10);
            let _ = rocks.push(Rock {
                x: ctx.rng.range(r, 128 - r) * Q,
                y: -r * Q,
                vx: ctx.rng.range(-6, 7),
                vy: 18 + level * 4 + ctx.rng.range(0, 20) - r,
                r,
                seed: ctx.rng.next_u32(),
            });
            spawn_in = (20 - level * 2).max(5) + ctx.rng.range(0, 10);
        }
        let before = rocks.len();
        for rock in rocks.iter_mut() {
            rock.x += rock.vx;
            rock.y += rock.vy;
        }
        rocks.retain(|r| r.y / Q - r.r < 128);
        score += (before - rocks.len()) as u32;
        for (i, s) in stars.iter_mut().enumerate() {
            s.1 = (s.1 + if i % 2 == 0 { 8 } else { 20 }) % (128 * Q);
        }

        let rx = x / Q;
        let hit = rocks.iter().any(|rock| hits(rx, rock));
        lcd.draw(|c| {
            c.clear(Color::White);
            for s in &stars {
                c.pixel(s.0, s.1 / Q, Color::Black);
            }
            for rock in &rocks {
                let (cx, cy) = (rock.x / Q, rock.y / Q);
                c.fill_circle(cx, cy, rock.r, Color::Black);
                // Two craters, placed by the rock's seed.
                for k in 0..2 {
                    let bits = rock.seed >> (k * 8);
                    let (dx, dy) = ((bits % 5) as i32 - 2, ((bits >> 3) % 5) as i32 - 2);
                    c.fill_circle(
                        cx + dx * rock.r / 4,
                        cy + dy * rock.r / 4,
                        (rock.r / 4).max(1),
                        Color::White,
                    );
                }
            }
            c.image(rx - 4, ROCKET_Y, &ROCKET, Color::Black, Some(Color::White));
            c.image(
                rx - 1,
                ROCKET_Y + 11,
                &FLAME[(frame / 2 % 2) as usize],
                Color::Black,
                None,
            );
            c.text_fmt(126, 1, format_args!("{score}"), &HUD.align(Align::Right).opaque());
        })
        .await;
        if hit {
            explode(ctx, rx).await;
            return End::Over(score);
        }
        frame += 1;
        ticker.next().await;
    }
}

/// Whether any lit pixel of the rocket at `rx` lies within the rock.
fn hits(rx: i32, rock: &Rock) -> bool {
    let (cx, cy, r) = (rock.x / Q, rock.y / Q, rock.r);
    if (cy - ROCKET_Y - 5).abs() > r + 8 || (cx - rx).abs() > r + 6 {
        return false;
    }
    (0..ROCKET.height as i32).any(|iy| {
        (0..ROCKET.width as i32).any(|ix| {
            let (px, py) = (rx - 4 + ix, ROCKET_Y + iy);
            ROCKET.pixel(ix, iy) && (px - cx).pow(2) + (py - cy).pow(2) <= r * r
        })
    })
}

/// Rings flashing out of the wreck.
async fn explode(ctx: &mut Ctx, rx: i32) {
    ctx.lcd.led().blink(Rgb::RED, 80, 60, 3);
    let (cx, cy) = (rx, ROCKET_Y + 5);
    for r in [3, 7, 12, 18, 25, 33] {
        ctx.lcd
            .draw(|c| {
                c.circle(cx, cy, r, Color::Invert);
                c.fill_circle(cx, cy, r / 3, Color::Invert);
            })
            .await;
        Timer::after(Duration::from_millis(50)).await;
    }
}
