//! OTA pairing: 3-way JOIN under the fixed public PAIRING_KEY (docs/radio.md). `impl Net`
//! block over [`super::Net`].
//!
//! The **joiner chooses its own address** and keeps it; the host only hands out the
//! per-node key; it does NOT assign or override the address. JOIN_REQ(addr) →
//! JOIN_RESP(key ‖ challenge) → JOIN_CONFIRM(addr ‖ challenge). Both sides commit
//! only after the confirm; a lost confirm leaves the host's window to time out (the
//! entry is never installed) and the joiner retries within its window.
//!
//! The host mints a fresh per-session **challenge** in the JOIN_RESP that the joiner
//! must echo in the JOIN_CONFIRM. This is anti-*replay* within the window (a confirm
//! captured from a prior session carries a stale challenge and is rejected) on top of
//! CCM integrity; it does NOT add confidentiality or mutual auth (the challenge rides
//! the public-key frames in the clear-after-decrypt payload).
//!
//! **OTA pairing only ever adds a NEW peer.** The host refuses (sends no JOIN_RESP to) a
//! JOIN_REQ for an address already in its peer table, a reserved address, or its own: with a
//! public pairing key anyone in range can JOIN_REQ *as a victim*, so answering would hand the
//! window's key to a hijacker and re-key the victim out of the network. Re-pairing a known node
//! therefore needs the operator to remove it first (an authenticated host-side action). The
//! joiner, in turn, only accepts a JOIN_RESP addressed to itself.
//!
//! **Keyed join (E4).** A node whose per-node key the host already holds (`add_peer`, e.g.
//! the gateway's NodeAdd) joins under that key instead: KEYED JOIN_REQ(addr), sealed under
//! the per-node key → KEYED JOIN_RESP(echo of the request's counter), sealed under the same
//! key. No key travels on air; the node proves the key and its freshness through the host's
//! replay lane, the host proves the key and its freshness through the echo; the node learns
//! the host's address. The host answers any time, with no window: the key is the
//! authorization. A stock host ignores it (dest 0 is not its address, and the frame does not
//! open under the pairing key), so the extension is additive.

use embassy_time::{Duration, Instant, Timer};
// The admission rules (who may be answered / which response is ours) and the confirm
// freshness rule (addr ‖ challenge echo) live in the host-testable `tower_net::pairing`
// kernel; the challenge minting (Net's PRNG), the windows and the JOIN frame exchange stay here.
use tower_net::pairing::{
    confirm_matches, join_request_admissible, join_response_for, keyed_req_admit, keyed_req_valid,
    keyed_resp_matches,
};

use super::{ACK_TURNAROUND, Net, TX_TIMEOUT, is_reserved_addr};
use crate::radio::duty;
use crate::radio::frame::{self, FrameType, Header, MAX_FRAME, flags};

/// Fixed, **publicly-known** OTA-pairing key (docs/radio.md). It gives the JOIN
/// frames a uniform CCM format with integrity + in-session replay protection, but
/// NO confidentiality (a sniffer in range during the window recovers the
/// delivered per-node key) and NO mutual authentication. Mitigate with a short
/// window, proximity, reduced power, user-initiated pairing.
pub const PAIRING_KEY: [u8; 16] = *b"TOWER-PAIR-KEY!\0";
/// Default pairing window: the host listens (and the joiner retries) for this
/// long. One minute gives a person time to put both ends into pairing mode.
pub const PAIRING_WINDOW: Duration = Duration::from_secs(60);
/// How long the joiner waits for a JOIN_RESP after a JOIN_REQ.
const JOIN_RESP_WINDOW: Duration = Duration::from_millis(300);
/// How long the host waits for a JOIN_CONFIRM after a JOIN_RESP.
const JOIN_CONFIRM_WINDOW: Duration = Duration::from_millis(300);
/// One listen slice of the host's pairing window between JOIN_REQ polls.
const PAIR_RX_SLICE: Duration = Duration::from_millis(500);

