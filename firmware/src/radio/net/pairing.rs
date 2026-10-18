//! OTA pairing (v3): an X25519 key agreement with a commitment round (numeric comparison)
//! between a joining node and a host with an open pairing window (docs/radio.md "OTA
//! pairing"), plus the keyed join (E4) for nodes whose key the host already holds. `impl Net`
//! block over [`super::Net`].
//!
//! ```text
//! node  JOIN_REQ      3 ‖ E_node             src = node, dest = 0     under JOIN_FRAME_KEY
//! host  JOIN_RESP     3 ‖ E_gw ‖ C_gw        src = host, dest = node  under JOIN_FRAME_KEY
//! node  JOIN_NONCE    3 ‖ N_node             src = node, dest = host  under JOIN_FRAME_KEY
//! host  JOIN_REVEAL   3 ‖ N_gw ‖ tag_gw      src = host, dest = node  under JOIN_FRAME_KEY
//! node  JOIN_CONFIRM  3 ‖ tag_node           src = node, dest = host  under the NEW link key
//! ```
//!
//! Both ends derive the per-node link key from the X25519 shared secret (the
//! `tower_net::handshake` kernel: HKDF-SHA256 over the transcript of both public keys, both
//! nonces and both addresses), so no key travels on air and a passive eavesdropper learns
//! nothing. The host commits to its nonce (`C_gw`) before it sees the node's, and the node sends
//! its nonce only once it holds that commitment and accepts the revealed nonce only if it opens
//! the commitment: the numeric comparison of Bluetooth LE Secure Connections, so the 6-digit
//! SAS both ends log (`pairing: SAS 123456`) catches an active man in the middle with
//! probability 1 - 10⁻⁶ when someone compares the two values. The host proves the key with
//! `tag_gw`, which the node checks before it confirms; the node proves it with `tag_node`,
//! inside a frame sealed under the link key; the host commits only after a valid confirm.
//!
//! [`JOIN_FRAME_KEY`] is public: it only gives the four clear-key frames a CCM format, so a
//! corrupted frame fails its tag and is dropped. Secrecy and authentication come from the key
//! agreement, the commitment and the confirmation tags.
//!
//! Ephemerals and nonces come from the hardware TRNG only (`board::trng_fill`: refused, never
//! drawn from the software fallback), one per JOIN_REQ sent or answered, zeroized after use.
//! Each side's own public key is computed before it is needed (the host's when the window opens
//! and again after each answered request, the node's before it transmits), so only one scalar
//! multiplication (the shared secret) sits on each critical path, and the peer's wait covers it
//! ([`PEER_KEX_TIME`]): the host computes after the JOIN_NONCE (the node waits for the
//! JOIN_REVEAL), the node after the JOIN_REVEAL (the host waits for the JOIN_CONFIRM). The
//! JOIN_RESP and the JOIN_NONCE need no scalar multiplication, so their waits are radio-only.
//!
//! The **joiner chooses its own address** and keeps it; the host does NOT assign or override
//! it. The host commits only on a valid confirm; a lost confirm leaves its window open (the
//! entry is never installed). The confirm itself is not acknowledged, so the joiner cannot know
//! whether the host committed: [`Net::join`] hands the key back **provisionally**, and a node
//! commits it (persists it) only once a confirmed uplink under it is acknowledged, retrying a
//! few times and discarding the key when the host never answers (the commit round,
//! `tower_net::pairing::commit_step`; the push-button app runs it). Otherwise a lost confirm
//! would leave a node that believes it is paired while the gateway drops all of its traffic.
//!
//! The host's window has a fixed end ([`PairingHost::prepare`] takes it before the first
//! ephemeral is drawn), and the host answers a JOIN_REQ only while a whole handshake still fits
//! before it ([`HOST_HANDSHAKE_TIME`], `tower_net::pairing::handshake_fits`): a node is never
//! committed after the window it joined has closed, when the operator's tooling may already have
//! reported a timeout.
//!
//! **OTA pairing only ever adds a NEW peer.** The host refuses (sends no JOIN_RESP to) a
//! JOIN_REQ from an address already in its peer table, a reserved address, or its own: the key
//! agreement is unauthenticated until the SAS is compared, so anyone in range can JOIN_REQ *as a
//! victim*, and answering would let a hijacker re-key the victim out of the network. Re-pairing
//! a known node therefore needs the operator to remove it first (an authenticated host-side
//! action). The joiner, in turn, only accepts host frames addressed to itself, and, once a host
//! answered, only that host's.
//!
//! **Keyed join (E4).** A node whose per-node key the host already holds (`add_peer`, e.g.
//! the gateway's NodeAdd: a node provisioned by cable or in the factory, or a replacement
//! gateway that imported the node keys) joins under that key instead: KEYED JOIN_REQ(addr), sealed under
//! the per-node key → KEYED JOIN_RESP(echo of the request's counter), sealed under the same
//! key. No key travels on air; the node proves the key and its freshness through the host's
//! replay lane, the host proves the key and its freshness through the echo; the node learns
//! the host's address. The host answers any time, with no window: the key is the
//! authorization. A host without the key ignores it (dest 0 is not its address, and the frame
//! does not open under the framing key or any key it holds).

