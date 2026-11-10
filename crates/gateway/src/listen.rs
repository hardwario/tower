//! Immediate delivery to **listening** nodes (taf, mains powered, receiver always on): when to
//! try a node again after it did not take its downlink.
//!
//! The gateway sends a listening node's queued downlink at once instead of holding it for the
//! node's next uplink. A node that does not answer (unplugged, out of range) must neither hold up
//! the commands queued for the other listening nodes nor keep the radio busy with attempts that
//! fail: each one keeps the gateway out of receive for about half a second. So every node that
//! missed gets a retry time of its own, doubling per miss from [`FIRST_BACKOFF_MS`] up to
//! [`MAX_BACKOFF_MS`], and the gateway tries the first queued node that is due. A delivery, or an
//! uplink from the node (it is there), clears it; a node with nothing queued is forgotten. An
//! attempt the gateway could not transmit at all (its duty budget, a busy channel) says nothing
//! about the node: it waits the first pause, without counting a miss ([`held`](ListenBackoff::held)).
//!
//! On US 915 the gateway hops (FCC §15.247) and an attempt is spread over hop slots: a slot
//! carries one transmission, on that slot's channel, where a duty-mode attempt repeats within
//! its send. So a transmission that went unanswered is repeated in the next slot, and only the
//! last of [`SLOT_TRIES`] counts as the node's miss ([`slot_missed`](ListenBackoff::slot_missed)).
//!
//! Time is the caller's (`now_ms`): kernel style, host-testable.

use crate::queue::QUEUE_CAP;

/// The pause after a node's first missed attempt.
pub const FIRST_BACKOFF_MS: u64 = 2_000;
/// The longest pause between attempts (the queue's TTL ends them).
pub const MAX_BACKOFF_MS: u64 = 64_000;
/// Transmissions of one attempt while hopping (US 915), each in a hop slot of its own: as many as
/// a duty-mode attempt sends (the gateway's `DOWNLINK_REPS`).
pub const SLOT_TRIES: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    addr: u32,
    retry_at_ms: u64,
    misses: u8,
    /// Unanswered transmissions of the attempt under way while hopping ([`SLOT_TRIES`]).
    tries: u8,
}

/// The retry times of the listening nodes that missed their last attempt: at most one per queue
/// item, as a node without a queued item is forgotten ([`retain`](Self::retain)).
#[derive(Debug, Default)]
pub struct ListenBackoff {
    entries: [Option<Entry>; QUEUE_CAP],
}

impl ListenBackoff {
    pub const fn new() -> Self {
        Self {
            entries: [None; QUEUE_CAP],
        }
    }

    /// Whether `addr` may be tried at `now_ms`.
    pub fn due(&self, addr: u32, now_ms: u64) -> bool {
        self.find(addr).is_none_or(|e| now_ms >= e.retry_at_ms)
    }

    /// `addr` did not take its downlink at `now_ms`: its next attempt waits, longer per miss.
    pub fn missed(&mut self, addr: u32, now_ms: u64) {
        let misses = self.find(addr).map_or(0, |e| e.misses).saturating_add(1);
        let pause = (FIRST_BACKOFF_MS << (misses - 1).min(5)).min(MAX_BACKOFF_MS);
        self.put(Entry {
            addr,
            retry_at_ms: now_ms + pause,
            misses,
            tries: 0,
        });
    }

    /// `addr` did not answer a transmission in a hop slot at `now_ms` (US 915). The attempt goes
    /// on in the next slot (the node stays due) until [`SLOT_TRIES`] went unanswered: that is
    /// the attempt's [`missed`](Self::missed).
    pub fn slot_missed(&mut self, addr: u32, now_ms: u64) {
        let (misses, tries) = self.find(addr).map_or((0, 0), |e| (e.misses, e.tries));
        if tries + 1 >= SLOT_TRIES {
            self.missed(addr, now_ms);
        } else {
            self.put(Entry {
                addr,
                retry_at_ms: now_ms,
                misses,
                tries: tries + 1,
            });
        }
    }

    /// The gateway could not transmit `addr`'s downlink at `now_ms` (its duty budget, a busy
    /// channel): no news about the node, so no miss counted, but not again at once either.
    pub fn held(&mut self, addr: u32, now_ms: u64) {
        let (misses, tries) = self.find(addr).map_or((0, 0), |e| (e.misses, e.tries));
        self.put(Entry {
            addr,
            retry_at_ms: now_ms + FIRST_BACKOFF_MS,
            misses,
            tries,
        });
    }

    fn find(&self, addr: u32) -> Option<&Entry> {
        self.entries.iter().flatten().find(|e| e.addr == addr)
    }