impl Net {
    /// Host: open a pairing window for `timeout` (use [`PAIRING_WINDOW`] for the
    /// default minute). On the first valid JOIN_REQ, hand out the per-node `key`
    /// (JOIN_RESP) and wait for the JOIN_CONFIRM. Returns the joiner's **own,
    /// self-chosen** address on commit (the caller installs the peer with
    /// `add_peer(peer_addr, key)`), or `None` on timeout / lost confirm. The host does
    /// NOT assign the address. Pairs the first joiner only.
    ///
    /// A JOIN_REQ for an address that is **already a registered peer**, reserved, or our own
    /// is ignored (no JOIN_RESP: the key is never sent; see the module docs): OTA pairing
    /// never re-keys an existing node. Remove the node first to re-pair it.
    pub async fn open_pairing(&mut self, timeout: Duration, key: &[u8; 16]) -> Option<u32> {
        if self.txc.locked() {
            return None; // fail closed (nonce safety): pairing replies consume TX counters
        }
        let deadline = Instant::now().checked_add(timeout)?;
        let mut buf = [0u8; 24];
        let mut refusal_logged = false;
        while Instant::now() < deadline {
            let Some((hdr, plen)) = self.rx_pair(&PAIRING_KEY, PAIR_RX_SLICE, &mut buf).await else {
                continue;
            };
            if hdr.frame_type != FrameType::JoinReq || plen < 4 {
                continue;
            }
            let peer_addr = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            let registered = self.peer_slot(peer_addr).is_some();
            if !join_request_admissible(peer_addr, hdr.src, self.addr, registered) {
                if !refusal_logged {
                    // Once per call: a refused joiner retries every ~300 ms for its whole window.
                    refusal_logged = true;
                    log::warn!(
                        target: "net",
                        "pairing: JOIN_REQ for {:08X} refused ({})",
                        peer_addr,
                        if registered {
                            "already registered - remove it first to re-pair"
                        } else {
                            "invalid address"
                        }
                    );
                }
                continue;
            }
            let challenge = self.rand_u32(); // fresh per session; the confirm must echo it

            // JOIN_RESP: the per-node key ‖ a fresh challenge (the joiner keeps its own address).
            let mut resp = [0u8; 20];
            resp[..16].copy_from_slice(key);
            resp[16..20].copy_from_slice(&challenge.to_le_bytes());
            // Counter allocated (consumed) before the turnaround/TX awaits; see `alloc_counter`.
            let counter = self.alloc_counter()?; // None = TX locked: fail closed
            let resp_hdr = Header {
                frame_type: FrameType::JoinResp,
                flags: 0,
                src: self.addr,
                dest: peer_addr,
                counter,
                bulk_index: None,
            };
            Timer::after(ACK_TURNAROUND).await;
            self.tx_pair(&PAIRING_KEY, &resp_hdr, &resp).await;

            // Wait for the JOIN_CONFIRM (echoes addr ‖ challenge); the freshness rule
            // (commit only when both match, so a confirm replayed from a prior session carries
            // a stale challenge and is rejected) is the kernel's `confirm_matches`. The confirm
            // must also come from that joiner and be addressed to us.
            let mut cbuf = [0u8; 8];
            if let Some((chdr, cplen)) = self.rx_pair(&PAIRING_KEY, JOIN_CONFIRM_WINDOW, &mut cbuf).await
                && chdr.frame_type == FrameType::JoinConfirm
                && chdr.src == peer_addr
                && chdr.dest == self.addr
                && confirm_matches(&cbuf[..cplen], peer_addr, challenge)
            {
                return Some(peer_addr);
            }
            // Lost confirm: discard, keep the window open for a retry.
        }
        None
    }

