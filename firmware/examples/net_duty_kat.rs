//! net_duty_kat: deterministic check of the EU duty governor (docs/radio.md).
//! Single board, no radio: exercises the token-bucket accounting with known
//! values and reports a verdict per check, then `all checks PASS` (or `some checks
//! FAIL`), so the airtime logic is verified independently of timing.
//!
//!   just flash example net_duty_kat

#![no_std]
#![no_main]

use log::{error, info};
use tower::radio::duty::{DutyGovernor, frame_toa_ms};
use tower::{app, board::Board};

async fn run(_b: Board) {
    let mut pass = true;

    // ToA: ⌈(4+4+1+30+2)·8·1000/19200⌉ = ⌈17.08⌉ = 18 ms (rounded up, docs/radio.md).
    let toa = frame_toa_ms(30);
    verdict("time on air of 30 B", toa, 18);
    pass &= toa == 18;
    // Max non-bulk frame (96 B): ⌈44.58⌉ = 45 ms (docs/radio.md).
    let toa_max = frame_toa_ms(96);
    verdict("time on air of 96 B", toa_max, 45);
    pass &= toa_max == 45;

    // Bucket cap 100 ms, 1 % refill. Consume 17 ms frames until refused.
    let mut g = DutyGovernor::new(100, 10);
    let mut allowed = 0;
    for _ in 0..10 {
        if g.try_consume(17) {
            allowed += 1;
        } else {
            break;
        }
    }
    if allowed == 5 && g.budget_ms() == 15 {
        info!(target: "duty", "17 ms frames from a 100 ms bucket MATCH (5 allowed, 15 ms left)");
    } else {
        error!(
            target: "duty",
            "17 ms frames from a 100 ms bucket MISMATCH ({} allowed, {} ms left; expected 5, 15 ms)",
            allowed,
            g.budget_ms()
        );
    }
    pass &= allowed == 5 && g.budget_ms() == 15;
    pass &= !g.try_consume(17); // 15 < 17 → refused

    // Refill 1000 ms wall → +10 ms airtime (1 %); then one more 17 ms fits.
    g.refill_ms(1000);
    verdict("budget after a 1 s refill", g.budget_ms(), 25);
    pass &= g.budget_ms() == 25;
    pass &= g.try_consume(17); // 25 → 8
    pass &= !g.try_consume(17); // 8 < 17

    // Huge refill is capped at the bucket size.
    g.refill_ms(1_000_000);
    pass &= g.budget_ms() == 100;

    // The console queue holds 4 frames and a log line that finds it full is dropped: let the
    // four verdicts above leave the wire before the summary (what the bench reads).
    tower::console::flush().await;
    if pass {
        info!(target: "duty", "time on air and token bucket, all checks PASS");
    } else {
        error!(target: "duty", "time on air and token bucket, some checks FAIL");
    }
    loop {
        embassy_time::Timer::after_secs(5).await;
    }
}

/// Log a value in ms against the one the governor must produce: a MATCH or MISMATCH verdict.
fn verdict(what: &str, got: u32, expected: u32) {
    if got == expected {
        info!(target: "duty", "{} MATCH ({} ms)", what, got);
    } else {
        error!(target: "duty", "{} MISMATCH ({} ms, expected {} ms)", what, got, expected);
    }
}

app!(run);
