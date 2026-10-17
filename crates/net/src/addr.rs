//! Radio-address validity rules (firmware/docs/radio.md "Reserved IDs"), shared by every path that
//! accepts an address from outside (OTA pairing, the gateway's cable `NodeAdd`, a node's
//! `Provision`, and the firmware's `Net::add_peer` itself), so they cannot disagree (before this, `NodeAdd`
//! refused `0` while OTA pairing accepted `0`, broadcast and the gateway's own address).

/// `0x0000_0000` ("unassigned / not yet known"): the JOIN_REQ `dest` before a joiner knows its
/// gateway. Never a device's own address.
pub const ADDR_UNASSIGNED: u32 = 0x0000_0000;
/// `0xFFFF_FFFF`: broadcast (FHSS beacons). Never a device's own address.
pub const ADDR_BROADCAST: u32 = 0xFFFF_FFFF;

/// Whether `addr` is one of the two reserved addresses.
#[must_use]
pub fn is_reserved_addr(addr: u32) -> bool {
    addr == ADDR_UNASSIGNED || addr == ADDR_BROADCAST
}

/// Whether `peer` may be bound as a peer of the device whose own address is `own`: not
/// reserved, and **not our own address**. The latter is a nonce-safety rule, not tidiness: the
/// CCM nonce is `src ‖ counter ‖ …` and the per-link key protects *both* directions, so the
/// directions are kept apart solely by `src`. A peer sharing our address would derive the very
/// same nonces as our own frames under the shared key whenever the counters coincide: a CCM
/// keystream/MAC reuse (firmware/docs/radio.md §Security model).
#[must_use]
pub fn peer_addr_valid(peer: u32, own: u32) -> bool {
    !is_reserved_addr(peer) && peer != own
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_addresses() {
        assert!(is_reserved_addr(0));
        assert!(is_reserved_addr(u32::MAX));
        assert!(!is_reserved_addr(1));
        assert!(!is_reserved_addr(0xFFFF_FFFE));
    }

    /// Reserved addresses and our own address are never valid peers; anything else is.
    #[test]
    fn peer_addr_rules() {
        let own = 0x1234_5678;
        assert!(!peer_addr_valid(ADDR_UNASSIGNED, own));
        assert!(!peer_addr_valid(ADDR_BROADCAST, own));
        assert!(
            !peer_addr_valid(own, own),
            "a peer at our own address collides nonces"
        );
        assert!(peer_addr_valid(0x1234_5679, own));
        assert!(peer_addr_valid(1, own));
    }
}
