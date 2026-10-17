//! TX-counter reserve-ahead watermark + fail-closed nonce-safety kernel (moved from
//! `firmware/src/radio/net/mod.rs`; firmware/docs/radio.md §Security model).
//!
//! The TX counter is the CCM nonce input, so the one invariant that must survive everything
//! (including power loss mid-write) is: **a counter value is never used twice under one key**.
//! The kernel guarantees it with a reserve-ahead watermark: a boot resumes the counter *at* the
//! last durably persisted watermark (strictly greater than any value actually sent) and
//! immediately reserves the next block, so at most one block of counter space is skipped per
//! reboot and no value repeats. Whenever durability can no longer be proven (the watermark
//! record is missing on a store that shows prior use, a watermark persist fails, or the counter
//! hits the 2³²−1 ceiling), TX **locks** (fail closed) rather than risk a nonce reuse.
//!
//! The kernel is pure: the caller (the firmware's `Net`) owns the EEPROM and reports persist
//! outcomes back via [`reserve_persisted`](TxCounter::reserve_persisted) / [`lock`](TxCounter::lock).
//!
//! **One allocator for every TX path.** A transfer obtains its counter from
//! [`alloc`](TxCounter::alloc), which hands out the value *already consumed*: taken, advanced
//! past, and (when a reserve block is exhausted) asking for the next watermark persist, in one
//! synchronous step, before the caller seals or awaits anything. The firmware's TX paths are
//! async: a future dropped mid-flight (a `select(send, Timer)` timing out, a caller abandoning a
//! bulk transfer) must not leave its counter unconsumed, or the next transfer re-reads it and
//! seals a *different* plaintext under the same `(key, nonce)`: a full CCM break. Consuming
//! "after the TX/ACK await" (the old shape of `fhss_send`/`afa_send`/bulk) was exactly that bug.
//!
//! **The counter is per device, keys move between devices.** The nonce is `src ‖ counter ‖ …`
//! and the counter belongs to the device, not to a key, so a key that was used on one counter
//! line and is then installed where the counter restarted (a gateway whose store was wiped
//! importing its old registry under the same address) would repeat that key's nonces from the
//! start. A key therefore travels with a counter **floor**: [`floor`](TxCounter::floor) on the
//! exporting side (above every counter it ever sent), [`raise_to`](TxCounter::raise_to) on the
//! importing side (nothing below it is ever sent again, durably) before the key is installed.

/// TX-counter reserve block: persist the watermark only once per `RESERVE`
/// transfers, and on boot resume *at* the watermark (> any value actually sent,
/// so a counter is never reused; ≤ one block is skipped per reboot, firmware/docs/radio.md).
pub const RESERVE: u32 = 1024;

/// The TX-counter state: live counter, reserved (persisted) limit, and the fail-closed lock.
#[derive(Debug, Clone, Copy)]
pub struct TxCounter {
    /// Monotonic TX counter, advanced by one per transfer (firmware/docs/radio.md).
    counter: u32,
    /// Highest reserved (persisted) counter value; `counter < reserve_limit`
    /// holds as an invariant only while [`locked`](Self::locked) is false:
    /// the counter is never used at or past the last *durably* persisted watermark.
    /// `u32::MAX` after a lost watermark: the history is unknown, so every counter may have
    /// been sent (what [`floor`](Self::floor) must report).
    reserve_limit: u32,
    /// Set once a reserve-watermark persist fails: TX is refused (fail closed) so a
    /// counter past the last durable watermark can never go on air (which after a
    /// reboot resuming at the stale watermark would reuse a CCM nonce). See
    /// [`advance`](Self::advance) and the firmware's `RadioError::NonceLocked`.
    locked: bool,
}