    fn put(&mut self, entry: Entry) {
        let addr = entry.addr;
        let slot = self
            .entries
            .iter()
            .position(|e| e.is_some_and(|e| e.addr == addr))
            .or_else(|| self.entries.iter().position(Option::is_none))
            // More nodes than queue items cannot happen after `retain`; replace the soonest due.
            .unwrap_or_else(|| {
                (0..QUEUE_CAP)
                    .min_by_key(|&i| self.entries[i].map_or(0, |e| e.retry_at_ms))
                    .unwrap_or(0)
            });
        self.entries[slot] = Some(entry);
    }

    /// `addr` took a downlink, or was heard: try it at once next time.
    pub fn clear(&mut self, addr: u32) {
        for e in &mut self.entries {
            if e.is_some_and(|e| e.addr == addr) {
                *e = None;
            }
        }
    }

    /// Forget the nodes for which `queued` says nothing is queued any more.
    pub fn retain(&mut self, queued: impl Fn(u32) -> bool) {
        for e in &mut self.entries {
            if e.is_some_and(|e| !queued(e.addr)) {
                *e = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missed_node_waits_longer_per_miss_and_others_stay_due() {
        let mut b = ListenBackoff::new();
        assert!(b.due(1, 0) && b.due(2, 0));
        b.missed(1, 0);
        assert!(!b.due(1, FIRST_BACKOFF_MS - 1), "waits after a miss");
        assert!(b.due(1, FIRST_BACKOFF_MS));
        assert!(b.due(2, 0), "another node is not held up");
        b.missed(1, 10_000);
        assert!(!b.due(1, 10_000 + 2 * FIRST_BACKOFF_MS - 1), "the pause doubles");
        assert!(b.due(1, 10_000 + 2 * FIRST_BACKOFF_MS));
        for _ in 0..20 {
            b.missed(1, 100_000);
        }
        assert!(b.due(1, 100_000 + MAX_BACKOFF_MS), "capped");
        b.clear(1);
        assert!(b.due(1, 100_000), "a delivery or an uplink clears it");
    }

    #[test]
    fn an_attempt_not_transmitted_pauses_without_counting_a_miss() {
        let mut b = ListenBackoff::new();
        b.held(1, 0);
        assert!(!b.due(1, FIRST_BACKOFF_MS - 1), "not again at once");
        assert!(b.due(1, FIRST_BACKOFF_MS));
        for _ in 0..10 {
            b.held(1, 0);
        }
        assert!(b.due(1, FIRST_BACKOFF_MS), "no doubling");
        b.missed(1, 0);
        b.missed(1, 0);
        b.held(1, 100_000);
        b.missed(1, 200_000);
        assert!(
            !b.due(1, 200_000 + 4 * FIRST_BACKOFF_MS - 1),
            "the misses before it still count"
        );
        assert!(b.due(1, 200_000 + 4 * FIRST_BACKOFF_MS));
    }

    #[test]
    fn a_hop_slot_attempt_goes_on_in_the_next_slot_before_it_counts_as_a_miss() {
        let mut b = ListenBackoff::new();
        // The first unanswered transmission: again in the next slot, no pause.
        b.slot_missed(1, 1_000);
        assert!(b.due(1, 1_000), "the next slot");
        // The second: the attempt missed, the first pause.
        b.slot_missed(1, 1_300);
        assert!(!b.due(1, 1_300 + FIRST_BACKOFF_MS - 1));
        assert!(b.due(1, 1_300 + FIRST_BACKOFF_MS));
        // The next attempt has its own two transmissions, and its miss doubles the pause.
        b.slot_missed(1, 10_000);
        assert!(b.due(1, 10_000));
        b.slot_missed(1, 10_300);
        assert!(!b.due(1, 10_300 + 2 * FIRST_BACKOFF_MS - 1), "the pause doubles");
        assert!(b.due(1, 10_300 + 2 * FIRST_BACKOFF_MS));
        // A slot the gateway could not send in pauses the attempt and keeps its count.
        b.slot_missed(1, 20_000);
        b.held(1, 20_300);
        assert!(!b.due(1, 20_300 + FIRST_BACKOFF_MS - 1));
        b.slot_missed(1, 30_000);
        assert!(
            !b.due(1, 30_000 + 4 * FIRST_BACKOFF_MS - 1),
            "the attempt's second transmission: the node's third miss"
        );
        // A delivery, or an uplink from the node, starts over.
        b.clear(1);
        b.slot_missed(1, 50_000);
        assert!(b.due(1, 50_000));
        // Another node's attempts are its own.
        b.slot_missed(2, 50_000);
        b.slot_missed(1, 50_300);
        assert!(!b.due(1, 50_300) && b.due(2, 50_300));
    }

    #[test]
    fn nodes_without_a_queued_item_are_forgotten() {
        let mut b = ListenBackoff::new();
        for addr in 1..=QUEUE_CAP as u32 {
            b.missed(addr, 0);
        }
        b.retain(|addr| addr != 2);
        assert!(b.due(2, 0));
        assert!(!b.due(1, 0));
        // A fifth node still gets a slot (the freed one).
        b.missed(9, 0);
        assert!(!b.due(9, 0));
    }
}
