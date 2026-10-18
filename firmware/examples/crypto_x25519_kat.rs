//! crypto_x25519_kat: validate the pairing-v3 crypto on the Core Module against RFC 7748 and the
//! host-tested handshake golden, and measure what pairing costs on this MCU. Single board, no
//! radio.
//!
//! Runs the two RFC 7748 §5.2 test vectors, the first step of its iterated test, and the §6.1
//! Diffie-Hellman test (both public keys and the shared secret from both sides), through the
//! same `tower_net::handshake` kernel the pairing uses, logging a MATCH / MISMATCH (a value
//! against its reference) or PASS / FAIL (a property) verdict per check.
//! Then the pairing-v3 handshake between those two keys (node = Alice, gateway = Bob, fixed
//! nonces): the gateway's commitment, the transcript's link key, SAS and both confirmation tags
//! must equal the golden values `crates/net` pins against an independent reference (so the
//! on-target SHA-256 / HMAC / HKDF agree with the host's), the revealed nonce must open the
//! commitment and a changed one must not, and both sides must agree. It times one scalar
//! multiplication, the commitment and the key schedule (SHA-256 + HKDF-SHA256 + two HMACs) with
//! `embassy_time::Instant` (the RTC-backed 32.768 kHz tick, ~31 µs resolution), compares the
//! handshake's critical path (shared secret + schedule, the work inside the JOIN_REVEAL and
//! JOIN_CONFIRM waits) with the `PEER_KEX_TIME` budget, and reports the stack high-water mark
//! (`tower::stack`, painted at boot).
//!
//! The verdict line `x25519: all checks PASS (...)` (or an `error!` ending `some checks FAIL`) is
//! re-emitted every 5 s with the measurements, so a `tower logs` attached late still sees it.
//!
//!   just flash example crypto_x25519_kat    (watch with: tower logs)

#![no_std]
#![no_main]

use embassy_time::{Duration, Instant, Timer};
use log::{error, info};
use tower::radio::net::PEER_KEX_TIME;
use tower::{app, board::Board, stack};
use tower_net::handshake::{
    Ephemeral, NONCE_LEN, Role, Transcript, commitment, commitment_matches, key_schedule, shared_secret,
};

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

