//! ACK acceptance / delivery resolution for the sender's ACK window (moved from
//! `firmware/src/radio/net/mod.rs::await_ack`; firmware/docs/radio.md).
//!
//! The sender of a confirmed frame opens an ACK window and offers every frame it hears to this
//! kernel; the kernel decides which single frame resolves the delivery. Everything else (a
//! foreign/undecodable frame, a non-ACK, an ACK from/to the wrong device, an ACK for a
//! *different*, stale/duplicate counter) is **ignored**, i.e. the caller keeps listening for
//! the rest of the window rather than treating it as a failed delivery. Once resolved, further
//! (duplicate) ACKs are ignored too: a delivery resolves at most once.

/// ACK payload length: `acked(4 LE) ‖ rssi(1, i8) ‖ flags(1)`. Every ACK is exactly this long; a
/// payload of any other length is not an ACK (a layout change then fails loudly as "no ACK"
/// instead of being half-read).
pub const ACK_LEN: usize = 6;

/// ACK payload flags byte (byte 5).
pub mod ack_flags {
    /// The ACKer (a gateway) holds a queued downlink for the ACKed sender, who
    /// should keep its radio in RX for a short window instead of going to sleep.
    pub const PENDING: u8 = 1 << 0;
}

/// Metadata parsed out of an ACK payload: the receiver-side RSSI report and the
/// pending-downlink flag (layout: [`ACK_LEN`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AckMeta {
    /// RSSI at which the receiver heard our frame, as it packed it (clamped i8 dBm).
    pub rssi: i8,
    /// The ACKer holds a queued downlink for us ([`ack_flags::PENDING`]).
    pub pending: bool,
}

/// Parse the metadata bytes of an ACK payload (pure; the caller has already matched the ACK
/// via [`AckWait::offer`], so `payload` is the accepted ACK's). `None` for a payload that is not
/// [`ACK_LEN`] long, which [`AckWait::offer`] never resolves.
#[must_use]
pub fn ack_meta(payload: &[u8]) -> Option<AckMeta> {
    let &[_, _, _, _, rssi, flags] = payload else {
        return None;
    };
    Some(AckMeta {
        rssi: rssi as i8,
        pending: flags & ack_flags::PENDING != 0,
    })
}

/// The verdict for one frame heard inside the ACK window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckVerdict {
    /// This is our ACK: the confirmed transfer is delivered (exactly once).
    Resolved,
    /// Not our ACK (foreign, non-ACK, wrong peer/dest, stale counter, or already resolved);
    /// keep waiting for the rest of the window.
    Ignore,
}

/// One pending confirmed delivery: who must acknowledge what, and whether it already resolved.
#[derive(Debug, Clone, Copy)]
pub struct AckWait {
    /// Our own address: the ACK must be addressed to us.
    me: u32,
    /// The peer we sent to: the ACK must come from it.
    peer: u32,
    /// The TX counter of the frame awaiting acknowledgement.
    counter: u32,
    /// Delivery already resolved: any further ACK copy is a duplicate (ignored).
    resolved: bool,
}

impl AckWait {
    /// Start waiting for `peer`'s ACK of our frame `counter` (we are `me`).
    #[must_use]
    pub fn new(me: u32, peer: u32, counter: u32) -> Self {
        Self {
            me,
            peer,
            counter,
            resolved: false,
        }
    }

