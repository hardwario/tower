//! OTA-pairing admission rules (moved from `firmware/src/radio/net/pairing.rs`;
//! firmware/docs/radio.md): which JOIN_REQ the host may answer and which JOIN_RESP is the
//! joiner's. The pairing-v3 key agreement itself (X25519, the commitment round, the key
//! schedule, the confirmation tags, the SAS) is the [`handshake`](crate::handshake) kernel;
//! the TRNG draw, the windows and the JOIN frame exchange stay in the firmware's radio flow.
//!
//! The **keyed join** (E4, docs/radio.md) has its own three predicates below: a node whose
//! per-node key the gateway already holds (NodeAdd) joins under that key: no key on air,
//! both ends prove the key, and the node learns the gateway address.
//!
//! Two timing rules frame the handshake: the host answers a JOIN_REQ only while the whole
//! handshake still fits its window ([`handshake_fits`]), and the joiner treats a fresh key as
//! **provisional** until a confirmed uplink under it is acknowledged, its commit round
//! ([`commit_step`]).

use crate::addr::{is_reserved_addr, peer_addr_valid};
use crate::replay::ReplayVerdict;

/// Host side: may a JOIN_REQ that arrives `remaining_ms` before the pairing window closes still
/// be answered? Only when the whole handshake (`handshake_ms`: the JOIN_RESP, the JOIN_NONCE
/// wait, the shared secret, the JOIN_REVEAL and the JOIN_CONFIRM wait) fits before the window
/// ends. A handshake answered later would commit the node after the window closed, when the
/// operator's tooling may already have given up on it and reported a timeout: a node paired
/// behind the operator's back. The refused joiner just retries; with nothing left of the window
/// it times out like any unanswered attempt.
#[must_use]
pub fn handshake_fits(remaining_ms: u64, handshake_ms: u64) -> bool {
    remaining_ms >= handshake_ms
}

/// Commit attempts the joiner makes under a freshly derived key before it gives the key up
/// ([`commit_step`]).
pub const COMMIT_ATTEMPTS: u8 = 6;
/// The joiner's pause between the JOIN_CONFIRM and its first commit attempt: the gateway accepts
/// the confirm, writes its registry and closes its window before it serves uplinks again
/// (a few hundred ms without an EEPROM compaction), so a first attempt sent at once would mostly
/// hit a deaf gateway.
pub const COMMIT_SETTLE_MS: u32 = 500;
/// The joiner's pause between two commit attempts (each attempt is a confirmed send with its own
/// retransmissions).
pub const COMMIT_RETRY_MS: u32 = 2_000;
/// The longest a gateway can stay deaf to uplinks after it accepted a JOIN_CONFIRM: its registry
/// commit absorbing a synchronous EEPROM compaction (~5.2 s worst case,
/// firmware/docs/storage.md), the console response and the rest of its loop turn. The commit
/// round must outlast it (pinned by a test), or a node whose gateway did commit would give the
/// key up.
pub const GATEWAY_COMMIT_STALL_MS: u32 = 6_000;

/// What the joiner does with a provisional key after a commit attempt ([`commit_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStep {
    /// The gateway acknowledged an uplink under the key, so it holds the key too: persist it.
    Commit,
    /// Not acknowledged yet: try again after this pause.
    Retry { after_ms: u32 },
    /// Never acknowledged: the gateway did not commit (a lost or late JOIN_CONFIRM, a full or
    /// failing registry), so discard the key and stay unpaired, free to pair again.
    Discard,
}

