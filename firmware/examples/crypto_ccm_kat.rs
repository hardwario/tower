//! crypto_ccm_kat: validate AES-128-CCM against RFC 3610 Packet Vector #1,
//! plus a tamper test. Single board, no radio.
//!
//! Encrypts the RFC 3610 #1 vector (N=13, M=8) and checks the ciphertext + tag,
//! then decrypts back, then flips one ciphertext byte and confirms `open()`
//! rejects it (tag mismatch). Proves the CCM construction before the network
//! layer relies on it.
//!
//!   just flash example crypto_ccm_kat    (watch with: tower logs)

#![no_std]
#![no_main]

use log::{error, info};
use tower::radio::ccm::Ccm;
use tower::{app, board::Board};

// RFC 3610 Packet Vector #1 (M=8, L=2):
const KEY: [u8; 16] = [
    0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf,
];
const NONCE: [u8; 13] = [
    0x00, 0x00, 0x00, 0x03, 0x02, 0x01, 0x00, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5,
];
const AAD: [u8; 8] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
const PLAIN: [u8; 23] = [
    0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
    0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
];
const EXPECT_CT: [u8; 23] = [
    0x58, 0x8c, 0x97, 0x9a, 0x61, 0xc6, 0x63, 0xd2, 0xf0, 0x66, 0xd0, 0xc2, 0xc0, 0xf9, 0x89, 0x80, 0x6d,
    0x5f, 0x6b, 0x61, 0xda, 0xc3, 0x84,
];
const EXPECT_TAG: [u8; 8] = [0x17, 0xe8, 0xd1, 0x2c, 0xfd, 0xf9, 0x26, 0xe0];

async fn run(_b: Board) {
    let mut ccm = Ccm::new();
    let mut pass = true;

    // 1) Seal and check ciphertext + tag against the vector.
    let mut buf = PLAIN;
    let tag = ccm.seal(&KEY, &NONCE, &AAD, &mut buf);
    let ct_ok = buf == EXPECT_CT;
    let tag_ok = tag == EXPECT_TAG;
    if ct_ok {
        info!(target: "ccm", "seal ciphertext MATCH");
    } else {
        error!(target: "ccm", "seal ciphertext MISMATCH");
    }
    if tag_ok {
        info!(target: "ccm", "seal tag MATCH");
    } else {
        error!(target: "ccm", "seal tag MISMATCH");
    }
    info!(target: "ccm", "tag {} (expected {})", Hex(&tag), Hex(&EXPECT_TAG));
    pass &= ct_ok && tag_ok;

    // 2) Open the genuine ciphertext+tag → must succeed and recover plaintext.
    if ccm.open(&KEY, &NONCE, &AAD, &mut buf, &tag) {
        if buf == PLAIN {
            info!(target: "ccm", "open of the genuine ciphertext PASS (plaintext recovered)");
        } else {
            error!(target: "ccm", "open of the genuine ciphertext FAIL (plaintext differs)");
            pass = false;
        }
    } else {
        error!(target: "ccm", "open of the genuine ciphertext FAIL (authentication failed)");
        pass = false;
    }

    // A log line that finds the console queue full is dropped (it holds some 20 lines), so let
    // the four lines above leave the wire before the next ones (the summary is what the bench
    // reads).
    tower::console::flush().await;

    // 3) Tamper: flip one ciphertext byte → open() must reject.
    let mut tampered = EXPECT_CT;
    tampered[0] ^= 0x01;
    if ccm.open(&KEY, &NONCE, &AAD, &mut tampered, &EXPECT_TAG) {
        error!(target: "ccm", "refusal of a tampered ciphertext FAIL (accepted)");
        pass = false;
    } else {
        info!(target: "ccm", "refusal of a tampered ciphertext PASS");
    }

    if pass {
        info!(target: "ccm", "RFC 3610 vector #1 and tamper, all checks PASS");
    } else {
        error!(target: "ccm", "RFC 3610 vector #1 and tamper, some checks FAIL");
    }

    loop {
        embassy_time::Timer::after_secs(5).await;
    }
}

struct Hex<'a>(&'a [u8]);
impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

app!(run);
