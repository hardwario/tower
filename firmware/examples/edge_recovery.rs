//! edge_recovery: stuck-state / timeout recovery (docs/radio.md). Single board.
//!
//!   just flash example edge_recovery
//!
//! Hammers the failure paths that must self-heal: an RX that times out (no sender)
//! must SABORT back to READY, a FIFO flush must leave the FIFOs empty, and an
//! RX→READY cycle must always settle, never wedging the state machine. After 10
//! timeout/recovery cycles the device ID is re-read to prove the chip is still
//! fully responsive. Prints a verdict per check, then `all checks PASS` or `some checks FAIL`.

#![no_std]
#![no_main]

use embassy_time::Duration;
use log::{error, info};
use tower::radio::regs;
use tower::radio::{RfConfig, Spirit1, config};
use tower::{app, board::Board};

async fn run(b: Board) {
    let mut radio = Spirit1::new(
        b.radio_spi,
        b.radio_sck,
        b.radio_mosi,
        b.radio_miso,
        b.radio_cs,
        b.radio_sdn,
        b.radio_irq,
    );
    if let Err(e) = radio.exit_shutdown().await {
        error!(target: "edge", "shutdown exit failed: {e}");
    }
    let id_before = radio.read_device_id();
    if let Err(e) = config::apply(
        &mut radio,
        &RfConfig {
            band: config::Band::DEFAULT,
            channel: 0,
        },
    )
    .await
    {
        error!(target: "edge", "RF config failed: {e}");
    }

    let mut pass = true;
    let mut buf = [0u8; 96];

    // 10 RX timeouts (no sender): each must SABORT back to READY, not wedge.
    let mut recovered = 0u32;
    for i in 0..10 {
        match radio.rx(&mut buf, Duration::from_millis(120)).await {
            Err(tower::radio::RadioError::Timeout) => {}
            Ok((len, _)) => info!(target: "edge", "cycle {}: RX received {} B instead of timing out", i, len),
            Err(e) => info!(target: "edge", "cycle {}: RX failed instead of timing out: {e}", i),
        }
        // After a timeout the part should be back in READY (SABORT recovery).
        match radio.mc_state() {
            Ok(s) if s == regs::STATE_READY => recovered += 1,
            Ok(s) => {
                error!(target: "edge", "cycle {} recovery FAIL (state 0x{:02x}, not ready)", i, s);
                let _ = radio.to_ready().await; // try to recover anyway
            }
            Err(e) => error!(target: "edge", "cycle {}: state read failed: {e}", i),
        }
    }
    if recovered == 10 {
        info!(target: "edge", "RX timeout recovery PASS (10 of 10 back in ready)");
    } else {
        error!(target: "edge", "RX timeout recovery FAIL ({} of 10 back in ready)", recovered);
    }
    pass &= recovered == 10;

    // FIFO flush leaves both FIFOs empty.
    let _ = radio.flush_fifos();
    let rxn = radio.rx_fifo_count().unwrap_or(0xFF);
    let txn = radio.tx_fifo_count().unwrap_or(0xFF);
    if rxn == 0 && txn == 0 {
        info!(target: "edge", "FIFO flush PASS (RX 0 B, TX 0 B)");
    } else {
        error!(target: "edge", "FIFO flush FAIL (RX {} B, TX {} B)", rxn, txn);
    }
    pass &= rxn == 0 && txn == 0;

    // RX→READY cycle: enter RX, let it actually reach RX, then abort to READY.
    let entered = radio.enter_rx().await.is_ok();
    embassy_time::Timer::after(Duration::from_millis(5)).await; // let the synth lock + reach RX
    let readied = radio.to_ready().await.is_ok();
    let in_ready = radio.mc_state().map(|s| s == regs::STATE_READY).unwrap_or(false);
    let cycle_ok = entered && readied && in_ready;
    if cycle_ok {
        info!(target: "edge", "RX to ready cycle PASS");
    } else {
        let yn = |b: bool| if b { "yes" } else { "no" };
        error!(
            target: "edge",
            "RX to ready cycle FAIL (RX entered {}, ready strobed {}, in ready {})",
            yn(entered),
            yn(readied),
            yn(in_ready)
        );
    }
    pass &= cycle_ok;

    // Chip still fully responsive after all the abuse.
    let id_after = radio.read_device_id();
    let id_ok = id_before.is_ok() && id_after.is_ok();
    if id_ok {
        info!(target: "edge", "device ID read PASS (before and after)");
    } else {
        let ok = |b: bool| if b { "ok" } else { "failed" };
        error!(target: "edge", "device ID read FAIL (before {}, after {})", ok(id_before.is_ok()), ok(id_after.is_ok()));
    }
    pass &= id_ok;

    // Let the verdicts above leave the wire first: the console queue holds 4 frames and a log
    // line that finds it full is dropped, and the summary is the line the bench waits for.
    tower::console::flush().await;
    if pass {
        info!(target: "edge", "recovery, all checks PASS (state machine never wedged)");
    } else {
        error!(target: "edge", "recovery, some checks FAIL");
    }
    loop {
        embassy_time::Timer::after_secs(5).await;
    }
}

app!(run);