/// Joiner side, the **commit round**: after commit attempt number `attempt` (1-based; one
/// confirmed uplink under the fresh key, with its retransmissions) that was `delivered` or not,
/// what next.
///
/// The handshake ends with the node's JOIN_CONFIRM, which nobody acknowledges: a lost or late
/// confirm leaves a node that holds a key its gateway never installed. A node that committed
/// (persisted) the key at that point would then believe it is paired while every uplink is
/// dropped as an unregistered sender, and an unpaired-only pairing gesture could never fix it.
/// So the commit point is the first **acknowledged** uplink under the new key: an ACK sealed
/// under that key proves the gateway installed it. Until then the key lives in RAM only; after
/// [`COMMIT_ATTEMPTS`] unacknowledged attempts, spaced so the round outlasts
/// [`GATEWAY_COMMIT_STALL_MS`], it is discarded. The residual asymmetry (the gateway committed but
/// every acknowledgement was lost) leaves a registered node that pairs again only after the
/// operator removes it from the gateway: OTA pairing never re-keys a registered address.
#[must_use]
pub fn commit_step(attempt: u8, delivered: bool) -> CommitStep {
    if delivered {
        CommitStep::Commit
    } else if attempt >= COMMIT_ATTEMPTS {
        CommitStep::Discard
    } else {
        CommitStep::Retry {
            after_ms: COMMIT_RETRY_MS,
        }
    }
}

/// Host side: may a JOIN_REQ from `joiner` (its header `src`) be answered at all? The joiner
/// must be a valid peer address (not reserved, not the host's `own`) and must **not already be
/// registered** (`registered`).
///
/// The last rule closes an OTA hijack: the key agreement is unauthenticated until someone
/// compares the SAS, so during an open window anyone can JOIN_REQ *with a victim's address* and
/// complete a handshake, and a host that accepted it would overwrite the victim's registry entry
/// and re-key its peer slot, locking the real node out. OTA pairing therefore only ever installs
/// a NEW address; re-pairing an existing node requires the operator to remove it first (the
/// gateway's `NodeRemove`), an authenticated host-side action.
#[must_use]
pub fn join_request_admissible(joiner: u32, own: u32, registered: bool) -> bool {
    peer_addr_valid(joiner, own) && !registered
}

/// Joiner side: is this JOIN_RESP addressed to **this** joiner (`joiner`), from a plausible host
/// (`src` not reserved, not ourselves)? Without the `dest` check, two nodes joining in the same
/// window could each adopt the response (and the key) meant for the other.
#[must_use]
pub fn join_response_for(dest: u32, src: u32, joiner: u32) -> bool {
    dest == joiner && !is_reserved_addr(src) && src != joiner
}

/// Keyed join, gateway side: a KEYED `JOIN_REQ` payload is the joiner's own address (4 LE),
/// which must equal the authenticated header `src` (the shape of the original public JOIN_REQ).
#[must_use]
pub fn keyed_req_valid(payload: &[u8], src: u32) -> bool {
    payload.len() >= 4 && u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) == src
}

/// Keyed join, gateway side: answer only a **registered** peer (its key opened the frame, not
/// the default key) whose JOIN_REQ counter is **fresh** on its replay lane. A replayed or
/// retransmitted request is dropped, so a recorded JOIN_REQ never draws a second response:
/// the lane is what authenticates the node's freshness to the gateway.
#[must_use]
pub fn keyed_req_admit(registered: bool, verdict: ReplayVerdict) -> bool {
    registered && verdict == ReplayVerdict::Fresh
}