impl TxCounter {
    /// Boot-time resume from the persisted state: `stored_watermark` is the watermark record
    /// (if readable), `prior_use` whether the store shows any other evidence of prior network
    /// use (see "What counts as `prior_use`" below). Resumes the counter *at* the
    /// watermark (1 on the very first boot, since 0 = "never sent") and reserves the next
    /// block. Returns the kernel plus the watermark the caller must now persist **and verify**, or
    /// `None` when the boot must not write one (see below), in which case the kernel starts
    /// locked. On a successful, read-back-verified persist the caller does nothing further; on
    /// failure it must call [`lock`](Self::lock).
    ///
    /// A store that shows prior use but whose watermark is now unreadable has almost certainly
    /// lost the watermark record to EEPROM corruption (`scan_half` stops at the first bad
    /// record, orphaning everything after it, and the watermark, rewritten every RESERVE
    /// transfers, tends to sit late in the log). Resuming the TX counter at 1 under the
    /// unchanged key would then reuse every CCM nonce 1..old. Fail **closed** WITHOUT rewriting
    /// a fresh low watermark (that would poison a later boot into resuming low and reusing
    /// nonces). The device can still receive; sending needs a re-key / factory reset. A
    /// genuinely virgin store (no watermark AND no other network record) legitimately starts
    /// at 1.
    ///
    /// **What counts as `prior_use`:** ANY persisted network-layer record other than the
    /// watermark itself: the default-lane last-seen, *any* per-peer replay-lane record, the
    /// FHSS master epoch. Every one of them is written only by a `Net` that already ran its
    /// boot (which persists a watermark first), so its presence proves a watermark existed.
    /// Counting only the default-lane last-seen (the original rule) left the guard dead in the
    /// product apps, which run on peer lanes alone (the gateway disables the default lane).
    /// It also counts the app's own word that it holds **stored keys** (a provisioning record,
    /// a non-empty registry): those are written once and early, while the watermark and the
    /// lanes are rewritten often and sit late in the log, so one corrupt record can orphan
    /// every network record at once and leave only the keys. Without that evidence the boot
    /// looked virgin and resumed at 1 under the old keys.
    ///
    /// **Residual (not detectable here): a *stale older* watermark.** If the corruption orphans
    /// the newest watermark record but an *older* watermark record of the same key precedes the
    /// bad record in the same KV half, the store reads back that older value (`Some(old)`, not
    /// `None`), and the boot resumes below counters already sent. Nothing in the network
    /// namespace records our own TX counter elsewhere, so this kernel cannot tell a stale
    /// watermark from a current one. A complete fix needs the KV layer to report mid-log
    /// corruption (valid-looking records *after* the first bad one, as opposed to a benign torn
    /// tail write) so the boot can fail closed on it. Torn writes at the log tail (the normal
    /// power-loss case) are safe: the interrupted record is the newest one, so nothing after it
    /// is lost.
    #[must_use = "the returned watermark must be persisted and verified (lock() on failure)"]
    pub fn resume(stored_watermark: Option<u32>, prior_use: bool) -> (Self, Option<u32>) {
        let resume = stored_watermark.unwrap_or(1).max(1);
        // A stored watermark AT the u32 ceiling: every value below it may already have been
        // sent, and `resume == reserve_limit == u32::MAX` leaves no durable headroom; handing
        // out MAX here would let *every* subsequent boot send one more frame under the same
        // pinned MAX nonce (each boot resumes at the same stored MAX). Locked, like the lost
        // case. (Reachable only after ~2³² transfers; pinned by `resume_at_ceiling_is_locked`.)
        let fail_closed = (stored_watermark.is_none() && prior_use) || resume == u32::MAX;
        // Saturating, not wrapping: a stored watermark within RESERVE of u32::MAX must pin
        // the limit at the ceiling; wrapping would persist a LOW watermark, and the boot
        // after that would resume low and reuse CCM nonces 1..old (the exact break this type
        // exists to prevent). Saturated, the counter just runs into `advance`'s u32::MAX
        // ceiling lock within one block: the link ends its life failing closed, never
        // repeating a nonce.
        let reserve_limit = if stored_watermark.is_none() && prior_use {
            u32::MAX // history lost: report the worst case (see `floor`), nothing is persisted
        } else {
            resume.saturating_add(RESERVE)
        };
        let txc = Self {
            counter: resume,
            reserve_limit,
            locked: fail_closed,
        };
        (txc, if fail_closed { None } else { Some(reserve_limit) })
    }

    /// Current live TX counter: the value the next frame rides (and the CCM nonce input).
    #[must_use]
    pub fn counter(&self) -> u32 {
        self.counter
    }

    /// Current reserved (persisted) watermark.
    #[must_use]
    pub fn reserve_limit(&self) -> u32 {
        self.reserve_limit
    }

