//! Pairing v3 handshake kernel: X25519 key agreement with a commitment round (numeric
//! comparison), the HKDF-SHA256 key schedule, the HMAC-SHA256 key-confirmation tags and the
//! 6-digit short authentication string (SAS) (firmware/docs/radio.md "OTA pairing").
//!
//! The firmware's `tower::radio::net` runs the radio exchange; everything that decides a key
//! or accepts a peer's proof is here, so it is tested on the host against published vectors and
//! an independent reference.
//!
//! The exchange is Bluetooth LE Secure Connections numeric comparison (Core Specification Vol 3
//! Part H 2.3.5.6.2) in our roles (node = initiator A, gateway = responder B), with HMAC-SHA256
//! and SHA-256 in place of the AES-CMAC functions f4 / f5 / f6 / g2:
//!
//! ```text
//! node  JOIN_REQ      VERSION ‖ E_node                   public framing key, dest 0
//! gw    JOIN_RESP     VERSION ‖ E_gw ‖ C_gw               public framing key  (C_gw: the commitment)
//! node  JOIN_NONCE    VERSION ‖ N_node                   public framing key
//! gw    JOIN_REVEAL   VERSION ‖ N_gw ‖ tag_gw             public framing key  (node checks C_gw)
//! node  JOIN_CONFIRM  VERSION ‖ tag_node                 sealed under the NEW link key
//! ```
//!
//! Values (both ends compute them from their own secret and the peer's public values):
//!
//! ```text
//! N_node, N_gw = 16-byte nonces, fresh per handshake, from the hardware TRNG only
//! C_gw     = HMAC-SHA256(key = N_gw, "tower pairing v3 commit" ‖ node_addr LE ‖ gw_addr LE
//!                        ‖ E_node ‖ E_gw)[..16]                                    (BLE f4)
//! th       = SHA-256("tower pairing v3" ‖ node_addr LE ‖ gw_addr LE ‖ E_node ‖ E_gw
//!                    ‖ N_node ‖ N_gw)                               (the transcript hash)
//! shared   = X25519(e_own, E_peer)                       (all-zero = refused, see below)
//! okm      = HKDF-SHA256(salt = th, ikm = shared, info = "tower pairing v3"), 32 bytes  (BLE f5)
//! link_key = okm[0..16]   confirm_key = okm[16..32]
//! tag_gw   = HMAC-SHA256(confirm_key, "gw" ‖ th)[..8]                              (BLE f6)
//! tag_node = HMAC-SHA256(confirm_key, "node" ‖ th)[..8]
//! SAS      = u64 LE(SHA-256("tower pairing v3 sas" ‖ th)[..8]) mod 10⁶             (BLE g2)
//! ```
//!
//! **Why the commitment.** Without it (the three-frame v2), a man in the middle that answers the
//! node could pick its own public key *after* seeing everything the node sends, and search on
//! the order of 10⁶ keys for one that gives the same SAS as its session with the gateway (about
//! 10⁶ X25519 operations, within reach of a PC inside the node's response wait). Here the gateway
//! commits to its nonce `N_gw` before it sees `N_node`, and the node reveals `N_node` only after
//! it holds that commitment. Whoever plays the gateway towards the node is bound to a nonce
//! chosen without knowing `N_node` (the commitment is binding: a different nonce fails
//! [`commitment_matches`]); whoever plays the node towards the gateway must send its nonce
//! without knowing `N_gw` (the commitment is hiding: a 128-bit HMAC key). Either way the SAS on
//! one side is a fresh random value the attacker could not steer, so its two sessions show the
//! same 6 digits with probability 10⁻⁶ per attempt, however much computing power it has.
//!
//! Every public value (both addresses, both public keys, both nonces) enters the transcript hash,
//! so the link key, both tags and the SAS bind all of them: a peer that saw anything different
//! derives a different key, its tag fails, and nothing is committed. The SAS, like BLE's g2, is a
//! function of the public transcript only (it would match even before the shared secret is
//! computed); the shared secret is bound to it through the public keys. The 2-byte vs 4-byte
//! role labels of the tags give messages of different lengths, so they can't collide.
//!
//! **What it protects against.** A passive eavesdropper learns nothing about the link key. An
//! *active* attacker inside the pairing window can still run one handshake with each side (an
//! unauthenticated Diffie-Hellman); the SAS is the out-of-band check against that: both ends log
//! it, and if the operator compares the two values the attacker is caught except with
//! probability 10⁻⁶. If nobody compares them, an active attacker is not detected.
//!
//! Secrets (`Ephemeral`, including its nonce until it is revealed, `Session`, the shared secret
//! and the HKDF output) are zeroized when dropped or right after use. Only copies the compiler
//! makes on its own stack can escape that.

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{X25519_BASEPOINT_BYTES, x25519};
use zeroize::Zeroize;

