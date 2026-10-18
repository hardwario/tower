//! radio_cw: CW carrier bring-up, verified with two boards (no SDR needed).
//!
//! Build twice with a role feature:
//!   TOWER_FEATURES=role-node    just flash example radio_cw   # TX: keys an unmodulated
//!                                                      #     carrier on/off
//!   TOWER_FEATURES=role-gateway just flash example radio_cw   # RX: sits in RX and logs
//!                                                      #     RSSI on the channel
//! (no feature defaults to the RX role).
//!
//! With both on the same band/channel (EU 868.1 MHz, ch0), the RX board's RSSI
//! jumps from the noise floor (~-110 dBm) to a strong level while the TX keys CW,
//! and drops back when it stops, proving the synthesizer, PA and frequency math
//! without lab instruments.

#![no_std]
#![no_main]

use embassy_time::Timer;
use log::{error, info};
use tower::radio::{RfConfig, Spirit1, config};
use tower::{app, board::Board};

const CHANNEL: u8 = 0;

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
        error!(target: "cw", "shutdown exit failed: {e}");
    }
    if let Err(e) = radio.read_device_id() {
        error!(target: "cw", "device ID check failed: {e}");
    }

    let cfg = RfConfig {
        band: config::Band::DEFAULT,
        channel: CHANNEL,
    };
    match config::apply(&mut radio, &cfg).await {
        Ok(()) => {
            info!(target: "cw", "RF configured: EU 868, channel {} (868.{} MHz)", CHANNEL, 1 + CHANNEL * 2)
        }
        Err(e) => error!(target: "cw", "RF config failed: {e}"),
    }

    #[cfg(feature = "role-node")]
    tx_role(&mut radio).await;
    #[cfg(not(feature = "role-node"))]
    rx_role(&mut radio).await;
}

#[cfg(feature = "role-node")]
async fn tx_role(radio: &mut Spirit1) -> ! {
    info!(target: "cw", "TX: keying the carrier on channel {}, 3 s on, 2 s off", CHANNEL);
    loop {
        match radio.cw_test(true).await {
            Ok(()) => {
                // Let the synth settle (cal ~54-80 µs), then read the steady state.
                Timer::after_millis(50).await;
                let st = radio.mc_state().unwrap_or(0xFF);
                let lock_err = radio.error_lock().unwrap_or(false);
                let what = match st {
                    0x5F => "TX, emitting",
                    0x13 => "LOCKWON, synth not locked",
                    0x4F => "still calibrating",
                    _ => "unexpected",
                };
                info!(
                    target: "cw",
                    "carrier on, state 0x{:02x} ({}){}",
                    st,
                    what,
                    if lock_err { ", synth lock error" } else { "" }
                );
            }
            Err(e) => error!(target: "cw", "carrier on failed: {e}"),
        }
        Timer::after_secs(3).await;
        let _ = radio.cw_test(false).await;
        info!(target: "cw", "carrier off");
        Timer::after_secs(2).await;
    }
}

#[cfg(not(feature = "role-node"))]
async fn rx_role(radio: &mut Spirit1) -> ! {
    info!(target: "cw", "RX: sampling RSSI on channel {} (carrier detect)", CHANNEL);
    let mut n = 0u32;
    loop {
        match radio.rssi_sample().await {
            Ok(raw) => {
                let dbm = config::rssi_to_dbm(raw);
                let carrier = dbm > -100;
                // First few samples: also report state/lock to confirm RX works.
                if n < 3 {
                    let st = radio.mc_state().unwrap_or(0xFF);
                    let lock_err = radio.error_lock().unwrap_or(false);
                    info!(
                        target: "cw",
                        "RX check: state 0x{:02x}{}",
                        st,
                        if lock_err { ", synth lock error" } else { "" }
                    );
                }
                info!(
                    target: "cw",
                    "RSSI {} dBm, {} (raw 0x{:02x})",
                    dbm,
                    if carrier { "carrier" } else { "noise floor" },
                    raw
                );
            }
            Err(e) => error!(target: "cw", "RSSI sample failed: {e}"),
        }
        n = n.wrapping_add(1);
        Timer::after_millis(500).await;
    }
}

app!(run);
