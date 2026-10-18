//! The device's console TX queue: a FIFO of **encoded** console payloads awaiting framing.
//!
//! A producer serializes its message with [`encode_payload`](crate::encode_payload) and
//! [`push`](PayloadRing::push)es the bytes; the console writer [`pop`](PayloadRing::pop)s the
//! oldest record and frames it with [`encode_frame_bytes`](crate::encode_frame_bytes), assigning
//! the `seq` only then (so a queue drop never consumes a sequence number). Records are stored at
//! their real size, `[type][len][payload]`, so the ring holds as many messages as their bytes
//! allow: a typical log line is ~50 B, where a queue of owned message structs spends a whole
//! largest-message slot (~280 B) on each. Hardware-free: the firmware wraps it in a critical
//! section and its wakers (`firmware/src/console.rs`).

use crate::{MAX_PAYLOAD, MsgType};

/// Bytes a record spends besides its payload: the message type and the payload length.
pub const RECORD_OVERHEAD: usize = 2;

// A record's length byte must hold any payload a frame can carry.
const _: () = assert!(MAX_PAYLOAD <= u8::MAX as usize);

/// A FIFO of `(MsgType, payload)` records in `N` bytes. Wraps around at the end of the buffer (a
/// record may straddle it), so no space is lost to padding.
pub struct PayloadRing<const N: usize> {
    buf: [u8; N],
    /// Index of the oldest record's first byte.
    head: usize,
    /// Bytes in use.
    len: usize,
}

impl<const N: usize> Default for PayloadRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> PayloadRing<N> {
    /// An empty ring. `N` must hold at least one record of the largest payload.
    pub const fn new() -> Self {
        const { assert!(N >= RECORD_OVERHEAD + MAX_PAYLOAD) };
        Self {
            buf: [0; N],
            head: 0,
            len: 0,
        }
    }

    /// Whether a record with a `payload_len`-byte payload fits now.
    pub fn fits(&self, payload_len: usize) -> bool {
        payload_len <= MAX_PAYLOAD && RECORD_OVERHEAD + payload_len <= N - self.len
    }

    /// Append one record. `false` (and nothing written) if it does not fit or the payload is
    /// longer than [`MAX_PAYLOAD`].
    pub fn push(&mut self, msg_type: MsgType, payload: &[u8]) -> bool {
        if !self.fits(payload.len()) {
            return false;
        }
        let at = (self.head + self.len) % N;
        self.write_at(at, &[msg_type as u8, payload.len() as u8]);
        self.write_at((at + RECORD_OVERHEAD) % N, payload);
        self.len += RECORD_OVERHEAD + payload.len();
        true
    }

    /// Remove the oldest record, copying its payload to the front of `out`; `None` when empty.
    /// `out` must hold [`MAX_PAYLOAD`] bytes (a shorter one gets the payload clipped).
    pub fn pop(&mut self, out: &mut [u8]) -> Option<(MsgType, usize)> {
        let (msg_type, n) = self.header()?;
        let take = n.min(out.len());
        self.read_at((self.head + RECORD_OVERHEAD) % N, &mut out[..take]);
        self.skip(n);
        Some((msg_type, take))
    }

    /// Whether no record is queued.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Discard every record; returns how many there were.
    pub fn clear(&mut self) -> u32 {
        let mut n = 0u32;
        while let Some((_, len)) = self.header() {
            self.skip(len);
            n += 1;
        }
        n
    }

    /// The oldest record's type and payload length.
    fn header(&mut self) -> Option<(MsgType, usize)> {
        if self.len == 0 {
            return None;
        }
        let mut hdr = [0u8; RECORD_OVERHEAD];
        self.read_at(self.head, &mut hdr);
        let Some(msg_type) = MsgType::from_u8(hdr[0]) else {
            // Only `push` writes records, always with a valid type: this is corrupt memory.
            // Empty the ring rather than desync every record after it, or stay stuck.
            (self.head, self.len) = (0, 0);
            return None;
        };
        Some((msg_type, hdr[1] as usize))
    }

    /// Drop the oldest record, whose payload is `payload_len` bytes.
    fn skip(&mut self, payload_len: usize) {
        let used = (RECORD_OVERHEAD + payload_len).min(self.len);
        self.head = (self.head + used) % N;
        self.len -= used;
        if self.len == 0 {
            self.head = 0; // keeps the next records contiguous for as long as possible
        }
    }

    fn write_at(&mut self, at: usize, bytes: &[u8]) {
        let first = bytes.len().min(N - at);
        self.buf[at..at + first].copy_from_slice(&bytes[..first]);
        self.buf[..bytes.len() - first].copy_from_slice(&bytes[first..]);
    }

