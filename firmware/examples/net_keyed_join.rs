//! net_keyed_join: the keyed join (E4) under a per-node key the host already holds
//! (docs/radio.md "Keyed join").
//!
//!   TOWER_FEATURES=role-gateway just flash example net_keyed_join   # host: knows the node
//!   TOWER_FEATURES=role-node    just flash example net_keyed_join   # joiner: has the key
//!
//! Both ends were provisioned with the same per-node key out of band (the gateway's
//! NodeAdd, a factory step, NFC…), so no key travels on air and no pairing window is
//! needed: the host answers a KEYED JOIN_REQ from a registered peer at any time, inside
//! its normal `recv()`. The joiner learns the host's address; both log it.
//!
//! The joiner repeats the join every 5 s, each time on a fresh TX counter (the echo the host
//! returns is that counter, so it proves the response is fresh), then sends one confirmed
//! uplink. Duty mode only: under LBT+AFA or FHSS `join_keyed` returns `WrongMode`. A refused
//! request (wrong key, stale counter, switched off) leaves the host's `net` log with a
//! rate-limited `keyed join from … refused: <reason>` line and the joiner with `no host
//! answered` (docs/test-plan.md J1b).

#![no_std]
#![no_main]

use embassy_time::Duration;
#[cfg(feature = "role-node")]
use embassy_time::Timer;
#[cfg(feature = "role-node")]
use log::warn;
use log::{error, info};
use tower::radio::Spirit1;
use tower::radio::config::Band;
use tower::radio::net::{Net, NetConfig};
use tower::{app, board::Board};

#[cfg(not(feature = "role-node"))]
const HOST_ADDR: u32 = 0x2222_2222;
// The joiner's own address, registered on the host together with its key.
const NODE_ADDR: u32 = 0x0000_00BB;
// The per-node key both ends hold (would be random per node in production).
const NODE_KEY: [u8; 16] = [
    0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF,
];

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
    let addr = NODE_ADDR;
    #[cfg(not(feature = "role-node"))]
    let addr = HOST_ADDR;

    let mut net = match Net::new(
        radio,
        b.kv,
        NetConfig {
            addr,
            key: NODE_KEY,
            band: Band::DEFAULT,
            channel: 0,
        },
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            error!(target: "kjoin", "net init: {e}");
            return;
        }
    };

    #[cfg(not(feature = "role-node"))]
    {
        // Host keyed joins (recv answers them from now on), then the NodeAdd equivalent: the
        // node's address and key are known up front.
        net.set_keyed_join(true);
        if !net.add_peer(NODE_ADDR, &NODE_KEY) {
            error!(target: "kjoin", "peer table full");
            return;
        }
        info!(target: "kjoin", "HOST {:08X}: node {:08X} registered, listening", HOST_ADDR, NODE_ADDR);
        loop {
            if let Some(rx) = net.recv(Duration::from_secs(1)).await {
                info!(target: "kjoin", "uplink from {:08X} ({} B)", rx.src, rx.data().len());
            }
            if let Some(node) = net.take_keyed_join() {
                info!(target: "kjoin", "JOINED *** node {:08X} (keyed, no key on air)", node);
            }
        }
    }

    #[cfg(feature = "role-node")]
    {
        info!(target: "kjoin", "JOINER {:08X}: keyed join", NODE_ADDR);
        loop {
            match net
                .join_keyed(NODE_ADDR, &NODE_KEY, Duration::from_secs(10))
                .await
            {
                Ok(gw_addr) => {
                    info!(target: "kjoin", "JOINED *** gw={:08X}", gw_addr);
                    let _ = net.add_peer(gw_addr, &NODE_KEY);
                    let r = net.send(gw_addr, b"hello", true, 3).await;
                    info!(target: "kjoin", "uplink: {:?}", r);
                }
                Err(e) => warn!(target: "kjoin", "keyed join failed: {e}"),
            }
            Timer::after_secs(5).await;
        }
    }
}

app!(run);