    /// Offer one authenticated frame heard in the window: its type (`is_ack`), clear-header
    /// `src`/`dest`, and decrypted payload.
    ///
    /// ACK payload: exactly [`ACK_LEN`] bytes, the acked counter first; any other length never
    /// resolves. A valid ACK for a *different* counter (a stale/duplicate ACK) also isn't ours;
    /// keep waiting for the right one.
    pub fn offer(&mut self, is_ack: bool, src: u32, dest: u32, payload: &[u8]) -> AckVerdict {
        if self.resolved {
            return AckVerdict::Ignore; // a delivery resolves at most once
        }
        if !is_ack || src != self.peer || dest != self.me {
            return AckVerdict::Ignore;
        }
        match payload {
            &[a, b, c, d, _, _] if u32::from_le_bytes([a, b, c, d]) == self.counter => {
                self.resolved = true;
                AckVerdict::Resolved
            }
            _ => AckVerdict::Ignore,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 0x1111_1111;
    const PEER: u32 = 0x2222_2222;

    /// A well-formed ACK payload: acked counter (4 LE) + rssi (1) + flags (1, none set).
    fn ack_payload(counter: u32, rssi: u8) -> [u8; ACK_LEN] {
        let mut p = [0u8; ACK_LEN];
        p[..4].copy_from_slice(&counter.to_le_bytes());
        p[4] = rssi;
        p
    }

    /// The matching ACK resolves the delivery.
    #[test]
    fn matching_ack_resolves() {
        let mut w = AckWait::new(ME, PEER, 42);
        assert_eq!(
            w.offer(true, PEER, ME, &ack_payload(42, 0xC8)),
            AckVerdict::Resolved
        );
    }

    /// A duplicate ACK after resolution is ignored; the delivery never double-resolves.
    #[test]
    fn duplicate_ack_does_not_double_resolve() {
        let mut w = AckWait::new(ME, PEER, 42);
        assert_eq!(
            w.offer(true, PEER, ME, &ack_payload(42, 0xC8)),
            AckVerdict::Resolved
        );
        assert_eq!(
            w.offer(true, PEER, ME, &ack_payload(42, 0xC8)),
            AckVerdict::Ignore
        );
        assert_eq!(
            w.offer(true, PEER, ME, &ack_payload(42, 0x10)),
            AckVerdict::Ignore
        );
    }

    /// A late/stale ACK for an old counter is ignored; the window keeps waiting for the right
    /// one instead of failing (the star-contention false-NotDelivered fix, firmware/docs/radio.md).
    #[test]
    fn stale_counter_ignored_then_right_one_resolves() {
        let mut w = AckWait::new(ME, PEER, 42);
        assert_eq!(w.offer(true, PEER, ME, &ack_payload(41, 0)), AckVerdict::Ignore);
        assert_eq!(w.offer(true, PEER, ME, &ack_payload(7, 0)), AckVerdict::Ignore);
        assert_eq!(w.offer(true, PEER, ME, &ack_payload(42, 0)), AckVerdict::Resolved);
    }

    /// Non-ACK frames are ignored, whoever they're from (a neighbouring uplink in our window).
    #[test]
    fn non_ack_frames_ignored() {
        let mut w = AckWait::new(ME, PEER, 42);
        assert_eq!(w.offer(false, PEER, ME, &ack_payload(42, 0)), AckVerdict::Ignore);
        assert_eq!(w.offer(false, 0x3333_3333, ME, b"data"), AckVerdict::Ignore);
    }

    /// An ACK from the wrong peer, or addressed to someone else, is not ours.
    #[test]
    fn wrong_src_or_dest_ignored() {
        let mut w = AckWait::new(ME, PEER, 42);
        assert_eq!(
            w.offer(true, 0x3333_3333, ME, &ack_payload(42, 0)),
            AckVerdict::Ignore
        );
        assert_eq!(
            w.offer(true, PEER, 0x3333_3333, &ack_payload(42, 0)),
            AckVerdict::Ignore
        );
        // Even our own counter echoed by a third party doesn't resolve.
        assert_eq!(w.offer(true, PEER, ME, &ack_payload(42, 0)), AckVerdict::Resolved);
    }

    /// Only an exactly [`ACK_LEN`]-byte payload resolves, even with the right counter: a
    /// shorter one lacks the rssi / flags bytes, a longer one is some other layout.
    #[test]
    fn payload_of_any_other_length_ignored() {
        let mut long = [0u8; ACK_LEN + 3];
        long[..ACK_LEN].copy_from_slice(&ack_payload(42, 0));
        for len in (0..long.len()).filter(|&l| l != ACK_LEN) {
            let mut w = AckWait::new(ME, PEER, 42);
            assert_eq!(
                w.offer(true, PEER, ME, &long[..len]),
                AckVerdict::Ignore,
                "len {len}"
            );
            assert_eq!(ack_meta(&long[..len]), None, "len {len}");
        }
        let mut w = AckWait::new(ME, PEER, 42);
        assert_eq!(w.offer(true, PEER, ME, &long[..ACK_LEN]), AckVerdict::Resolved);
    }

    /// `ack_meta` reads the rssi and flags bytes; unknown flag bits never read as pending.
    #[test]
    fn ack_meta_fields() {
        let mut p = ack_payload(42, -67i8 as u8);
        assert_eq!(
            ack_meta(&p),
            Some(AckMeta {
                rssi: -67,
                pending: false
            })
        );
        p[5] = ack_flags::PENDING;
        assert_eq!(
            ack_meta(&p),
            Some(AckMeta {
                rssi: -67,
                pending: true
            })
        );
        p[5] = 0xFE; // every other bit
        assert_eq!(ack_meta(&p).map(|m| m.pending), Some(false));
    }

    /// Counter matching is exact over the full u32 range (LE decode of the payload).
    #[test]
    fn counter_match_is_exact_u32() {
        for counter in [0u32, 1, 0x8000_0000, u32::MAX - 1, u32::MAX] {
            let mut w = AckWait::new(ME, PEER, counter);
            assert_eq!(
                w.offer(true, PEER, ME, &ack_payload(counter.wrapping_add(1), 0)),
                AckVerdict::Ignore
            );
            assert_eq!(
                w.offer(true, PEER, ME, &ack_payload(counter, 0)),
                AckVerdict::Resolved
            );
        }
    }
}
