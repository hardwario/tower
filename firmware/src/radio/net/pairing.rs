//! OTA pairing (v2): an X25519 key agreement between a joining node and a host with an open
//! pairing window (docs/radio.md "OTA pairing"), plus the keyed join (E4) for nodes whose key
//! the host already holds. `impl Net` block over [`super::Net`].
//!
//! ```text
//! node  JOIN_REQ      2 ‖ E_node            src = node, dest = 0     under JOIN_FRAME_KEY
//! host  JOIN_RESP     2 ‖ E_gw ‖ tag_gw     src = host, dest = node  under JOIN_FRAME_KEY
//! node  JOIN_CONFIRM  2 ‖ tag_node          src = node, dest = host  under the NEW link key
//! ```
//!
//! Both ends derive the per-node link key from the X25519 shared secret (the
//! `tower_net::handshake` kernel: HKDF-SHA256 over both public keys and both addresses), so no
//! key travels on air and a passive eavesdropper learns nothing. The host proves the key with
//! `tag_gw`, which the node checks before it confirms; the node proves it with `tag_node`,
//! inside a frame sealed under the link key; the host commits only after a valid confirm. Both
//! ends log the same 6-digit SAS (`pairing: SAS 123456`) for the optional out-of-band check
//! against an active man in the middle during the window.
//!
//! [`JOIN_FRAME_KEY`] is public: it only gives the two clear-key frames a CCM format, so a
//! corrupted frame fails its tag and is dropped. Secrecy and authentication come from the key
//! agreement and the confirmation tags.
//!
//! Ephemerals come from the hardware TRNG only (`board::trng_fill`: refused, never drawn from
//! the software fallback), one per JOIN_REQ sent or answered, zeroized after use. Each side's
//! own public key is computed before it is needed (the host's when the window opens and again
//! after each answered request, the node's before it transmits), so only one scalar
//! multiplication (the shared secret) sits on each critical path, and the peer's wait covers it
//! ([`PEER_KEX_TIME`]).
//!
//! The **joiner chooses its own address** and keeps it; the host does NOT assign or override
//! it. Both sides commit only on success; a lost confirm leaves the host's window open (the
//! entry is never installed).
//!
//! **OTA pairing only ever adds a NEW peer.** The host refuses (sends no JOIN_RESP to) a
//! JOIN_REQ from an address already in its peer table, a reserved address, or its own: the key
//! agreement is unauthenticated until the SAS is compared, so anyone in range can JOIN_REQ *as a
//! victim*, and answering would let a hijacker re-key the victim out of the network. Re-pairing
//! a known node therefore needs the operator to remove it first (an authenticated host-side
//! action). The joiner, in turn, only accepts a JOIN_RESP addressed to itself.
//!
//! **Keyed join (E4).** A node whose per-node key the host already holds (`add_peer`, e.g.
//! the gateway's NodeAdd) joins under that key instead: KEYED JOIN_REQ(addr), sealed under
//! the per-node key → KEYED JOIN_RESP(echo of the request's counter), sealed under the same
//! key. No key travels on air; the node proves the key and its freshness through the host's
//! replay lane, the host proves the key and its freshness through the echo; the node learns
//! the host's address. The host answers any time, with no window: the key is the
//! authorization. A host without the key ignores it (dest 0 is not its address, and the frame
//! does not open under the framing key or any key it holds).

use embassy_time::{Duration, Instant, Timer};
// The pairing-v2 handshake (key schedule, confirmation tags, SAS) and the admission rules (who
// may be answered / which response is ours) live in the host-testable `tower_net` kernels; the
// TRNG draw, the windows and the JOIN frame exchange stay here.
use tower_net::handshake::{
    CONFIRM_LEN, ENTROPY_LEN, Ephemeral, PUBLIC_LEN, REQ_LEN, RESP_LEN, Role, Session, VERSION,
};
use tower_net::pairing::{
    join_request_admissible, join_response_for, keyed_req_admit, keyed_req_valid, keyed_resp_matches,
};
use zeroize::Zeroize;

