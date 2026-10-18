//! fhss_kat (F3/F4/F5): known-answer tests for the FHSS building blocks (1 board,
//! no RF link). Proves the hop permutation, the per-channel dwell bound, and the
//! beacon frame round-trip before the on-air link is built.
//!
//!   just flash example fhss_kat
//!
//! F5: hop_channel(seed,cycle,·) over i=0..N is a permutation of 0..N every cycle
//!     (each channel exactly once ⇒ equal use), for several cycles + seeds.
//! F4: the per-channel dwell governor caps transmitted airtime at ≤300 ms in any
//!     20 s window (100 ms burst + 1 % refill), 25 % under the 0.4 s §15.247 limit.
//! F3: a Beacon frame seals + opens, recovering its (cycle, slot) payload.

#![no_std]
#![no_main]

use log::{error, info};
use tower::radio::ccm::Ccm;
use tower::radio::config::FHSS_N;
use tower::radio::duty::DutyGovernor;
use tower::radio::frame::{self, FrameType, Header};
use tower::radio::net::hop_channel;
use tower::{app, board::Board};

const KEY: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
];

async fn run(_b: Board) {
    let mut pass = true;

    // --- F5: hop_channel is a full permutation each cycle (equal use) ---
    let seeds = [0x1234_5678u32, 0xC0FF_EE01, 0xA5A5_0001];
    let mut perm_ok = true;
    for &seed in &seeds {
        for cycle in 0..5u32 {
            let mut seen = [false; FHSS_N as usize];
            for i in 0..FHSS_N {
                let ch = hop_channel(seed, cycle, i);
                if ch >= FHSS_N || seen[ch as usize] {
                    perm_ok = false; // out of range or repeated → not a permutation
                } else {
                    seen[ch as usize] = true;
                }
            }
            if seen.iter().any(|&s| !s) {
                perm_ok = false; // a channel was never visited
            }
        }
    }
    if perm_ok {
        info!(target: "fhss", "F5 hop permutation PASS ({} channels, 3 seeds x 5 cycles)", FHSS_N);
    } else {
        error!(target: "fhss", "F5 hop permutation FAIL ({} channels, 3 seeds x 5 cycles)", FHSS_N);
    }
    pass &= perm_ok;

    // --- F4: per-channel dwell governor worst-case ≤ 300 ms / 20 s ---
    // Greedy incremental drain over 20 s: initial 100 ms burst + 1 %·20 s = 300 ms.
    let mut g = DutyGovernor::fhss_channel();
    let mut total = 0u32;
    while g.try_consume(10) {
        total += 10; // drain the initial burst (100 ms)
    }
    for _ in 0..20 {
        g.refill_ms(1000); // +10 ms / s (1 %)
        while g.try_consume(10) {
            total += 10;
        }
    }
    if total == 300 {
        info!(target: "fhss", "F4 dwell worst case PASS ({} ms in 20 s, expected 300, limit 400)", total);
    } else {
        error!(target: "fhss", "F4 dwell worst case FAIL ({} ms in 20 s, expected 300, limit 400)", total);
    }
    pass &= total == 300 && total <= 400;
    // Independence: a fresh channel bucket is unaffected.
    let mut g2 = DutyGovernor::fhss_channel();
    pass &= g2.try_consume(100) && !g2.try_consume(1);

    // --- F3: Beacon frame round-trip ---
    let mut ccm = Ccm::new();
    let hdr = Header {
        frame_type: FrameType::Beacon,
        flags: 0,
        src: 0x2222_2222,
        dest: 0xFFFF_FFFF,
        counter: 7,
        bulk_index: None,
    };
    let payload = [0x39, 0x05, 0x00, 0x00, 0x2A]; // cycle=0x0539, slot=0x2A
    let mut buf = [0u8; 96];
    let beacon_ok = match frame::seal_frame(&mut ccm, &KEY, &hdr, &payload, &mut buf) {
        Ok(n) => match frame::open_frame(&mut ccm, &KEY, &mut buf[..n]) {
            Ok((rh, range)) => rh.frame_type == FrameType::Beacon && buf[range] == payload[..],
            Err(_) => false,
        },
        Err(_) => false,
    };
    if beacon_ok {
        info!(target: "fhss", "F3 beacon frame round-trip PASS");
    } else {
        error!(target: "fhss", "F3 beacon frame round-trip FAIL");
    }
    pass &= beacon_ok;

    // Let the verdicts above leave the wire first: the console queue holds 4 frames and a log
    // line that finds it full is dropped, and the summary is the line the bench waits for.
    tower::console::flush().await;
    if pass {
        info!(target: "fhss", "hop permutation, dwell and beacon, all checks PASS");
    } else {
        error!(target: "fhss", "hop permutation, dwell and beacon, some checks FAIL");
    }
    loop {
        embassy_time::Timer::after_secs(5).await;
    }
}

app!(run);
