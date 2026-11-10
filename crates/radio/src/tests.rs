//! Host unit tests for the pure radio-timing core.
//!
//! These are the regulatory-arithmetic properties the firmware relies on but cannot test
//! itself (no_std, thumbv6m default target, no libtest). They pin the invariants the
//! security/compliance fixes established: the duty-governor residue carry (no refill lost to
//! truncation), the fixed-permutation FHSS spacing (§15.247), and non-under-counted airtime.

use super::*;

/// FHSS channel count the firmware uses (`config::FHSS_N`). Mirrored here so the permutation
/// tests exercise the real size without importing the firmware crate.
const FHSS_N: usize = 80;
/// FHSS slot length (ms) from `firmware/src/radio/net/fhss.rs` (`FHSS_SLOT_MS`). The cycle is
/// `FHSS_N · FHSS_SLOT_MS`; the compliance test asserts a channel's successive visits are
/// exactly one cycle apart, which must exceed the 20 s averaging window.
const FHSS_SLOT_MS: u64 = 300;

// --- DutyGovernor: residue / refill-truncation property (the parent's fix) ---------------

/// 1000 calls of `refill_ms(50)` must credit *exactly* the same airtime as one
/// `refill_ms(50_000)`: no credit lost to per-call integer truncation. This is the residue
/// accumulator the parent added: at 1 % (permil = 10), a 50 ms call earns `50·10/1000 = 0` ms
/// of whole-millisecond refill and, without carrying the 500-permil-ms remainder, would credit
/// *nothing* over 1000 calls while a single 50 s call credits 500 ms.
#[test]
fn refill_residue_no_truncation_loss() {
    // Drain both buckets to the same known-empty state first (cap is EU_CAP_MS, permil 10).
    let mut many = DutyGovernor::eu();
    let mut once = DutyGovernor::eu();
    assert!(many.try_consume(EU_CAP_MS)); // empty
    assert!(once.try_consume(EU_CAP_MS)); // empty
    assert_eq!(many.budget_ms(), 0);
    assert_eq!(once.budget_ms(), 0);

    for _ in 0..1000 {
        many.refill_ms(50);
    }
    once.refill_ms(50_000);

    // 50_000 ms · 10‰ = 500 ms credited, both ways, to the millisecond.
    assert_eq!(many.budget_ms(), 500, "residue carry lost refill to truncation");
    assert_eq!(once.budget_ms(), 500);
    assert_eq!(many.budget_ms(), once.budget_ms());
}

/// The same identity across a range of split counts and intervals: N refills of `step`
/// always equal one refill of `N·step` (mod the shared cap), for several permils.
#[test]
fn refill_split_equals_single_over_many_shapes() {
    for &permil in &[1u32, 10, 33, 250, 1000] {
        for &(n, step) in &[(1000u32, 50u32), (100, 7), (10_000, 1), (3, 33333)] {
            let cap = EU_CAP_MS;
            let mut split = DutyGovernor::new(cap, permil);
            let mut single = DutyGovernor::new(cap, permil);
            assert!(split.try_consume(cap));
            assert!(single.try_consume(cap));
            for _ in 0..n {
                split.refill_ms(step);
            }
            single.refill_ms(n.saturating_mul(step));
            assert_eq!(
                split.budget_ms(),
                single.budget_ms(),
                "split refill mismatch: permil={permil} n={n} step={step}"
            );
        }
    }
}

/// The bucket never exceeds its cap regardless of how much wall-clock is credited.
#[test]
fn refill_never_exceeds_cap() {
    let mut g = DutyGovernor::new(FHSS_DWELL_BURST_MS, FHSS_PERMIL);
    // Full already; credit a huge span.
    g.refill_ms(u32::MAX);
    assert_eq!(g.budget_ms(), FHSS_DWELL_BURST_MS);
    // Consume a little, over-refill again, still clamped.
    assert!(g.try_consume(40));
    g.refill_ms(1_000_000);
    assert_eq!(g.budget_ms(), FHSS_DWELL_BURST_MS);
}

/// `try_consume` boundary: succeeds iff the request fits the current budget, and decrements
/// exactly. An exact-budget consume drains to zero; one more byte is refused.
#[test]
fn try_consume_boundary() {
    let mut g = DutyGovernor::new(100, 0); // permil 0 → never refills, easy to reason about
    assert!(g.try_consume(60));
    assert_eq!(g.budget_ms(), 40);
    assert!(!g.try_consume(41), "over-budget consume must fail");
    assert_eq!(g.budget_ms(), 40, "a refused consume must not change the budget");
    assert!(g.try_consume(40), "exact-budget consume must succeed");
    assert_eq!(g.budget_ms(), 0);
    assert!(!g.try_consume(1), "empty bucket refuses any TX");
    assert!(g.try_consume(0), "a zero-airtime consume always fits");
}