use super::{ACK_TURNAROUND, Access, Net, TX_TIMEOUT, is_reserved_addr};
use crate::radio::ccm::TAG_LEN;
use crate::radio::duty;
use crate::radio::frame::{self, FrameType, Header, MAX_FRAME, flags};

/// Public CCM framing key of the v2 JOIN_REQ / JOIN_RESP (docs/radio.md). **Not a secret** and
/// never used for anything else: it gives the two frames that precede the link key a uniform
/// CCM format, so a corrupted frame fails its tag and is dropped. The key agreement's secrecy
/// and authentication do not depend on it.
pub const JOIN_FRAME_KEY: [u8; 16] = *b"TOWER-JOIN-v2\0\0\0";
/// Default pairing window: the host listens (and the joiner retries) for this
/// long. One minute gives a person time to put both ends into pairing mode.
pub const PAIRING_WINDOW: Duration = Duration::from_secs(60);
/// Radio part of the joiner's wait for a JOIN_RESP after a JOIN_REQ (the host's turnaround,
/// the response's time on air, RX set-up, margin). The keyed join waits exactly this; the
/// pairing handshake adds [`PEER_KEX_TIME`].
const JOIN_RESP_WINDOW: Duration = Duration::from_millis(300);
/// Radio part of the host's wait for a JOIN_CONFIRM after its JOIN_RESP (plus
/// [`PEER_KEX_TIME`]).
const JOIN_CONFIRM_WINDOW: Duration = Duration::from_millis(300);
/// The peer's computation inside each pairing wait: one X25519 scalar multiplication plus the
/// HKDF / HMAC key schedule, on a Cortex-M0+ at 16 MHz (HSI16, no wait states). Estimate,
/// not a bench measurement: the curve25519-dalek u32 ladder executes ~12.0 M instructions
/// (counted in an instruction-accurate emulator; v6-M has no 32x32→64 multiply, so every limb
/// product is a library call), i.e. 0.9-1.2 s at a CPI of 1.2-1.6, plus ~0.01 s for HKDF and two
/// HMACs. 1.5 s leaves ~25 % over the slow end. Err long: too short fails every pairing (the
/// answer lands after the wait closed), too long only slows a retry. Re-tune from the
/// `crypto_x25519_kat` measurement on the board.
const PEER_KEX_TIME: Duration = Duration::from_millis(1500);
/// One listen slice of the host's pairing window between JOIN_REQ polls.
const PAIR_RX_SLICE: Duration = Duration::from_millis(500);

/// The host side of an open pairing window: the precomputed X25519 ephemeral for the next
/// JOIN_REQ it answers, so an answer only pays for the shared secret. Create it when the window
/// opens ([`prepare`](Self::prepare)), pass it to every [`open_pairing`](Net::open_pairing)
/// slice of that window, drop it when the window closes (the secret is zeroized). Each
/// ephemeral answers **one** JOIN_REQ and is then replaced: a key reused across the window
/// would give a man in the middle the whole window, instead of one response wait, to search for
/// a colliding SAS.
pub struct PairingHost {
    eph: Option<Ephemeral>,
    /// The TRNG failed once already (log the refusal once per window, not per slice).
    trng_failed: bool,
}

impl PairingHost {
    /// Draw the window's first ephemeral from the hardware TRNG (one X25519 scalar
    /// multiplication, about a second on the M0+). `None` (logged) when the TRNG is unavailable:
    /// pairing is refused rather than keyed from the software fallback.
    #[must_use]
    pub fn prepare() -> Option<Self> {
        Some(Self {
            eph: Some(trng_ephemeral(true)?),
            trng_failed: false,
        })
    }

    /// Make sure an ephemeral is ready (draw the next one after the last was used); `false`
    /// when the TRNG can't deliver.
    fn ready(&mut self) -> bool {
        if self.eph.is_none() {
            self.eph = trng_ephemeral(!self.trng_failed);
            self.trng_failed |= self.eph.is_none();
        }
        self.eph.is_some()
    }
}

