//! net_bulk: bulk transfer via the pull mechanism (docs/radio.md).
//!
//!   TOWER_FEATURES=role-gateway just flash example net_bulk   # sender: serves a blob
//!   TOWER_FEATURES=role-node    just flash example net_bulk   # requester: pulls + verifies
//!
//! The sender announces a 200-byte blob (byte[i] = i) and answers BULK_REQ(index)
//! with BULK_DATA(index, ≤64 B); the requester pulls all 4 chunks, reassembles,
//! and verifies the pattern. Demonstrates announce → pull → reassemble with the
//! 24-bit chunk index in each chunk's nonce.

#![no_std]
#![no_main]

use embassy_time::Timer;
use log::{error, info, warn};
use tower::radio::Spirit1;
use tower::radio::config::Band;
use tower::radio::net::{Net, NetConfig};
use tower::{app, board::Board};

const NODE_ADDR: u32 = 0x1111_1111;
const GW_ADDR: u32 = 0x2222_2222;
const KEY: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
];
#[cfg(not(feature = "role-node"))]
const BLOB_LEN: usize = 200;

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
            error!(target: "bulk", "net init failed: {e}");
            return;
        }
    };

    #[cfg(not(feature = "role-node"))]
    {
        info!(target: "bulk", "sender 0x{:08x}: serving a {} B blob", GW_ADDR, BLOB_LEN);
        loop {
            let mut blob = [0u8; BLOB_LEN];
            for (i, b) in blob.iter_mut().enumerate() {
                *b = i as u8;
            }
            if net.bulk_serve(NODE_ADDR, &blob).await {
                info!(target: "bulk", "blob served");
            } else {
                warn!(target: "bulk", "blob not served to the end (session idled out)");
            }
            Timer::after_secs(1).await;
        }
    }

    #[cfg(feature = "role-node")]
    {
        info!(target: "bulk", "requester 0x{:08x}: pulling the blob from 0x{:08x}", NODE_ADDR, GW_ADDR);
        loop {
            let mut out = [0u8; 256];
            match net.bulk_fetch(GW_ADDR, &mut out).await {
                Some(n) => {
                    if (0..n).all(|i| out[i] == i as u8) {
                        info!(target: "bulk", "fetched {} B in {} chunks, pattern MATCH", n, n.div_ceil(64));
                    } else {
                        error!(target: "bulk", "fetched {} B in {} chunks, pattern MISMATCH", n, n.div_ceil(64));
                    }
                }
                None => warn!(target: "bulk", "fetch failed or timed out"),
            }
            Timer::after_secs(1).await;
        }
    }
}

app!(run);
