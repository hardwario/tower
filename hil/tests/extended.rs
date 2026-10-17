//! HIL extended group: the full example-matrix sweep the two-board bench can exercise
//! (firmware/docs/test-plan.md scope, beyond the committed smoke/radio groups).
//!
//! Single-board KATs (crypto, FHSS, duty governor, frame-codec edges) run on the Dongle slot;
//! two-board RF scenarios (P2P, secure ping, OTA pairing, bulk pull, rapid-fire ordering) use
//! the `core` slot as the second radio peer: the bench's TOWER Core Module, alongside the TOWER
//! Radio Dongle. Both are flashed over the UART bootloader via their FTDI (`tower flash`), so the
//! J-Link/PPK2 wiring the power group needs is not required here.
//!
//! `#[ignore]`d like the other groups: `cargo test` only compiles them; run on the bench with
//! `just hil ext` (`--test-threads=1`; the serial ports are exclusive).

use std::time::{Duration, Instant};

use tower_hil::{Bench, Console, Frame, Role, bench_or_fail, build_image, frame_text};

/// Build an example (`features` empty = none; `build_image` always sets TOWER_FEATURES explicitly)
/// and flash it to the board in `role`, identity re-checked right before the erase. Sequential by
/// design: concurrent FTDI flashing is unreliable (see firmware/tools/hwtest/README.md).
fn build_and_flash(bench: &Bench, role: Role, example: &str, features: &str) -> Result<(), String> {
    let bin = build_image("example", example, features)?;
    bench.flash(role, &bin)
}

/// Flash a one-shot KAT to the Dongle, reset, and await its verdict: pass only if a frame contains
/// `pass` AND none of `fails` (so a pass token that is the tail of a fail token, like `MATCH ***` in
/// `MISMATCH ***`, can never pass), fail fast on any frame containing one of `fails`. Tracks the
/// device's `Dropped` markers so a verdict lost to the console writer's drop-newest queue
/// (TX_DEPTH) is diagnosed as such, not as "did it boot?". Also asserts gap-free transport (seq gaps ≠ writer drops:
/// the writer assigns seq at encode, so its drops surface ONLY via the Dropped marker).
fn run_kat(example: &str, pass: &str, fails: &[&str], secs: u64) {
    let bench = bench_or_fail();
    build_and_flash(&bench, Role::Dongle, example, "").expect("build+flash KAT");

    let mut console = Console::open(&bench.dongle.serial).expect("open Dongle console");
    console.reset_into_app().expect("reset the Dongle into the app");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut dropped: u32 = 0;
    let mut verdict: Option<String> = None;
    while Instant::now() < deadline {
        match console.next(Duration::from_millis(500)).expect("console read") {
            Some(Frame::Dropped { count }) => dropped += count,
            Some(f) => {
                if let Some(t) = frame_text(&f)
                    && (t.contains(pass) || fails.iter().any(|n| t.contains(n)))
                {
                    verdict = Some(t.to_string());
                    break;
                }
            }
            None => {}
        }
    }

    let text = verdict.unwrap_or_else(|| {
        if dropped > 0 {
            panic!(
                "{example}: no verdict within {secs} s, and the device reported {dropped} \
                 dropped console frame(s); the verdict line likely fell to the writer's \
                 drop-newest queue (src/console.rs TX_DEPTH); re-emit the verdict periodically"
            )
        }
        panic!("{example}: no verdict within {secs} s; did it boot?")
    });
    assert!(
        text.contains(pass) && !fails.iter().any(|n| text.contains(n)),
        "{example} reported a failure: {text:?}"
    );
    assert_eq!(
        console.seq_gaps(),
        0,
        "{example}: transport dropped frames during the KAT"
    );
}