/// A fresh X25519 ephemeral from the hardware TRNG ([`ENTROPY_LEN`] bytes, hashed into the
/// secret; one scalar multiplication for the public key). `None` when the TRNG cannot deliver:
/// pairing refuses rather than derive key material from the software fallback (logged when
/// `log`). Not inlined: the curve arithmetic's ~2.4 KB of stack must exist only while it runs,
/// never inside an async poll frame that every poll of the caller would carry.
#[inline(never)]
fn trng_ephemeral(log: bool) -> Option<Ephemeral> {
    let mut entropy = [0u8; ENTROPY_LEN];
    let e = crate::board::trng_fill(&mut entropy).then(|| Ephemeral::from_entropy(&entropy));
    entropy.zeroize();
    if e.is_none() && log {
        log::error!(
            target: "net",
            "pairing: hardware TRNG unavailable, refusing to pair (ephemeral keys must not come from the software fallback)"
        );
    }
    e
}

/// This side's half of the handshake: the X25519 shared secret (the one scalar multiplication on
/// the critical path), the key schedule and both tags. `None` for a low-order peer key. Not
/// inlined, for the same stack reason as [`trng_ephemeral`].
#[inline(never)]
fn handshake(own: &Ephemeral, peer: &[u8], role: Role, node_addr: u32, gw_addr: u32) -> Option<Session> {
    let mut e = [0u8; PUBLIC_LEN];
    e.copy_from_slice(peer);
    own.session(&e, role, node_addr, gw_addr)
}

/// Why a keyed join ([`Net::join_keyed`]) returned without a host address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    /// No host answered within the timeout: none in range holds this node's key with the node
    /// registered, the host refused (keyed joins switched off, a stale counter), or every
    /// attempt was duty-limited.
    Timeout,
    /// The radio runs LBT+AFA or FHSS ([`Access`]): the keyed join is a fixed-channel
    /// request/response with no listen-before-talk and no hop schedule, so it is refused there
    /// like a plain `send` ([`SendResult::WrongMode`](super::SendResult::WrongMode)).
    WrongMode,
    /// TX is locked (fail closed, nonce safety; `RadioError::NonceLocked`).
    NonceLocked,
    /// `addr` is reserved (`0` / broadcast) and can never join.
    BadAddr,
}

/// Why the host did not answer a keyed JOIN_REQ ([`Net::answer_keyed_join`]); logged by
/// [`Net::recv`], rate-limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyedRefusal {
    /// Switched off ([`Net::set_keyed_join`], the gateway's `keyed-join` setting).
    Off,
    /// LBT+AFA or FHSS mode (see [`JoinError::WrongMode`]).
    WrongMode,
    /// The src is not a registered peer (no per-node key to authenticate it with).
    Unregistered,
    /// The frame does not open under the src's registered key (a key mismatch, or a forgery).
    AuthFail,
    /// Opened, but the payload is not the src's own address.
    Malformed,
    /// The request counter is not fresh on the src's replay lane: a replayed request, or a node
    /// that restarted its counter under an unchanged key (re-add it under a new key).
    Stale { counter: u32, last_seen: u32 },
    /// TX is locked (fail closed, nonce safety).
    NonceLocked,
    /// The EU duty budget refused the response.
    DutyLimited,
    /// The radio failed to transmit the response.
    TxFailed,
}

impl core::fmt::Display for KeyedRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KeyedRefusal::Off => f.write_str("keyed joins are switched off"),
            KeyedRefusal::WrongMode => f.write_str("radio not in Duty mode"),
            KeyedRefusal::Unregistered => f.write_str("not a registered node"),
            KeyedRefusal::AuthFail => f.write_str("does not open under its registered key"),
            KeyedRefusal::Malformed => f.write_str("malformed request"),
            KeyedRefusal::Stale { counter, last_seen } => write!(
                f,
                "stale counter {counter} <= last-seen {last_seen} (replayed, or a counter restart \
                 under an unchanged key)"
            ),
            KeyedRefusal::NonceLocked => f.write_str("TX locked (nonce safety)"),
            KeyedRefusal::DutyLimited => f.write_str("duty-cycle limited"),
            KeyedRefusal::TxFailed => f.write_str("response TX failed"),
        }
    }
}