use embassy_time::{Duration, Instant, Timer};
// The pairing-v3 handshake (commitment, key schedule, confirmation tags, SAS) and the admission
// rules (who may be answered / which response is ours) live in the host-testable `tower_net`
// kernels; the TRNG draw, the windows and the JOIN frame exchange stay here.
use tower_net::handshake::{
    COMMIT_LEN, CONFIRM_LEN, ENTROPY_LEN, Ephemeral, NONCE_LEN, NONCE_MSG_LEN, PUBLIC_LEN, REQ_LEN, RESP_LEN,
    REVEAL_LEN, Role, Session, VERSION, commitment_matches,
};
use tower_net::pairing::{
    handshake_fits, join_request_admissible, join_response_for, keyed_req_admit, keyed_req_valid,
    keyed_resp_matches,
};
use zeroize::Zeroize;

use super::{ACK_TURNAROUND, Access, Net, TX_TIMEOUT, is_reserved_addr};
use crate::radio::ccm::TAG_LEN;
use crate::radio::duty;
use crate::radio::frame::{self, FrameType, Header, MAX_FRAME, flags};

/// Public CCM framing key of the four pairing frames before the link key (JOIN_REQ, JOIN_RESP,
/// JOIN_NONCE, JOIN_REVEAL; docs/radio.md). **Not a secret** and never used for anything else:
/// it gives those frames a uniform CCM format, so a corrupted frame fails its tag and is
/// dropped. The key agreement's secrecy and authentication do not depend on it. The bytes keep
/// their original spelling on purpose: a JOIN_REQ from a node built from an older checkout still
/// opens, so the host can refuse it on its handshake byte with a logged reason.
pub const JOIN_FRAME_KEY: [u8; 16] = *b"TOWER-JOIN-v2\0\0\0";
/// Default pairing window: the host listens (and the joiner retries) for this
/// long. One minute gives a person time to put both ends into pairing mode.
pub const PAIRING_WINDOW: Duration = Duration::from_secs(60);
/// Radio part of every wait for the peer's next pairing frame (its turnaround, the frame's time
/// on air, RX set-up, margin), and the whole of the keyed join's response wait. The waits for
/// the JOIN_RESP and the JOIN_NONCE are exactly this (the peer computes nothing expensive
/// first); the waits for the JOIN_REVEAL and the JOIN_CONFIRM add [`PEER_KEX_TIME`].
const JOIN_STEP_WINDOW: Duration = Duration::from_millis(300);
/// The peer's computation inside the JOIN_REVEAL and JOIN_CONFIRM waits: one X25519 scalar
/// multiplication plus the HKDF / HMAC key schedule, on a Cortex-M0+ at 16 MHz (HSI16, no wait
/// states). Measured on a Core Module by `crypto_x25519_kat`: 1175 ms per scalar
/// multiplication, 1185 ms for the whole handshake critical path (shared secret + schedule,
/// measured with sha2's unrolled SHA-256; the compact backend the firmware now builds costs a
/// few ms more over the ~20 compressions of a schedule), so 1.5 s leaves ~25 % margin. (The estimate before the measurement: the curve25519-dalek u32
/// ladder executes ~12.0 M instructions, counted in an instruction-accurate emulator; v6-M has
/// no 32x32→64 multiply, so every limb product is a library call.) Err long: too short fails
/// every pairing (the answer lands after the wait closed), too long only slows a retry. Re-tune
/// from the KAT's `timing:` line if a toolchain or crate change moves it.
pub const PEER_KEX_TIME: Duration = Duration::from_millis(1500);
/// The joiner's pace: one JOIN_REQ per this period while no host answers. The request, the
/// [`JOIN_STEP_WINDOW`] for an answer, then an idle wait (radio off, the executor free for
/// the button and the console) to the next one: the air time and the duty cycle the join used
/// when each attempt still drew its own ephemeral (~1.2 s of curve arithmetic).
const JOIN_ATTEMPT_PERIOD: Duration = Duration::from_millis(1500);
/// One listen slice of the host's pairing window between JOIN_REQ polls.
const PAIR_RX_SLICE: Duration = Duration::from_millis(500);
/// The host's whole handshake, from an answered JOIN_REQ to the end of its JOIN_CONFIRM wait: two
/// turnarounds and two frames on air (the JOIN_RESP and the JOIN_REVEAL, each bounded by the
/// radio's `TX_TIMEOUT`), the JOIN_NONCE wait, our own shared secret (the same computation the
/// peer's [`PEER_KEX_TIME`] bounds) and the JOIN_CONFIRM wait: ~3.9 s. A JOIN_REQ is answered only
/// while this much of the window remains, so a handshake never runs past the window's end
/// (`tower_net::pairing::handshake_fits`).
pub const HOST_HANDSHAKE_TIME: Duration = Duration::from_ticks(
    2 * (ACK_TURNAROUND.as_ticks() + TX_TIMEOUT.as_ticks())
        + 2 * (JOIN_STEP_WINDOW.as_ticks() + PEER_KEX_TIME.as_ticks()),
);