/// Handshake identifier, the first payload byte of every pairing frame. A peer built from a
/// checkout with another handshake (the three-frame v2 used `2`) is refused on it with a logged
/// reason, instead of failing later without one.
pub const VERSION: u8 = 3;
/// X25519 public key length.
pub const PUBLIC_LEN: usize = 32;
/// Handshake nonce length (`N_node`, `N_gw`), as BLE's Na / Nb.
pub const NONCE_LEN: usize = 16;
/// Truncated HMAC-SHA256 commitment length (128 bits, as BLE's Cb).
pub const COMMIT_LEN: usize = 16;
/// Truncated HMAC-SHA256 confirmation tag length.
pub const TAG_LEN: usize = 8;
/// Derived AES-128 link key length.
pub const LINK_KEY_LEN: usize = 16;
/// JOIN_REQ payload: `VERSION ‖ E_node`.
pub const REQ_LEN: usize = 1 + PUBLIC_LEN;
/// JOIN_RESP payload: `VERSION ‖ E_gw ‖ C_gw`.
pub const RESP_LEN: usize = 1 + PUBLIC_LEN + COMMIT_LEN;
/// JOIN_NONCE payload: `VERSION ‖ N_node`.
pub const NONCE_MSG_LEN: usize = 1 + NONCE_LEN;
/// JOIN_REVEAL payload: `VERSION ‖ N_gw ‖ tag_gw`.
pub const REVEAL_LEN: usize = 1 + NONCE_LEN + TAG_LEN;
/// JOIN_CONFIRM payload: `VERSION ‖ tag_node`.
pub const CONFIRM_LEN: usize = 1 + TAG_LEN;
/// Domain label: the transcript prefix and the HKDF info.
pub const LABEL: &[u8; 16] = b"tower pairing v3";
/// Hardware TRNG output an [`Ephemeral`] is derived from: [`SECRET_ENTROPY_LEN`] bytes for the
/// X25519 secret, then [`NONCE_ENTROPY_LEN`] for the nonce.
pub const ENTROPY_LEN: usize = SECRET_ENTROPY_LEN + NONCE_ENTROPY_LEN;
/// TRNG bytes condensed into the 32-byte X25519 secret.
pub const SECRET_ENTROPY_LEN: usize = 64;
/// TRNG bytes condensed into the 16-byte nonce.
pub const NONCE_ENTROPY_LEN: usize = 32;
/// The SAS is a 6-digit decimal number: `0..SAS_MODULUS`.
pub const SAS_MODULUS: u32 = 1_000_000;
/// Domain label for hashing the TRNG entropy into an X25519 secret.
const EPHEMERAL_LABEL: &[u8] = b"tower pairing v3 ephemeral";
/// Domain label for hashing the TRNG entropy into a nonce.
const NONCE_LABEL: &[u8] = b"tower pairing v3 nonce";
/// Domain label of the gateway's commitment.
const COMMIT_LABEL: &[u8] = b"tower pairing v3 commit";
/// Domain label of the SAS.
const SAS_LABEL: &[u8] = b"tower pairing v3 sas";

// Every pairing payload fits the radio MTU (74 B, `tower::radio::frame::MAX_PAYLOAD`; the
// network layer refuses anything larger). Mirrored here like the CCM nonce length in `nonce`.
const _: () = assert!(REQ_LEN <= 74 && RESP_LEN <= 74 && NONCE_MSG_LEN <= 74 && REVEAL_LEN <= 74);

/// Which side computes or checks a confirmation tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The gateway (the host that opened the pairing window, BLE's responder); it commits to its
    /// nonce in the JOIN_RESP and its tag rides the JOIN_REVEAL.
    Gateway,
    /// The joining node (BLE's initiator); its tag rides the JOIN_CONFIRM.
    Node,
}

impl Role {
    fn label(self) -> &'static [u8] {
        match self {
            Role::Gateway => b"gw",
            Role::Node => b"node",
        }
    }
}

/// One side's ephemeral X25519 key pair and handshake nonce, used for **one** handshake (the
/// firmware draws a fresh one per JOIN_REQ it sends or answers). Zeroized on drop.
pub struct Ephemeral {
    secret: [u8; 32],
    public: [u8; PUBLIC_LEN],
    nonce: [u8; NONCE_LEN],
}

