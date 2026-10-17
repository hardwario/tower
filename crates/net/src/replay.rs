//! Per-peer replay-acceptance rule (moved from `firmware/src/radio/net/mod.rs::recv`; firmware/docs/radio.md
//! §Security model).
//!
//! One [`ReplayLane`] per sender: its last-seen counter plus the lazy-persist cadence. The rule
//! is **strictly monotonic**: `counter > last_seen` is fresh, `== last_seen` is the benign
//! retransmit of the most-recently-accepted transfer (re-ACK, don't re-deliver), `< last_seen`
//! is a replay (drop silently, replay state untouched). There is no in-RAM acceptance window
//! below last-seen; the "replay window ≤ P across a reboot" (firmware/docs/storage.md) is purely the
//! persistence lag: after a reset a lane restored from EEPROM can be up to [`P`] accepts
//! behind, so up to `P` already-seen counters are re-accepted once each.
//!
//! **Ordering contract (firmware/docs/radio.md):** on receive, CCM-verify FIRST (this authenticates the
//! header, including the counter), *then* [`classify`](ReplayLane::classify) and (only for a
//! fresh frame) [`accept`](ReplayLane::accept). A forged high counter therefore can't poison
//! replay state: it never reaches this kernel, because CCM rejects the frame before the network
//! layer acts. Callers must preserve that order.

/// Receiver last-seen lazy-persist period: the replay window across a reboot is
/// ≤ `P` transfers (firmware/docs/radio.md).
pub const P: u32 = 32;

// The accept cadence counter is a wrapping `u8` (RAM: it shares the lane's padding with the
// re-ACK counter, so a lane stays 8 B across all 17 lanes). `is_multiple_of(P)` stays exact
// across the wrap only while P divides 256.
const _: () = assert!(P <= 128 && 256 % P == 0);

/// Maximum transmissions of ONE counter by a legitimate sender: the confirmed-send repetition
/// ceiling (`Net::send` / `afa_send` clamp `reps` to `1..=MAX_REPS`; `fhss_send` sends once).
pub const MAX_REPS: u8 = 10;

/// Re-ACK cap per `(src, counter)`: a legitimate sender transmits one counter at most
/// [`MAX_REPS`] times: the original (answered by the [`Fresh`](ReplayVerdict::Fresh) ACK) plus
/// at most `MAX_REPS − 1` retransmits, so it can never need more than `MAX_REPS − 1` re-ACKs,
/// i.e. ≤ `MAX_REPS` ACKs per counter in total.
///
/// Why a cap at all: re-sending a peer's most recent confirmed frame needs **no key** (it is a
/// byte-identical capture), and each copy used to cost the receiver a fresh ACK: a TX counter,
/// EU duty budget (≈2250 ACKs drain the 36 s bucket → the gateway can neither ACK nor downlink
/// for up to an hour) and, in AFA mode, un-metered airtime. Capped, a captured frame buys an
/// attacker at most `MAX_REACKS` ACKs until the peer's next fresh transfer resets the count.
pub const MAX_REACKS: u8 = MAX_REPS - 1;

/// The replay-rule verdict for one authenticated frame (see the module docs for the exact
/// semantics of each arm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayVerdict {
    /// `counter > last_seen`: fresh: deliver, advance the lane, ACK if requested.
    Fresh,
    /// `counter == last_seen`: benign retransmit of this peer's most-recently-accepted
    /// counter (its ACK was lost); re-ACK deterministically, do NOT re-deliver.
    Retransmit,
    /// `counter < last_seen`: replay: drop silently (replay state untouched).
    Replay,
}

/// One sender's replay lane: last-seen counter + accepted-transfer count since the last
/// persist + re-ACKs spent on the current last-seen. On key install last-seen starts at **0**
/// (the TX counter starts at 1; `0` = "never sent"); a re-key resets both ends (a new key is a
/// disjoint nonce space, firmware/docs/radio.md).
#[derive(Debug, Clone, Copy)]
pub struct ReplayLane {
    /// Replay last-seen: the highest counter accepted from this sender.
    last_seen: u32,
    /// Accepted-transfer count on this lane (wrapping; only its residue mod [`P`] matters).
    accepts: u8,
    /// Re-ACKs already sent for `last_seen` (reset on every fresh accept), capped at
    /// [`MAX_REACKS`].
    reacks: u8,
}

