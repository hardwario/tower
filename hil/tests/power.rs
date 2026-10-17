//! HIL power group (feature `power`): measure the Core Module's STOP-mode current floor.
//!
//! Flash the `lowpower` example over SWD with probe-rs (NOT `tower flash`: the measurement must not
//! depend on the FTDI), power-cycle the PPK2 at 1.8 V, sample 1 s, and assert on BOTH statistics
//! the sidecar returns: the **median** (the quiescent STOP floor; documented 5.1 µA @ 1.8 V in
//! examples/lowpower.rs) must sit in [1, 50) µA, and the **mean** (the true average draw: floor
//! plus the ~500 ms console VBUS-poll wake spikes) must stay under 100 µA.
//!
//! Topology (the whole reason this is subtle): the supported power bench is a **PPK2-powered**
//! Core whose FTDI adapter is **data-only** (no VBUS); it may stay plugged (the Core's port can
//! be visible in the roster; nothing here looks at it). What invalidates a run is **VBUS**: a
//! VBUS-carrying USB back-feeds VDD through the board's LDO above the PPK2's 1.8 V rail, so the
//! PPK2 sources ~0 and reads ~0.3 µA (which would PASS a bare `< 50 µA`), and VBUS-sense high
//! keeps the SDK out of STOP (by design). The **plausibility floor** (median ≥ 1 µA; the true floor
//! is 5.1 µA) is therefore the VBUS detector: a back-fed board fails it with an actionable message.
//! (If VBUS were present WITHOUT back-feeding, STOP inhibition reads mA and fails the 50 µA bound.)
//! The test never skips: a skip reads as a pass, and it only ever runs when a power run was asked
//! for.
//!
//! Doubly gated: the whole file is behind `#[cfg(feature = "power")]` AND the test is `#[ignore]`d
//! (it needs the J-Link + PPK2). `cargo test` compiles nothing here unless `--features power`, and
//! runs it only under `just hil-power` (`--features power -- --ignored --test-threads=1`).

#![cfg(feature = "power")]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tower_hil::{Ppk2, bench_or_fail_power, firmware_dir};

/// Upper bound on the STOP floor (median).
const FLOOR_MAX_UA: f64 = 50.0;
/// Plausibility floor, and the VBUS detector: the true DUT floor is 5.1 µA @ 1.8 V
/// (examples/lowpower.rs), so a median under 1 µA is not this board in STOP. It is a VBUS-powered
/// board (the LDO back-feeds VDD above the PPK2's rail, which then reads ~0.3 µA) or a
/// desynced/zero sample stream. Fail rather than pass on it.
const FLOOR_MIN_PLAUSIBLE_UA: f64 = 1.0;
/// Upper bound on the mean. The mean legitimately exceeds the median: every ~500 ms the console
/// manager wakes to poll VBUS (a short RUN burst at mA level), and those spikes are part of the
/// real average draw. 2× the floor bound budgets them generously while still catching a wake
/// storm / busy-loop regression, which averages into the hundreds of µA or mA.
const MEAN_MAX_UA: f64 = 100.0;

