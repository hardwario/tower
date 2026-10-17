//! OTA-pairing JOIN_CONFIRM freshness/acceptance rule (moved from `firmware/src/radio/net/pairing.rs`;
//! firmware/docs/radio.md).
//!
//! The host mints a fresh per-session **challenge** in the JOIN_RESP that the joiner
//! must echo in the JOIN_CONFIRM. This is anti-*replay* within the window (a confirm
//! captured from a prior session carries a stale challenge and is rejected) on top of
//! CCM integrity; it does NOT add confidentiality or mutual auth (the challenge rides
//! the public-key frames in the clear-after-decrypt payload).
//!
//! Only this acceptance predicate is extracted; the challenge *minting* (the firmware's
//! non-cryptographic xorshift32 PRNG), the pairing windows/turnarounds and the JOIN frame
//! exchange stay in the firmware's radio flow.

use crate::addr::{is_reserved_addr, peer_addr_valid};

/// Host side: may a JOIN_REQ from `joiner` be answered at all (i.e. may the per-node key be
/// handed out in a JOIN_RESP)? The joiner must be a valid peer address (not reserved, not the
/// host's `own`), its header `src` must agree with the address it asks to pair (`hdr_src`), and
/// it must **not already be registered** (`registered`).
///
/// The last rule closes an OTA hijack: the pairing key is public, so during an open window
/// anyone can JOIN_REQ *with a victim's address*, decrypt the JOIN_RESP and confirm, and the
/// host used to overwrite the victim's registry entry and re-key its peer slot, locking the real
/// node out. OTA pairing therefore only ever installs a NEW address; re-pairing an existing node
/// requires the operator to remove it first (the gateway's `NodeRemove`), an authenticated
/// host-side action. Refusing *before* the JOIN_RESP also keeps the window's key away from the
/// would-be hijacker.
#[must_use]
pub fn join_request_admissible(joiner: u32, hdr_src: u32, own: u32, registered: bool) -> bool {
    hdr_src == joiner && peer_addr_valid(joiner, own) && !registered
}

/// Joiner side: is this JOIN_RESP addressed to **this** joiner (`joiner`), from a plausible host
/// (`src` not reserved, not ourselves)? Without the `dest` check, two nodes joining in the same
/// window could each adopt the response (and the key) meant for the other.
#[must_use]
pub fn join_response_for(dest: u32, src: u32, joiner: u32) -> bool {
    dest == joiner && !is_reserved_addr(src) && src != joiner
}

/// Validate a JOIN_CONFIRM payload against this session's expectations. The payload echoes
/// `addr` (4 LE) ‖ `challenge` (4 LE); commit only when both match, so a confirm replayed
/// from a prior session (stale challenge) is rejected.
#[must_use]
pub fn confirm_matches(payload: &[u8], addr: u32, challenge: u32) -> bool {
    payload.len() >= 8
        && u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) == addr
        && u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) == challenge
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirm(addr: u32, challenge: u32) -> [u8; 8] {
        let mut p = [0u8; 8];
        p[..4].copy_from_slice(&addr.to_le_bytes());
        p[4..].copy_from_slice(&challenge.to_le_bytes());
        p
    }

    /// The genuine confirm (correct id, this session's challenge) commits.
    #[test]
    fn genuine_confirm_accepted() {
        assert!(confirm_matches(
            &confirm(0xAABB_CCDD, 0x1234_5678),
            0xAABB_CCDD,
            0x1234_5678
        ));
    }

    /// A confirm replayed from a PRIOR session carries that session's (stale) challenge; the
    /// freshness rule rejects it even though the node id matches and the frame authenticated
    /// under the (public) pairing key.
    #[test]
    fn stale_challenge_rejected() {
        let old_session = confirm(0xAABB_CCDD, 0x1111_1111);
        assert!(!confirm_matches(&old_session, 0xAABB_CCDD, 0x2222_2222));
    }

    /// A confirm echoing the wrong node address (another joiner's confirm in the window) is not a
    /// commit for THIS exchange.
    #[test]
    fn wrong_addr_rejected() {
        assert!(!confirm_matches(
            &confirm(0x0000_0001, 0x1234_5678),
            0x0000_0002,
            0x1234_5678
        ));
    }

    /// Both must match; one right field is not enough.
    #[test]
    fn one_matching_field_is_not_enough() {
        assert!(!confirm_matches(&confirm(1, 1), 1, 2));
        assert!(!confirm_matches(&confirm(1, 1), 2, 1));
        assert!(!confirm_matches(&confirm(1, 1), 2, 2));
    }

    /// Short payloads (missing/truncated echo) never commit.
    #[test]
    fn short_payload_rejected() {
        let full = confirm(1, 2);
        for len in 0..8 {
            assert!(!confirm_matches(&full[..len], 1, 2), "len {len}");
        }
    }

    /// The rule is `len >= 8`: trailing extra bytes are tolerated (pin the current contract).
    #[test]
    fn longer_payload_tolerated() {
        let mut long = [0u8; 12];
        long[..8].copy_from_slice(&confirm(1, 2));
        assert!(confirm_matches(&long, 1, 2));
    }

    // --- admission rules (finding #2) --------------------------------------------------------

    const HOST: u32 = 0x6000_0001;

    /// A new, valid, self-consistent joiner is admitted.
    #[test]
    fn join_request_new_node_admitted() {
        assert!(join_request_admissible(0xAABB_CCDD, 0xAABB_CCDD, HOST, false));
    }

    /// THE hijack: a JOIN_REQ for an already-registered address is refused (no JOIN_RESP, no
    /// re-key); the operator must remove the node first.
    #[test]
    fn join_request_for_registered_addr_refused() {
        assert!(!join_request_admissible(0xAABB_CCDD, 0xAABB_CCDD, HOST, true));
    }

    /// Reserved addresses and the host's own address can never pair.
    #[test]
    fn join_request_reserved_or_own_refused() {
        for bad in [0, u32::MAX, HOST] {
            assert!(!join_request_admissible(bad, bad, HOST, false), "{bad:#x}");
        }
    }

    /// The header `src` must agree with the requested address.
    #[test]
    fn join_request_src_mismatch_refused() {
        assert!(!join_request_admissible(0xAABB_CCDD, 0x1111_1111, HOST, false));
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

    /// Exactness over the full u32 corners of both fields.
    #[test]
    fn u32_corners_exact() {
        for &id in &[0u32, 1, u32::MAX] {
            for &ch in &[0u32, 1, u32::MAX] {
                assert!(confirm_matches(&confirm(id, ch), id, ch));
                assert!(!confirm_matches(&confirm(id, ch), id, ch.wrapping_add(1)));
                assert!(!confirm_matches(&confirm(id, ch), id.wrapping_add(1), ch));
            }
        }
    }
}