impl ReplayLane {
    /// A lane resuming at `last_seen` (0 for a fresh lane; the persisted value on restore).
    #[must_use]
    pub fn new(last_seen: u32) -> Self {
        Self {
            last_seen,
            accepts: 0,
            reacks: 0,
        }
    }

    /// The lane's last-seen counter (diagnostics / persistence).
    #[must_use]
    pub fn last_seen(&self) -> u32 {
        self.last_seen
    }

    /// Classify an **authenticated** frame's counter against this lane (CCM-verify first; see
    /// the module docs). Pure: does not touch the lane.
    ///
    /// Counter `0` is unconditionally a [`Replay`](ReplayVerdict::Replay): `0` means "never
    /// sent" (TX counters start at 1), so no legitimate frame ever carries it. Without this
    /// rule a zero-counter frame on a *fresh* lane (last-seen 0) would compare equal and be
    /// re-ACKed as a "retransmit" of a transfer that never happened.
    #[must_use]
    pub fn classify(&self, counter: u32) -> ReplayVerdict {
        if counter == 0 {
            ReplayVerdict::Replay
        } else if counter > self.last_seen {
            ReplayVerdict::Fresh
        } else if counter == self.last_seen {
            ReplayVerdict::Retransmit
        } else {
            ReplayVerdict::Replay
        }
    }

    /// Record acceptance of `counter` (a [`Fresh`](ReplayVerdict::Fresh) frame): advance the
    /// lane's last-seen. Returns whether the caller must lazy-persist the lane now: true once
    /// every [`P`] accepts (replay window ≤ P across a reboot, firmware/docs/radio.md).
    #[must_use = "a true return means the lane must be persisted now"]
    pub fn accept(&mut self, counter: u32) -> bool {
        self.last_seen = counter;
        self.reacks = 0; // a new last-seen: its retransmits get a fresh re-ACK allowance
        self.accepts = self.accepts.wrapping_add(1);
        self.accepts.is_multiple_of(P as u8)
    }

    /// A [`Retransmit`](ReplayVerdict::Retransmit) of a *confirmed* frame arrived: may it be
    /// re-ACKed? Spends one of the [`MAX_REACKS`] re-ACKs allowed per last-seen counter and
    /// returns `false` once they are exhausted (drop silently: a legitimate sender has given
    /// up by then; see [`MAX_REACKS`] for the DoS this bounds). Reset by the next
    /// [`accept`](Self::accept).
    #[must_use = "false means: do NOT send the re-ACK"]
    pub fn reack(&mut self) -> bool {
        if self.reacks >= MAX_REACKS {
            return false;
        }
        self.reacks += 1;
        true
    }
}

/// One persisted replay-lane record as read back from EEPROM: the peer `addr` it belongs to,
/// its persisted last-seen `seen`, and the **key-check value** of the key that lane was tracked
/// under. A record of any other shape than this one (12 bytes on EEPROM) is invalid: the
/// caller reads it as absent.
///
/// The KCV (the first 4 bytes of `AES_K(0¹²⁸)`, computed by the firmware's AES engine) lets a
/// re-added peer restore its lane **only under the same key**. The all-zero block is never an
/// AES input in this stack's CCM (every B₀ / Aᵢ block carries a non-zero flags byte), so the
/// KCV exposes no keystream or MAC block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneRecord {
    pub addr: u32,
    pub seen: u32,
    pub kcv: u32,
}

/// Where a peer's replay lane is persisted. The result of [`assign_lane`]. A `fresh` binding
/// must be written now (`(addr, 0, kcv)` at `index`: it claims the local for this addr + key);
/// a restored one needs no write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneBinding {
    /// Index (`0..records.len()`) of the persisted lane local this peer's lane lives at.
    pub index: usize,
    /// Restored last-seen (the persisted value on restore; `0` when the lane starts fresh).
    pub seen: u32,
    /// `true` = the lane starts fresh at 0 (a newly claimed local, an evicted removed peer's
    /// local, or this addr's own record under a *different* key); `false` = restored.
    pub fresh: bool,
}