// --- hop_channel: perfect permutation + §15.247 fixed-spacing compliance ------------------

/// For many seeds, `hop_channel` over slots `0..N` is a perfect permutation of `0..N`:
/// every channel appears exactly once (⇒ equal use). This is the "by construction" claim in
/// the fhss.rs module docs.
#[test]
fn hop_channel_is_perfect_permutation() {
    for seed in 0u32..5000 {
        let mut seen = [false; FHSS_N];
        for i in 0..FHSS_N as u8 {
            let ch = hop_channel::<FHSS_N>(seed, 0, i);
            assert!((ch as usize) < FHSS_N, "channel {ch} out of range (seed {seed})");
            assert!(
                !seen[ch as usize],
                "channel {ch} repeated (seed {seed}): not a permutation"
            );
            seen[ch as usize] = true;
        }
        assert!(seen.iter().all(|&s| s), "not every channel visited (seed {seed})");
    }
}

/// The §15.247 compliance property the fixed-permutation fix guarantees: since the permutation
/// is cycle-invariant, a given channel occupies the **same slot offset every cycle**, so its
/// successive on-air visits are exactly `FHSS_N · FHSS_SLOT_MS` apart. That spacing must exceed
/// the 20 s occupancy window (so a channel is tuned at most once per window). We assert, over a
/// large seed sweep, that (a) the permutation is identical across cycles and (b) the resulting
/// inter-visit spacing is the full cycle and strictly greater than 20 000 ms.
#[test]
fn hop_channel_fixed_across_cycles_bounds_occupancy() {
    let cycle_ms = FHSS_N as u64 * FHSS_SLOT_MS;
    assert!(
        cycle_ms > 20_000,
        "cycle {cycle_ms} ms must exceed the 20 s window (compliance invariant)"
    );

    for seed in 0u32..8000 {
        // Build the reference (cycle 0) permutation and each channel's slot offset within it.
        let mut slot_of_channel = [usize::MAX; FHSS_N];
        for i in 0..FHSS_N as u8 {
            let ch = hop_channel::<FHSS_N>(seed, 0, i) as usize;
            slot_of_channel[ch] = i as usize;
        }

        // Check several later cycles land the same permutation → the same slot for each channel.
        for &cycle in &[1u32, 2, 7, 100, 4321] {
            for i in 0..FHSS_N as u8 {
                assert_eq!(
                    hop_channel::<FHSS_N>(seed, cycle, i),
                    hop_channel::<FHSS_N>(seed, 0, i),
                    "permutation changed across cycles (seed {seed}, slot {i}); would break §15.247 spacing"
                );
            }
        }

        // Successive visits of any channel are exactly one cycle apart, > the 20 s window.
        for (ch, &slot) in slot_of_channel.iter().enumerate() {
            assert_ne!(slot, usize::MAX, "channel {ch} never scheduled (seed {seed})");
            // Absolute slot in cycle c is c·FHSS_N + slot; the on-air time is that ·FHSS_SLOT_MS.
            let t0 = slot as u64 * FHSS_SLOT_MS;
            let t1 = (FHSS_N + slot) as u64 * FHSS_SLOT_MS;
            let spacing = t1 - t0;
            assert_eq!(spacing, cycle_ms);
            assert!(
                spacing > FHSS_WINDOW_MS as u64,
                "channel {ch} visited twice inside the 20 s window (seed {seed})"
            );
        }
    }
}