// The pairing-v3 golden of `crates/net` (`handshake::tests::key_schedule_golden`, pinned against
// an independent Python reference): node = Alice, gateway = Bob, these addresses and nonces.
const NODE: u32 = 0x0000_00BB;
const GW: u32 = 0x2222_2222;
const N_NODE: [u8; NONCE_LEN] = [
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
const N_GW: [u8; NONCE_LEN] = [
    0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
];
const GOLDEN_COMMIT: [u8; 16] = [
    0x05, 0x18, 0x72, 0xd7, 0x8b, 0xd0, 0xec, 0xda, 0x37, 0xbc, 0xdd, 0x5f, 0xa8, 0xc9, 0xd5, 0xef,
];
const GOLDEN_LINK_KEY: [u8; 16] = [
    0xb3, 0x40, 0xd4, 0xd6, 0x1f, 0x5f, 0x5b, 0x5a, 0xe1, 0x4a, 0x0d, 0x88, 0x19, 0x81, 0x28, 0x09,
];
const GOLDEN_SAS: u32 = 211_658;
const GOLDEN_TAG_GW: [u8; 8] = [0xa2, 0xd3, 0x49, 0xdb, 0x39, 0x94, 0xf7, 0x14];
const GOLDEN_TAG_NODE: [u8; 8] = [0x13, 0xd3, 0x91, 0xcc, 0x78, 0x72, 0x4b, 0x93];

/// The verdict words of a check: a value against its reference, or a property.
const VALUE: [&str; 2] = ["MATCH", "MISMATCH"];
const PROPERTY: [&str; 2] = ["PASS", "FAIL"];

/// Log one check (its verdict the last word, as the bench reads it) and fold it into the verdict.
fn check(pass: &mut bool, what: &str, ok: bool, words: [&str; 2]) {
    if ok {
        info!(target: "x25519", "{} {}", what, words[0]);
    } else {
        error!(target: "x25519", "{} {}", what, words[1]);
    }
    *pass &= ok;
}

async fn run(_b: Board) {
    let mut pass = true;
    info!(target: "x25519", "running RFC 7748 vectors (each scalar multiplication takes ~1 s)");
    // Let the boot backlog drain first: the computations below block the executor.
    Timer::after_millis(200).await;

    for (i, (k, u, out)) in V52.iter().enumerate() {
        let what = if i == 0 {
            "RFC 7748 5.2 vector 1"
        } else {
            "RFC 7748 5.2 vector 2"
        };
        check(&mut pass, what, shared_secret(k, u) == Some(*out), VALUE);
        Timer::after_millis(10).await; // let the console flush between the blocking computations
    }
    check(
        &mut pass,
        "RFC 7748 5.2 iterated, 1 iteration",
        shared_secret(&BASEPOINT, &BASEPOINT) == Some(ITER_1),
        VALUE,
    );
    Timer::after_millis(10).await;

    // §6.1, with the scalar multiplication timed on Alice's public key.
    let t0 = Instant::now();
    let alice = Ephemeral::from_parts(&ALICE_SK, &N_NODE);
    let t_mult = t0.elapsed();
    check(
        &mut pass,
        "RFC 7748 6.1 Alice public key",
        alice.public() == &ALICE_PK,
        VALUE,
    );
    Timer::after_millis(10).await;
    let bob = Ephemeral::from_parts(&BOB_SK, &N_GW);
    check(
        &mut pass,
        "RFC 7748 6.1 Bob public key",
        bob.public() == &BOB_PK,
        VALUE,
    );
    Timer::after_millis(10).await;
    check(
        &mut pass,
        "RFC 7748 6.1 shared secret, Alice side",
        shared_secret(&ALICE_SK, bob.public()) == Some(SHARED),
        VALUE,
    );
    Timer::after_millis(10).await;
    check(
        &mut pass,
        "RFC 7748 6.1 shared secret, Bob side",
        shared_secret(&BOB_SK, alice.public()) == Some(SHARED),
        VALUE,
    );
    Timer::after_millis(10).await;

    // The pairing-v3 handshake. JOIN_RESP: the gateway's commitment (timed: it is all the
    // gateway computes before answering), which the revealed nonce must open and a changed one
    // must not.
    let t0 = Instant::now();
    let commit = bob.commitment(alice.public(), NODE, GW);
    let t_commit = t0.elapsed();
    check(
        &mut pass,
        "handshake: commitment vs the golden",
        commit == GOLDEN_COMMIT,
        VALUE,
    );
    let mut changed = N_GW;
    changed[0] ^= 1;
    check(
        &mut pass,
        "handshake: only the revealed nonce opens the commitment",
        commitment_matches(&commit, &N_GW, &ALICE_PK, &BOB_PK, NODE, GW)
            && !commitment_matches(&commit, &changed, &ALICE_PK, &BOB_PK, NODE, GW)
            && commitment(&changed, &ALICE_PK, &BOB_PK, NODE, GW) != commit,
        PROPERTY,
    );
    Timer::after_millis(10).await;

    // The key schedule alone (golden), then the full critical path (shared secret + schedule +
    // tags + SAS) as each side runs it, with both sides agreeing.
    let transcript = Transcript {
        node_addr: NODE,
        gw_addr: GW,
        e_node: &ALICE_PK,
        e_gw: &BOB_PK,
        n_node: &N_NODE,
        n_gw: &N_GW,
    };
    let t0 = Instant::now();
    let schedule = key_schedule(&SHARED, &transcript);
    let t_schedule = t0.elapsed();
    check(
        &mut pass,
        "handshake: link key, SAS and both tags vs the golden",
        schedule.link_key() == &GOLDEN_LINK_KEY
            && schedule.sas() == GOLDEN_SAS
            && schedule.tag(Role::Gateway) == &GOLDEN_TAG_GW
            && schedule.tag(Role::Node) == &GOLDEN_TAG_NODE,
        VALUE,
    );
    Timer::after_millis(10).await;
    let t0 = Instant::now();
    let at_gw = bob.session(alice.public(), alice.nonce(), Role::Gateway, NODE, GW);
    let t_handshake = t0.elapsed();
    Timer::after_millis(10).await;
    let at_node = alice.session(bob.public(), bob.nonce(), Role::Node, NODE, GW);
    let agree = match (&at_gw, &at_node) {
        (Some(g), Some(n)) => {
            g.link_key() == schedule.link_key()
                && n.link_key() == schedule.link_key()
                && g.sas() == n.sas()
                && n.verify(Role::Gateway, g.tag(Role::Gateway))
                && g.verify(Role::Node, n.tag(Role::Node))
        }
        _ => false,
    };
    check(
        &mut pass,
        "handshake: both sides agree on key and SAS, accept each other's tag",
        agree,
        PROPERTY,
    );

    let ms = |d: Duration| d.as_micros() / 1000;
    let budget_ok = t_handshake < PEER_KEX_TIME;
    info!(
        target: "x25519",
        "timing: scalar mult {} ms, commitment {} ms, key schedule {} ms, handshake {} ms ({} the {} ms budget{})",
        ms(t_mult),
        ms(t_commit),
        ms(t_schedule),
        ms(t_handshake),
        if budget_ok { "within" } else { "over" },
        ms(PEER_KEX_TIME),
        if budget_ok { "" } else { ": raise PEER_KEX_TIME" }
    );

    // Let the verdicts above leave the wire first: the console queue holds 4 frames and a log
    // line that finds it full is dropped, and the summary is the line the bench waits for.
    tower::console::flush().await;
    loop {
        let (used, total) = (stack::used(), stack::total());
        if pass {
            info!(
                target: "x25519",
                "all checks PASS (scalar mult {} ms, handshake {} ms, stack peak {} of {} B)",
                ms(t_mult),
                ms(t_handshake),
                used,
                total
            );
        } else {
            error!(
                target: "x25519",
                "some checks FAIL (scalar mult {} ms, stack peak {} of {} B)",
                ms(t_mult),
                used,
                total
            );
        }
        Timer::after_secs(5).await;
    }
}

app!(run);