    /// Whether TX is locked (fail closed: no counter may be allocated / no frame emitted).
    #[must_use]
    pub fn locked(&self) -> bool {
        self.locked
    }

    /// This device's **counter floor**: every counter it has ever sent, under any key, is below
    /// it (the durable watermark; `u32::MAX` when the history is unknown after a lost watermark,
    /// or the counter is exhausted). A key exported from here carries it, so the device that
    /// imports the key can [`raise_to`](Self::raise_to) it before sending anything under that
    /// key: the counter is per device, the key is not, and a device whose counter restarted
    /// would otherwise repeat the key's nonces. A snapshot: it covers what was sent until now.
    #[must_use]
    pub fn floor(&self) -> u32 {
        self.reserve_limit.max(self.counter)
    }

    /// Raise the counter to at least `floor` (an imported key's [`floor`](Self::floor)), durably:
    /// returns `true` once no counter below `floor` can ever be sent again, in this boot or after
    /// any reboot, which is what the caller must have before it persists or installs the key.
    ///
    /// When the durable watermark already covers `floor` nothing is written. Otherwise the
    /// watermark `floor + RESERVE` (saturating) is handed to `persist`, which must write **and
    /// read back** it; the raise is committed only on `true`. A failed persist locks TX and
    /// returns `false`: the key must not be installed, since a reboot would resume at the old
    /// watermark, below `floor`. A raise
    /// to `u32::MAX` persists the ceiling and locks: no counter is left for that key, here or
    /// after a reboot (a key with an unknown history should be refused before it gets here).
    ///
    /// While locked nothing is sent this boot, and the next boot resumes at the durable
    /// watermark (`reserve_limit`), so the floor is covered exactly when it is at or below it.
    #[must_use = "false: the floor is not durable, the key must not be installed"]
    pub fn raise_to(&mut self, floor: u32, persist: impl FnOnce(u32) -> bool) -> bool {
        if self.locked {
            return floor <= self.reserve_limit;
        }
        if floor <= self.counter {
            return true; // already past it (and the counter is below the durable watermark)
        }
        if floor < self.reserve_limit {
            self.counter = floor; // the durable watermark already covers it
            return true;
        }
        let watermark = floor.saturating_add(RESERVE);
        if !persist(watermark) {
            self.locked = true;
            return false;
        }
        self.reserve_limit = watermark;
        self.counter = floor;
        if floor == u32::MAX {
            self.locked = true; // the ceiling: MAX itself is never sent (see `advance`)
        }
        true
    }

    /// Advance the TX counter by one transfer. Returns the next watermark to persist when the
    /// current reserve is exhausted; the caller must persist it and report back with
    /// [`reserve_persisted`](Self::reserve_persisted) (success) or [`lock`](Self::lock)
    /// (failure) before allocating another counter.
    ///
    /// **Saturating, not wrapping**: the counter is the CCM nonce input, so it must never
    /// wrap back to a reused value. At the 2³²−1 ceiling (≈136 yr at 1 Hz, practically
    /// unreachable) it **locks TX** (`locked = true`): every subsequent send fails closed
    /// rather than transmitting another frame under the pinned `u32::MAX` nonce. A reused
    /// `(key, nonce)` is a CCM confidentiality *and* integrity break (keystream + MAC reuse),
    /// not merely a delivery failure the replay rule would reject, so we must stop emitting,
    /// not rely on the peer to drop it. Re-key well before then.
    #[must_use = "a returned watermark must be persisted (reserve_persisted / lock on failure)"]
    pub fn advance(&mut self) -> Option<u32> {
        self.counter = self.counter.saturating_add(1);
        if self.counter == u32::MAX {
            // Ceiling reached: fail closed. Do not emit any further frame (the next send would
            // reuse the pinned MAX nonce) and stop churning the reserve watermark.
            self.locked = true;
            return None;
        }
        if self.counter >= self.reserve_limit {
            // Extend the reservation, but only trust it once the write lands: the caller
            // persists this value and reports back. See `reserve_persisted` / `lock`.
            return Some(self.reserve_limit.saturating_add(RESERVE));
        }
        None
    }