/// A listening node's downlink goes out in the hop slot that is current when the host queued
/// it, on that slot's channel (the firmware's `Net::fhss_master_slot`), never on a channel of
/// its own. Two things follow for §15.247, and are pinned here.
///
/// The occupancy bound stays the schedule's: the most a master transmits in one slot (its
/// beacon, the ACK of an uplink, one full-length downlink) fits the slot, and the channel
/// comes round a whole cycle later, so no traffic pattern, not even every command falling on
/// one slot of the cycle, puts more than a slot's worth on a channel in 20 s.
///
/// And the channels are used evenly: commands whose timing has nothing to do with the schedule
/// spread their downlinks over the 80 channels as the slots themselves do.
#[test]
fn downlinks_in_the_current_slot_keep_the_dwell_bound_and_use_the_channels_evenly() {
    /// FCC §15.247: at most 0.4 s on a channel within any 20 s.
    const DWELL_LIMIT_MS: u64 = 400;
    let beacon = frame_toa_ms(14 + 9 + 8) as u64; // header + beacon payload + tag
    let ack = frame_toa_ms(14 + 6 + 8) as u64;
    let downlink = frame_toa_ms(96) as u64; // a full-length frame
    assert!(
        beacon + ack + downlink <= FHSS_SLOT_MS,
        "a slot's transmissions fit the slot"
    );
    // And a slot is all a channel gets per cycle, which is longer than the 20 s window.
    const { assert!(FHSS_SLOT_MS <= DWELL_LIMIT_MS) };
    const { assert!(FHSS_N as u64 * FHSS_SLOT_MS > FHSS_WINDOW_MS as u64) };

    // A day of commands 0.5 to 60 s apart (a xorshift stream: independent of the hop schedule),
    // each sent in the slot after the one it arrived in.
    const SEED: u32 = 0x2468_ace1;
    let (mut x, mut t_ms) = (0x9e37_79b9u32, 0u64);
    let mut sent = [0u64; FHSS_N];
    let mut commands = 0u64;
    while t_ms < 24 * 3600 * 1000 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        t_ms += 500 + u64::from(x % 59_500);
        let slot = t_ms / FHSS_SLOT_MS + 1;
        let cycle = (slot / FHSS_N as u64) as u32;
        let ch = hop_channel::<FHSS_N>(SEED, cycle, (slot % FHSS_N as u64) as u8);
        sent[ch as usize] += 1;
        commands += 1;
    }
    let mean = commands / FHSS_N as u64;
    let (min, max) = (*sent.iter().min().unwrap(), *sent.iter().max().unwrap());
    assert!(mean > 30, "enough commands per channel to compare ({mean})");
    assert!(
        min * 2 > mean && max < mean * 2,
        "downlinks per channel {min}..{max} around {mean}: no channel is favoured"
    );
    // With the beacons (one per channel and cycle) the transmitted airtime is flatter still.
    let cycles = t_ms / (FHSS_N as u64 * FHSS_SLOT_MS);
    let airtime = |n: u64| cycles * beacon + n * downlink;
    let mean_airtime = cycles * beacon + commands * downlink / FHSS_N as u64;
    assert!(
        airtime(max) * 10 < mean_airtime * 11,
        "busiest channel within 10 % of the mean"
    );
    assert!(
        airtime(min) * 10 > mean_airtime * 9,
        "quietest channel within 10 % of the mean"
    );
}

/// The hop sequence itself, pinned: both ends of a US 915 link compute it from the seed, so a
/// gateway and a node built from different versions of this function would never meet again
/// outside the rendezvous channel. The properties above (a perfect permutation, the same every
/// cycle) hold for any shuffle; these values hold this one. A change here is a decision.
#[test]
fn hop_channel_sequence_is_pinned() {
    let head = |seed: u32| -> [u8; 10] {
        let mut v = [0u8; 10];
        for i in 0..10u8 {
            v[i as usize] = hop_channel::<FHSS_N>(seed, 0, i);
        }
        v
    };
    assert_eq!(head(1), [66, 36, 7, 52, 43, 9, 45, 55, 14, 23]);
    assert_eq!(head(0xDEAD_BEEF), [40, 37, 63, 58, 77, 64, 47, 78, 57, 46]);
    assert_eq!(head(0x4EBB_5A77), [28, 77, 39, 73, 42, 43, 40, 8, 20, 69]);
    // The last slot of the cycle too (the shuffle's first swap).
    assert_eq!(hop_channel::<FHSS_N>(1, 0, 79), 49);
    assert_eq!(hop_channel::<FHSS_N>(0xDEAD_BEEF, 0, 79), 71);
    // A seed of 0 would stall the xorshift: it stands for a fixed non-zero one.
    assert_eq!(head(0), [19, 78, 7, 25, 67, 30, 41, 65, 17, 21]);
    assert_eq!(head(0), head(0xA5A5_A5A5));
    // And a short sequence in full.
    let mut small = [0u8; 8];
    for i in 0..8u8 {
        small[i as usize] = hop_channel::<8>(7, 0, i);
    }
    assert_eq!(small, [5, 2, 4, 3, 6, 1, 0, 7]);
}

// --- frame_toa_ms: never under-counts ----------------------------------------------------