impl Ephemeral {
    /// Derive a key pair and a nonce from [`ENTROPY_LEN`] bytes of hardware TRNG output: the
    /// secret is `SHA-256("tower pairing v3 ephemeral" ‖ entropy[..64])` (a condensing step:
    /// twice the entropy the scalar needs, hashed, so a modestly biased TRNG word still yields a
    /// uniform scalar), clamped by X25519 itself; the nonce is
    /// `SHA-256("tower pairing v3 nonce" ‖ entropy[64..96])[..16]` (condensed the same way, from
    /// TRNG bytes the secret does not use). Costs one X25519 scalar multiplication (the public
    /// key).
    #[must_use]
    pub fn from_entropy(entropy: &[u8; ENTROPY_LEN]) -> Self {
        let (secret_entropy, nonce_entropy) = entropy.split_at(SECRET_ENTROPY_LEN);
        let mut secret: [u8; 32] = Sha256::new()
            .chain_update(EPHEMERAL_LABEL)
            .chain_update(secret_entropy)
            .finalize()
            .into();
        let mut digest: [u8; 32] = Sha256::new()
            .chain_update(NONCE_LABEL)
            .chain_update(nonce_entropy)
            .finalize()
            .into();
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&digest[..NONCE_LEN]);
        let e = Self::from_parts(&secret, &nonce);
        secret.zeroize();
        digest.zeroize();
        nonce.zeroize();
        e
    }

    /// A key pair and nonce from a given 32-byte X25519 secret and nonce (RFC 7748 test vectors
    /// and the KATs; production code uses [`from_entropy`](Self::from_entropy)).
    #[must_use]
    pub fn from_parts(secret: &[u8; 32], nonce: &[u8; NONCE_LEN]) -> Self {
        Self {
            secret: *secret,
            public: x25519(*secret, X25519_BASEPOINT_BYTES),
            nonce: *nonce,
        }
    }

    /// The public key `E` sent to the peer.
    #[must_use]
    pub fn public(&self) -> &[u8; PUBLIC_LEN] {
        &self.public
    }

    /// This side's nonce. The gateway reveals it (JOIN_REVEAL) only after the node's nonce
    /// arrived; the node sends it (JOIN_NONCE) only after it holds the gateway's commitment.
    #[must_use]
    pub fn nonce(&self) -> &[u8; NONCE_LEN] {
        &self.nonce
    }

    /// Gateway: the commitment `C_gw` to this ephemeral's nonce, for the JOIN_RESP answering
    /// `e_node` (see [`commitment`]).
    #[must_use]
    pub fn commitment(&self, e_node: &[u8; PUBLIC_LEN], node_addr: u32, gw_addr: u32) -> [u8; COMMIT_LEN] {
        commitment(&self.nonce, e_node, &self.public, node_addr, gw_addr)
    }

    /// Run this side's half of the handshake against the peer's public key and nonce: the X25519
    /// shared secret, the key schedule, both confirmation tags and the SAS. `own_role` says which
    /// public key and nonce are the node's and which the gateway's. `None` if the peer's key is a
    /// low-order point (the shared secret would be all zero, i.e. known to anyone: refused, never
    /// used as key material).
    ///
    /// The node must call it only after [`commitment_matches`] accepted the gateway's nonce.
    #[must_use]
    pub fn session(
        &self,
        peer: &[u8; PUBLIC_LEN],
        peer_nonce: &[u8; NONCE_LEN],
        own_role: Role,
        node_addr: u32,
        gw_addr: u32,
    ) -> Option<Session> {
        let mut shared = shared_secret(&self.secret, peer)?;
        let t = match own_role {
            Role::Node => Transcript {
                node_addr,
                gw_addr,
                e_node: &self.public,
                e_gw: peer,
                n_node: &self.nonce,
                n_gw: peer_nonce,
            },
            Role::Gateway => Transcript {
                node_addr,
                gw_addr,
                e_node: peer,
                e_gw: &self.public,
                n_node: peer_nonce,
                n_gw: &self.nonce,
            },
        };
        let s = key_schedule(&shared, &t);
        shared.zeroize();
        Some(s)
    }
}

impl Drop for Ephemeral {
    fn drop(&mut self) {
        self.secret.zeroize();
        self.nonce.zeroize();
    }
}

/// The gateway's commitment to its nonce (BLE f4 with HMAC-SHA256):
/// `HMAC-SHA256(key = n_gw, "tower pairing v3 commit" ‖ node_addr LE ‖ gw_addr LE ‖ e_node ‖
/// e_gw)[..16]`. Hiding (the nonce is a 128-bit PRF key the node does not know yet) and binding
/// (another nonce giving the same 128 bits is a second preimage of truncated HMAC-SHA256), and
/// tied to both public keys and both addresses, so it can't be replayed into another handshake.
#[must_use]
pub fn commitment(
    n_gw: &[u8; NONCE_LEN],
    e_node: &[u8; PUBLIC_LEN],
    e_gw: &[u8; PUBLIC_LEN],
    node_addr: u32,
    gw_addr: u32,
) -> [u8; COMMIT_LEN] {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(n_gw).expect("HMAC takes any key length");
    mac.update(COMMIT_LABEL);
    mac.update(&node_addr.to_le_bytes());
    mac.update(&gw_addr.to_le_bytes());
    mac.update(e_node);
    mac.update(e_gw);
    let full = mac.finalize().into_bytes();
    let mut c = [0u8; COMMIT_LEN];
    c.copy_from_slice(&full[..COMMIT_LEN]);
    c
}

/// Node: whether the nonce `n_gw` revealed in the JOIN_REVEAL opens the commitment `commit` the
/// JOIN_RESP carried (constant time over the commitment). `false` means the peer changed its
/// nonce after seeing the node's (the man-in-the-middle move the commitment exists to stop), or
/// a reveal from someone else: ignore it, never derive a key from it.
#[must_use]
pub fn commitment_matches(
    commit: &[u8],
    n_gw: &[u8; NONCE_LEN],
    e_node: &[u8; PUBLIC_LEN],
    e_gw: &[u8; PUBLIC_LEN],
    node_addr: u32,
    gw_addr: u32,
) -> bool {
    commit.len() == COMMIT_LEN && ct_eq(&commitment(n_gw, e_node, e_gw, node_addr, gw_addr), commit)
}