    /// Allocate the counter for **one transfer**: the single entry point every TX path uses
    /// (data, ACK, beacon, bulk announce/session/request, pairing). See the module docs.
    ///
    /// Returns `None` while locked (fail closed: no frame may be sealed). Otherwise returns the
    /// counter the transfer must ride, **already consumed**: the kernel has advanced past it,
    /// so no later `alloc` can ever return it again, whatever the caller does next (drops its
    /// future mid-await, aborts, never transmits: a never-sent value is just a harmless hole in
    /// the monotonic sequence). The second element is a watermark to persist *now*, before the
    /// caller awaits anything, reported back with [`reserve_persisted`](Self::reserve_persisted)
    /// or [`lock`](Self::lock) exactly as for [`advance`](Self::advance).
    ///
    /// The returned counter is nonce-safe **even if that persist then fails**: it was taken
    /// while unlocked, so it is strictly below the last *durable* watermark (`counter <
    /// reserve_limit`); a failed persist locks only the *next* allocation. Retransmissions of
    /// one transfer re-send the byte-identical frame and must NOT allocate again.
    #[must_use = "the counter is consumed; a returned watermark must be persisted (reserve_persisted / lock)"]
    pub fn alloc(&mut self) -> Option<(u32, Option<u32>)> {
        if self.locked {
            return None;
        }
        let counter = self.counter;
        let persist = self.advance();
        Some((counter, persist))
    }

    /// The watermark returned by [`advance`](Self::advance) was durably persisted: commit it as
    /// the new reserve limit.
    pub fn reserve_persisted(&mut self, watermark: u32) {
        self.reserve_limit = watermark;
    }