    /// Joiner: request pairing using `addr` (the joiner's **own** address, which it
    /// keeps) for up to `timeout` ([`PAIRING_WINDOW`] for the default). Sends
    /// JOIN_REQ, waits for the JOIN_RESP (the per-node key), sends JOIN_CONFIRM,
    /// and returns `(gw_addr, key)` on commit (or `None` on timeout); the gateway
    /// address is what a product node persists so it knows *which* gateway to talk to
    /// after a reboot.
    ///
    /// Only a JOIN_RESP addressed to `addr` (from a non-reserved host) is accepted: with two
    /// nodes joining in one window, each must not adopt the other's key. A reserved `addr`
    /// (`0` / broadcast) can never join.
    pub async fn join(&mut self, addr: u32, timeout: Duration) -> Option<(u32, [u8; 16])> {
        if self.txc.locked() {
            return None; // fail closed (nonce safety): JOIN_REQ/CONFIRM consume TX counters
        }
        if is_reserved_addr(addr) {
            return None; // a host refuses it anyway; don't advertise it on air
        }
        let deadline = Instant::now().checked_add(timeout)?;
        while Instant::now() < deadline {
            // Each JOIN_REQ consumes a fresh counter, allocated before its TX await.
            let counter = self.alloc_counter()?; // None = TX locked: fail closed
            let req_hdr = Header {
                frame_type: FrameType::JoinReq,
                flags: 0,
                src: addr,
                dest: 0, // gateway address not yet known
                counter,
                bulk_index: None,
            };
            self.tx_pair(&PAIRING_KEY, &req_hdr, &addr.to_le_bytes()).await;

            let mut buf = [0u8; 24];
            if let Some((hdr, plen)) = self.rx_pair(&PAIRING_KEY, JOIN_RESP_WINDOW, &mut buf).await
                && hdr.frame_type == FrameType::JoinResp
                && plen >= 20
                && join_response_for(hdr.dest, hdr.src, addr)
            {
                let mut key = [0u8; 16];
                key.copy_from_slice(&buf[..16]);
                // Confirm: echo addr ‖ the host's challenge, then commit.
                let mut conf = [0u8; 8];
                conf[..4].copy_from_slice(&addr.to_le_bytes());
                conf[4..8].copy_from_slice(&buf[16..20]);
                let counter = self.alloc_counter()?; // before the turnaround/TX awaits
                let conf_hdr = Header {
                    frame_type: FrameType::JoinConfirm,
                    flags: 0,
                    src: addr,
                    dest: hdr.src,
                    counter,
                    bulk_index: None,
                };
                Timer::after(ACK_TURNAROUND).await;
                self.tx_pair(&PAIRING_KEY, &conf_hdr, &conf).await;
                return Some((hdr.src, key));
            }
        }
        None
    }

    /// Joiner, keyed (E4): join a host that already holds this node's `key`, using `addr` (the
    /// node's own address) for up to `timeout`. Sends a KEYED JOIN_REQ under `key` and waits
    /// for the KEYED JOIN_RESP echoing that request's counter; returns the host's address on a
    /// match (or `None` on timeout). Nothing is committed here: the caller already has the key
    /// and installs the host with `add_peer(gw_addr, key)`. Each attempt uses a fresh TX
    /// counter, which is what makes the echo a freshness proof (`keyed_resp_matches`).
    pub async fn join_keyed(&mut self, addr: u32, key: &[u8; 16], timeout: Duration) -> Option<u32> {
        if self.txc.locked() {
            return None; // fail closed (nonce safety): JOIN_REQ consumes TX counters
        }
        let deadline = Instant::now().checked_add(timeout)?;
        while Instant::now() < deadline {
            let req_counter = self.txc.counter();
            let req_hdr = Header {
                frame_type: FrameType::JoinReq,
                flags: flags::KEYED,
                src: addr,
                dest: 0, // gateway address not yet known
                counter: req_counter,
                bulk_index: None,
            };
            let sent = self.tx_pair(key, &req_hdr, &addr.to_le_bytes()).await;
            let _ = self.alloc_counter();
            if !sent {
                // Duty-limited or a radio error: back off one response window, don't spin.
                Timer::after(JOIN_RESP_WINDOW).await;
                continue;
            }

            // Listen out the whole response window: a foreign frame (another node's uplink,
            // not openable under our key) must not end it early (same rule as await_ack).
            let resp_end = Instant::now() + JOIN_RESP_WINDOW;
            loop {
                let remaining = resp_end.saturating_duration_since(Instant::now());
                if remaining.as_ticks() == 0 {
                    break;
                }
                let mut buf = [0u8; 8];
                if let Some((hdr, plen)) = self.rx_pair(key, remaining, &mut buf).await
                    && hdr.frame_type == FrameType::JoinResp
                    && hdr.flags & flags::KEYED != 0
                    && hdr.dest == addr
                    && keyed_resp_matches(&buf[..plen], req_counter)
                {
                    return Some(hdr.src);
                }
            }
        }
        None
    }

