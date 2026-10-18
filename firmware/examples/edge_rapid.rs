//! edge_rapid: back-to-back transfers, one-at-a-time, monotonic counters (docs/radio.md).
//!
//!   TOWER_FEATURES=role-node    just flash example edge_rapid   # hammers confirmed sends
//!   TOWER_FEATURES=role-gateway just flash example edge_rapid   # checks ordering
//!
//! The node fires confirmed sends with NO inter-send delay (the Net serializes
//! them: one transfer at a time, docs/radio.md). The gateway asserts every accepted frame's
//! counter is strictly greater than the last (monotonic, no reorder, no double-
//! accept); a violation latches a FAIL. Gaps (lost frames) are fine; what must
//! never happen is an out-of-order or repeated counter being accepted.

#![no_std]
#![no_main]

use log::{error, info};
use tower::radio::Spirit1;
use tower::radio::config::Band;
use tower::radio::net::{Net, NetConfig};
use tower::{app, board::Board};

#[cfg(not(feature = "role-node"))]
use embassy_time::Duration;
#[cfg(feature = "role-node")]
use tower::radio::net::SendResult;

const NODE_ADDR: u32 = 0x1111_1111;
const GW_ADDR: u32 = 0x2222_2222;
const KEY: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
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
    let addr = GW_ADDR;

    let mut net = match Net::new(
        radio,
        b.kv,
        NetConfig {
            addr,
            key: KEY,
            band: Band::DEFAULT,
            channel: 0,
            keys_stored: false,
        },
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            error!(target: "rapid", "net init failed: {e}");
            return;
        }
    };

    #[cfg(feature = "role-node")]
    {
        net.add_peer(GW_ADDR, &KEY);
        info!(target: "rapid", "node 0x{:08x}: confirmed sends back to back, no delay", NODE_ADDR);
        let mut seq: u32 = 0;
        let (mut ok, mut fail) = (0u32, 0u32);
        loop {
            match net.send(GW_ADDR, &seq.to_le_bytes(), true, 2).await {
                SendResult::Delivered => ok += 1,
                _ => fail += 1,
            }
            if seq.is_multiple_of(20) {
                info!(target: "rapid", "send #{}: {} delivered, {} not delivered", seq, ok, fail);
            }
            seq = seq.wrapping_add(1);
        }
    }

    #[cfg(not(feature = "role-node"))]
    {
        net.add_peer(NODE_ADDR, &KEY);
        info!(target: "rapid", "gateway 0x{:08x}: checking that accepted counters only rise", GW_ADDR);
        let mut last: Option<u32> = None;
        let (mut accepted, mut violations) = (0u32, 0u32);
        loop {
            if let Some(rx) = net.recv(Duration::from_secs(10)).await {
                accepted += 1;
                if let Some(p) = last
                    && rx.counter <= p
                {
                    violations += 1;
                    error!(target: "rapid", "counters monotonic FAIL (counter {} accepted after {})", rx.counter, p);
                }
                last = Some(rx.counter);
                if accepted.is_multiple_of(20) {
                    if violations == 0 {
                        info!(
                            target: "rapid",
                            "{} accepted, last counter {}, counters monotonic PASS",
                            accepted, rx.counter
                        );
                    } else {
                        error!(
                            target: "rapid",
                            "{} accepted, last counter {}, {} out of order, counters monotonic FAIL",
                            accepted, rx.counter, violations
                        );
                    }
                }
            }
        }
    }
}

app!(run);