/// The outcome of a completed pairing handshake ([`Net::open_pairing`] / [`Net::join`]).
#[derive(Clone, Copy)]
pub struct PairedPeer {
    /// The peer's address: the joiner's own, self-chosen one on the host; the host's on the
    /// joiner.
    pub addr: u32,
    /// The derived per-node link key (install it with [`Net::add_peer`]).
    pub key: [u8; 16],
    /// The 6-digit short authentication string, `0..1_000_000` (both ends log it as
    /// `pairing: SAS 012345`): the value a person compares between the two ends.
    pub sas: u32,
}

/// The host side of an open pairing window: the window's end and the precomputed X25519
/// ephemeral (and nonce) for the next JOIN_REQ it answers, so an answer only pays for the shared
/// secret. Create it when the window opens ([`prepare`](Self::prepare)), pass it to every
/// [`open_pairing`](Net::open_pairing) slice of that window, drop it when the window closes (the
/// secret is zeroized). Each ephemeral answers **one** JOIN_REQ and is then replaced, whether or
/// not that handshake completes: its nonce was committed to that one joiner's key, and a fresh
/// key pair per answer keeps every session's key material independent.
pub struct PairingHost {
    eph: Option<Ephemeral>,
    /// The TRNG failed once already (log the refusal once per window, not per slice).
    trng_failed: bool,
    /// When the window closes: no JOIN_REQ is answered unless its whole handshake ends by then.
    window_end: Instant,
}

impl PairingHost {
    /// Open a window of `window` and draw its first ephemeral from the hardware TRNG (one X25519
    /// scalar multiplication, about a second on the M0+). The window's end is fixed **before**
    /// that draw, so it closes `window` after the request that opened it (the moment the
    /// requester counts from), not ~1.2 s later. `None` (logged) when the TRNG is unavailable:
    /// pairing is refused rather than keyed from the software fallback.
    #[must_use]
    pub fn prepare(window: Duration) -> Option<Self> {
        let window_end = Instant::now().checked_add(window)?;
        Some(Self {
            eph: Some(trng_ephemeral(true)?),
            trng_failed: false,
            window_end,
        })
    }