    fn read_at(&self, at: usize, out: &mut [u8]) {
        let first = out.len().min(N - at);
        out[..first].copy_from_slice(&self.buf[at..at + first]);
        let rest = out.len() - first;
        out[first..].copy_from_slice(&self.buf[..rest]);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn payload(seed: u8, len: usize) -> [u8; MAX_PAYLOAD] {
        let mut p = [0u8; MAX_PAYLOAD];
        for (i, b) in p[..len].iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        p
    }

    #[test]
    fn records_come_out_in_order_with_their_type_and_bytes() {
        let mut r = PayloadRing::<512>::new();
        assert!(r.push(MsgType::Log, &payload(1, 40)[..40]));
        assert!(r.push(MsgType::Event, &payload(2, 0)[..0]));
        assert!(r.push(MsgType::Uplink, &payload(3, MAX_PAYLOAD)));
        let mut out = [0u8; MAX_PAYLOAD];
        assert_eq!(r.pop(&mut out), Some((MsgType::Log, 40)));
        assert_eq!(out[..40], payload(1, 40)[..40]);
        assert_eq!(r.pop(&mut out), Some((MsgType::Event, 0)));
        assert_eq!(r.pop(&mut out), Some((MsgType::Uplink, MAX_PAYLOAD)));
        assert_eq!(out, payload(3, MAX_PAYLOAD));
        assert_eq!(r.pop(&mut out), None);
        assert!(r.is_empty());
    }

    #[test]
    fn a_full_ring_refuses_whole_records_only() {
        let mut r = PayloadRing::<{ RECORD_OVERHEAD + MAX_PAYLOAD }>::new();
        assert!(r.push(MsgType::Print, &payload(0, 200)[..200]));
        // 47 bytes left: a 45-byte payload fits exactly, a 46-byte one does not.
        assert!(!r.fits(46));
        assert!(!r.push(MsgType::Print, &payload(0, 46)[..46]));
        assert!(r.push(MsgType::Print, &payload(9, 45)[..45]));
        assert!(!r.push(MsgType::Print, &[]));
        assert!(!r.fits(MAX_PAYLOAD + 1), "never more than a frame carries");
        let mut out = [0u8; MAX_PAYLOAD];
        assert_eq!(r.pop(&mut out), Some((MsgType::Print, 200)));
        assert_eq!(r.pop(&mut out), Some((MsgType::Print, 45)));
        assert_eq!(out[..45], payload(9, 45)[..45]);
    }

    /// Records of every size straddle the end of the buffer at every offset: a long run against a
    /// reference FIFO, the ring kept close to full.
    #[test]
    fn wraparound_matches_a_reference_fifo() {
        let mut r = PayloadRing::<301>::new(); // odd size: the seam lands everywhere
        let mut model: std::collections::VecDeque<(MsgType, std::vec::Vec<u8>)> = Default::default();
        let mut rng = 0x1234_5678u32;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        let types = [MsgType::Log, MsgType::Print, MsgType::Event, MsgType::Uplink];
        let mut out = [0u8; MAX_PAYLOAD];
        for step in 0..20_000u32 {
            if next() % 3 != 0 {
                let len = (next() % (MAX_PAYLOAD as u32 + 1)) as usize;
                let ty = types[(next() % 4) as usize];
                let p = payload(step as u8, len);
                let used: usize = model.iter().map(|(_, b)| RECORD_OVERHEAD + b.len()).sum();
                let fits = RECORD_OVERHEAD + len <= 301 - used;
                assert_eq!(r.push(ty, &p[..len]), fits, "step {step}");
                if fits {
                    model.push_back((ty, p[..len].to_vec()));
                }
            } else {
                let got = r.pop(&mut out).map(|(t, n)| (t, out[..n].to_vec()));
                assert_eq!(got, model.pop_front(), "step {step}");
            }
            assert_eq!(r.is_empty(), model.is_empty());
        }
        assert_eq!(r.clear() as usize, model.len());
        assert!(r.is_empty());
    }

    #[test]
    fn a_corrupt_record_empties_the_ring_instead_of_wedging_it() {
        let mut r = PayloadRing::<512>::new();
        assert!(r.push(MsgType::Log, &payload(1, 8)[..8]));
        assert!(r.push(MsgType::Log, &payload(2, 8)[..8]));
        r.buf[0] = 0xEE; // no such message type
        let mut out = [0u8; MAX_PAYLOAD];
        assert_eq!(r.pop(&mut out), None);
        assert!(r.is_empty());
        assert!(r.push(MsgType::Print, &payload(3, 4)[..4]));
        assert_eq!(r.pop(&mut out), Some((MsgType::Print, 4)));
    }

    #[test]
    fn a_short_out_buffer_clips_and_still_drops_the_whole_record() {
        let mut r = PayloadRing::<512>::new();
        assert!(r.push(MsgType::Log, &payload(5, 100)[..100]));
        assert!(r.push(MsgType::Print, &payload(6, 3)[..3]));
        let mut small = [0u8; 10];
        assert_eq!(r.pop(&mut small), Some((MsgType::Log, 10)));
        assert_eq!(small, payload(5, 10)[..10]);
        assert_eq!(r.pop(&mut small), Some((MsgType::Print, 3)));
    }
}