/// The public values both ends must agree on (each from its own point of view): the two
/// addresses, the two public keys and the two nonces.
#[derive(Debug, Clone, Copy)]
pub struct Transcript<'a> {
    pub node_addr: u32,
    pub gw_addr: u32,
    pub e_node: &'a [u8; PUBLIC_LEN],
    pub e_gw: &'a [u8; PUBLIC_LEN],
    pub n_node: &'a [u8; NONCE_LEN],
    pub n_gw: &'a [u8; NONCE_LEN],
}

impl Transcript<'_> {
    /// The transcript hash `th = SHA-256("tower pairing v3" ‖ node_addr LE ‖ gw_addr LE ‖ E_node
    /// ‖ E_gw ‖ N_node ‖ N_gw)`: every field has a fixed length, so the encoding is unambiguous.
    #[must_use]
    pub fn hash(&self) -> [u8; 32] {
        Sha256::new()
            .chain_update(LABEL)
            .chain_update(self.node_addr.to_le_bytes())
            .chain_update(self.gw_addr.to_le_bytes())
            .chain_update(self.e_node)
            .chain_update(self.e_gw)
            .chain_update(self.n_node)
            .chain_update(self.n_gw)
            .finalize()
            .into()
    }

    /// The 6-digit SAS (BLE g2): `u64 LE(SHA-256("tower pairing v3 sas" ‖ th)[..8]) mod 10⁶`. A
    /// function of the public transcript only, so a passive listener can compute it too; what it
    /// proves is that both ends saw the same transcript.
    #[must_use]
    pub fn sas(&self) -> u32 {
        sas_from_hash(&self.hash())
    }
}

fn sas_from_hash(th: &[u8; 32]) -> u32 {
    let d = Sha256::new().chain_update(SAS_LABEL).chain_update(th).finalize();
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&d[..8]);
    // 64 bits reduced mod 10⁶: the bias is below 2⁻⁴⁴, irrelevant for a 6-digit comparison.
    (u64::from_le_bytes(raw) % SAS_MODULUS as u64) as u32
}

/// The outcome of a handshake on one side: the link key both ends now share, the SAS and the
/// two expected confirmation tags. Zeroized on drop.
pub struct Session {
    link_key: [u8; LINK_KEY_LEN],
    sas: u32,
    tag_gw: [u8; TAG_LEN],
    tag_node: [u8; TAG_LEN],
}

impl Session {
    /// The AES-128 link key (the per-node key both ends install).
    #[must_use]
    pub fn link_key(&self) -> &[u8; LINK_KEY_LEN] {
        &self.link_key
    }

    /// The short authentication string, `0..SAS_MODULUS` (show it as 6 digits, zero-padded).
    #[must_use]
    pub fn sas(&self) -> u32 {
        self.sas
    }

    /// The confirmation tag `role` sends.
    #[must_use]
    pub fn tag(&self, role: Role) -> &[u8; TAG_LEN] {
        match role {
            Role::Gateway => &self.tag_gw,
            Role::Node => &self.tag_node,
        }
    }

    /// Whether `received` is exactly `role`'s confirmation tag (constant time over the tag).
    #[must_use]
    pub fn verify(&self, role: Role, received: &[u8]) -> bool {
        received.len() == TAG_LEN && ct_eq(self.tag(role), received)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.link_key.zeroize();
        self.sas.zeroize();
        self.tag_gw.zeroize();
        self.tag_node.zeroize();
    }
}

/// X25519 (RFC 7748) of `secret` and `peer`, refusing an all-zero result: the output of a
/// low-order (small-subgroup) peer point, which an attacker can force and then knows. The check
/// is the RFC 7748 §6.1 "MAY check" made mandatory.
#[must_use]
pub fn shared_secret(secret: &[u8; 32], peer: &[u8; PUBLIC_LEN]) -> Option<[u8; 32]> {
    let mut k = x25519(*secret, *peer);
    if k.iter().fold(0u8, |acc, &b| acc | b) == 0 {
        k.zeroize();
        return None;
    }
    Some(k)
}

/// The key schedule (see the module docs) from an X25519 `shared` secret and the transcript.
#[must_use]
pub fn key_schedule(shared: &[u8; 32], t: &Transcript) -> Session {
    let th = t.hash();
    let mut okm = [0u8; 2 * LINK_KEY_LEN];
    Hkdf::<Sha256>::new(Some(&th), shared)
        .expand(LABEL, &mut okm)
        .expect("32 B is far below HKDF-SHA256's 8160 B limit");

    let mut link_key = [0u8; LINK_KEY_LEN];
    link_key.copy_from_slice(&okm[..LINK_KEY_LEN]);
    let confirm_key = &okm[LINK_KEY_LEN..];
    let tag = |role: Role| {
        let mut mac =
            <Hmac<Sha256> as KeyInit>::new_from_slice(confirm_key).expect("HMAC takes any key length");
        mac.update(role.label());
        mac.update(&th);
        let full = mac.finalize().into_bytes();
        let mut t = [0u8; TAG_LEN];
        t.copy_from_slice(&full[..TAG_LEN]);
        t
    };
    let s = Session {
        link_key,
        sas: sas_from_hash(&th),
        tag_gw: tag(Role::Gateway),
        tag_node: tag(Role::Node),
    };
    okm.zeroize();
    link_key.zeroize();
    s
}