/// Time-on-air is rounded **up** (div_ceil), so the reported airtime is always ≥ the exact
/// airtime; the regulatory budget is never under-counted. Verify across the full frame-length
/// range that `frame_toa_ms(len)` ≥ the exact ceiling of the on-air bit-time.
#[test]
fn frame_toa_never_undercounts() {
    for len in 0usize..=255 {
        let toa = frame_toa_ms(len) as u64;
        // True (fractional) airtime scaled by BITRATE, to compare without floating point:
        //   true_ms = bits · 1000 / BITRATE.  We assert (independently of the impl's div_ceil):
        //   (a) never under-counts:  toa · BITRATE ≥ bits · 1000
        //   (b) it is the *tight* ceiling, not over by a whole ms: (toa-1)·BITRATE < bits·1000
        let bits = (4 + 4 + 1 + len as u64 + 2) * 8;
        let true_x_bitrate = bits * 1000;
        assert!(
            toa * BITRATE as u64 >= true_x_bitrate,
            "toa under-counts airtime at len={len}"
        );
        assert!(
            toa == 0 || (toa - 1) * (BITRATE as u64) < true_x_bitrate,
            "toa over-counts by a whole ms at len={len} (not the tight ceiling)"
        );
    }
}

/// Monotonic: a longer frame never reports less airtime.
#[test]
fn frame_toa_monotonic() {
    let mut prev = 0;
    for len in 0usize..=255 {
        let toa = frame_toa_ms(len);
        assert!(toa >= prev, "toa decreased at len={len}");
        prev = toa;
    }
}

/// Regression (found by the `radio_kernels` fuzz target): `frame_toa_ms` used to compute in `u32`,
/// so a length past ~536 KB overflowed the bit-time product (a panic with overflow checks, a wrap
/// to a tiny airtime without) and a length past `u32::MAX` was truncated by the cast; both
/// under-count. Any length must give the exact ceiling while it fits a `u32`, then saturate.
#[test]
fn frame_toa_total_for_any_length() {
    let exact = |len: usize| ((len as u128 + 11) * 8 * 1000).div_ceil(BITRATE as u128);
    // The old overflow edge (u32::MAX / 8000 bytes) and well past it, still exact.
    for len in [536_859usize, 536_860, 1_000_000, 100_000_000] {
        assert_eq!(frame_toa_ms(len) as u128, exact(len), "len={len}");
    }
    // Lengths only a 64-bit host can name (the tests run on the host).
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(
            frame_toa_ms(usize::MAX),
            u32::MAX,
            "saturates, never wraps to a small value"
        );
        // The last length whose airtime fits a u32, then saturation.
        let max_fit = (u32::MAX as u128 * BITRATE as u128 / 8000 - 11) as usize;
        assert_eq!(frame_toa_ms(max_fit) as u128, exact(max_fit));
        assert_eq!(frame_toa_ms(max_fit + 1), u32::MAX);
        // Past u32::MAX bytes: the old cast truncated this one to a 5-byte frame.
        let len = u32::MAX as usize + 5;
        assert_eq!(frame_toa_ms(len) as u128, exact(len), "no truncating cast");
        // The fuzzer's input.
        assert_eq!(frame_toa_ms(17_582_054_044_766_043_904), u32::MAX);
    }
}

// --- uplink backoff (a sleeping node whose gateway is gone) ------------------------------

/// No miss: the configured period. Each miss doubles it, up to the heartbeat cap, and it never
/// overflows however many misses pile up.
#[test]
fn uplink_backoff_doubles_to_the_cap() {
    assert_eq!(uplink_backoff_s(60, 900, 0), 60);
    assert_eq!(uplink_backoff_s(60, 900, 1), 120);
    assert_eq!(uplink_backoff_s(60, 900, 3), 480);
    assert_eq!(uplink_backoff_s(60, 900, 4), 900);
    assert_eq!(uplink_backoff_s(60, 900, 255), 900);
    assert_eq!(uplink_backoff_s(u64::MAX / 2, u64::MAX, 255), u64::MAX);
}

/// A cap below the period (heartbeat shorter than the measuring period) never shortens the
/// period itself, and a zero period still waits a second.
#[test]
fn uplink_backoff_never_below_the_period() {
    assert_eq!(uplink_backoff_s(300, 60, 0), 300);
    assert_eq!(uplink_backoff_s(300, 60, 5), 300);
    assert_eq!(uplink_backoff_s(0, 0, 0), 1);
}
