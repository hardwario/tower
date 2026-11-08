//! Bounce: a paddle, a ball, walls and bricks. Where the ball meets the paddle sets its angle.
//! Three balls; clear the bricks for the next, faster round. Hold a button to move the paddle; or
//! swipe left or right to send it gliding that way (a swipe up or down stops it); or lean the board
//! (the paddle sits where it leans).

use embassy_time::{Duration, Ticker, Timer};
use tower::hmi::{Color, Rect};
use tower::lcd::{Event, Gesture, Rgb};

use crate::{Control, Ctx, End, HUD, Pad};

const BRICK_W: i32 = 15;
const BRICK_H: i32 = 5;
const COLS: i32 = 8;
const ROWS: i32 = 4;
const BRICKS_Y: i32 = 16;
const TOP: i32 = 11;
const PADDLE_Y: i32 = 118;
const PADDLE_W: i32 = 24;
/// Ball positions and speeds in 1/256 pixel.
const Q: i32 = 256;

pub async fn play(ctx: &mut Ctx) -> End {
    let end = game(ctx).await;
    ctx.lcd.led().set(Rgb::OFF);
    end
}

/// The paddle's speed in the buttons' and swipes' ways, 1/256 pixel a frame.
const PADDLE_V: i32 = 3 * Q;

async fn game(ctx: &mut Ctx) -> End {
    let lcd = ctx.lcd;
    let mut pad = Pad::default();
    // Swipes: where the paddle glides, -1, 0 or 1.
    let mut glide = 0;
    let mut bricks = [(1u16 << COLS) - 1; ROWS as usize];
    let (mut score, mut lives, mut round) = (0u32, 3, 0);
    let mut paddle = 64 * Q;
    let mut ticker = Ticker::every(Duration::from_millis(30));
    'serve: loop {
        // Speed grows with each round.
        let speed = (380 + round * 50).min(700);
        let (mut bx, mut by) = (paddle, (PADDLE_Y - 4) * Q);
        let (mut vx, mut vy) = (
            speed / 3
                * if ctx.rng.next_u32().is_multiple_of(2) {
                    1
                } else {
                    -1
                },
            -speed,
        );
        // The serve: the ball rides the paddle for a moment.
        for i in 0..40 {
            if drain(ctx, &mut pad, &mut glide) {
                return End::Quit;
            }
            paddle = steer(ctx, &pad, glide, paddle);
            bx = paddle;
            draw(ctx, &bricks, paddle, (bx, by), score, lives, i % 10 < 5).await;
            ticker.next().await;
        }
        loop {
            if drain(ctx, &mut pad, &mut glide) {
                return End::Quit;
            }
            paddle = steer(ctx, &pad, glide, paddle);
            // Two half steps per frame, so the ball never skips a brick.
            for _ in 0..2 {
                bx += vx / 2;
                by += vy / 2;
                let (x, y) = (bx / Q, by / Q);
                if x <= 1 || x >= 126 {
                    vx = -vx;
                    bx = bx.clamp(2 * Q, 125 * Q);
                }
                if y <= TOP + 1 {
                    vy = vy.abs();
                }
                // Bricks: which one holds the ball's center.
                let (col, row) = (x / (BRICK_W + 1), (y - BRICKS_Y) / (BRICK_H + 1));
                if y >= BRICKS_Y
                    && (0..ROWS).contains(&row)
                    && (0..COLS).contains(&col)
                    && bricks[row as usize] & (1 << col) != 0
                {
                    bricks[row as usize] &= !(1 << col);
                    score += (ROWS - row) as u32 * 5;
                    // From the side when it came in from the side, else from above or below.
                    let prev_col = (bx - vx / 2) / Q / (BRICK_W + 1);
                    if prev_col != col { vx = -vx } else { vy = -vy }
                }
                // The paddle: a hit nearer its end sends the ball out wider.
                let p = paddle / Q;
                if vy > 0 && (PADDLE_Y - 2..=PADDLE_Y).contains(&y) && (x - p).abs() <= PADDLE_W / 2 + 1 {
                    vx = (x - p) * speed / (PADDLE_W / 2 + 2);
                    vy = -(speed - vx.abs() / 3).max(speed / 2);
                    lcd.led().blink(Rgb::BLUE, 20, 0, 1);
                }
            }
            if by / Q > 128 {
                lives -= 1;
                lcd.led().blink(Rgb::RED, 150, 0, 1);
                if lives == 0 {
                    return End::Over(score);
                }
                continue 'serve;
            }
            if bricks.iter().all(|r| *r == 0) {
                lcd.led().blink(Rgb::GREEN, 80, 60, 3);
                bricks = [(1u16 << COLS) - 1; ROWS as usize];
                round += 1;
                score += 50;
                Timer::after(Duration::from_millis(400)).await;
                continue 'serve;
            }
            draw(ctx, &bricks, paddle, (bx, by), score, lives, true).await;
            ticker.next().await;
        }
    }
}

/// Drain the events into the pad and the swipes' glide; `true` to quit.
fn drain(ctx: &Ctx, pad: &mut Pad, glide: &mut i32) -> bool {
    ctx.drain(pad, |ev| match ev {
        Event::Gesture(Gesture::Left) => *glide = -1,
        Event::Gesture(Gesture::Right) => *glide = 1,
        Event::Gesture(Gesture::Up | Gesture::Down) => *glide = 0,
        _ => {}
    });
    pad.quit()
}

/// Where the paddle goes next: a held button moves it, a swipe sets it gliding, the board's lean
/// places it (about 25 degrees to either side reaches the walls).
fn steer(ctx: &Ctx, pad: &Pad, glide: i32, paddle: i32) -> i32 {
    let half = PADDLE_W / 2 * Q;
    let next = match ctx.control {
        Control::Buttons => paddle + pad.dir() * PADDLE_V,
        Control::Swipes => paddle + glide * PADDLE_V * 2 / 3,
        Control::Tilt => {
            let target = 64 * Q + ctx.lean().0.clamp(-450, 450) * 64 * Q / 450;
            paddle + (target - paddle) / 3
        }
    };
    next.clamp(half, 128 * Q - half)
}

async fn draw(
    ctx: &mut Ctx,
    bricks: &[u16; ROWS as usize],
    paddle: i32,
    ball: (i32, i32),
    score: u32,
    lives: i32,
    show_ball: bool,
) {
    ctx.lcd
        .draw(|c| {
            c.clear(Color::White);
            c.text_fmt(2, 1, format_args!("{score}"), &HUD);
            for i in 0..lives {
                c.fill_circle(124 - i * 7, 4, 2, Color::Black);
            }
            c.hline(0, TOP, 128, Color::Black);
            for row in 0..ROWS {
                for col in 0..COLS {
                    if bricks[row as usize] & (1 << col) != 0 {
                        let r = Rect::new(
                            col * (BRICK_W + 1),
                            BRICKS_Y + row * (BRICK_H + 1),
                            BRICK_W,
                            BRICK_H,
                        );
                        // Rows alternate solid and hatched.
                        if row % 2 == 0 {
                            c.fill_rect(r, Color::Black);
                        } else {
                            c.rect(r, Color::Black);
                            c.hline(r.x + 2, r.y + 2, r.w - 4, Color::Black);
                        }
                    }
                }
            }
            let p = paddle / Q;
            c.fill_round_rect(
                Rect::new(p - PADDLE_W / 2, PADDLE_Y, PADDLE_W, 4),
                2,
                Color::Black,
            );
            if show_ball {
                c.fill_circle(ball.0 / Q, ball.1 / Q, 2, Color::Black);
            }
        })
        .await;
}
