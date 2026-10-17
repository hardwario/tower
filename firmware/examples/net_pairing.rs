//! net_pairing: OTA pairing by X25519 key agreement (pairing v2, docs/radio.md).
//!
//!   TOWER_FEATURES=role-gateway just flash example net_pairing   # host: opens a window
//!   TOWER_FEATURES=role-node    just flash example net_pairing   # joiner: requests to join
//!
//! The host precomputes its ephemeral key and opens a 1-minute pairing window
//! ([`PAIRING_WINDOW`]); the joiner sends JOIN_REQ with its own ephemeral public key, the host
//! answers with its public key and a confirmation tag (JOIN_RESP), the joiner checks it and
//! confirms under the newly derived link key (JOIN_CONFIRM). No key travels on air: both ends
//! derive the same link key. The **joiner chooses its own address** and keeps it. Both log the
//! first key bytes (they must match), the joiner's address (the same on both sides) and the net
//! layer's `pairing: SAS 123456` line (the same 6 digits on both sides: the out-of-band check
//! that no man in the middle sat between them).

#![no_std]
#![no_main]

use embassy_time::Timer;
use log::{error, info, warn};
use tower::radio::Spirit1;
use tower::radio::config::Band;
#[cfg(not(feature = "role-node"))]
use tower::radio::net::PairingHost;
use tower::radio::net::{Net, NetConfig, PAIRING_WINDOW};
use tower::{app, board::Board};

#[cfg(not(feature = "role-node"))]
const HOST_ADDR: u32 = 0x2222_2222;
// The joiner's OWN, self-chosen address (kept after pairing, not assigned by the host).
#[cfg(feature = "role-node")]
const MY_ADDR: u32 = 0x0000_00BB;

async fn run(b: Board) {
    let radio = Spirit1::new(
        b.radio_spi,
        b.radio_sck,
        b.radio_mosi,
        b.radio_miso,
        b.radio_cs,
        b.radio_sdn,
        b.radio_irq,
    );

    #[cfg(feature = "role-node")]
    let addr = MY_ADDR;
    #[cfg(not(feature = "role-node"))]
    let addr = HOST_ADDR;

    // The Net's own (default-lane) key is unused during pairing: the JOIN frames use the public
    // framing key and then the derived link key.
    let mut net = match Net::new(
        radio,
        b.kv,
        NetConfig {
            addr,
            key: [0; 16],
            band: Band::DEFAULT,
            channel: 0,
        },
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            error!(target: "pair", "net init: {e}");
            return;
        }
    };

    #[cfg(not(feature = "role-node"))]
    {
        info!(target: "pair", "HOST {:08X}: opening pairing window (1 min)", HOST_ADDR);
        loop {
            // A fresh window, a fresh precomputed ephemeral (TRNG only; refused without it).
            let Some(mut host) = PairingHost::prepare() else {
                error!(target: "pair", "no hardware TRNG: pairing refused");
                Timer::after_secs(10).await;
                continue;
            };
            match net.open_pairing(&mut host, PAIRING_WINDOW).await {
                // The joiner brought its own address; the host installs (peer_addr, key).
                Some((peer_addr, key)) => info!(
                    target: "pair",
                    "PAIRED *** node addr={:08X} (joiner-chosen) key[..4]={:02x}{:02x}{:02x}{:02x}",
                    peer_addr, key[0], key[1], key[2], key[3]
                ),
                None => warn!(target: "pair", "pairing window closed (no joiner / lost confirm)"),
            }
            Timer::after_secs(2).await;
        }
    }

    #[cfg(feature = "role-node")]
    {
        info!(target: "pair", "JOINER: requesting to join with my own address {:08X} (1 min)", MY_ADDR);
        loop {
            match net.join(MY_ADDR, PAIRING_WINDOW).await {
                Some((gw_addr, key)) => info!(
                    target: "pair",
                    "JOINED *** addr={:08X} (mine) gw={:08X} key[..4]={:02x}{:02x}{:02x}{:02x} (expect the host's) uid {}",
                    MY_ADDR, gw_addr, key[0], key[1], key[2], key[3], tower::board::UniqueId::read()
                ),
                None => warn!(target: "pair", "join failed (no host in range)"),
            }
            Timer::after_secs(3).await;
        }
    }
}

app!(run);