/// Bind a peer address (installed under the key whose KCV is `kcv`) to one of `records.len()`
/// persisted replay-lane locals, **keyed by peer addr, not by the caller's table slot**.
///
/// Addr-keying is the fix for a real bug: the lane used to be persisted at `base + table_slot`,
/// but a peer's table slot is assigned by registration order, which *changes* when an earlier
/// peer is removed and the registry compacts. On the next reboot the surviving later peers landed
/// on different slots → their addr-tagged records no longer matched → their replay lanes reset to
/// 0, reopening the window until they next transmitted. Keying by addr makes a peer find its own
/// lane wherever it sits.
///
/// `records[l]` = the stored record at local `l` (`None` if never written). `used` bit `l`
/// marks a local already bound to a *currently registered* peer this session (so two live peers
/// never share a local; a removed peer's record stays behind for resume-on-re-add while its local
/// becomes reusable). Priority:
///
/// 1. **This addr's own record** at a free local. Restored when its KCV matches `kcv`. If the
///    addr's only record carries a *different* KCV, the peer was re-added under a **new key**: a new key is a disjoint nonce
///    space and the peer may have restarted its counter (a factory-reset / re-paired node), so
///    restoring the old high last-seen would classify all its fresh frames as replays and drop
///    them silently. That local is reused, fresh at 0 (at most one record per addr). Among
///    several acceptable own records (only possible after a failed claim write) the highest
///    last-seen wins: the conservative choice for replay protection.
/// 2. **A never-written** free local.
/// 3. Only then **evict** a removed peer's record (the lowest such free local). Taking a
///    removed peer's local while never-written ones remain was a replay hole: remove A → add B
///    (overwrites A's record) → re-add A under the same key → A's lane restarted at 0 → A's
///    captured frames replayed as fresh. Eviction is now a capacity limit only (more distinct
///    peers ever registered than there are locals); a peer evicted that way and later re-added
///    under the *same* key restarts at 0; re-add it under a fresh key.
///
/// `None` only if every local is bound to a live peer, which the caller prevents by only calling
/// when it has a free table slot. `records.len()` ≤ 32 (the `used` bitmask width).
#[must_use]
pub fn assign_lane(addr: u32, kcv: u32, records: &[Option<LaneRecord>], used: u32) -> Option<LaneBinding> {
    let free = |l: usize| used & (1 << l) == 0;
    let fresh = |index| {
        Some(LaneBinding {
            index,
            seen: 0,
            fresh: true,
        })
    };
    // 1. This addr's own record(s), at locals no live peer occupies.
    let mut restore: Option<LaneBinding> = None;
    let mut stale_own: Option<usize> = None;
    for (l, rec) in records.iter().enumerate() {
        let Some(r) = *rec else { continue };
        if r.addr != addr || !free(l) {
            continue;
        }
        if r.kcv != kcv {
            stale_own.get_or_insert(l);
        } else if restore.is_none_or(|b| r.seen > b.seen) {
            // KCV match: restore (no write needed).
            restore = Some(LaneBinding {
                index: l,
                seen: r.seen,
                fresh: false,
            });
        }
    }
    if restore.is_some() {
        return restore;
    }
    if let Some(l) = stale_own {
        return fresh(l); // same addr, different key: reuse its local, fresh at 0
    }
    // 2. The lowest never-written free local.
    if let Some(l) = (0..records.len()).find(|&l| free(l) && records[l].is_none()) {
        return fresh(l);
    }
    // 3. Evict the lowest free local holding a removed peer's record.
    (0..records.len()).find(|&l| free(l)).and_then(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First message on a fresh lane (last-seen 0, sender's counter starts at 1) is fresh.
    #[test]
    fn first_message_accepted() {
        let lane = ReplayLane::new(0);
        assert_eq!(lane.classify(1), ReplayVerdict::Fresh);
    }

    /// An exact duplicate of the last-accepted counter is the benign-retransmit case: re-ACK,
    /// no re-delivery.
    #[test]
    fn exact_duplicate_is_retransmit() {
        let mut lane = ReplayLane::new(0);
        assert_eq!(lane.classify(5), ReplayVerdict::Fresh);
        let _ = lane.accept(5);
        assert_eq!(lane.classify(5), ReplayVerdict::Retransmit);
        // Still a retransmit on every further copy; the verdict is deterministic.
        assert_eq!(lane.classify(5), ReplayVerdict::Retransmit);
    }

    /// The rule is strictly monotonic: ANY counter below last-seen is a replay; there is no
    /// in-RAM acceptance window for "slightly older" frames (out-of-order delivery below the
    /// high-water mark is rejected by design; firmware/docs/radio.md).
    #[test]
    fn older_counters_rejected() {
        let mut lane = ReplayLane::new(0);
        let _ = lane.accept(10);
        for c in 0..10 {
            assert_eq!(lane.classify(c), ReplayVerdict::Replay, "counter {c}");
        }
    }

    /// Far-behind counters (e.g. a capture replayed much later) are equally rejected.
    #[test]
    fn behind_window_rejected() {
        let mut lane = ReplayLane::new(0);
        let _ = lane.accept(100_000);
        assert_eq!(lane.classify(1), ReplayVerdict::Replay);
        assert_eq!(lane.classify(99_999), ReplayVerdict::Replay);
    }

    /// A large forward jump (sender rebooted onto its reserve watermark, counter jumped by up
    /// to RESERVE) is fresh, and the lane slides forward; everything at or below the jump is
    /// then dead.
    #[test]
    fn large_forward_jump_accepted_and_slides() {
        let mut lane = ReplayLane::new(0);
        let _ = lane.accept(3);
        assert_eq!(lane.classify(3 + 1024), ReplayVerdict::Fresh);
        let _ = lane.accept(3 + 1024);
        assert_eq!(lane.last_seen(), 3 + 1024);
        assert_eq!(lane.classify(4), ReplayVerdict::Replay);
        assert_eq!(lane.classify(3 + 1024), ReplayVerdict::Retransmit);
        assert_eq!(lane.classify(3 + 1025), ReplayVerdict::Fresh);
    }

    /// Counter 0 is a Replay in EVERY lane state (fixed 2026-07-05; previously a fresh lane
    /// compared 0 == 0 and re-ACKed it as a "retransmit" of a transfer that never happened):
    /// 0 means "never sent" (TX counters start at 1), so a zero-counter frame is dropped
    /// silently: never delivered, never ACKed. (Per the ordering contract, such a frame only
    /// reaches the kernel at all if it authenticated under the key.)
    #[test]
    fn counter_zero_is_always_replay() {
        let fresh = ReplayLane::new(0);
        assert_eq!(fresh.classify(0), ReplayVerdict::Replay);
        let mut used = ReplayLane::new(0);
        let _ = used.accept(7);
        assert_eq!(used.classify(0), ReplayVerdict::Replay);
    }

    /// A forged huge counter never reaches the lane (CCM rejects first: the ordering contract
    /// in the module docs). What the kernel itself guarantees: `classify` is pure, so merely
    /// *looking* at any counter, however huge, cannot poison the lane.
    #[test]
    fn classify_never_mutates() {
        let mut lane = ReplayLane::new(0);
        let _ = lane.accept(7);
        let _ = lane.classify(u32::MAX);
        assert_eq!(lane.last_seen(), 7);
        assert_eq!(lane.classify(8), ReplayVerdict::Fresh); // lane unchanged by the probe
    }

    /// Reboot-window semantics (firmware/docs/storage.md): the lane persisted its last-seen up to P
    /// accepts ago, so a lane restored from that stale value re-accepts the last ≤ P counters
    /// once each, and each re-accept slides the lane so a *second* copy is rejected.
    #[test]
    fn reboot_replay_window_is_persistence_lag() {
        // Live lane accepted 1..=40; the persist fired at the 32nd accept (counter 32).
        let mut live = ReplayLane::new(0);
        let mut persisted = None;
        for c in 1..=40u32 {
            if live.accept(c) {
                persisted = Some(live.last_seen());
            }
        }
        assert_eq!(persisted, Some(32));
        // "Reboot": restore from the persisted value. Counters 33..=40 (≤ P of them) are
        // accepted AGAIN; that is exactly the documented ≤ P window…
        let mut rebooted = ReplayLane::new(persisted.unwrap());
        for c in 33..=40u32 {
            assert_eq!(rebooted.classify(c), ReplayVerdict::Fresh, "counter {c}");
            let _ = rebooted.accept(c);
            // …once each: the second copy of the same counter is no longer fresh.
            assert_eq!(rebooted.classify(c), ReplayVerdict::Retransmit);
        }
        // And anything at or below the persisted mark stays dead.
        assert_eq!(rebooted.classify(32), ReplayVerdict::Replay);
    }

    /// The lazy-persist cadence: `accept` returns true exactly on every P-th accept (the 32nd,
    /// 64th, …), independent of the counter values accepted.
    #[test]
    fn persist_cadence_every_p_accepts() {
        let mut lane = ReplayLane::new(0);
        let mut counter = 0u32;
        for n in 1..=(3 * P) {
            counter += 7; // arbitrary stride; cadence follows accepts, not counters
            let persist = lane.accept(counter);
            assert_eq!(persist, n % P == 0, "accept #{n}");
        }
    }

    /// u32 ceiling: once last-seen sits at 2³²−1 nothing can ever be fresh again: the strict
    /// `counter > last_seen` rule rejects every further frame, so the link fails **closed**
    /// (firmware/docs/radio.md: the TX side saturates at the same ceiling; re-key well before).
    #[test]
    fn u32_boundary_fails_closed() {
        let mut lane = ReplayLane::new(u32::MAX - 1);
        assert_eq!(lane.classify(u32::MAX), ReplayVerdict::Fresh);
        let _ = lane.accept(u32::MAX);
        assert_eq!(lane.classify(u32::MAX), ReplayVerdict::Retransmit);
        assert_eq!(lane.classify(0), ReplayVerdict::Replay);
        assert_eq!(lane.classify(u32::MAX - 1), ReplayVerdict::Replay);
        // No wrap-around acceptance exists anywhere in the space.
        for c in [1u32, 1000, u32::MAX / 2] {
            assert_eq!(lane.classify(c), ReplayVerdict::Replay);
        }
    }

    /// A restored lane (peer re-added, addr-matched record) resumes its window rather than
    /// reopening it: nothing at or below the restored last-seen is accepted.
    #[test]
    fn restored_lane_resumes_window() {
        let lane = ReplayLane::new(500);
        assert_eq!(lane.classify(500), ReplayVerdict::Retransmit);
        assert_eq!(lane.classify(499), ReplayVerdict::Replay);
        assert_eq!(lane.classify(501), ReplayVerdict::Fresh);
    }

    // --- re-ACK cap -------------------------------------------------------------------------

    /// A legitimate sender's retransmits of one counter (≤ MAX_REPS − 1 of them) are all
    /// re-ACKed; a capture replayed beyond that gets no further ACK.
    #[test]
    fn reack_capped_per_counter() {
        let mut lane = ReplayLane::new(0);
        let _ = lane.accept(5);
        for n in 0..MAX_REACKS {
            assert_eq!(lane.classify(5), ReplayVerdict::Retransmit);
            assert!(lane.reack(), "legitimate retransmit #{n} must be re-ACKed");
        }
        for _ in 0..100 {
            assert!(!lane.reack(), "replayed capture must not buy more ACKs");
        }
        // The cap bounds total ACKs for one counter at the sender's max transmissions.
        assert_eq!(1 + MAX_REACKS as u32, MAX_REPS as u32);
    }

    /// The allowance is per last-seen counter: the next fresh accept resets it.
    #[test]
    fn reack_allowance_resets_on_fresh_accept() {
        let mut lane = ReplayLane::new(0);
        let _ = lane.accept(1);
        while lane.reack() {}
        let _ = lane.accept(2);
        assert!(lane.reack(), "a new counter gets a fresh re-ACK allowance");
    }

    /// The u8 accept counter keeps the lazy-persist cadence exact across its wrap.
    #[test]
    fn persist_cadence_survives_u8_wrap() {
        let mut lane = ReplayLane::new(0);
        for n in 1..=(20 * P) {
            assert_eq!(lane.accept(n), n % P == 0, "accept #{n}");
        }
    }

    // --- assign_lane (addr-keyed, KCV-checked replay-lane binding) ------------------------------

    const K1: u32 = 0x1111_1111;
    const K2: u32 = 0x2222_2222;

    fn rec(addr: u32, seen: u32, kcv: u32) -> Option<LaneRecord> {
        Some(LaneRecord { addr, seen, kcv })
    }

    fn bind(index: usize, seen: u32, fresh: bool) -> Option<LaneBinding> {
        Some(LaneBinding { index, seen, fresh })
    }

    /// An unknown peer with no record claims the lowest free local (fresh, persisted); an
    /// occupied local is skipped.
    #[test]
    fn assign_lane_claims_lowest_free() {
        let records = [None, None, None, None];
        assert_eq!(assign_lane(0xAAAA, K1, &records, 0), bind(0, 0, true));
        assert_eq!(assign_lane(0xAAAA, K1, &records, 0b0001), bind(1, 0, true));
    }

    /// A peer finds its OWN record by addr, at whatever local it sits: the restore path (same
    /// key → no write).
    #[test]
    fn assign_lane_restores_own_record_by_addr() {
        let records = [
            rec(0xA, 10, K1),
            rec(0xB, 20, K1),
            rec(0xC, 30, K1),
            rec(0xD, 40, K2),
        ];
        assert_eq!(assign_lane(0xC, K1, &records, 0), bind(2, 30, false));
        assert_eq!(assign_lane(0xD, K2, &records, 0), bind(3, 40, false));
    }

    /// THE slot-shift regression: A,B,C,D registered (records A@0,B@1,C@2,D@3) → B removed (its
    /// record persists) → on reboot the boot mirror adds A,C,D in registry order; C and D must
    /// RESTORE their own lanes even though their table slots shifted.
    #[test]
    fn assign_lane_survives_remove_then_reboot() {
        let records = [
            rec(0xA, 10, K1),
            rec(0xB, 20, K1),
            rec(0xC, 30, K1),
            rec(0xD, 40, K1),
        ];
        let mut used = 0u32;
        let a = assign_lane(0xA, K1, &records, used).unwrap();
        assert_eq!((a.index, a.seen, a.fresh), (0, 10, false));
        used |= 1 << a.index;
        let c = assign_lane(0xC, K1, &records, used).unwrap();
        assert_eq!(
            (c.index, c.seen, c.fresh),
            (2, 30, false),
            "C must restore, not reset"
        );
        used |= 1 << c.index;
        let d = assign_lane(0xD, K1, &records, used).unwrap();
        assert_eq!(
            (d.index, d.seen, d.fresh),
            (3, 40, false),
            "D must restore, not reset"
        );
    }

    /// Finding #5(a), THE replay hole: remove A → add B → re-add A under the SAME key. B must
    /// take a never-written local (not A's), so A's record survives and A's lane resumes;
    /// otherwise A restarts at 0 and its captured frames replay as fresh.
    #[test]
    fn assign_lane_prefers_never_written_over_removed_peers_record() {
        // A was at local 0 (last-seen 500), then removed: nothing live.
        let mut records = [rec(0xA, 500, K1), None, None, None];
        let b = assign_lane(0xB, K2, &records, 0).unwrap();
        assert_eq!(
            b.index, 1,
            "B must not evict removed A's record while never-written locals remain"
        );
        records[b.index] = rec(0xB, 0, K2);
        let a = assign_lane(0xA, K1, &records, 1 << b.index).unwrap();
        assert_eq!(
            a,
            LaneBinding {
                index: 0,
                seen: 500,
                fresh: false
            }
        );
    }

    /// Eviction happens only when every free local holds a removed peer's record; the lowest
    /// such local is taken.
    #[test]
    fn assign_lane_evicts_only_when_no_never_written_local() {
        let records = [
            rec(0xA, 10, K1),
            rec(0xB, 20, K1),
            rec(0xC, 30, K1),
            rec(0xD, 40, K1),
        ];
        let used = 0b1101; // A@0, C@2, D@3 live; local 1 holds removed B's record.
        assert_eq!(assign_lane(0xE, K1, &records, used), bind(1, 0, true));
    }

    /// Finding #5(b): a peer re-added under a DIFFERENT key must not inherit the old key's
    /// last-seen: its counter may have restarted (factory reset / re-pair), and restoring would
    /// drop every fresh frame as a replay. Its own local is reused, fresh at 0, persisted.
    #[test]
    fn assign_lane_key_change_starts_fresh_at_own_local() {
        let records = [rec(0xB, 7, K1), rec(0xA, 900, K1), None];
        assert_eq!(assign_lane(0xA, K2, &records, 0), bind(1, 0, true));
    }

    /// Duplicate own records (only after a failed claim write): the highest acceptable last-seen
    /// is restored; a mismatched-key duplicate is ignored when an acceptable one exists.
    #[test]
    fn assign_lane_duplicates_restore_highest_acceptable() {
        let records = [rec(0xA, 10, K1), rec(0xA, 9999, K2), rec(0xA, 50, K1), None];
        assert_eq!(assign_lane(0xA, K1, &records, 0), bind(2, 50, false));
    }

    /// No free local (every local bound to a live peer) → None.
    #[test]
    fn assign_lane_none_when_full() {
        let records = [rec(0xA, 1, K1), rec(0xB, 2, K1)];
        assert_eq!(assign_lane(0xC, K1, &records, 0b11), None);
    }

    /// An addr-match at a local already held by a live peer is skipped (no two live peers on one
    /// local); fall through to a claim.
    #[test]
    fn assign_lane_skips_addr_match_at_occupied_local() {
        let records = [rec(0xA, 10, K1), None];
        assert_eq!(assign_lane(0xA, K1, &records, 0b01), bind(1, 0, true));
    }
}