impl core::fmt::Display for JoinError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            JoinError::Timeout => "no host answered",
            JoinError::WrongMode => "wrong access mode (keyed join needs Duty mode)",
            JoinError::NonceLocked => "TX locked (nonce safety)",
            JoinError::BadAddr => "reserved address",
        })
    }
}

impl Net {
    /// Host: listen for a pairing JOIN_REQ for up to `timeout` (one slice of a window; use
    /// [`PAIRING_WINDOW`] for a single default minute), answering with the precomputed
    /// ephemeral of `host` (create it with [`PairingHost::prepare`] when the window opens and
    /// keep passing the same one). On the first JOIN_REQ that completes the handshake (a valid
    /// JOIN_CONFIRM under the derived key), logs the SAS and returns the joiner's **own,
    /// self-chosen** address and the derived **link key**: the caller installs the peer with
    /// `add_peer(peer_addr, &key)` (and persists it). `None` on timeout / a lost or invalid
    /// confirm. Pairs the first joiner only.
    ///
    /// A JOIN_REQ from an address that is **already a registered peer**, reserved, or our own
    /// is ignored (no JOIN_RESP; see the module docs): OTA pairing never re-keys an existing node.
    /// Remove the node first to re-pair it. Duty mode only (a fixed-channel exchange, like
    /// [`join_keyed`](Self::join_keyed)).
    pub async fn open_pairing(
        &mut self,
        host: &mut PairingHost,
        timeout: Duration,
    ) -> Option<(u32, [u8; 16])> {
        if self.txc.locked() {
            return None; // fail closed (nonce safety): pairing replies consume TX counters
        }
        let deadline = Instant::now().checked_add(timeout)?;
        if self.access != Access::Duty {
            log::warn!(target: "net", "pairing: refused, the radio is not in Duty mode");
            Timer::at(deadline).await;
            return None;
        }
        let mut buf = [0u8; REQ_LEN];
        let mut refusal_logged = false;
        while Instant::now() < deadline {
            if !host.ready() {
                Timer::at(deadline).await; // TRNG down: nothing to answer with (logged once)
                return None;
            }
            let Some((hdr, plen)) = self.rx_pair(&JOIN_FRAME_KEY, PAIR_RX_SLICE, &mut buf).await else {
                continue;
            };
            if hdr.frame_type != FrameType::JoinReq {
                continue;
            }
            let peer_addr = hdr.src;
            let registered = self.peer_slot(peer_addr).is_some();
            let refusal = if plen != REQ_LEN || buf[0] != VERSION {
                Some("not a pairing v2 request")
            } else if !join_request_admissible(peer_addr, self.addr, registered) {
                Some(if registered {
                    "already registered - remove it first to re-pair"
                } else {
                    "invalid address"
                })
            } else {
                None
            };
            if let Some(why) = refusal {
                if !refusal_logged {
                    // Once per call: a refused joiner retries for its whole window.
                    refusal_logged = true;
                    log::warn!(target: "net", "pairing: JOIN_REQ from {:08X} refused ({})", peer_addr, why);
                }
                continue;
            }

            // The handshake: one scalar multiplication (~1 s), then this ephemeral is spent.
            let Some(eph) = host.eph.take() else {
                continue; // unreachable: `ready()` above
            };
            let Some(session) = handshake(&eph, &buf[1..REQ_LEN], Role::Gateway, peer_addr, self.addr) else {
                log::warn!(target: "net", "pairing: JOIN_REQ from {:08X} refused (low-order key)", peer_addr);
                continue;
            };
            let mut resp = [0u8; RESP_LEN];
            resp[0] = VERSION;
            resp[1..1 + PUBLIC_LEN].copy_from_slice(eph.public());
            resp[1 + PUBLIC_LEN..].copy_from_slice(session.tag(Role::Gateway));
            drop(eph); // zeroize the secret before anything awaits

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
            self.tx_pair(&JOIN_FRAME_KEY, &resp_hdr, &resp).await;

            // Wait out the whole confirm window (the joiner computes its shared secret first): a
            // foreign frame, or one that does not open under the new link key, must not end it.
            // Commit only on the joiner's tag, from that joiner, addressed to us.
            let confirm_end = Instant::now() + JOIN_CONFIRM_WINDOW + PEER_KEX_TIME;
            loop {
                let remaining = confirm_end.saturating_duration_since(Instant::now());
                if remaining.as_ticks() == 0 {
                    break; // lost confirm: keep the window open for a retry
                }
                let mut cbuf = [0u8; CONFIRM_LEN];
                if let Some((chdr, cplen)) = self.rx_pair(session.link_key(), remaining, &mut cbuf).await
                    && chdr.frame_type == FrameType::JoinConfirm
                    && chdr.src == peer_addr
                    && chdr.dest == self.addr
                    && cplen == CONFIRM_LEN
                    && cbuf[0] == VERSION
                    && session.verify(Role::Node, &cbuf[1..])
                {
                    log::info!(target: "net", "pairing: SAS {:06}", session.sas());
                    return Some((peer_addr, *session.link_key()));
                }
            }
        }
        None
    }