/// Parse the decimal number right after `key` in `text` (e.g. `tx_counter=` -> 1025).
fn field_u64(text: &str, key: &str) -> Option<u64> {
    let rest = &text[text.find(key)? + key.len()..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Parse the 8-hex-digit id right after `key` (e.g. `id=` -> 0x1234ABCD).
fn field_hex32(text: &str, key: &str) -> Option<u32> {
    let rest = &text[text.find(key)? + key.len()..];
    u32::from_str_radix(rest.get(..8)?, 16).ok()
}

/// The 6-digit SAS from a `pairing: SAS dddddd` line.
fn field_sas(text: &str) -> Option<u32> {
    let rest = &text[text.find("pairing: SAS ")? + "pairing: SAS ".len()..];
    rest.get(..6)?.parse().ok()
}

// ---------------------------------------------------------------------------
// Single-board KATs (Dongle slot)
// ---------------------------------------------------------------------------

/// L0 hardware AES vs the FIPS-197 App. B vector (key load, byte order, block compute).
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn ext_crypto_aes_kat() {
    // The full verdict prefix, not a bare "MATCH ***" (the tail of a "MISMATCH ***").
    run_kat("crypto_aes_kat", "vector: MATCH ***", &["MISMATCH"], 8);
}

/// Frame codec + nonce + CCM loopback, incl. tamper + wrong-key rejection.
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn ext_crypto_frame_loopback() {
    // 12 s: the example re-emits its verdict every 5 s (the boot-burst copy can be a casualty
    // of the TX queue), so the window must cover at least one re-emission cycle.
    run_kat(
        "crypto_frame_loopback",
        "ALL PASS ***",
        &["MISMATCH", "expected AuthFail"],
        12,
    );
}

/// FHSS hop permutation + beacon frame round-trip KAT.
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn ext_fhss_kat() {
    run_kat("fhss_kat", "ALL PASS ***", &["FAIL"], 10);
}

/// The pairing-v3 crypto on the MCU: X25519 vs RFC 7748 §5.2 / §6.1, then the handshake golden
/// (commitment, link key, SAS, tags) the host tests pin; ~12 s of compute before the verdict,
/// which repeats every 5 s with the timing and the stack high-water.
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn ext_crypto_x25519_kat() {
    run_kat(
        "crypto_x25519_kat",
        "x25519_kat: ALL PASS ***",
        &["MISMATCH", "FAILURES"],
        40,
    );
}

/// Duty-cycle governor KAT (the regulatory token-bucket arithmetic, on target).
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn ext_net_duty_kat() {
    run_kat("net_duty_kat", "ALL PASS ***", &["FAIL"], 10);
}

/// Frame-size limits / boundary conditions of the codec.
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn ext_edge_frame_limits() {
    run_kat("edge_frame_limits", "ALL PASS ***", &["FAIL"], 10);
}

/// Radio state machine recovery: must never wedge.
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn ext_edge_recovery() {
    run_kat("edge_recovery", "ALL PASS ***", &["FAIL"], 25);
}

// ---------------------------------------------------------------------------
// Two-board RF scenarios (core slot = second Dongle as radio peer)
// ---------------------------------------------------------------------------

/// Bidirectional confirmed P2P: A pings (Delivered), B receives + pongs (Delivered),
/// A receives the pong: both directions of the half-duplex link with per-link keys.
#[test]
#[ignore = "requires the HIL bench (two TOWER boards in RF range)"]
fn ext_net_p2p_bidirectional() {
    let bench = bench_or_fail();
    build_and_flash(&bench, Role::Dongle, "net_p2p", "role-peer-b").expect("flash peer B");
    build_and_flash(&bench, Role::Core, "net_p2p", "role-peer-a").expect("flash peer A");

    let mut a = Console::open(&bench.core.serial).expect("open A console");
    let mut b = Console::open(&bench.dongle.serial).expect("open B console");
    b.reset_into_app().expect("reset B");
    a.reset_into_app().expect("reset A");

    // A: confirmed PING delivered, and B's PONG received back.
    let a_ping = a
        .wait_for(Duration::from_secs(30), |f| {
            frame_text(f).is_some_and(|t| t.contains("PING") && t.contains("Delivered"))
        })
        .expect("A console read");
    assert!(a_ping.is_some(), "A never reported a Delivered PING within 30 s");
    assert!(
        a.wait_for_text(Duration::from_secs(15), "A: rx")
            .expect("A console read"),
        "A never received B's PONG"
    );

    // B: received A's PING (auto-ACKed) and its own confirmed PONG delivered.
    assert!(
        b.wait_for_text(Duration::from_secs(15), "B: rx")
            .expect("B console read"),
        "B never logged A's PING"
    );
    let b_pong = b
        .wait_for(Duration::from_secs(15), |f| {
            frame_text(f).is_some_and(|t| t.contains("PONG") && t.contains("Delivered"))
        })
        .expect("B console read");
    assert!(b_pong.is_some(), "B never reported a Delivered PONG");
}

/// Full stack over the air: CCM-sealed DATA sent by the node, authenticated + decrypted by
/// the gateway (AUTH OK), with no auth failures in the capture window.
#[test]
#[ignore = "requires the HIL bench (two TOWER boards in RF range)"]
fn ext_net_secure_ping_auth() {
    let bench = bench_or_fail();
    build_and_flash(&bench, Role::Dongle, "net_secure_ping", "").expect("flash gateway");
    build_and_flash(&bench, Role::Core, "net_secure_ping", "role-node").expect("flash node");

    let mut node = Console::open(&bench.core.serial).expect("open node console");
    let mut gw = Console::open(&bench.dongle.serial).expect("open gateway console");
    gw.reset_into_app().expect("reset gateway");
    node.reset_into_app().expect("reset node");

    assert!(
        node.wait_for_text(Duration::from_secs(20), "tx cnt=")
            .expect("node console read"),
        "node never sent a sealed frame"
    );
    let frames = gw
        .collect_for(Duration::from_secs(15))
        .expect("gateway console read");
    let auth_ok = frames
        .iter()
        .filter_map(frame_text)
        .filter(|t| t.contains("AUTH OK"))
        .count();
    let auth_fail = frames
        .iter()
        .filter_map(frame_text)
        .filter(|t| t.contains("auth FAIL"))
        .count();
    assert!(
        auth_ok > 0,
        "gateway never authenticated a frame (frames: {frames:?})"
    );
    assert_eq!(auth_fail, 0, "gateway saw CCM auth failures");
}

/// OTA pairing (v3, X25519 key agreement with a commitment round: JOIN_REQ, JOIN_RESP,
/// JOIN_NONCE, JOIN_REVEAL, JOIN_CONFIRM): host opens a window, joiner joins with its own
/// address; both sides must log the SAME node address, the same derived key prefix and the same
/// SAS (`net: pairing: SAS dddddd`, logged just before each side's PAIRED / JOINED line), and the
/// joiner must never have seen a reveal that breaks the host's commitment or fails its tag.
#[test]
#[ignore = "requires the HIL bench (two TOWER boards in RF range)"]
fn ext_net_pairing_handshake() {
    let bench = bench_or_fail();
    build_and_flash(&bench, Role::Dongle, "net_pairing", "").expect("flash host (gateway role)");
    build_and_flash(&bench, Role::Core, "net_pairing", "role-node").expect("flash joiner");

    let mut host = Console::open(&bench.dongle.serial).expect("open host console");
    let mut joiner = Console::open(&bench.core.serial).expect("open joiner console");
    // Host first: it opens the pairing window the joiner's JOIN_REQ must land in.
    host.reset_into_app().expect("reset host");
    joiner.reset_into_app().expect("reset joiner");

    // The joiner logs its SAS, then JOINED (or `join failed` when the window passed unjoined).
    // A reveal that does not open the commitment, or fails key confirmation, is what a man in the
    // middle (or a broken handshake) looks like on a bench with only the two boards: fail on it.
    let sas = joiner
        .wait_for(Duration::from_secs(70), |f| {
            frame_text(f).is_some_and(|t| {
                t.contains("pairing: SAS")
                    || t.contains("join failed")
                    || t.contains("does not open its commitment")
                    || t.contains("failed key confirmation")
            })
        })
        .expect("joiner console read")
        .expect("joiner never concluded the handshake within 70 s");
    let st = frame_text(&sas).unwrap_or("");
    assert!(
        !st.contains("commitment") && !st.contains("key confirmation"),
        "the joiner refused the host's JOIN_REVEAL: {st:?}"
    );
    let joiner_sas = field_sas(st).expect("join failed on the joiner");
    let joined = joiner
        .wait_for(Duration::from_secs(10), |f| {
            frame_text(f).is_some_and(|t| t.contains("JOINED ***"))
        })
        .expect("joiner console read")
        .expect("joiner logged a SAS but no JOINED");
    let jt = frame_text(&joined).unwrap_or("").to_string();

    let sas = host
        .wait_for(Duration::from_secs(15), |f| {
            frame_text(f).is_some_and(|t| t.contains("pairing: SAS"))
        })
        .expect("host console read")
        .expect("host never logged a SAS");
    let host_sas = field_sas(frame_text(&sas).unwrap_or("")).expect("host SAS line malformed");
    let paired = host
        .wait_for(Duration::from_secs(10), |f| {
            frame_text(f).is_some_and(|t| t.contains("PAIRED ***"))
        })
        .expect("host console read")
        .expect("host never logged PAIRED");
    let ht = frame_text(&paired).unwrap_or("").to_string();

    // No key travels on air: both ends DERIVE it, so the prefixes must agree (and differ per
    // run), and the SAS values must match (a mismatch is what a man in the middle looks like).
    let host_key = field_hex32(&ht, "key[..4]=").expect("host PAIRED line has no key prefix");
    let joiner_key = field_hex32(&jt, "key[..4]=").expect("joiner JOINED line has no key prefix");
    assert_eq!(
        host_key, joiner_key,
        "the two ends derived different keys: {ht:?} / {jt:?}"
    );
    assert_eq!(host_sas, joiner_sas, "SAS mismatch between the two ends");

    // The joiner chooses its address; the host must have learned exactly that address. (The
    // firmware logs `addr=` since the 2026-07-12 radio-address rename; matching the old `id=`
    // key would panic here on the bench.)
    let host_addr = field_hex32(&ht, "addr=").expect("host PAIRED line has no addr=");
    let joiner_addr = field_hex32(&jt, "addr=").expect("joiner JOINED line has no addr=");
    assert_eq!(
        host_addr, joiner_addr,
        "host installed a different node address than the joiner chose"
    );
}

/// Bulk pull: requester fetches the 200-byte blob in ≤64 B chunks and verifies the pattern.
#[test]
#[ignore = "requires the HIL bench (two TOWER boards in RF range)"]
fn ext_net_bulk_pull() {
    let bench = bench_or_fail();
    build_and_flash(&bench, Role::Dongle, "net_bulk", "").expect("flash sender (gateway role)");
    build_and_flash(&bench, Role::Core, "net_bulk", "role-node").expect("flash requester");

    let mut req = Console::open(&bench.core.serial).expect("open requester console");
    let mut snd = Console::open(&bench.dongle.serial).expect("open sender console");

    // RF flake tolerance: a chunked bulk session can die to a single lost window mid-air
    // ("bulk_fetch failed/timeout", observed once on 2026-07-05, clean on retry). The boards
    // are flashed once; a retry is just a re-reset + re-observe. ONLY that transport failure is
    // retried: a `MISMATCH` is corrupted data (never RF luck) and no verdict at all means the
    // requester never ran; both fail at once. One retry keeps a real regression loud: two
    // consecutive full-session failures is not RF luck.
    let mut last = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            eprintln!("HIL: bulk pull attempt 1 failed ({last:?}); resetting both boards for one retry");
        }
        snd.reset_into_app().expect("reset sender");
        req.reset_into_app().expect("reset requester");

        let verdict = req
            .wait_for(Duration::from_secs(40), |f| {
                frame_text(f).is_some_and(|t| {
                    t.contains("verify OK ***") || t.contains("MISMATCH") || t.contains("bulk_fetch failed")
                })
            })
            .expect("requester console read")
            .expect("bulk pull: no verdict frame within 40 s; did the requester run?");
        let t = frame_text(&verdict).unwrap_or("");
        if t.contains("verify OK ***") {
            assert!(t.contains("fetched 200 bytes"), "unexpected blob size: {t:?}");
            return;
        }
        assert!(
            t.contains("bulk_fetch failed"),
            "bulk pull returned corrupted data (not a flake): {t:?}"
        );
        last = t.to_string();
    }
    panic!("bulk pull failed twice in a row (not a flake): {last:?}");
}

