//! HIL smoke group: flash a self-checking KAT example to the Dongle, decode its framed boot
//! output natively, and await the `ALL PASS` verdict (firmware/docs/test-plan.md §1:
//! self-checking KATs).
//!
//! These need the bench, so they are `#[ignore]`d: `cargo test` COMPILES them (proving the
//! harness API stays in sync) but runs none without `--ignored`. Run on the bench with
//! `just hil` (which passes `--test-threads=1`; the serial ports are exclusive).

use std::time::Duration;

use tower_hil::{Console, Frame, Role, bench_or_fail, build_image, frame_text};

/// crypto_ccm_kat is a one-shot KAT: it prints RFC 3610 vector verdicts then `*** ALL PASS ***`
/// (or an `error!` FAIL line) within ms of boot. Flash it to the Dongle, reset, and await PASS.
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn smoke_ccm_kat_all_pass() {
    let bench = bench_or_fail();
    let bin = build_image("example", "crypto_ccm_kat", "").expect("build crypto_ccm_kat");
    bench.flash(Role::Dongle, &bin).expect("flash the Dongle");

    // Reset into the app so the one-shot verdict is emitted while we're listening, then decode.
    // The console pulses NRST on its OWN handle (a second open of the same tty fails under
    // serialport 4.9's exclusive flock, Console::reset_into_app) and drops pre-reset input.
    let mut console = Console::open(&bench.dongle.serial).expect("open Dongle console");
    console.reset_into_app().expect("reset the Dongle into the app");

    // Await a positive verdict; a FAIL/MISMATCH/VIOLATION line fails fast.
    let verdict = console
        .wait_for(Duration::from_secs(8), |f| {
            frame_text(f).is_some_and(|t| {
                t.contains("ALL PASS")
                    || t.contains("PASS ***")
                    || t.contains("FAIL")
                    || t.contains("MISMATCH")
                    || t.contains("VIOLATION")
            })
        })
        .expect("console read")
        .expect("no verdict line within 8 s; did the KAT boot?");

    let text = frame_text(&verdict).unwrap_or("");
    assert!(
        !text.contains("FAIL") && !text.contains("MISMATCH") && !text.contains("VIOLATION"),
        "crypto_ccm_kat reported a failure: {text:?}"
    );

    // Bonus over `strings | grep`: the boot stream must be gap-free (no dropped console frames).
    assert_eq!(
        console.seq_gaps(),
        0,
        "console dropped frames during the KAT boot"
    );
}

/// The boot banner is a typed `Hello` we can inspect (protocol version must match the wire crate
/// the harness pins; a mismatch is exactly the silent-mis-decode hazard the golden rule guards).
/// A mismatched firmware's frames fail the header's version check before any `Hello` payload is
/// parsed, so in practice `Console::next` fails fast with the lockstep message; the assert below
/// is the belt-and-braces check on the decoded field.
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn smoke_hello_banner_decodes() {
    let bench = bench_or_fail();
    let bin = build_image("example", "blinky", "").expect("build blinky");
    bench.flash(Role::Dongle, &bin).expect("flash the Dongle");

    let mut console = Console::open(&bench.dongle.serial).expect("open Dongle console");
    console.reset_into_app().expect("reset the Dongle into the app");

    // Boot/attach timing tolerance (no RF involved): the Hello can miss a short window when
    // the reset pulse races the port attach (observed once on 2026-07-05, clean on retry).
    // 10 s window + one re-pulse keeps a real regression loud: two consecutive silent boots
    // is not a flake.
    let mut hello = None;
    for attempt in 0..2 {
        if attempt > 0 {
            eprintln!("HIL: no Hello on attempt 1; re-pulsing reset for one retry");
            console
                .reset_into_app()
                .expect("reset the Dongle into the app (retry)");
        }
        if let Some(f) = console
            .wait_for(Duration::from_secs(10), |f| matches!(f, Frame::Hello { .. }))
            .expect("console read")
        {
            hello = Some(f);
            break;
        }
    }
    let hello = hello.expect("no Hello banner within 10 s, even after a reset retry");
    if let Frame::Hello {
        protocol_version,
        firmware_name,
        firmware_version,
        session_id,
    } = hello
    {
        assert_eq!(
            protocol_version,
            tower_protocol::PROTOCOL_VERSION,
            "protocol version drift"
        );
        assert!(!firmware_name.is_empty(), "empty firmware name in Hello");
        assert!(!firmware_version.is_empty(), "empty firmware version in Hello");
        assert!(session_id > 0, "session_id should be a bumped boot counter (>=1)");
    }
}
