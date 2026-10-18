//! radio_fhss: US 915 FHSS link (FCC §15.247), star topology.
//!
//!   TOWER_FEATURES=role-gateway just flash example radio_fhss   # hop time-master (beacons)
//!   TOWER_FEATURES=role-node    just flash example radio_fhss   # follower (scans → locks → sends)
//!
//! The gateway runs a free-running hop clock over 80 channels (903.0-926.7 MHz),
//! beaconing each 300 ms slot's pseudo-random channel and then listening. The node
//! parks on the rendezvous channel, catches a beacon within ≤1 cycle (~24 s),
//! reconstructs the gateway's slot clock, and then hops in lockstep, sending a
//! confirmed uplink each slot (the gateway is listening on that slot's channel) and
//! re-aligning on every beacon. Reset the gateway to see the node log `sync lost,
//! rescanning` and then lock again (F9). Both ends hop; the channel index moves every slot.
//!
//! US 915 FHSS is the compliant high-power path (unlike the narrowband bench mode);
//! exact §15.247 parameters are config to verify before a product claim.

#![no_std]
#![no_main]

use log::{error, info};
use tower::radio::Spirit1;
use tower::radio::config::Band;
use tower::radio::net::{FhssConfig, FhssRole, Net, NetConfig};
use tower::{app, board::Board};

#[cfg(feature = "role-node")]
use {
    log::warn,
    tower::radio::net::{FhssState, SendResult},
};

const NODE_ADDR: u32 = 0x1111_1111;
const GW_ADDR: u32 = 0x2222_2222;
const KEY: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
];

#[cfg(not(feature = "role-node"))]
fn seq_of(d: &[u8]) -> u32 {
    if d.len() >= 4 {
        u32::from_le_bytes([d[0], d[1], d[2], d[3]])
    } else {
        0
    }
}

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
            band: Band::Us915,
            channel: 0,
            keys_stored: false,
        },
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            error!(target: "fhss", "net init failed: {e}");
            return;
        }
    };

    #[cfg(not(feature = "role-node"))]
    {
        net.add_peer(NODE_ADDR, &KEY);
        if let Err(e) = net.enable_fhss(FhssRole::Master, FhssConfig::default()).await {
            error!(target: "fhss", "enabling failed: {e}");
            return;
        }
        info!(
            target: "fhss",
            "master 0x{:08x}: hopping 80 channels, beacon and listen per 300 ms slot",
            GW_ADDR
        );
        loop {
            let Some(s) = net.fhss_master_tick().await else {
                continue; // master mode not active (shouldn't happen: enabled above)
            };
            if let Some(rx) = s.received {
                info!(
                    target: "fhss",
                    "uplink #{} on channel {}, slot {}, from 0x{:08x}, {} dBm{}",
                    seq_of(rx.data()), s.channel, s.slot, rx.src, rx.rssi,
                    if rx.confirmed { " (ACKed)" } else { "" }
                );
            } else if s.slot.is_multiple_of(30) {
                info!(target: "fhss", "beaconing, slot {} on channel {} (no uplink)", s.slot, s.channel);
            }
        }
    }

    #[cfg(feature = "role-node")]
    {
        net.add_peer(GW_ADDR, &KEY);
        if let Err(e) = net.enable_fhss(FhssRole::Node, FhssConfig::default()).await {
            error!(target: "fhss", "enabling failed: {e}");
            return;
        }
        info!(
            target: "fhss",
            "node 0x{:08x}: scanning for a gateway beacon (parked on the rendezvous channel)",
            NODE_ADDR
        );
        let mut seq: u32 = 0;
        let mut was_synced = false;
        loop {
            let slot = net.fhss_node_tick().await;
            match slot.state {
                FhssState::Scanning => {
                    if was_synced {
                        warn!(target: "fhss", "sync lost, rescanning");
                        was_synced = false;
                    }
                }
                FhssState::Synced => {
                    if !was_synced {
                        info!(target: "fhss", "locked to the hop schedule, slot {} on channel {}", slot.slot, slot.channel);
                        was_synced = true;
                    }
                    if slot.got_beacon {
                        match net.fhss_send(GW_ADDR, &seq.to_le_bytes(), true).await {
                            SendResult::Delivered => {
                                info!(target: "fhss", "#{} delivered on channel {}", seq, net.fhss_current_channel())
                            }
                            SendResult::NotDelivered => {
                                warn!(target: "fhss", "#{} not delivered on channel {}, no ACK", seq, net.fhss_current_channel())
                            }
                            _ => {}
                        }
                        seq = seq.wrapping_add(1);
                    }
                }
            }
        }
    }
}

app!(run);