    /// Host, keyed (E4): answer a KEYED JOIN_REQ (`frame` = the whole received frame, dest 0)
    /// from a peer registered with `add_peer`. Opens it under that peer's key, admits it only
    /// when the counter is fresh on the peer's replay lane (and advances the lane), then sends
    /// the KEYED JOIN_RESP echoing the request's counter after `ACK_TURNAROUND`. Returns the
    /// joiner's address when a response went out. Called from [`Net::recv`]; nothing else
    /// changes: the peer, its key and its lane were already installed.
    pub(super) async fn answer_keyed_join(&mut self, frame: &mut [u8]) -> Option<u32> {
        if self.txc.locked() {
            return None; // fail closed (nonce safety): the JOIN_RESP consumes a TX counter
        }
        let (peek, _) = frame::Header::parse(frame).ok()?;
        let registered = self.peer_slot(peek.src).is_some();
        if !registered {
            return None; // never the default key: the per-node key is the authorization
        }
        let key = self.key_for(peek.src);
        let (hdr, range) = frame::open_frame(&mut self.ccm, &key, frame).ok()?;
        if !keyed_req_valid(&frame[range], hdr.src)
            || !keyed_req_admit(registered, self.lane(hdr.src).classify(hdr.counter))
        {
            return None;
        }
        self.lane_accept(hdr.src, hdr.counter);

        let resp_hdr = Header {
            frame_type: FrameType::JoinResp,
            flags: flags::KEYED,
            src: self.addr,
            dest: hdr.src,
            counter: self.txc.counter(),
            bulk_index: None,
        };
        Timer::after(ACK_TURNAROUND).await;
        let sent = self.tx_pair(&key, &resp_hdr, &hdr.counter.to_le_bytes()).await;
        let _ = self.alloc_counter();
        sent.then_some(hdr.src)
    }

    /// Seal `hdr`+`payload` under `key` (the pairing key, or a per-node key for the keyed
    /// join) and transmit (duty-metered).
    async fn tx_pair(&mut self, key: &[u8; 16], hdr: &Header, payload: &[u8]) -> bool {
        let mut buf = [0u8; MAX_FRAME];
        let Ok(n) = frame::seal_frame(&mut self.ccm, key, hdr, payload, &mut buf) else {
            return false;
        };
        if !self.duty.try_tx(duty::frame_toa_ms(n)) {
            return false;
        }
        self.radio.tx(&buf[..n], false, TX_TIMEOUT).await.is_ok()
    }

    /// Receive + CCM-open a frame under `key`; copy plaintext to `out`.
    async fn rx_pair(
        &mut self,
        key: &[u8; 16],
        timeout: Duration,
        out: &mut [u8],
    ) -> Option<(Header, usize)> {
        let mut buf = [0u8; MAX_FRAME];
        let (len, _) = self.radio.rx(&mut buf, timeout).await.ok()?;
        let (hdr, range) = frame::open_frame(&mut self.ccm, key, &mut buf[..len]).ok()?;
        let plen = range.end - range.start;
        if plen > out.len() {
            return None;
        }
        out[..plen].copy_from_slice(&buf[range]);
        Some((hdr, plen))
    }
}