    /// When the window closes.
    #[must_use]
    pub fn window_end(&self) -> Instant {
        self.window_end
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

/// A fresh X25519 ephemeral and nonce from the hardware TRNG ([`ENTROPY_LEN`] bytes, hashed into
/// the secret and the nonce; one scalar multiplication for the public key). `None` when the TRNG
/// cannot deliver: pairing refuses rather than derive key material from the software fallback
/// (logged when `log`). Not inlined: the curve arithmetic's ~2.4 KB of stack must exist only
/// while it runs, never inside an async poll frame that every poll of the caller would carry.
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
/// the critical path), the key schedule, both tags and the SAS. `None` for a low-order peer key.
/// Not inlined, for the same stack reason as [`trng_ephemeral`].
#[inline(never)]
fn handshake(
    own: &Ephemeral,
    peer: &[u8],
    peer_nonce: &[u8],
    role: Role,
    node_addr: u32,
    gw_addr: u32,
) -> Option<Session> {
    let mut e = [0u8; PUBLIC_LEN];
    e.copy_from_slice(peer);
    let mut n = [0u8; NONCE_LEN];
    n.copy_from_slice(peer_nonce);
    own.session(&e, &n, role, node_addr, gw_addr)
}

/// The host's JOIN_RESP payload `VERSION ‖ E_gw ‖ C_gw` answering `e_node` (the commitment's
/// HMAC, not inlined for the same stack reason as [`trng_ephemeral`]).
#[inline(never)]
fn join_resp(eph: &Ephemeral, e_node: &[u8], node_addr: u32, gw_addr: u32) -> [u8; RESP_LEN] {
    let mut e = [0u8; PUBLIC_LEN];
    e.copy_from_slice(e_node);
    let mut resp = [0u8; RESP_LEN];
    resp[0] = VERSION;
    resp[1..1 + PUBLIC_LEN].copy_from_slice(eph.public());
    resp[1 + PUBLIC_LEN..].copy_from_slice(&eph.commitment(&e, node_addr, gw_addr));
    resp
}

/// The joiner's check of a JOIN_REVEAL against the JOIN_RESP's commitment (not inlined, see
/// [`join_resp`]).
#[inline(never)]
fn reveal_opens(
    commit: &[u8; COMMIT_LEN],
    n_gw: &[u8],
    e_node: &[u8; PUBLIC_LEN],
    e_gw: &[u8; PUBLIC_LEN],
    node_addr: u32,
    gw_addr: u32,
) -> bool {
    let mut n = [0u8; NONCE_LEN];
    n.copy_from_slice(n_gw);
    commitment_matches(commit, &n, e_node, e_gw, node_addr, gw_addr)
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

/// On-air length of a keyed JOIN_REQ / JOIN_RESP: header + the 4 B payload (the joiner's address
/// / the counter echo) + the CCM tag.
const KEYED_FRAME_LEN: usize = frame::HDR_LEN + 4 + TAG_LEN;

/// [`Net::recv`]'s keyed-join responder: checks a keyed JOIN_REQ and, when it may be answered,
/// returns the sealed JOIN_RESP ([`Net::prepare_keyed_answer`], installed by
/// [`Net::set_keyed_join`]).
pub(super) type KeyedResponder = fn(&mut Net, &mut [u8]) -> Option<KeyedAnswer>;

/// A sealed keyed JOIN_RESP, ready to transmit after the turnaround.
pub(super) struct KeyedAnswer {
    buf: [u8; KEYED_FRAME_LEN],
    /// The joiner it answers.
    pub(super) peer: u32,
}

impl KeyedAnswer {
    /// The on-air bytes.
    pub(super) fn frame(&self) -> &[u8] {
        &self.buf
    }
}

/// Why the host did not answer a keyed JOIN_REQ ([`Net::prepare_keyed_answer`]); logged there,
/// rate-limited.
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
    /// Host: listen for a pairing JOIN_REQ for up to `slice` (one slice of the window `host`
    /// was prepared for; pass the whole window to listen through it in one call), answering with
    /// the precomputed ephemeral of `host` (create it with [`PairingHost::prepare`] when the
    /// window opens and keep passing the same one). On the first JOIN_REQ that completes the
    /// handshake (the commitment round, then a valid JOIN_CONFIRM under the derived key), logs
    /// the SAS and returns the joiner's **own, self-chosen** address, the derived **link key** and
    /// the SAS: the caller installs the peer with `add_peer(peer.addr, &peer.key)` (and persists
    /// it), and shows the SAS to whoever compares it. `None` when the slice or the window ends /
    /// on a lost nonce or confirm / an invalid confirm. Pairs the first joiner only.
    ///
    /// A JOIN_REQ from an address that is **already a registered peer**, reserved, or our own
    /// is ignored (no JOIN_RESP; see the module docs): OTA pairing never re-keys an existing node.
    /// Remove the node first to re-pair it. Neither is one answered when less than
    /// [`HOST_HANDSHAKE_TIME`] of the window remains: its handshake would end after the window.
    /// Duty mode only (a fixed-channel exchange, like [`join_keyed`](Self::join_keyed)).
    ///
    /// Bounds: the listening stops at the slice's end or the window's, whichever is first, but a
    /// JOIN_REQ answered before then runs its handshake to the end (at most
    /// [`HOST_HANDSHAKE_TIME`], ~3.9 s, and never past the window); after a spent handshake the
    /// next ephemeral costs ~1.2 s. One call therefore lasts at most `slice` + one handshake + one
    /// ephemeral, ~7 s for the gateway's 2 s slices (its watchdog check-in budget counts on that).
    pub async fn open_pairing(&mut self, host: &mut PairingHost, slice: Duration) -> Option<PairedPeer> {
        let deadline = Instant::now().checked_add(slice)?.min(host.window_end);
        if self.txc.locked() {
            // Fail closed (nonce safety): pairing replies consume TX counters. Wait the slice out
            // like the other refusals, so a caller's loop never spins without yielding.
            Timer::at(deadline).await;
            return None;
        }
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
            // Never listen past the deadline (a frame whose sync lands before it still completes).
            let listen = PAIR_RX_SLICE.min(deadline.saturating_duration_since(Instant::now()));
            if listen.as_ticks() == 0 {
                break;
            }
            let Some((hdr, plen)) = self.rx_pair(&JOIN_FRAME_KEY, listen, &mut buf).await else {
                continue;
            };
            if hdr.frame_type != FrameType::JoinReq {
                continue;
            }
            let peer_addr = hdr.src;
            let registered = self.peer_slot(peer_addr).is_some();
            let window_left = host.window_end.saturating_duration_since(Instant::now());
            let refusal = if plen != REQ_LEN || buf[0] != VERSION {
                // Typically a node built from another checkout (another pairing handshake).
                Some("not a pairing v3 request")
            } else if !join_request_admissible(peer_addr, self.addr, registered) {
                Some(if registered {
                    "already registered - remove it first to re-pair"
                } else {
                    "invalid address"
                })
            } else if !handshake_fits(window_left.as_millis(), HOST_HANDSHAKE_TIME.as_millis()) {
                // Committing it after the window closed could pair a node the operator's tooling
                // already reported as a timeout.
                Some("the window closes before a handshake could finish")
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

            // This ephemeral answers this JOIN_REQ only (dropped, i.e. zeroized, on every path
            // out of this iteration). JOIN_RESP: our public key and the commitment to our nonce,
            // bound to the joiner's key; no scalar multiplication yet, so the joiner's wait for
            // it is radio-only.
            let Some(eph) = host.eph.take() else {
                continue; // unreachable: `ready()` above
            };
            // `take` moved the ephemeral out by copy: its secret and nonce bytes are still in the
            // (now `None`) slot of the long-lived window state, where the moved value's
            // zeroize-on-drop cannot reach them. Wipe the slot.
            host.eph.zeroize();
            let mut e_node = [0u8; PUBLIC_LEN];
            e_node.copy_from_slice(&buf[1..REQ_LEN]);
            let resp = join_resp(&eph, &e_node, peer_addr, self.addr);
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
            if !self.tx_pair(&JOIN_FRAME_KEY, &resp_hdr, &resp).await {
                continue; // duty-limited / radio error: no answer went out, nothing to wait for
            }

            // JOIN_NONCE: the joiner's nonce, from that joiner, addressed to us. A foreign frame
            // must not end the wait.
            let nonce_end = Instant::now() + JOIN_STEP_WINDOW;
            let mut n_node = None;
            loop {
                let remaining = nonce_end.saturating_duration_since(Instant::now());
                if remaining.as_ticks() == 0 {
                    break;
                }
                let mut nbuf = [0u8; NONCE_MSG_LEN];
                if let Some((nhdr, nplen)) = self.rx_pair(&JOIN_FRAME_KEY, remaining, &mut nbuf).await
                    && nhdr.frame_type == FrameType::JoinNonce
                    && nhdr.src == peer_addr
                    && nhdr.dest == self.addr
                    && nplen == NONCE_MSG_LEN
                    && nbuf[0] == VERSION
                {
                    n_node = Some(nbuf);
                    break;
                }
            }
            let Some(n_node) = n_node else {
                continue; // lost nonce: this ephemeral is spent, the window stays open
            };

            // The handshake: one scalar multiplication (~1.2 s, the joiner's JOIN_REVEAL wait
            // covers it), then the reveal of the nonce we committed to, with our tag.
            let Some(session) = handshake(&eph, &e_node, &n_node[1..], Role::Gateway, peer_addr, self.addr)
            else {
                log::warn!(target: "net", "pairing: JOIN_REQ from {:08X} refused (low-order key)", peer_addr);
                continue;
            };
            let mut reveal = [0u8; REVEAL_LEN];
            reveal[0] = VERSION;
            reveal[1..1 + NONCE_LEN].copy_from_slice(eph.nonce());
            reveal[1 + NONCE_LEN..].copy_from_slice(session.tag(Role::Gateway));
            drop(eph); // zeroize the secret before anything awaits
            let counter = self.alloc_counter()?; // before the turnaround/TX awaits
            let reveal_hdr = Header {
                frame_type: FrameType::JoinReveal,
                flags: 0,
                src: self.addr,
                dest: peer_addr,
                counter,
                bulk_index: None,
            };
            Timer::after(ACK_TURNAROUND).await;
            if !self.tx_pair(&JOIN_FRAME_KEY, &reveal_hdr, &reveal).await {
                continue; // no reveal went out: no confirm to wait for
            }

            // Wait out the whole confirm window (the joiner computes its shared secret first): a
            // foreign frame, or one that does not open under the new link key, must not end it.
            // Commit only on the joiner's tag, from that joiner, addressed to us.
            let confirm_end = Instant::now() + JOIN_STEP_WINDOW + PEER_KEX_TIME;
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
                    return Some(PairedPeer {
                        addr: peer_addr,
                        key: *session.link_key(),
                        sas: session.sas(),
                    });
                }
            }
        }
        None
    }

