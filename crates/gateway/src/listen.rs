//! Immediate delivery to **listening** nodes (taf, mains powered, receiver always on): when to
//! try a node again after it did not take its downlink.
//!
//! The gateway sends a listening node's queued downlink at once instead of holding it for the
//! node's next uplink. A node that does not answer (unplugged, out of range) must neither hold up
//! the commands queued for the other listening nodes nor keep the radio busy with attempts that
//! fail: each one keeps the gateway out of receive for about half a second. So every node that
//! missed gets a retry time of its own, doubling per miss from [`FIRST_BACKOFF_MS`] up to
//! [`MAX_BACKOFF_MS`], and the gateway tries the first queued node that is due. A delivery, or an
//! uplink from the node (it is there), clears it; a node with nothing queued is forgotten.
//!
//! Time is the caller's (`now_ms`): kernel style, host-testable.

use crate::queue::QUEUE_CAP;

/// The pause after a node's first missed attempt.
pub const FIRST_BACKOFF_MS: u64 = 2_000;
/// The longest pause between attempts (the queue's TTL ends them).
pub const MAX_BACKOFF_MS: u64 = 64_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    addr: u32,
    retry_at_ms: u64,
    misses: u8,
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
        self.entries
            .iter()
            .flatten()
            .find(|e| e.addr == addr)
            .is_none_or(|e| now_ms >= e.retry_at_ms)
    }

    /// `addr` did not take its downlink at `now_ms`: its next attempt waits, longer per miss.
    pub fn missed(&mut self, addr: u32, now_ms: u64) {
        let misses = self
            .entries
            .iter()
            .flatten()
            .find(|e| e.addr == addr)
            .map_or(1, |e| e.misses.saturating_add(1));
        let pause = (FIRST_BACKOFF_MS << (misses - 1).min(5)).min(MAX_BACKOFF_MS);
        let entry = Entry {
            addr,
            retry_at_ms: now_ms + pause,
            misses,
        };
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