/// Constant-time equality of two equal-length byte strings (the length itself is public).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    core::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a 64-hex-digit test vector.
    fn h32(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    // RFC 7748 §6.1 Diffie-Hellman test.
    const ALICE_SK: &str = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
    const ALICE_PK: &str = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";
    const BOB_SK: &str = "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb";
    const BOB_PK: &str = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f";
    const SHARED: &str = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";

    const NODE: u32 = 0x0000_00BB;
    const GW: u32 = 0x2222_2222;
    /// The nonces of the golden handshake: 0x10..=0x1F for the node, 0xA0..=0xAF for the gateway.
    const N_NODE: [u8; NONCE_LEN] = [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    ];
    const N_GW: [u8; NONCE_LEN] = [
        0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
    ];

    /// Node = Alice, gateway = Bob of RFC 7748 §6.1, with the golden nonces.
    fn node() -> Ephemeral {
        Ephemeral::from_parts(&h32(ALICE_SK), &N_NODE)
    }
    fn gateway() -> Ephemeral {
        Ephemeral::from_parts(&h32(BOB_SK), &N_GW)
    }

    /// RFC 7748 §5.2: the two single-shot X25519 vectors (scalar, u-coordinate) → output.
    #[test]
    fn rfc7748_5_2_vectors() {
        let v = [
            (
                "a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4",
                "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c",
                "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552",
            ),
            (
                "4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d",
                "e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493",
                "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957",
            ),
        ];
        for (k, u, out) in v {
            assert_eq!(shared_secret(&h32(k), &h32(u)), Some(h32(out)));
        }
    }

    /// RFC 7748 §5.2 iterated test: k = u = 9, then `k, u = X25519(k, u), k`; pinned after 1
    /// and 1000 iterations (the 1,000,000-iteration value is too slow for a unit test).
    #[test]
    fn rfc7748_5_2_iterated() {
        let (mut k, mut u) = (X25519_BASEPOINT_BYTES, X25519_BASEPOINT_BYTES);
        for i in 1..=1000 {
            let next = x25519(k, u);
            u = k;
            k = next;
            if i == 1 {
                assert_eq!(
                    k,
                    h32("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079")
                );
            }
        }
        assert_eq!(
            k,
            h32("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51")
        );
    }

    /// RFC 7748 §6.1: both public keys and the shared secret, from both sides.
    #[test]
    fn rfc7748_6_1_diffie_hellman() {
        let (alice, bob) = (node(), gateway());
        assert_eq!(alice.public(), &h32(ALICE_PK));
        assert_eq!(bob.public(), &h32(BOB_PK));
        assert_eq!(shared_secret(&h32(ALICE_SK), bob.public()), Some(h32(SHARED)));
        assert_eq!(shared_secret(&h32(BOB_SK), alice.public()), Some(h32(SHARED)));
    }

    /// RFC 5869 test case 1 (HKDF-SHA256), through the same `hkdf` + `sha2` the schedule uses.
    #[test]
    fn rfc5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: [u8; 13] = core::array::from_fn(|i| i as u8);
        let info: [u8; 10] = core::array::from_fn(|i| 0xf0 + i as u8);
        let mut okm = [0u8; 42];
        Hkdf::<Sha256>::new(Some(&salt), &ikm)
            .expand(&info, &mut okm)
            .unwrap();
        let expect = [
            0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a, 0x90, 0x43, 0x4f, 0x64, 0xd0, 0x36, 0x2f, 0x2a,
            0x2d, 0x2d, 0x0a, 0x90, 0xcf, 0x1a, 0x5a, 0x4c, 0x5d, 0xb0, 0x2d, 0x56, 0xec, 0xc4, 0xc5, 0xbf,
            0x34, 0x00, 0x72, 0x08, 0xd5, 0xb8, 0x87, 0x18, 0x58, 0x65,
        ];
        assert_eq!(okm, expect);
    }

    /// The golden transcript: node = Alice, gateway = Bob, the golden nonces and addresses.
    fn golden_transcript<'a>(e_node: &'a [u8; 32], e_gw: &'a [u8; 32]) -> Transcript<'a> {
        Transcript {
            node_addr: NODE,
            gw_addr: GW,
            e_node,
            e_gw,
            n_node: &N_NODE,
            n_gw: &N_GW,
        }
    }

    /// The commitment, the transcript hash and the whole schedule, pinned against an independent
    /// implementation (Python: RFC 7748 ladder + stdlib `hmac`/`hashlib`).
    #[test]
    fn key_schedule_golden() {
        let (e_node, e_gw) = (h32(ALICE_PK), h32(BOB_PK));
        assert_eq!(
            commitment(&N_GW, &e_node, &e_gw, NODE, GW),
            [
                0x05, 0x18, 0x72, 0xd7, 0x8b, 0xd0, 0xec, 0xda, 0x37, 0xbc, 0xdd, 0x5f, 0xa8, 0xc9, 0xd5,
                0xef
            ]
        );
        let t = golden_transcript(&e_node, &e_gw);
        assert_eq!(
            t.hash(),
            h32("a37f4ba487a454139ce57681da041a75901d9a91631edb6642a1c07020bc619e")
        );
        let s = key_schedule(&h32(SHARED), &t);
        assert_eq!(
            s.link_key(),
            &[
                0xb3, 0x40, 0xd4, 0xd6, 0x1f, 0x5f, 0x5b, 0x5a, 0xe1, 0x4a, 0x0d, 0x88, 0x19, 0x81, 0x28,
                0x09
            ]
        );
        assert_eq!(s.sas(), 211_658);
        assert_eq!(t.sas(), 211_658);
        assert_eq!(
            s.tag(Role::Gateway),
            &[0xa2, 0xd3, 0x49, 0xdb, 0x39, 0x94, 0xf7, 0x14]
        );
        assert_eq!(
            s.tag(Role::Node),
            &[0x13, 0xd3, 0x91, 0xcc, 0x78, 0x72, 0x4b, 0x93]
        );
    }

    /// The honest exchange, message by message: the gateway's commitment opens with its revealed
    /// nonce, both sides agree on the link key, the SAS and both tags, and each verifies the
    /// other's tag.
    #[test]
    fn both_sides_agree() {
        let (n, g) = (node(), gateway());
        // JOIN_RESP: the gateway commits to its nonce for this node's key.
        let c = g.commitment(n.public(), NODE, GW);
        // JOIN_REVEAL: the node checks the revealed nonce against it.
        assert!(commitment_matches(
            &c,
            g.nonce(),
            n.public(),
            g.public(),
            NODE,
            GW
        ));
        let at_node = n.session(g.public(), g.nonce(), Role::Node, NODE, GW).unwrap();
        let at_gw = g.session(n.public(), n.nonce(), Role::Gateway, NODE, GW).unwrap();
        assert_eq!(at_node.link_key(), at_gw.link_key());
        assert_eq!(at_node.sas(), at_gw.sas());
        assert_eq!(at_node.sas(), 211_658, "same as the golden schedule");
        assert!(at_node.verify(Role::Gateway, at_gw.tag(Role::Gateway)));
        assert!(at_gw.verify(Role::Node, at_node.tag(Role::Node)));
        // The roles' tags differ, so one side's tag can't be reflected back as the other's.
        assert_ne!(at_gw.tag(Role::Gateway), at_gw.tag(Role::Node));
        assert!(!at_gw.verify(Role::Node, at_gw.tag(Role::Gateway)));
    }

    /// The commitment binds the nonce, both public keys and both addresses: any other value of
    /// any of them fails to open it, as does a commitment of the wrong length.
    #[test]
    fn commitment_binds_every_input() {
        let (e_node, e_gw) = (h32(ALICE_PK), h32(BOB_PK));
        let c = commitment(&N_GW, &e_node, &e_gw, NODE, GW);
        assert!(commitment_matches(&c, &N_GW, &e_node, &e_gw, NODE, GW));
        for byte in 0..NONCE_LEN {
            for bit in 0..8 {
                let mut n = N_GW;
                n[byte] ^= 1 << bit;
                assert!(
                    !commitment_matches(&c, &n, &e_node, &e_gw, NODE, GW),
                    "N_gw[{byte}] bit {bit}"
                );
            }
        }
        for byte in 0..PUBLIC_LEN {
            let flip = |mut a: [u8; 32]| {
                a[byte] ^= 0x80;
                a
            };
            assert!(
                !commitment_matches(&c, &N_GW, &flip(e_node), &e_gw, NODE, GW),
                "E_node[{byte}]"
            );
            assert!(
                !commitment_matches(&c, &N_GW, &e_node, &flip(e_gw), NODE, GW),
                "E_gw[{byte}]"
            );
        }
        for bit in 0..32 {
            assert!(!commitment_matches(
                &c,
                &N_GW,
                &e_node,
                &e_gw,
                NODE ^ (1 << bit),
                GW
            ));
            assert!(!commitment_matches(
                &c,
                &N_GW,
                &e_node,
                &e_gw,
                NODE,
                GW ^ (1 << bit)
            ));
        }
        // The two public keys are ordered, and the commitment must be exact.
        assert!(!commitment_matches(&c, &N_GW, &e_gw, &e_node, NODE, GW));
        assert!(!commitment_matches(
            &c[..COMMIT_LEN - 1],
            &N_GW,
            &e_node,
            &e_gw,
            NODE,
            GW
        ));
        let mut long = [0u8; COMMIT_LEN + 1];
        long[..COMMIT_LEN].copy_from_slice(&c);
        assert!(!commitment_matches(&long, &N_GW, &e_node, &e_gw, NODE, GW));
        // Hiding in practice: a different nonce gives an unrelated commitment.
        let mut other = N_GW;
        other[0] ^= 1;
        assert_ne!(commitment(&other, &e_node, &e_gw, NODE, GW), c);
    }

    /// THE attack the commitment round stops. A man in the middle answers the node with its own
    /// key and a commitment to its own nonce; once the node has sent `N_node` it would like to
    /// reveal a different nonce (one it picked to make the node's SAS equal the SAS of its
    /// session with the gateway). Any nonce other than the committed one fails the node's check,
    /// so the node never derives a key from it; only the committed nonce, chosen blind, opens.
    #[test]
    fn mitm_changing_its_nonce_after_seeing_the_nodes_is_detected() {
        let n = node();
        let mitm = Ephemeral::from_entropy(&[0x5a; ENTROPY_LEN]);
        // JOIN_RESP from the attacker, committing to its nonce for the node's key.
        let c = mitm.commitment(n.public(), NODE, GW);
        // The node's JOIN_NONCE is now public. The attacker tries every nonce it could steer
        // towards: here 256 candidates derived from N_node.
        for i in 0..=255u8 {
            let mut steered = *n.nonce();
            steered[0] ^= i;
            steered[15] = steered[15].wrapping_add(i);
            if steered == *mitm.nonce() {
                continue;
            }
            assert!(
                !commitment_matches(&c, &steered, n.public(), mitm.public(), NODE, GW),
                "a changed nonce opened the commitment"
            );
        }
        // The committed nonce opens it: the honest path of whoever sent the JOIN_RESP.
        assert!(commitment_matches(
            &c,
            mitm.nonce(),
            n.public(),
            mitm.public(),
            NODE,
            GW
        ));
        // A commitment made for one node's key does not open for another node's key.
        let other_node = Ephemeral::from_entropy(&[0x33; ENTROPY_LEN]);
        assert!(!commitment_matches(
            &c,
            mitm.nonce(),
            other_node.public(),
            mitm.public(),
            NODE,
            GW
        ));
    }

    /// A man in the middle that runs one handshake with each side (its own key towards both)
    /// ends up with a different session on each side: the node rejects the gateway's tag if it
    /// is relayed, the keys differ, and the two SAS values differ (the operator's check).
    #[test]
    fn mitm_sessions_differ_on_the_two_sides() {
        let (n, g) = (node(), gateway());
        let mitm = Ephemeral::from_entropy(&[0x5a; ENTROPY_LEN]);
        // Towards the gateway the attacker plays the node; towards the node, the gateway.
        let at_gw = g
            .session(mitm.public(), mitm.nonce(), Role::Gateway, NODE, GW)
            .unwrap();
        let at_node = n
            .session(mitm.public(), mitm.nonce(), Role::Node, NODE, GW)
            .unwrap();
        assert!(!at_node.verify(Role::Gateway, at_gw.tag(Role::Gateway)));
        assert_ne!(at_node.link_key(), at_gw.link_key());
        assert_ne!(at_node.sas(), at_gw.sas());
    }

    /// The link key, both tags and the SAS bind all four handshake values (both public keys,
    /// both nonces) and both addresses: flipping any single bit of any of them changes the key
    /// and both tags, and (for all but a ~10⁻⁶ chance per flip; this deterministic set has no
    /// collision) the SAS. The shared secret is bound into the key and the tags as well.
    #[test]
    fn keys_tags_and_sas_bind_every_value() {
        let (shared, e_node, e_gw) = (h32(SHARED), h32(ALICE_PK), h32(BOB_PK));
        let base_t = golden_transcript(&e_node, &e_gw);
        let base = key_schedule(&shared, &base_t);
        let key_and_tags_differ = |s: &Session| {
            s.link_key() != base.link_key()
                && s.tag(Role::Gateway) != base.tag(Role::Gateway)
                && s.tag(Role::Node) != base.tag(Role::Node)
        };
        let all_differ = |s: &Session| key_and_tags_differ(s) && s.sas() != base.sas();
        for byte in 0..32 {
            for bit in [0u8, 7] {
                let flip = |mut a: [u8; 32]| {
                    a[byte] ^= 1 << bit;
                    a
                };
                // The shared secret: key and tags only (the SAS is over the public transcript).
                let s = key_schedule(&flip(shared), &base_t);
                assert!(key_and_tags_differ(&s) && s.sas() == base.sas(), "shared[{byte}]");
                let (fe_node, fe_gw) = (flip(e_node), flip(e_gw));
                let t = Transcript {
                    e_node: &fe_node,
                    ..base_t
                };
                assert!(all_differ(&key_schedule(&shared, &t)), "E_node[{byte}]");
                let t = Transcript {
                    e_gw: &fe_gw,
                    ..base_t
                };
                assert!(all_differ(&key_schedule(&shared, &t)), "E_gw[{byte}]");
            }
        }
        for byte in 0..NONCE_LEN {
            for bit in [0u8, 7] {
                let flip = |mut a: [u8; NONCE_LEN]| {
                    a[byte] ^= 1 << bit;
                    a
                };
                let (fn_node, fn_gw) = (flip(N_NODE), flip(N_GW));
                let t = Transcript {
                    n_node: &fn_node,
                    ..base_t
                };
                assert!(all_differ(&key_schedule(&shared, &t)), "N_node[{byte}]");
                let t = Transcript {
                    n_gw: &fn_gw,
                    ..base_t
                };
                assert!(all_differ(&key_schedule(&shared, &t)), "N_gw[{byte}]");
            }
        }
        for bit in 0..32 {
            let t = Transcript {
                node_addr: NODE ^ (1 << bit),
                ..base_t
            };
            assert!(all_differ(&key_schedule(&shared, &t)), "node {bit}");
            let t = Transcript {
                gw_addr: GW ^ (1 << bit),
                ..base_t
            };
            assert!(all_differ(&key_schedule(&shared, &t)), "gw {bit}");
        }
        // The roles are not interchangeable: swapped public keys, swapped nonces or swapped
        // addresses all give a different session.
        let swapped_keys = Transcript {
            e_node: &e_gw,
            e_gw: &e_node,
            ..base_t
        };
        assert!(all_differ(&key_schedule(&shared, &swapped_keys)));
        let swapped_nonces = Transcript {
            n_node: &N_GW,
            n_gw: &N_NODE,
            ..base_t
        };
        assert!(all_differ(&key_schedule(&shared, &swapped_nonces)));
        let swapped_addrs = Transcript {
            node_addr: GW,
            gw_addr: NODE,
            ..base_t
        };
        assert!(all_differ(&key_schedule(&shared, &swapped_addrs)));
    }

    /// A transcript tampered in flight (one side saw a different nonce: a corrupted or replaced
    /// JOIN_NONCE / JOIN_REVEAL) gives the two sides different keys and tags, so the confirmation
    /// fails on both and nothing is committed.
    #[test]
    fn transcript_tamper_fails_confirmation() {
        let (n, g) = (node(), gateway());
        let mut seen_by_gw = *n.nonce();
        seen_by_gw[3] ^= 0x01;
        let at_gw = g
            .session(n.public(), &seen_by_gw, Role::Gateway, NODE, GW)
            .unwrap();
        let at_node = n.session(g.public(), g.nonce(), Role::Node, NODE, GW).unwrap();
        assert_ne!(at_node.link_key(), at_gw.link_key());
        assert!(!at_node.verify(Role::Gateway, at_gw.tag(Role::Gateway)));
        assert!(!at_gw.verify(Role::Node, at_node.tag(Role::Node)));
        assert_ne!(at_node.sas(), at_gw.sas());
        // The same with an address the two sides disagree on.
        let at_gw = g
            .session(n.public(), n.nonce(), Role::Gateway, NODE + 1, GW)
            .unwrap();
        assert!(!at_node.verify(Role::Gateway, at_gw.tag(Role::Gateway)));
    }

    /// Low-order peer points give an all-zero shared secret: refused, never used as a key.
    #[test]
    fn low_order_points_refused() {
        let me = node();
        let mut one = [0u8; 32];
        one[0] = 1;
        // 0, 1 and the order-8 point from the curve25519 small-subgroup list.
        let order8 = h32("e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800");
        for p in [[0u8; 32], one, order8] {
            assert!(shared_secret(&h32(ALICE_SK), &p).is_none());
            assert!(me.session(&p, &N_GW, Role::Node, NODE, GW).is_none());
        }
    }

    /// The ephemeral derivation (pinned against the Python reference): the secret is the hash of
    /// its label and the first 64 entropy bytes, the nonce the hash of its label and the last 32;
    /// each depends only on its own part of the entropy.
    #[test]
    fn ephemeral_from_entropy() {
        let entropy: [u8; ENTROPY_LEN] = core::array::from_fn(|i| i as u8);
        let e = Ephemeral::from_entropy(&entropy);
        assert_eq!(
            e.public(),
            &h32("323058aaf4831f463fa34c2e902c4c5c1e567a865b22938f326b43668dc67a70")
        );
        assert_eq!(
            e.nonce(),
            &[
                0xdf, 0x5c, 0xa8, 0x73, 0x94, 0xf0, 0x70, 0x2d, 0x8f, 0xcf, 0xa9, 0x04, 0xc0, 0x4e, 0xd3,
                0x0c
            ]
        );
        let mut other = entropy;
        other[SECRET_ENTROPY_LEN - 1] ^= 1;
        let o = Ephemeral::from_entropy(&other);
        assert_ne!(o.public(), e.public());
        assert_eq!(o.nonce(), e.nonce());
        let mut other = entropy;
        other[ENTROPY_LEN - 1] ^= 1;
        let o = Ephemeral::from_entropy(&other);
        assert_eq!(o.public(), e.public());
        assert_ne!(o.nonce(), e.nonce());
    }

    /// `verify` needs the exact tag: wrong length, any flipped bit, or the other role's tag fail.
    #[test]
    fn verify_is_exact() {
        let (e_node, e_gw) = (h32(ALICE_PK), h32(BOB_PK));
        let s = key_schedule(&h32(SHARED), &golden_transcript(&e_node, &e_gw));
        let good = *s.tag(Role::Node);
        assert!(s.verify(Role::Node, &good));
        assert!(!s.verify(Role::Node, &good[..TAG_LEN - 1]));
        let mut long = [0u8; TAG_LEN + 1];
        long[..TAG_LEN].copy_from_slice(&good);
        assert!(!s.verify(Role::Node, &long));
        for i in 0..TAG_LEN {
            let mut bad = good;
            bad[i] ^= 0x01;
            assert!(!s.verify(Role::Node, &bad), "byte {i}");
        }
    }

    /// The payload sizes of the five frames (the docs' table), all within the 74-byte radio MTU.
    #[test]
    fn payload_sizes() {
        assert_eq!(
            [REQ_LEN, RESP_LEN, NONCE_MSG_LEN, REVEAL_LEN, CONFIRM_LEN],
            [33, 49, 17, 25, 9]
        );
    }
}