    /// Joiner: pair with a host that has a window open, using `addr` (the joiner's **own**
    /// address, which it keeps) for up to `timeout` ([`PAIRING_WINDOW`] for the default).
    /// Each attempt sends the JOIN_REQ (an ephemeral and nonce from the TRNG, see below), takes
    /// the host's commitment from the JOIN_RESP, sends its nonce (JOIN_NONCE), checks the
    /// revealed nonce against the commitment and the host's confirmation tag (JOIN_REVEAL), and
    /// answers with the JOIN_CONFIRM under the derived key. Returns the host's address, the link
    /// key and the SAS on success (after logging the SAS), or `None` on timeout, when the TRNG is
    /// unavailable (logged; never keyed from the fallback), or outside Duty mode. The gateway
    /// address is what a product node persists so it knows *which* gateway to talk to after a
    /// reboot.
    ///
    /// The result is **provisional**: nothing acknowledges the JOIN_CONFIRM, so the host may not
    /// have committed (a lost confirm, a full registry). Install the key in RAM, then commit it
    /// only once a confirmed uplink under it is acknowledged (`tower_net::pairing::commit_step`,
    /// the commit round), and drop it when none is.
    ///
    /// Only host frames addressed to `addr` (from a non-reserved host) are accepted, and once a
    /// host answered, only that host's: with two nodes joining in one window, each completes
    /// only its own handshake. A reserved `addr` (`0` / broadcast) can never join.
    ///
    /// Unanswered attempts go out every [`JOIN_ATTEMPT_PERIOD`] (1.5 s); a whole handshake takes
    /// ~3.8 s.
    ///
    /// One ephemeral (~1.2 s of curve arithmetic, which holds the single-threaded executor:
    /// the button and the console stall meanwhile) serves every request until a host answers:
    /// until then only its public key has been on air, and its nonce, the value the
    /// commitment round protects, never left the node. Once a host answers, the ephemeral
    /// serves that handshake only (its nonce goes out in the JOIN_NONCE), and the next attempt
    /// draws a fresh one.
    pub async fn join(&mut self, addr: u32, timeout: Duration) -> Option<PairedPeer> {
        if self.txc.locked() {
            return None; // fail closed (nonce safety): the join frames consume TX counters
        }
        if is_reserved_addr(addr) {
            return None; // a host refuses it anyway; don't advertise it on air
        }
        if self.access != Access::Duty {
            log::warn!(target: "net", "pairing: refused, the radio is not in Duty mode");
            return None;
        }
        let deadline = Instant::now().checked_add(timeout)?;
        // The ephemeral of the unanswered requests (see above), drawn BEFORE a request goes
        // out (its public key is what the request carries), so only the shared secret is
        // computed after the reveal. TRNG down: refuse (logged), never fall back.
        let mut unanswered: Option<Ephemeral> = None;
        while Instant::now() < deadline {
            let attempt_start = Instant::now();
            if unanswered.is_none() {
                unanswered = Some(trng_ephemeral(true)?);
            }
            let mut req = [0u8; REQ_LEN];
            req[0] = VERSION;
            req[1..].copy_from_slice(unanswered.as_ref().expect("drawn above").public());
            // Each join frame consumes a fresh counter, allocated before its TX await.
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
                Timer::after(JOIN_STEP_WINDOW).await; // duty-limited / radio error: back off
                continue;
            }

            // JOIN_RESP: the host's public key and its commitment. The host computes nothing
            // expensive before it answers, so this wait is radio-only. A foreign frame or another
            // joiner's response must not end it.
            let resp_end = Instant::now() + JOIN_STEP_WINDOW;
            let mut answer = None;
            loop {
                let remaining = resp_end.saturating_duration_since(Instant::now());
                if remaining.as_ticks() == 0 {
                    break;
                }
                let mut buf = [0u8; RESP_LEN];
                if let Some((hdr, plen)) = self.rx_pair(&JOIN_FRAME_KEY, remaining, &mut buf).await
                    && hdr.frame_type == FrameType::JoinResp
                    && plen == RESP_LEN
                    && buf[0] == VERSION
                    && join_response_for(hdr.dest, hdr.src, addr)
                {
                    answer = Some((hdr.src, buf));
                    break;
                }
            }
            let Some((gw_addr, resp)) = answer else {
                // No host answered: the next request at the attempt pace, idle until then.
                Timer::at(attempt_start + JOIN_ATTEMPT_PERIOD).await;
                continue;
            };
            // A host answered: this ephemeral now serves this handshake only.
            let eph = unanswered.take().expect("drawn above");
            let mut e_gw = [0u8; PUBLIC_LEN];
            e_gw.copy_from_slice(&resp[1..1 + PUBLIC_LEN]);
            let mut commit = [0u8; COMMIT_LEN];
            commit.copy_from_slice(&resp[1 + PUBLIC_LEN..]);

            // JOIN_NONCE: our nonce, only now that the host is bound to its own.
            let mut nonce_msg = [0u8; NONCE_MSG_LEN];
            nonce_msg[0] = VERSION;
            nonce_msg[1..].copy_from_slice(eph.nonce());
            let counter = self.alloc_counter()?; // before the turnaround/TX awaits
            let nonce_hdr = Header {
                frame_type: FrameType::JoinNonce,
                flags: 0,
                src: addr,
                dest: gw_addr,
                counter,
                bulk_index: None,
            };
            Timer::after(ACK_TURNAROUND).await;
            if !self.tx_pair(&JOIN_FRAME_KEY, &nonce_hdr, &nonce_msg).await {
                Timer::after(JOIN_STEP_WINDOW).await; // duty-limited / radio error: back off
                continue;
            }

            // JOIN_REVEAL: listen out the whole wait (the host computes its shared secret
            // first). Only that host's reveal, addressed to us, whose nonce opens the commitment
            // and whose tag checks out; anything else is ignored and the wait goes on (only the
            // committed nonce can ever open the commitment).
            let reveal_end = Instant::now() + JOIN_STEP_WINDOW + PEER_KEX_TIME;
            let mut broken_logged = false;
            loop {
                let remaining = reveal_end.saturating_duration_since(Instant::now());
                if remaining.as_ticks() == 0 {
                    break;
                }
                let mut buf = [0u8; REVEAL_LEN];
                let Some((hdr, plen)) = self.rx_pair(&JOIN_FRAME_KEY, remaining, &mut buf).await else {
                    continue;
                };
                if hdr.frame_type != FrameType::JoinReveal
                    || hdr.src != gw_addr
                    || hdr.dest != addr
                    || plen != REVEAL_LEN
                    || buf[0] != VERSION
                {
                    continue;
                }
                let n_gw = &buf[1..1 + NONCE_LEN];
                if !reveal_opens(&commit, n_gw, eph.public(), &e_gw, addr, gw_addr) {
                    // A nonce other than the committed one: the move of a man in the middle that
                    // picks its nonce after seeing ours. Never derive a key from it.
                    if !broken_logged {
                        broken_logged = true;
                        log::warn!(
                            target: "net",
                            "pairing: JOIN_REVEAL from {:08X} does not open its commitment, ignored",
                            gw_addr
                        );
                    }
                    continue;
                }
                let Some(session) = handshake(&eph, &e_gw, n_gw, Role::Node, addr, gw_addr) else {
                    continue; // low-order key: not a host we can pair with
                };
                if !session.verify(Role::Gateway, &buf[1 + NONCE_LEN..]) {
                    log::warn!(
                        target: "net",
                        "pairing: JOIN_REVEAL from {:08X} failed key confirmation, ignored",
                        gw_addr
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
                    dest: gw_addr,
                    counter,
                    bulk_index: None,
                };
                Timer::after(ACK_TURNAROUND).await;
                if !self.tx_pair(session.link_key(), &conf_hdr, &conf).await {
                    // The host commits only on this confirm: committing without it would leave
                    // the node holding a key its gateway never installed. Start a new attempt.
                    log::warn!(target: "net", "pairing: JOIN_CONFIRM not sent (duty-limited or radio error)");
                    break;
                }
                // Sent, but not known to have arrived: the caller's commit round settles that.
                log::info!(target: "net", "pairing: SAS {:06}", session.sas());
                return Some(PairedPeer {
                    addr: gw_addr,
                    key: *session.link_key(),
                    sas: session.sas(),
                });
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
            if !self.duty.try_tx(duty::frame_toa_ms(KEYED_FRAME_LEN)) {
                // Duty-limited: back off one response window, don't spin.
                Timer::after(JOIN_STEP_WINDOW).await;
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
                Timer::after(JOIN_STEP_WINDOW).await;
                continue;
            }

            // Listen out the whole response window: a foreign frame (another node's uplink,
            // not openable under our key) must not end it early (same rule as await_ack).
            let resp_end = Instant::now() + JOIN_STEP_WINDOW;
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

    /// Host, keyed (E4): prepare the answer to a KEYED JOIN_REQ (`rx` = the whole received
    /// frame, dest 0) from a peer registered with `add_peer`. Opens it under that peer's key,
    /// admits it only when the counter is fresh on the peer's replay lane (and advances the lane),
    /// then seals the KEYED JOIN_RESP echoing the request's counter; [`Net::recv`] sends it after
    /// `ACK_TURNAROUND`. Returns `None` when it must not answer, after logging why (rate-limited).
    /// Nothing else changes: the peer, its key and its lane were already installed.
    ///
    /// Installed as [`Net::recv`]'s responder by [`Net::set_keyed_join`] (a [`KeyedResponder`]),
    /// so only images that host keyed joins link it.
    ///
    /// The answer follows `send_ack`'s discipline: duty gate, then the counter from
    /// [`alloc_counter`](Net::alloc_counter) (which re-checks the TX lock), then the seal, all
    /// synchronous, so dropping `recv` mid-response cannot leave the counter unconsumed.
    ///
    /// Duty mode only (see [`JoinError::WrongMode`]): in LBT+AFA or FHSS mode the request is
    /// dropped unanswered (`fhss_master_tick` and `afa_serve` receive through `recv`). Refused
    /// up front, before any CCM work, when switched off ([`Net::set_keyed_join`]`(false)`).
    pub(super) fn prepare_keyed_answer(&mut self, rx: &mut [u8]) -> Option<KeyedAnswer> {
        let src = frame::Header::parse(rx).map(|(h, _)| h.src).unwrap_or(0);
        match self.keyed_answer(rx) {
            Ok(answer) => Some(answer),
            Err(why) => {
                if let Some(n) = self.keyed_log.hit() {
                    log::warn!(
                        target: "net",
                        "keyed join from {:08X} refused: {} ({} refusal(s) since last report)",
                        src,
                        why,
                        n
                    );
                }
                None
            }
        }
    }

    /// The decision and the seal behind [`prepare_keyed_answer`](Self::prepare_keyed_answer).
    fn keyed_answer(&mut self, rx: &mut [u8]) -> Result<KeyedAnswer, KeyedRefusal> {
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
        if !self.duty.try_tx(duty::frame_toa_ms(KEYED_FRAME_LEN)) {
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
        let mut answer = KeyedAnswer {
            buf: [0u8; KEYED_FRAME_LEN],
            peer: hdr.src,
        };
        frame::seal_frame(
            &mut self.ccm,
            &key,
            &resp_hdr,
            &hdr.counter.to_le_bytes(),
            &mut answer.buf,
        )
        .map_err(|_| KeyedRefusal::Malformed)?; // unreachable: the buffer fits exactly
        Ok(answer)
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