/// Rapid-fire confirmed sends: the gateway must accept counters strictly monotonically:
/// an out-of-order or replayed counter is a latched ORDER VIOLATION.
#[test]
#[ignore = "requires the HIL bench (two TOWER boards in RF range)"]
fn ext_edge_rapid_monotonic() {
    let bench = bench_or_fail();
    build_and_flash(&bench, Role::Dongle, "edge_rapid", "").expect("flash gateway");
    build_and_flash(&bench, Role::Core, "edge_rapid", "role-node").expect("flash node");

    let mut gw = Console::open(&bench.dongle.serial).expect("open gateway console");
    gw.reset_into_app().expect("reset gateway");
    // Reset the node WITHOUT keeping its console open; the gateway capture is the assertion.
    tower_hil::reset_into_app(&bench.core.serial).expect("reset node");

    let frames = gw
        .collect_for(Duration::from_secs(30))
        .expect("gateway console read");
    let texts: Vec<&str> = frames.iter().filter_map(frame_text).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("accepted=") && t.contains("(monotonic OK)")),
        "gateway never reported monotonic progress (saw: {texts:?})"
    );
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("ORDER VIOLATION") || t.contains("(FAILED)")),
        "gateway accepted an out-of-order/replayed counter: {texts:?}"
    );
}