/// Keyed join, joiner side: the KEYED `JOIN_RESP` payload echoes the JOIN_REQ's counter
/// (4 LE). The joiner's TX counter never repeats (reserve-ahead watermark), so a response
/// recorded from any earlier attempt, before or after a reboot, carries a stale echo and is
/// rejected: the gateway's freshness proof without a random nonce.
#[must_use]
pub fn keyed_resp_matches(payload: &[u8], req_counter: u32) -> bool {
    payload.len() >= 4 && u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) == req_counter
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- admission rules (finding #2) --------------------------------------------------------

    const HOST: u32 = 0x6000_0001;

    /// A new, valid joiner is admitted.
    #[test]
    fn join_request_new_node_admitted() {
        assert!(join_request_admissible(0xAABB_CCDD, HOST, false));
    }

    /// THE hijack: a JOIN_REQ for an already-registered address is refused (no JOIN_RESP, no
    /// re-key); the operator must remove the node first.
    #[test]
    fn join_request_for_registered_addr_refused() {
        assert!(!join_request_admissible(0xAABB_CCDD, HOST, true));
    }

    /// Reserved addresses and the host's own address can never pair.
    #[test]
    fn join_request_reserved_or_own_refused() {
        for bad in [0, u32::MAX, HOST] {
            assert!(!join_request_admissible(bad, HOST, false), "{bad:#x}");
        }
    }

    // --- timing rules -------------------------------------------------------------------------

    /// The host answers a JOIN_REQ only while the whole handshake fits the rest of its window
    /// (the boundary included: a handshake that ends exactly at the close still commits inside).
    #[test]
    fn handshake_must_fit_the_window() {
        const HS: u64 = 3_900;
        assert!(handshake_fits(60_000, HS));
        assert!(handshake_fits(HS, HS));
        assert!(!handshake_fits(HS - 1, HS));
        assert!(!handshake_fits(0, HS));
    }

    /// The commit round: an acknowledged attempt commits at once, at any attempt; unacknowledged
    /// attempts retry until the last, which discards the key.
    #[test]
    fn commit_round_commits_on_ack_and_discards_after_the_last_attempt() {
        for attempt in 1..=COMMIT_ATTEMPTS {
            assert_eq!(
                commit_step(attempt, true),
                CommitStep::Commit,
                "attempt {attempt}"
            );
        }
        for attempt in 1..COMMIT_ATTEMPTS {
            assert_eq!(
                commit_step(attempt, false),
                CommitStep::Retry {
                    after_ms: COMMIT_RETRY_MS
                },
                "attempt {attempt}"
            );
        }
        assert_eq!(commit_step(COMMIT_ATTEMPTS, false), CommitStep::Discard);
        // A caller that miscounts past the end still discards, never retries forever.
        assert_eq!(commit_step(COMMIT_ATTEMPTS + 1, false), CommitStep::Discard);
    }

    /// The commit round outlasts the gateway's worst post-confirm stall with room for lost
    /// frames: counting only the pauses (each attempt's own airtime and ACK windows only add to
    /// it), at least two attempts start after the gateway is listening again, so a node whose
    /// gateway did commit is not talked out of its key by one unlucky attempt.
    #[test]
    fn commit_round_outlasts_the_gateway_commit_stall() {
        let starts = (0..COMMIT_ATTEMPTS as u32).map(|k| COMMIT_SETTLE_MS + k * COMMIT_RETRY_MS);
        let after_stall = starts.filter(|&t| t > GATEWAY_COMMIT_STALL_MS).count();
        assert!(
            after_stall >= 2,
            "only {after_stall} commit attempt(s) after the stall"
        );
    }

    /// The joiner accepts only a JOIN_RESP addressed to itself, from a non-reserved host that
    /// isn't itself.
    #[test]
    fn join_response_dest_and_src_checked() {
        let me = 0xAABB_CCDD;
        assert!(join_response_for(me, HOST, me));
        assert!(
            !join_response_for(0x1111_1111, HOST, me),
            "another joiner's response"
        );
        assert!(!join_response_for(me, 0, me));
        assert!(!join_response_for(me, u32::MAX, me));
        assert!(!join_response_for(me, me, me));
    }

    /// Keyed JOIN_REQ: the payload address must be the authenticated src, and 4 bytes are
    /// required.
    #[test]
    fn keyed_req_payload_is_the_src() {
        assert!(keyed_req_valid(&0x0000_0413u32.to_le_bytes(), 0x0000_0413));
        assert!(!keyed_req_valid(&0x0000_0414u32.to_le_bytes(), 0x0000_0413));
        let full = 7u32.to_le_bytes();
        for len in 0..4 {
            assert!(!keyed_req_valid(&full[..len], 7), "len {len}");
        }
    }

    /// The gateway answers a registered peer's fresh request only.
    #[test]
    fn keyed_req_admits_registered_fresh_only() {
        assert!(keyed_req_admit(true, ReplayVerdict::Fresh));
        assert!(!keyed_req_admit(true, ReplayVerdict::Retransmit));
        assert!(!keyed_req_admit(true, ReplayVerdict::Replay));
        assert!(!keyed_req_admit(false, ReplayVerdict::Fresh));
    }

    /// The response must echo this attempt's request counter exactly; a response from an
    /// earlier attempt (any other counter) or a short payload is rejected.
    #[test]
    fn keyed_resp_echoes_the_request_counter() {
        assert!(keyed_resp_matches(&88u32.to_le_bytes(), 88));
        assert!(!keyed_resp_matches(&87u32.to_le_bytes(), 88));
        assert!(!keyed_resp_matches(&u32::MAX.to_le_bytes(), 0));
        let full = 88u32.to_le_bytes();
        for len in 0..4 {
            assert!(!keyed_resp_matches(&full[..len], 88), "len {len}");
        }
    }

    /// The keyed join composes with the key-checked lane restore (`replay::assign_lane`): a node
    /// re-provisioned with a NEW key and a RESTARTED counter (factory reset, then a cable
    /// `NodeAdd` under the new key) can keyed-join, also after a gateway reboot, while a restart
    /// under the UNCHANGED key is refused as stale (re-add it under a fresh key).
    #[test]
    fn keyed_join_after_rekey_with_restarted_counter() {
        use crate::replay::{LaneRecord, ReplayLane, assign_lane};
        const NODE: u32 = 0x0000_00BB;
        const KCV_OLD: u32 = 0x1111_1111;
        const KCV_NEW: u32 = 0x2222_2222;
        // The gateway has tracked NODE under the old key up to counter 5000 (persisted).
        let mut records = [None; 4];
        records[2] = Some(LaneRecord {
            addr: NODE,
            seen: 5000,
            kcv: KCV_OLD,
        });

        // Re-provisioned under a new key: the lane binds fresh at 0, so the node's first
        // request (counter 1, the TX counter restarts at 1 on a wiped EEPROM) is admitted, and
        // its echo check is independent of the lane.
        let bind = assign_lane(NODE, KCV_NEW, &records, 0).unwrap();
        assert!(bind.fresh && bind.seen == 0 && bind.index == 2);
        let mut lane = ReplayLane::new(bind.seen);
        assert!(keyed_req_admit(true, lane.classify(1)));
        let _ = lane.accept(1);
        assert!(keyed_resp_matches(&1u32.to_le_bytes(), 1));
        // The same request replayed is refused (the lane advanced).
        assert!(!keyed_req_admit(true, lane.classify(1)));

        // After a gateway reboot the lane record (now carrying the new key's KCV) is restored,
        // and the node's next request is still fresh.
        records[bind.index] = Some(LaneRecord {
            addr: NODE,
            seen: lane.last_seen(),
            kcv: KCV_NEW,
        });
        let bind = assign_lane(NODE, KCV_NEW, &records, 0).unwrap();
        assert!(!bind.fresh && bind.seen == 1);
        assert!(keyed_req_admit(true, ReplayLane::new(bind.seen).classify(2)));

        // Contrast: re-added under the OLD key, the restored lane (5000) refuses the restarted
        // counter as a replay. That is the documented rule: a node that restarts its counter
        // must come back under a fresh key.
        records[2] = Some(LaneRecord {
            addr: NODE,
            seen: 5000,
            kcv: KCV_OLD,
        });
        let bind = assign_lane(NODE, KCV_OLD, &records, 0).unwrap();
        assert!(!bind.fresh && bind.seen == 5000);
        assert!(!keyed_req_admit(true, ReplayLane::new(bind.seen).classify(1)));
    }
}