    /// Joiner: pair with a host that has a window open, using `addr` (the joiner's **own**
    /// address, which it keeps) for up to `timeout` ([`PAIRING_WINDOW`] for the default).
    /// Each attempt draws a fresh ephemeral from the TRNG, sends the JOIN_REQ, checks the
    /// host's confirmation tag in the JOIN_RESP, and answers with the JOIN_CONFIRM under the
    /// derived key. Returns `(gw_addr, link_key)` on success (after logging the SAS), or `None`
    /// on timeout, when the TRNG is unavailable (logged; never keyed from the fallback), or
    /// outside Duty mode. The gateway address is what a product node persists so it knows
    /// *which* gateway to talk to after a reboot.
    ///
    /// Only a JOIN_RESP addressed to `addr` (from a non-reserved host) is accepted, and only
    /// with a valid tag: with two nodes joining in one window, each completes only its own
    /// handshake. A reserved `addr` (`0` / broadcast) can never join.
    pub async fn join(&mut self, addr: u32, timeout: Duration) -> Option<(u32, [u8; 16])> {
        if self.txc.locked() {
            return None; // fail closed (nonce safety): JOIN_REQ/CONFIRM consume TX counters
        }
        if is_reserved_addr(addr) {
            return None; // a host refuses it anyway; don't advertise it on air
        }
        if self.access != Access::Duty {
            log::warn!(target: "net", "pairing: refused, the radio is not in Duty mode");
            return None;
        }
        let deadline = Instant::now().checked_add(timeout)?;
        while Instant::now() < deadline {
            // A fresh ephemeral per attempt, drawn BEFORE the request goes out (its public key is
            // what the request carries), so only the shared secret is computed after the
            // response. Fresh per attempt also bounds a man in the middle's SAS search to one
            // response wait. TRNG down: refuse (logged), never fall back.
            let eph = trng_ephemeral(true)?;
            let mut req = [0u8; REQ_LEN];
            req[0] = VERSION;
            req[1..].copy_from_slice(eph.public());
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
            if !self.tx_pair(&JOIN_FRAME_KEY, &req_hdr, &req).await {
                Timer::after(JOIN_RESP_WINDOW).await; // duty-limited / radio error: back off
                continue;
            }

            // Listen out the whole response wait (the host computes its shared secret first): a
            // foreign frame, another joiner's response or a response that fails key confirmation
            // must not end it.
            let resp_end = Instant::now() + JOIN_RESP_WINDOW + PEER_KEX_TIME;
            loop {
                let remaining = resp_end.saturating_duration_since(Instant::now());
                if remaining.as_ticks() == 0 {
                    break;
                }
                let mut buf = [0u8; RESP_LEN];
                let Some((hdr, plen)) = self.rx_pair(&JOIN_FRAME_KEY, remaining, &mut buf).await else {
                    continue;
                };
                if hdr.frame_type != FrameType::JoinResp
                    || plen != RESP_LEN
                    || buf[0] != VERSION
                    || !join_response_for(hdr.dest, hdr.src, addr)
                {
                    continue;
                }
                let Some(session) = handshake(&eph, &buf[1..1 + PUBLIC_LEN], Role::Node, addr, hdr.src)
                else {
                    continue; // low-order key: not a host we can pair with
                };
                if !session.verify(Role::Gateway, &buf[1 + PUBLIC_LEN..]) {
                    log::warn!(
                        target: "net",
                        "pairing: JOIN_RESP from {:08X} failed key confirmation, ignored",
                        hdr.src
                    );
                    continue;
                }
                let mut conf = [0u8; CONFIRM_LEN];
                conf[0] = VERSION;
                conf[1..].copy_from_slice(session.tag(Role::Node));
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
                self.tx_pair(session.link_key(), &conf_hdr, &conf).await;
                log::info!(target: "net", "pairing: SAS {:06}", session.sas());
                return Some((hdr.src, *session.link_key()));
            }
        }
        None
    }