/// TX-counter persistence: across a reset, the counter must resume AT the persisted reserve
/// watermark (jump ahead, never reuse): the EEPROM anti-replay contract (docs/radio.md).
#[test]
#[ignore = "requires the HIL bench (TOWER Radio Dongle on the fixture port)"]
fn ext_net_persist_counter_resumes() {
    let bench = bench_or_fail();
    build_and_flash(&bench, Role::Dongle, "net_persist", "").expect("flash net_persist");

    // reset_into_app drops everything buffered while the chip is held in reset, so boot 1's banner
    // below can't be the post-flash boot's (which `tower flash` started before this console
    // attached); wm1 is boot 1's own watermark.
    let mut console = Console::open(&bench.dongle.serial).expect("open console");
    console.reset_into_app().expect("reset (boot 1)");

    let boot1 = console
        .wait_for(Duration::from_secs(10), |f| {
            frame_text(f).is_some_and(|t| t.contains("BOOT: resumed tx_counter="))
        })
        .expect("console read")
        .expect("no BOOT banner (boot 1)");
    let t1 = frame_text(&boot1).unwrap_or("").to_string();
    let cnt1 = field_u64(&t1, "tx_counter=").expect("boot 1: no tx_counter");
    let wm1 = field_u64(&t1, "reserve_watermark=").expect("boot 1: no watermark");
    assert!(
        wm1 > cnt1,
        "watermark must be reserved ahead of the live counter: {t1:?}"
    );

    // Let it burn a few counter values, then reboot and require the resume-at-watermark jump.
    assert!(
        console
            .wait_for_text(Duration::from_secs(10), "sent; tx_counter now")
            .expect("read"),
        "net_persist never sent (boot 1)"
    );
    console.reset_into_app().expect("reset (boot 2)");

    let boot2 = console
        .wait_for(Duration::from_secs(10), |f| {
            frame_text(f).is_some_and(|t| t.contains("BOOT: resumed tx_counter="))
        })
        .expect("console read")
        .expect("no BOOT banner (boot 2)");
    let t2 = frame_text(&boot2).unwrap_or("").to_string();
    let cnt2 = field_u64(&t2, "tx_counter=").expect("boot 2: no tx_counter");
    let wm2 = field_u64(&t2, "reserve_watermark=").expect("boot 2: no watermark");
    assert!(
        cnt2 >= wm1,
        "boot 2 resumed at {cnt2}, BELOW boot 1's watermark {wm1}: counter reuse (replay hazard): {t2:?}"
    );
    assert!(wm2 > cnt2, "boot 2 watermark not re-reserved: {t2:?}");
}
