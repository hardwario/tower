//! HIL radio group: a two-board RF test. Flash `net_confirmed` to both boards, the Core as
//! `role-node` (sender) and the Dongle as the gateway (default role), and assert the confirmed
//! exchange over the air (the node logs `delivered`/ACKed, the gateway logs the received frame).
//!
//! Sequential flash, then concurrent capture (concurrent FTDI flashing is unreliable; see
//! firmware/tools/hwtest/README.md). `#[ignore]`d: needs the bench + two boards in RF
//! range; run with `just bench` (`--test-threads=1`, exclusive ports).

use std::time::{Duration, Instant};

use tower_hil::{Bench, Console, Role, bench_or_fail, build_image, frame_text};

/// Build `net_confirmed` with the given cargo features (empty = none; `build_image` sets
/// TOWER_FEATURES explicitly, so an operator's exported value can't leak in), then flash the board
/// in `role` (identity re-checked right before the erase).
fn build_and_flash(bench: &Bench, role: Role, features: &str) -> Result<(), String> {
    let bin = build_image("example", "net_confirmed", features)?;
    bench.flash(role, &bin)
}

/// Core = node (sends confirmed frames), Dongle = gateway (receives + auto-ACKs). Assert the
/// node's confirmed send is delivered (ACK received) and the gateway logs a decrypted RX.
#[test]
#[ignore = "requires the HIL bench (two TOWER boards in RF range)"]
fn radio_net_confirmed_delivered_and_acked() {
    let bench = bench_or_fail();

    // Sequential flash: gateway first (Dongle, default role), then node (Core, role-node).
    build_and_flash(&bench, Role::Dongle, "").expect("flash gateway (Dongle)");
    build_and_flash(&bench, Role::Core, "role-node").expect("flash node (Core)");

    // Concurrent capture: open both consoles, reset both into the app, then read a window.
    // Each console pulses NRST on its own handle (Console::reset_into_app); a second open of an
    // attached tty fails under serialport 4.9's exclusive flock.
    let mut node = Console::open(&bench.core.serial).expect("open node console");
    let mut gw = Console::open(&bench.dongle.serial).expect("open gateway console");
    gw.reset_into_app().expect("reset gateway");
    node.reset_into_app().expect("reset node");

    // Node must report a confirmed delivery (an ACK came back). net_confirmed logs one outcome
    // line per send, starting with its sequence number: `#3 delivered (35 ms)`, `#3 not
    // delivered, no ACK (…)`, `#3 not sent: channel busy` (or `duty-cycle limited`), `#3 failed:
    // …`. We require at least ONE `delivered` within the window, NOT the strict first outcome:
    // the very first send after a fresh dual-reset lands in the SPIRIT1 warm-up and reliably
    // reports a one-shot `#0 failed: timeout` (~120 ms after radio init, before the
    // separately-reset gateway is armed); every subsequent send at 2 s cadence is delivered. A bare
    // `contains("delivered")` would also match `not delivered`, so exclude that phrase explicitly.
    // ONE overall deadline: the node logs an outcome every ~2 s forever, so restarting a fresh
    // per-line window on each undelivered outcome would never time out.
    let deadline = Instant::now() + Duration::from_secs(18);
    let mut delivered = false;
    let mut last = String::new();
    while !delivered {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Some(f) = node
            .wait_for(remaining, |f| frame_text(f).is_some_and(|t| t.starts_with('#')))
            .expect("node console read")
        else {
            break;
        };
        last = frame_text(&f).unwrap_or_default().to_string();
        delivered = last.contains("delivered") && !last.contains("not delivered");
    }
    assert!(
        delivered,
        "node never reported a confirmed delivery within 18 s (last outcome: {last:?})"
    );

    // Gateway must log at least one received/decrypted frame from the node: net_confirmed's
    // gateway RX line, `rx from 0x11111111, counter N, … dBm: "…" (ACKed)`, not just any log
    // that happens to contain "rx".
    let got_rx = gw
        .wait_for(Duration::from_secs(10), |f| {
            frame_text(f).is_some_and(|t| t.starts_with("rx from ") && t.contains("(ACKed)"))
        })
        .expect("gateway console read");
    assert!(
        got_rx.is_some(),
        "gateway never logged a received + ACKed frame from the node"
    );
}
