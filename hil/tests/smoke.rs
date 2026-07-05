//! HIL smoke group: flash a self-checking KAT example to the Dongle, decode its framed boot
//! output natively, and await the `ALL PASS` verdict (tower-firmware docs/test-plan.md §1 —
//! self-checking KATs).
//!
//! These need the bench, so they are `#[ignore]`d: `cargo test` COMPILES them (proving the
//! harness API stays in sync) but runs none without `--ignored`. Run on the bench with
//! `just hil` (which passes `--test-threads=1` — the serial ports are exclusive).

use std::process::Command;
use std::time::Duration;

use tower_hil::{Frame, bench_or_fail, firmware_dir, frame_text, Console};

/// Flash a raw `.bin` to a board over the UART bootloader via `tower flash`. Sequential by design
/// (concurrent FTDI flashing is unreliable — Write-Memory timeouts / re-enumeration, see
/// tower-firmware's tools/hwtest/README.md). Returns the CLI's combined output on failure.
fn tower_flash(port: &str, bin: &str) -> Result<(), String> {
    let out = Command::new("tower")
        .args(["-d", port, "flash", bin])
        .output()
        .map_err(|e| format!("HIL: spawn `tower flash`: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "HIL: `tower -d {port} flash {bin}` failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Build a firmware example to a raw `.bin` with `just build example <name>`, returning the bin
/// path. (Kept here rather than re-implementing objcopy — the justfile owns the build.)
fn build_example(name: &str) -> Result<String, String> {
    let repo = firmware_dir()?;
    let out = Command::new("just")
        .current_dir(&repo)
        .args(["build", "example", name])
        .output()
        .map_err(|e| format!("HIL: spawn `just build example {name}`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "HIL: `just build example {name}` failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // `just build` writes the merged image to target/firmware.bin (see the justfile `bin`).
    Ok(repo.join("target/firmware.bin").display().to_string())
}

/// crypto_ccm_kat is a one-shot KAT: it prints RFC 3610 vector verdicts then `*** ALL PASS ***`
/// (or an `error!` FAIL line) within ms of boot. Flash it to the Dongle, reset, and await PASS.
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn smoke_ccm_kat_all_pass() {
    let bench = bench_or_fail();
    let bin = build_example("crypto_ccm_kat").expect("build crypto_ccm_kat");
    tower_flash(&bench.dongle.serial, &bin).expect("flash the Dongle");

    // Reset into the app so the one-shot verdict is emitted while we're listening, then decode.
    // The console LOANS its handle to jolt for the pulse — a second open of the same tty fails
    // under serialport 4.9's exclusive flock (Console::reset_into_app).
    let mut console = Console::open(&bench.dongle.serial).expect("open Dongle console");
    console.resync();
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
        .expect("no verdict line within 8 s — did the KAT boot?");

    let text = frame_text(&verdict).unwrap_or("");
    assert!(
        !text.contains("FAIL") && !text.contains("MISMATCH") && !text.contains("VIOLATION"),
        "crypto_ccm_kat reported a failure: {text:?}"
    );

    // Bonus over `strings | grep`: the boot stream must be gap-free (no dropped console frames).
    assert_eq!(console.seq_gaps(), 0, "console dropped frames during the KAT boot");
}

/// The boot banner is a typed `Hello` we can inspect (protocol version must match the wire crate
/// the harness pins — a mismatch is exactly the silent-mis-decode hazard the golden rule guards).
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn smoke_hello_banner_decodes() {
    let bench = bench_or_fail();
    let bin = build_example("blinky").expect("build blinky");
    tower_flash(&bench.dongle.serial, &bin).expect("flash the Dongle");

    let mut console = Console::open(&bench.dongle.serial).expect("open Dongle console");
    console.resync();
    console.reset_into_app().expect("reset the Dongle into the app");

    // Boot/attach timing tolerance (no RF involved): the Hello can miss a short window when
    // the reset pulse races the port attach (observed once on 2026-07-05, clean on retry).
    // 10 s window + one re-pulse keeps a real regression loud — two consecutive silent boots
    // is not a flake.
    let mut hello = None;
    for attempt in 0..2 {
        if attempt > 0 {
            eprintln!("HIL: no Hello on attempt 1 — re-pulsing reset for one retry");
            console.resync();
            console.reset_into_app().expect("reset the Dongle into the app (retry)");
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
    if let Frame::Hello { protocol_version, firmware_name, firmware_version, session_id } = hello {
        assert_eq!(protocol_version, tower_protocol::PROTOCOL_VERSION, "protocol version drift");
        assert!(!firmware_name.is_empty(), "empty firmware name in Hello");
        assert!(!firmware_version.is_empty(), "empty firmware version in Hello");
        assert!(session_id > 0, "session_id should be a bumped boot counter (>=1)");
    }
}