    /// Joiner, keyed (E4): join a host that already holds this node's `key`, using `addr` (the
    /// node's own address) for up to `timeout`. Sends a KEYED JOIN_REQ under `key` and waits
    /// for the KEYED JOIN_RESP echoing that request's counter; returns the host's address on a
    /// match. Nothing is committed here: the caller already has the key
    /// and installs the host with `add_peer(gw_addr, key)`. Each attempt uses a fresh TX
    /// counter, which is what makes the echo a freshness proof (`keyed_resp_matches`).
    ///
    /// Counter discipline (as in `send_ack`): the duty gate runs first (a refused attempt
    /// burns no counter and no watermark wear), then each attempt takes its counter from
    /// [`alloc_counter`](Net::alloc_counter) and seals the request **before** the first await,
    /// so a caller that drops this future mid-TX or mid-window (a `select` timeout) can never
    /// leave the counter for the next transfer to seal different plaintext under the same
    /// nonce. The allocation also re-checks the TX lock on every attempt.
    ///
    /// Duty mode only: under LBT+AFA or FHSS ([`Access`]) it returns [`JoinError::WrongMode`]
    /// without transmitting (a fixed-channel exchange there would skip the listen-before-talk
    /// or break the hop schedule; the host refuses to answer in those modes for the same
    /// reason).
    pub async fn join_keyed(
        &mut self,
        addr: u32,
        key: &[u8; 16],
        timeout: Duration,
    ) -> Result<u32, JoinError> {
        if self.access != Access::Duty {
            return Err(JoinError::WrongMode);
        }
        if is_reserved_addr(addr) {
            return Err(JoinError::BadAddr); // a host refuses it anyway; don't advertise it on air
        }
        if self.txc.locked() {
            return Err(JoinError::NonceLocked); // fail closed: JOIN_REQ consumes TX counters
        }
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return Err(JoinError::Timeout);
        };
        while Instant::now() < deadline {
            // The request's length is fixed (header + 4 B addr + tag), so the duty gate can run
            // before anything is allocated or sealed.
            if !self.duty.try_tx(duty::frame_toa_ms(frame::HDR_LEN + 4 + TAG_LEN)) {
                // Duty-limited: back off one response window, don't spin.
                Timer::after(JOIN_RESP_WINDOW).await;
                continue;
            }
            // None = TX locked (fail closed, nonce safety): stop, don't retry.
            let Some(req_counter) = self.alloc_counter() else {
                return Err(JoinError::NonceLocked);
            };
            let req_hdr = Header {
                frame_type: FrameType::JoinReq,
                flags: flags::KEYED,
                src: addr,
                dest: 0, // gateway address not yet known
                counter: req_counter,
                bulk_index: None,
            };
            let mut req = [0u8; MAX_FRAME];
            let Ok(n) = frame::seal_frame(&mut self.ccm, key, &req_hdr, &addr.to_le_bytes(), &mut req) else {
                return Err(JoinError::Timeout); // unreachable: a 4 B payload always fits
            };
            if self.radio.tx(&req[..n], false, TX_TIMEOUT).await.is_err() {
                // A radio error: back off one response window, don't spin.
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
                // `join_response_for`: addressed to us, from a plausible host (not reserved,
                // not our own address, which `add_peer` would refuse anyway: shared nonces).
                if let Some((hdr, plen)) = self.rx_pair(key, remaining, &mut buf).await
                    && hdr.frame_type == FrameType::JoinResp
                    && hdr.flags & flags::KEYED != 0
                    && join_response_for(hdr.dest, hdr.src, addr)
                    && keyed_resp_matches(&buf[..plen], req_counter)
                {
                    return Ok(hdr.src);
                }
            }
        }
        Err(JoinError::Timeout)
    }

    /// Host, keyed (E4): answer a KEYED JOIN_REQ (`rx` = the whole received frame, dest 0)
    /// from a peer registered with `add_peer`. Opens it under that peer's key, admits it only
    /// when the counter is fresh on the peer's replay lane (and advances the lane), then sends
    /// the KEYED JOIN_RESP echoing the request's counter after `ACK_TURNAROUND`. Returns the
    /// joiner's address when a response went out, else why not ([`Net::recv`] logs it,
    /// rate-limited). Called from [`Net::recv`]; nothing else
    /// changes: the peer, its key and its lane were already installed.
    ///
    /// The response follows `send_ack`'s discipline: duty gate, then the counter from
    /// [`alloc_counter`](Net::alloc_counter) (which re-checks the TX lock), then the seal, all
    /// before the turnaround await, so dropping `recv` mid-response cannot leave the counter
    /// unconsumed.
    ///
    /// Duty mode only (see [`JoinError::WrongMode`]): in LBT+AFA or FHSS mode the request is
    /// dropped unanswered (`fhss_master_tick` and `afa_serve` receive through `recv`). Refused
    /// up front, before any CCM work, when switched off ([`Net::set_keyed_join`]).
    pub(super) async fn answer_keyed_join(&mut self, rx: &mut [u8]) -> Result<u32, KeyedRefusal> {
        if !self.keyed_join_enabled {
            return Err(KeyedRefusal::Off); // the operator's switch: no CCM work at all
        }
        if self.access != Access::Duty {
            return Err(KeyedRefusal::WrongMode);
        }
        if self.txc.locked() {
            return Err(KeyedRefusal::NonceLocked); // the JOIN_RESP would consume a TX counter
        }
        let (peek, _) = frame::Header::parse(rx).map_err(|_| KeyedRefusal::Malformed)?;
        let registered = self.peer_slot(peek.src).is_some();
        if !registered {
            // Never the default key: the per-node key is the authorization.
            return Err(KeyedRefusal::Unregistered);
        }
        let key = self.key_for(peek.src);
        let (hdr, range) = frame::open_frame(&mut self.ccm, &key, rx).map_err(|_| KeyedRefusal::AuthFail)?;
        if !keyed_req_valid(&rx[range], hdr.src) {
            return Err(KeyedRefusal::Malformed);
        }
        let lane = self.lane(hdr.src);
        if !keyed_req_admit(registered, lane.classify(hdr.counter)) {
            return Err(KeyedRefusal::Stale {
                counter: hdr.counter,
                last_seen: lane.last_seen(),
            });
        }
        self.lane_accept(hdr.src, hdr.counter);

        // Fixed length (header + 4 B echo + tag): gate on the duty budget before the counter,
        // so a refused response burns no counter (the joiner simply retries).
        if !self.duty.try_tx(duty::frame_toa_ms(frame::HDR_LEN + 4 + TAG_LEN)) {
            return Err(KeyedRefusal::DutyLimited);
        }
        let counter = self.alloc_counter().ok_or(KeyedRefusal::NonceLocked)?; // fail closed
        let resp_hdr = Header {
            frame_type: FrameType::JoinResp,
            flags: flags::KEYED,
            src: self.addr,
            dest: hdr.src,
            counter,
            bulk_index: None,
        };
        let mut resp = [0u8; MAX_FRAME];
        let n = frame::seal_frame(
            &mut self.ccm,
            &key,
            &resp_hdr,
            &hdr.counter.to_le_bytes(),
            &mut resp,
        )
        .map_err(|_| KeyedRefusal::Malformed)?; // unreachable: a 4 B payload always fits
        Timer::after(ACK_TURNAROUND).await;
        match self.radio.tx(&resp[..n], false, TX_TIMEOUT).await {
            Ok(()) => Ok(hdr.src),
            Err(_) => Err(KeyedRefusal::TxFailed),
        }
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
