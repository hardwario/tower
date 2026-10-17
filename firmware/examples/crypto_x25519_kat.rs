//! crypto_x25519_kat: validate the pairing-v2 X25519 on the Core Module against RFC 7748, and
//! measure what pairing costs on this MCU. Single board, no radio.
//!
//! Runs the two RFC 7748 §5.2 test vectors, the first step of its iterated test, and the §6.1
//! Diffie-Hellman test (both public keys and the shared secret from both sides), through the
//! same `tower_net::handshake` kernel the pairing uses, logging MATCH / MISMATCH per check.
//! Then it times one scalar multiplication and the key schedule (HKDF-SHA256 + two HMACs) with
//! `embassy_time::Instant` (the RTC-backed 32.768 kHz tick, ~31 µs resolution), compares the
//! handshake's critical path with the `PEER_KEX_TIME` budget the pairing waits allow for it, and
//! reports the stack high-water mark (`tower::stack`, painted at boot).
//!
//! The verdict line `x25519_kat: ALL PASS ***` (or an `error!` with FAILURES) is re-emitted every
//! 5 s with the measurements, so a `tower logs` attached late still sees it.
//!
//!   just flash example crypto_x25519_kat    (watch with: tower logs)

#![no_std]
#![no_main]

use embassy_time::{Duration, Instant, Timer};
use log::{error, info};
use tower::radio::net::PEER_KEX_TIME;
use tower::{app, board::Board, stack};
use tower_net::handshake::{Ephemeral, Role, key_schedule, shared_secret};

/// Parse a 64-hex-digit test vector (compile-time constants, so a typo fails the KAT loudly
/// rather than panicking: an invalid digit reads as 0 and the vector mismatches).
const fn h32(s: &str) -> [u8; 32] {
    const fn nib(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => 0,
        }
    }
    let b = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]);
        i += 1;
    }
    out
}

const BASEPOINT: [u8; 32] = h32("0900000000000000000000000000000000000000000000000000000000000000");

// RFC 7748 §5.2 (scalar, input u-coordinate, output u-coordinate).
const V52: [([u8; 32], [u8; 32], [u8; 32]); 2] = [
    (
        h32("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4"),
        h32("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c"),
        h32("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552"),
    ),
    (
        h32("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d"),
        h32("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493"),
        h32("95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957"),
    ),
];
// RFC 7748 §5.2 iterated test after one iteration (k = u = 9).
const ITER_1: [u8; 32] = h32("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079");
// RFC 7748 §6.1.
const ALICE_SK: [u8; 32] = h32("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
const ALICE_PK: [u8; 32] = h32("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
const BOB_SK: [u8; 32] = h32("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
const BOB_PK: [u8; 32] = h32("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
const SHARED: [u8; 32] = h32("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");

/// Log one check and fold it into the verdict.
fn check(pass: &mut bool, what: &str, ok: bool) {
    if ok {
        info!(target: "x25519_kat", "{}: MATCH", what);
    } else {
        error!(target: "x25519_kat", "{}: MISMATCH", what);
    }
    *pass &= ok;
}

async fn run(_b: Board) {
    let mut pass = true;
    info!(target: "x25519_kat", "running RFC 7748 vectors (each scalar multiplication takes ~1 s)");
    // Let the boot backlog drain first: the computations below block the executor.
    Timer::after_millis(200).await;

    for (i, (k, u, out)) in V52.iter().enumerate() {
        let what = if i == 0 {
            "RFC 7748 5.2 vector 1"
        } else {
            "RFC 7748 5.2 vector 2"
        };
        check(&mut pass, what, shared_secret(k, u) == Some(*out));
        Timer::after_millis(10).await; // let the console flush between the blocking computations
    }
    check(
        &mut pass,
        "RFC 7748 5.2 iterated, 1 iteration",
        shared_secret(&BASEPOINT, &BASEPOINT) == Some(ITER_1),
    );
    Timer::after_millis(10).await;

    // §6.1, with the scalar multiplication timed on Alice's public key.
    let t0 = Instant::now();
    let alice = Ephemeral::from_secret(&ALICE_SK);
    let t_mult = t0.elapsed();
    check(
        &mut pass,
        "RFC 7748 6.1 Alice public key",
        alice.public() == &ALICE_PK,
    );
    Timer::after_millis(10).await;
    let bob = Ephemeral::from_secret(&BOB_SK);
    check(&mut pass, "RFC 7748 6.1 Bob public key", bob.public() == &BOB_PK);
    Timer::after_millis(10).await;
    check(
        &mut pass,
        "RFC 7748 6.1 shared secret (Alice side)",
        shared_secret(&ALICE_SK, bob.public()) == Some(SHARED),
    );
    Timer::after_millis(10).await;
    check(
        &mut pass,
        "RFC 7748 6.1 shared secret (Bob side)",
        shared_secret(&BOB_SK, alice.public()) == Some(SHARED),
    );
    Timer::after_millis(10).await;

    // The key schedule alone, then the full critical path (shared secret + schedule + tags)
    // as the pairing runs it, with both sides agreeing.
    let t0 = Instant::now();
    let schedule = key_schedule(&SHARED, &ALICE_PK, &BOB_PK, 0x0000_00BB, 0x2222_2222);
    let t_schedule = t0.elapsed();
    let t0 = Instant::now();
    let at_gw = bob.session(alice.public(), Role::Gateway, 0x0000_00BB, 0x2222_2222);
    let t_handshake = t0.elapsed();
    let agree = at_gw
        .as_ref()
        .is_some_and(|s| s.link_key() == schedule.link_key() && s.sas() == schedule.sas());
    check(
        &mut pass,
        "handshake: both sides derive the same key and SAS",
        agree,
    );

    let ms = |d: Duration| d.as_micros() / 1000;
    let budget_ok = t_handshake < PEER_KEX_TIME;
    info!(
        target: "x25519_kat",
        "timing: scalar multiplication {} ms, key schedule {} ms, handshake critical path {} ms (PEER_KEX_TIME budget {} ms: {})",
        ms(t_mult),
        ms(t_schedule),
        ms(t_handshake),
        ms(PEER_KEX_TIME),
        if budget_ok { "OK" } else { "EXCEEDED, raise it" }
    );

    loop {
        let (used, total) = (stack::used(), stack::total());
        if pass {
            info!(
                target: "x25519_kat",
                "x25519_kat: ALL PASS *** (scalar mult {} ms, handshake {} ms, stack high-water {} B of {} B)",
                ms(t_mult),
                ms(t_handshake),
                used,
                total
            );
        } else {
            error!(
                target: "x25519_kat",
                "x25519_kat: FAILURES above (scalar mult {} ms, stack high-water {} B of {} B)",
                ms(t_mult),
                used,
                total
            );
        }
        Timer::after_secs(5).await;
    }
}

app!(run);