/// Build the `lowpower` release ELF in the firmware checkout (so its committed .cargo/config.toml
/// applies: default target thumbv6m, flip-link). Same sequence as examples/lowpower.rs's
/// header. Deliberately a plain cargo build: no `just`, so no `TOWER_FEATURES` leaks in.
fn build_lowpower() -> Result<PathBuf, String> {
    let repo = firmware_dir()?;
    let out = Command::new("cargo")
        .current_dir(&repo)
        .args(["build", "--release", "--example", "lowpower"])
        .output()
        .map_err(|e| format!("HIL: spawn cargo build lowpower: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "HIL: build lowpower failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(repo.join("target/thumbv6m-none-eabi/release/examples/lowpower"))
}

/// `probe-rs download` the ELF over SWD (chip STM32L083CZTx), then reset+detach. This path never
/// touches the FTDI, so it works on the PPK2-powered bench whatever the FTDI's state.
fn probe_rs_download(elf: &PathBuf) -> Result<(), String> {
    // `--connect-under-reset` asserts NRST across the connect, halting the core at reset, before
    // the running firmware drops it into STOP, which gates the debug AP ("AP has the wrong type").
    // NOTE: that needs NRST wired from the J-Link to the Core, and on THIS bench it is NOT (see
    // docs/bench.md); there the flag is inert and what makes the attach work is the SEQUENCING
    // in the test: a fresh PPK2 power-cycle immediately before this call, so probe-rs connects
    // inside the post-power-on RUN window (clock/LSE start-up, before the app first sleeps).
    let out = Command::new("probe-rs")
        .args([
            "download",
            "--chip",
            "STM32L083CZTx",
            "--connect-under-reset",
            "--binary-format",
            "elf",
        ])
        .arg(elf)
        .output()
        .map_err(|e| format!("HIL: spawn probe-rs download: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "HIL: probe-rs download failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // Detach the probe so the core runs free (the debug domain would otherwise add ~200 µA,
    // confounder #1; the PPK2 power-cycle before measuring also clears it).
    let _ = Command::new("probe-rs")
        .args(["reset", "--chip", "STM32L083CZTx"])
        .output();
    Ok(())
}

/// Assert the Core's STOP floor (median) is in [1, 50) µA and its mean under 100 µA at 1.8 V.
#[test]
#[ignore = "requires the HIL bench (Core on J-Link + PPK2, PPK2-powered, no VBUS on its USB)"]
fn power_stop_floor_under_50ua() {
    // Power-group resolution: needs no FTDI port (present or not); this test reaches the Core over
    // SWD + PPK2, not the console. A data-only FTDI may stay plugged; VBUS is caught by the floor.
    let bench = bench_or_fail_power();
    let mv = bench.ppk2.supply_mv;

    // Build FIRST. A release LTO build takes tens of seconds; done after powering the board it
    // would eat the short post-power-on RUN window the attach depends on (NRST isn't wired).
    let elf = build_lowpower().expect("build lowpower");

    // Then power-CYCLE (not just `on`: a PPK2 left on by an earlier session means the core is long
    // since asleep in STOP, where SWD can't attach) and attach IMMEDIATELY, inside the fresh
    // boot's RUN window. On the data-only-FTDI bench the PPK2 is the board's ONLY supply, so flashing
    // before sourcing would hit "VTref 0.00 V". One retry (re-cycle + re-attach): a missed window
    // is timing, two misses in a row is not.
    let mut ppk2 = Ppk2::spawn().expect("spawn PPK2 sidecar");
    let mut flashed = Err(String::new());
    for attempt in 0..2 {
        if attempt > 0 {
            eprintln!("HIL: SWD attach missed the boot window ({flashed:?}); re-cycling for one retry");
        }
        ppk2.cycle(mv).expect("PPK2 power-cycle before the SWD attach");
        // Rail rise only; probe-rs's own start-up adds a few hundred ms before it senses VTref.
        std::thread::sleep(Duration::from_millis(100));
        flashed = probe_rs_download(&elf);
        if flashed.is_ok() {
            break;
        }
    }
    flashed.expect("probe-rs flash lowpower (attach inside the post-power-on RUN window)");

    // Clean power-cycle at the bench rail (clears the debug-domain residual the flash left,
    // confounder #1; the rail is pinned at 1.8 V, see ppk2d.py #2), let the app settle into STOP,
    // then sample. The sidecar / harness reject a >50 mA read as a CDC desync (confounder #3).
    ppk2.cycle(mv).expect("PPK2 power-cycle");
    // 5 s settle: lowpower runs the app! boot indicator (500 ms + 2 s LED + 500 ms ≈ 3 s) before its
    // run loop drops into STOP, so a shorter settle would average the active boot current, not the
    // floor. 5 s leaves a comfortable margin past the ~3 s boot for the rail + STOP entry to settle.
    std::thread::sleep(Duration::from_secs(5));
    let r = ppk2.measure(1000).expect("PPK2 sample window");

    eprintln!(
        "Core STOP floor: median {:.1} µA, mean {:.1} µA over {} samples @ {mv} mV (backend: {})",
        r.median_ua,
        r.mean_ua,
        r.samples,
        ppk2.backend()
    );
    assert!(
        r.median_ua >= FLOOR_MIN_PLAUSIBLE_UA,
        "Core STOP median {:.2} µA < {FLOOR_MIN_PLAUSIBLE_UA} µA at {mv} mV: implausible for this \
         board (true floor 5.1 µA). The board is probably USB/VBUS-powered (reads ~0 µA: the LDO \
         back-feeds VDD above the PPK2's rail); unplug the VBUS-carrying USB, or use a data-only \
         FTDI adapter. (Otherwise the PPK2 sample stream desynced.) Not a pass.",
        r.median_ua
    );
    assert!(
        r.median_ua < FLOOR_MAX_UA,
        "Core STOP floor (median) {:.1} µA ≥ {FLOOR_MAX_UA} µA at {mv} mV: regression in the \
         USB-gated STOP path? (Or VBUS-sense is high: a VBUS-carrying USB keeps the SDK out of STOP \
         by design; use a data-only FTDI adapter.)",
        r.median_ua
    );
    assert!(
        r.mean_ua < MEAN_MAX_UA,
        "Core mean draw {:.1} µA ≥ {MEAN_MAX_UA} µA at {mv} mV (floor {:.1} µA): the board wakes \
         far more than the ~500 ms VBUS poll: a wake-storm / busy-loop regression?",
        r.mean_ua,
        r.median_ua
    );
}