    /// A watermark persist failed (or did not read back): lock TX, permanently. We must NOT
    /// advance `reserve_limit` on a failed persist; doing so would let us keep emitting
    /// counters past the last *durable* watermark, and a reboot that then resumes at the stale
    /// watermark would reuse those CCM nonces. Instead fail closed: the guard on every send
    /// path refuses to transmit while locked. Nothing unlocks a `TxCounter`.
    pub fn lock(&mut self) {
        self.locked = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a kernel through `n` transfers with an always-succeeding persist store, returning
    /// every counter value consumed. Mirrors the firmware's advance_tx_counter loop.
    fn run(txc: &mut TxCounter, store: &mut Option<u32>, n: u32, out: &mut impl FnMut(u32)) {
        for _ in 0..n {
            assert!(!txc.locked());
            out(txc.counter());
            if let Some(next) = txc.advance() {
                *store = Some(next); // persist OK
                txc.reserve_persisted(next);
            }
        }
    }

    /// Fresh start (virgin store): counter resumes at 1 ("0 = never sent"), the first block is
    /// reserved at 1 + RESERVE, and the boot asks for that watermark to be persisted.
    #[test]
    fn fresh_start() {
        let (txc, persist) = TxCounter::resume(None, false);
        assert!(!txc.locked());
        assert_eq!(txc.counter(), 1);
        assert_eq!(txc.reserve_limit(), 1 + RESERVE);
        assert_eq!(persist, Some(1 + RESERVE));
    }

    /// A stored watermark of 0 still resumes at 1 (the `.max(1)` floor; counter 0 is reserved
    /// for "never sent").
    #[test]
    fn stored_zero_watermark_resumes_at_one() {
        let (txc, persist) = TxCounter::resume(Some(0), true);
        assert_eq!(txc.counter(), 1);
        assert_eq!(persist, Some(1u32.wrapping_add(RESERVE)));
        assert!(!txc.locked());
    }

    /// Resume-from-watermark never reuses: the counter resumes AT the persisted watermark
    /// (strictly greater than anything sent in the previous life), and the new reserve jumps
    /// ahead by RESERVE.
    #[test]
    fn resume_at_watermark_never_reuses() {
        // Life 1: fresh boot, send 700 transfers (persisted watermark stays 1 + RESERVE).
        let mut store = None;
        let (mut txc, persist) = TxCounter::resume(None, false);
        store = persist.or(store);
        let mut max_sent = 0;
        run(&mut txc, &mut store, 700, &mut |c| max_sent = max_sent.max(c));
        assert_eq!(max_sent, 700);
        assert_eq!(store, Some(1 + RESERVE));

        // Life 2 (power lost without further persists): resumes at the watermark.
        let (txc2, persist2) = TxCounter::resume(store, true);
        assert!(txc2.counter() > max_sent, "resume must exceed anything sent");
        assert_eq!(txc2.counter(), 1 + RESERVE);
        assert_eq!(persist2, Some(1 + 2 * RESERVE)); // watermark jumps by RESERVE
    }

    /// Repeated power-loss cycles: counters are strictly increasing across lives, with no
    /// value ever consumed twice, and each boot advances the watermark monotonically.
    #[test]
    fn repeated_power_loss_is_monotonic() {
        let mut store: Option<u32> = None;
        let mut prior_use = false;
        let mut last_counter = 0u32;
        let mut last_watermark = 0u32;
        for life in 0u32..20 {
            let (mut txc, persist) = TxCounter::resume(store, prior_use);
            assert!(!txc.locked(), "life {life}");
            let wm = persist.expect("boot persists a watermark");
            assert!(wm > last_watermark, "watermark monotone (life {life})");
            store = Some(wm);
            last_watermark = wm;
            // Send an irregular number of transfers, some lives crossing a reserve boundary.
            let sends = 1 + (life * 397) % (RESERVE + 300);
            run(&mut txc, &mut store, sends, &mut |c| {
                assert!(c > last_counter, "counter {c} reused/regressed in life {life}");
                last_counter = c;
            });
            prior_use = true; // later lives have receive/lane history
        }
    }

    /// Watermark-lost boot (prior use but no readable watermark): locked, and NO watermark is
    /// written (a fresh low watermark would poison a later boot into resuming low). `prior_use`
    /// includes the app's stored keys: a store that lost every network record at once but kept
    /// a provisioning record fails closed the same way.
    #[test]
    fn watermark_lost_fails_closed_without_writing() {
        let (txc, persist) = TxCounter::resume(None, true);
        assert!(txc.locked());
        assert_eq!(persist, None, "must not rewrite a fresh low watermark");
    }

    /// A genuinely virgin store (no watermark AND no prior use) legitimately starts unlocked.
    #[test]
    fn virgin_store_is_not_watermark_lost() {
        let (txc, persist) = TxCounter::resume(None, false);
        assert!(!txc.locked());
        assert!(persist.is_some());
    }

    /// Boot persist failure: the caller locks the kernel; it stays locked.
    #[test]
    fn boot_persist_failure_locks() {
        let (mut txc, persist) = TxCounter::resume(None, false);
        assert!(persist.is_some());
        txc.lock(); // write failed / did not read back
        assert!(txc.locked());
    }

    /// Off-by-one at the reserve boundary: from a fresh boot (counter 1, limit 1+RESERVE) the
    /// counter values 2..=RESERVE advance with no persist request; stepping onto the limit
    /// (counter == 1+RESERVE) requests the next watermark. While unlocked, the counter never
    /// passes the durable watermark.
    #[test]
    fn reserve_boundary_exact() {
        let (mut txc, _persist) = TxCounter::resume(None, false);
        let limit = txc.reserve_limit();
        // Advances that keep counter < limit request nothing.
        for _ in 0..(RESERVE - 1) {
            assert!(txc.counter() < limit);
            assert_eq!(txc.advance(), None);
        }
        assert_eq!(txc.counter(), RESERVE); // == limit − 1: last counter under the watermark
        // The advance that reaches the watermark must re-reserve BEFORE further sends.
        assert_eq!(txc.advance(), Some(limit + RESERVE));
        assert_eq!(txc.counter(), limit);
        txc.reserve_persisted(limit + RESERVE);
        assert_eq!(txc.reserve_limit(), limit + RESERVE);
        assert_eq!(txc.advance(), None); // back under the (new) watermark
    }

    /// Reserve-persist failure mid-life: the kernel locks and the reserve limit is NOT
    /// advanced (emitting past the last durable watermark is exactly the nonce-reuse hazard).
    #[test]
    fn reserve_persist_failure_locks_without_advancing_limit() {
        let (mut txc, _persist) = TxCounter::resume(None, false);
        let limit = txc.reserve_limit();
        for _ in 0..(RESERVE - 1) {
            assert_eq!(txc.advance(), None);
        }
        let req = txc.advance();
        assert_eq!(req, Some(limit + RESERVE));
        txc.lock(); // persist failed
        assert!(txc.locked());
        assert_eq!(
            txc.reserve_limit(),
            limit,
            "failed persist must not extend the reserve"
        );
    }

    /// Near-exhaustion: reaching the 2³²−1 ceiling locks TX, and it STAYS locked (nothing
    /// unlocks a TxCounter; further advances keep saturating at MAX and re-locking).
    #[test]
    fn exhaustion_locks_and_stays_locked() {
        // Start near the ceiling via a (huge) stored watermark.
        let (mut txc, _persist) = TxCounter::resume(Some(u32::MAX - 3), true);
        assert_eq!(txc.counter(), u32::MAX - 3);
        let mut advances = 0;
        while !txc.locked() {
            if let Some(next) = txc.advance() {
                txc.reserve_persisted(next);
            }
            advances += 1;
            assert!(advances < 10, "must lock within a few advances of the ceiling");
        }
        // Locked exactly when the counter pinned at MAX.
        assert_eq!(txc.counter(), u32::MAX);
        assert_eq!(advances, 3);
        // Sticky: even the (firmware-unreachable) advance-while-locked path keeps it locked.
        assert_eq!(txc.advance(), None);
        assert!(txc.locked());
        assert_eq!(txc.counter(), u32::MAX); // saturating: the MAX nonce is never exceeded…
    }

    /// The exact ceiling edge: advancing from MAX−1 pins the counter at MAX and locks in the
    /// same step, with no watermark churn (no persist request).
    #[test]
    fn ceiling_edge_off_by_one() {
        let (mut txc, _persist) = TxCounter::resume(Some(u32::MAX - 1), true);
        assert!(!txc.locked());
        assert_eq!(txc.counter(), u32::MAX - 1); // one frame may still ride MAX−1
        assert_eq!(txc.advance(), None, "ceiling lock must not churn the watermark");
        assert!(txc.locked());
        assert_eq!(txc.counter(), u32::MAX);
    }

    // --- alloc (the single TX-counter allocator) -----------------------------------------------

    /// Drive `alloc` with a persist store that always succeeds; returns the allocated counter.
    fn alloc_ok(txc: &mut TxCounter, store: &mut Option<u32>) -> Option<u32> {
        let (c, persist) = txc.alloc()?;
        if let Some(next) = persist {
            *store = Some(next);
            txc.reserve_persisted(next);
        }
        Some(c)
    }

    /// The cancellation model: a transfer that allocates and is then dropped (never transmits,
    /// never "advances afterwards") has still consumed its counter: the next transfer gets a
    /// strictly greater one. This is the property `fhss_send`/`afa_send`/bulk violated when they
    /// advanced only after their TX/ACK awaits.
    #[test]
    fn alloc_consumes_even_if_the_transfer_is_abandoned() {
        let (mut txc, persist) = TxCounter::resume(None, false);
        let mut store = persist;
        let first = alloc_ok(&mut txc, &mut store).unwrap();
        // ...the caller's future is dropped here: no further call on the kernel...
        let second = alloc_ok(&mut txc, &mut store).unwrap();
        assert_eq!(first, 1);
        assert!(
            second > first,
            "an abandoned transfer's counter must never be handed out again"
        );
    }

    /// Every allocation across many lives (each ending in power loss at an arbitrary point) is
    /// unique and strictly increasing, and never at/above the watermark that was durable when
    /// it was handed out.
    #[test]
    fn alloc_is_unique_across_lives_and_below_durable_watermark() {
        let mut store: Option<u32> = None;
        let mut prior_use = false;
        let mut last = 0u32;
        for life in 0u32..12 {
            let (mut txc, persist) = TxCounter::resume(store, prior_use);
            store = persist.or(store);
            prior_use = true;
            for _ in 0..(1 + (life * 611) % (2 * RESERVE + 7)) {
                let durable = store.unwrap();
                let c = alloc_ok(&mut txc, &mut store).unwrap();
                assert!(c > last, "counter {c} reused/regressed (life {life})");
                assert!(
                    c < durable,
                    "counter {c} handed out at/above durable watermark {durable}"
                );
                last = c;
            }
        }
    }

    /// `alloc` refuses while locked (fail closed), and the lock is sticky.
    #[test]
    fn alloc_none_when_locked() {
        let (mut txc, _) = TxCounter::resume(None, true); // watermark lost → locked
        assert_eq!(txc.alloc(), None);
        let (mut txc, _) = TxCounter::resume(None, false);
        txc.lock();
        assert_eq!(txc.alloc(), None);
        assert_eq!(txc.alloc(), None);
    }

    /// The allocation that exhausts the reserve block returns a counter that is still below the
    /// durable watermark (safe to transmit even if the persist it requests FAILS), and the
    /// failure locks only the NEXT allocation. After a reboot (resuming at the old, still-durable
    /// watermark) the abandoned value is never reissued.
    #[test]
    fn alloc_persist_failure_keeps_issued_counter_safe_and_locks_next() {
        let (mut txc, persist) = TxCounter::resume(None, false);
        let durable = persist.unwrap();
        for _ in 0..(RESERVE - 1) {
            let (_, p) = txc.alloc().unwrap();
            assert_eq!(p, None);
        }
        let (c, p) = txc.alloc().unwrap();
        assert!(p.is_some(), "this allocation exhausts the block");
        assert!(
            c < durable,
            "the issued counter is covered by the durable watermark"
        );
        txc.lock(); // persist failed
        assert_eq!(
            txc.alloc(),
            None,
            "nothing past the durable watermark may be issued"
        );
        let (rebooted, _) = TxCounter::resume(Some(durable), true);
        assert!(
            rebooted.counter() > c,
            "a reboot never reissues an issued counter"
        );
    }

    /// `alloc` at the ceiling: MAX−1 is the last value ever issued; the allocation that issues it
    /// pins the counter at MAX and locks.
    #[test]
    fn alloc_ceiling_issues_max_minus_one_last() {
        let (mut txc, _) = TxCounter::resume(Some(u32::MAX - 2), true);
        assert_eq!(txc.alloc().map(|a| a.0), Some(u32::MAX - 2));
        assert_eq!(txc.alloc().map(|a| a.0), Some(u32::MAX - 1));
        assert!(txc.locked());
        assert_eq!(txc.alloc(), None);
    }

    /// A stored watermark AT the ceiling boots locked and writes nothing. Without this, every
    /// boot resumed at the stored MAX and could send one more frame under the same pinned MAX
    /// nonce (fixed 2026-10).
    #[test]
    fn resume_at_ceiling_is_locked() {
        let (mut txc, persist) = TxCounter::resume(Some(u32::MAX), true);
        assert!(txc.locked());
        assert_eq!(persist, None);
        assert_eq!(txc.alloc(), None);
        // Two consecutive boots at the ceiling: still nothing issued.
        let (mut again, _) = TxCounter::resume(Some(u32::MAX), true);
        assert_eq!(again.alloc(), None);
    }

    // --- floor / raise_to (a key's counter history travels with it) ----------------------------

    /// The floor is above every counter ever handed out, and a lost history reports the worst
    /// case (an importer must not trust a floor nobody knows).
    #[test]
    fn floor_covers_everything_sent() {
        let (mut txc, persist) = TxCounter::resume(None, false);
        let mut store = persist;
        let mut max = 0;
        for _ in 0..(RESERVE + 10) {
            max = alloc_ok(&mut txc, &mut store).unwrap();
        }
        assert!(txc.floor() > max);
        assert_eq!(Some(txc.floor()), store, "the floor is the durable watermark");
        let (lost, _) = TxCounter::resume(None, true);
        assert_eq!(lost.floor(), u32::MAX, "a lost watermark: history unknown");
    }

    /// THE nonce-reuse scenario: a gateway sends under key K, exports it with its floor, loses
    /// its store (counter back to 1), and imports K again. Raised to the floor first, it never
    /// re-sends one of the old counters, and neither does any later life of the new store.
    #[test]
    fn raise_to_floor_never_reuses_an_exported_counter() {
        // Life A: fresh store, ~2.5 blocks of traffic, then export.
        let (mut a, p) = TxCounter::resume(None, false);
        let mut store_a = p;
        let mut sent_a = 0;
        for _ in 0..(2 * RESERVE + 500) {
            sent_a = alloc_ok(&mut a, &mut store_a).unwrap();
        }
        let floor = a.floor();
        // Life B: the wiped store (virgin), then the import raises the floor.
        let (mut b, p) = TxCounter::resume(None, false);
        let mut store_b = p;
        assert!(b.raise_to(floor, |w| {
            store_b = Some(w);
            true
        }));
        let first = alloc_ok(&mut b, &mut store_b).unwrap();
        assert!(first > sent_a, "the first counter after the import must be new");
        // A reboot of B resumes at its own durable watermark, still above the floor.
        let (b2, _) = TxCounter::resume(store_b, true);
        assert!(b2.counter() > sent_a);
    }

    /// A floor the counter already passed, or one the durable watermark covers, writes nothing.
    #[test]
    fn raise_to_without_a_write_when_covered() {
        let (mut txc, _) = TxCounter::resume(Some(5000), true);
        let no_write = |_| panic!("no persist expected");
        assert!(txc.raise_to(0, no_write));
        assert!(txc.raise_to(4000, no_write));
        assert_eq!(txc.counter(), 5000, "never lowered");
        assert!(txc.raise_to(5000 + RESERVE - 1, no_write));
        assert_eq!(txc.counter(), 5000 + RESERVE - 1);
        assert!(!txc.locked());
    }

    /// A failed persist of the raised watermark locks TX and reports the floor as NOT durable
    /// (the caller must not install the key: a reboot would resume below it).
    #[test]
    fn raise_to_persist_failure_locks_and_refuses() {
        let (mut txc, _) = TxCounter::resume(None, false);
        assert!(!txc.raise_to(1_000_000, |_| false));
        assert!(txc.locked());
        assert_eq!(txc.alloc(), None);
        assert_eq!(
            txc.reserve_limit(),
            1 + RESERVE,
            "the durable watermark did not move"
        );
    }

    /// While locked, the floor counts as covered only when the durable watermark (where the next
    /// boot resumes) is at or above it.
    #[test]
    fn raise_to_while_locked_reports_the_durable_watermark() {
        let (mut txc, _) = TxCounter::resume(Some(3000), true);
        txc.lock();
        let no_write = |_| panic!("locked: nothing is written");
        assert!(txc.raise_to(3000 + RESERVE, no_write));
        assert!(!txc.raise_to(3001 + RESERVE, no_write));
        let (mut lost, _) = TxCounter::resume(None, true);
        assert!(
            lost.raise_to(u32::MAX - 1, no_write),
            "lost: locked for good anyway"
        );
    }

    /// A raise to the ceiling persists the ceiling and locks: nothing is ever sent under that
    /// history again, and the next boot (resuming at the stored MAX) stays locked too.
    #[test]
    fn raise_to_ceiling_locks_durably() {
        let (mut txc, _) = TxCounter::resume(None, false);
        let mut stored = None;
        assert!(txc.raise_to(u32::MAX, |w| {
            stored = Some(w);
            true
        }));
        assert_eq!(stored, Some(u32::MAX));
        assert!(txc.locked());
        assert_eq!(txc.alloc(), None);
        let (mut next, _) = TxCounter::resume(stored, true);
        assert_eq!(next.alloc(), None);
    }

    /// A near-MAX stored watermark must SATURATE the reserve limit at u32::MAX (fixed
    /// 2026-07-05; previously `wrapping_add` could persist a LOW watermark here, and the boot
    /// after that would resume low and reuse CCM nonces 1..old). Reachable only after ~2³²
    /// transfers, but the fail-closed guarantee now holds at the very edge too: the counter
    /// drains the final saturated block and runs into `advance`'s ceiling lock.
    #[test]
    fn near_max_resume_saturates_reserve_limit_and_ends_locked() {
        let (mut txc, persist) = TxCounter::resume(Some(u32::MAX - 100), true);
        assert_eq!(txc.counter(), u32::MAX - 100);
        assert_eq!(txc.reserve_limit(), u32::MAX);
        assert_eq!(
            persist,
            Some(u32::MAX),
            "the boot-persisted watermark must never be low"
        );
        assert!(!txc.locked());
        // Drain the final saturated block: no watermark extension is ever requested (the
        // limit already sits at the ceiling), and the ceiling locks TX, never a wrap.
        for _ in 0..200 {
            assert!(txc.advance().is_none(), "no extension past a saturated limit");
            if txc.locked() {
                break;
            }
        }
        assert!(
            txc.locked(),
            "the ceiling must lock TX: the link ends closed, never wraps"
        );
        assert_eq!(txc.counter(), u32::MAX);
    }
}
