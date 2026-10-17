//! Pairing v2 handshake kernel: X25519 key agreement, the HKDF-SHA256 key schedule, the
//! HMAC-SHA256 key-confirmation tags and the short authentication string (SAS)
//! (firmware/docs/radio.md "OTA pairing").
//!
//! The firmware's `tower::radio::net` runs the radio exchange; everything that decides a key
//! or accepts a peer's proof is here, so it is tested on the host against published vectors.
//!
//! ```text
//! node  JOIN_REQ      VERSION ‖ E_node                  public framing key, dest 0
//! gw    JOIN_RESP     VERSION ‖ E_gw ‖ tag_gw            public framing key
//! node  JOIN_CONFIRM  VERSION ‖ tag_node                 sealed under the NEW link key
//! ```
//!
//! Key schedule (both ends compute it from their own secret and the peer's public key):
//!
//! ```text
//! shared   = X25519(e_own, E_peer)                       (all-zero = refused, see below)
//! okm      = HKDF-SHA256(salt = E_node ‖ E_gw, ikm = shared,
//!                        info = "tower pairing v2" ‖ node_addr LE ‖ gw_addr LE), 40 bytes
//! link_key = okm[0..16]   confirm_key = okm[16..32]   SAS = u64 LE(okm[32..40]) mod 10⁶
//! T        = "tower pairing v2" ‖ node_addr LE ‖ gw_addr LE ‖ E_node ‖ E_gw   (the transcript)
//! tag_gw   = HMAC-SHA256(confirm_key, "gw" ‖ T)[..8]
//! tag_node = HMAC-SHA256(confirm_key, "node" ‖ T)[..8]
//! ```
//!
//! Both addresses and both public keys enter the key derivation and the tags, so a peer that
//! saw anything different (a substituted key or address) derives different keys, its tag
//! fails, and nothing is committed. The 2-byte vs 4-byte role labels can't collide because the
//! transcript that follows starts with the fixed label.
//!
//! **What it protects against.** A passive eavesdropper learns nothing about the link key (the
//! v1 key transport sent it under a public key). An *active* attacker inside the pairing window
//! can still run one handshake with each side (an unauthenticated Diffie-Hellman); the SAS is
//! the optional out-of-band check against that: both ends log it, and a man in the middle ends
//! up with different values on the two sides unless it finds a colliding one inside the node's
//! response wait (see the docs for that bound).
//!
//! Secrets (`Ephemeral`, `Session`, the shared secret and the HKDF output) are zeroized when
//! dropped or right after use. Only copies the compiler makes on its own stack can escape that.

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{X25519_BASEPOINT_BYTES, x25519};
use zeroize::Zeroize;

/// Handshake version, the first payload byte of every v2 JOIN frame.
pub const VERSION: u8 = 2;
/// X25519 public key length.
pub const PUBLIC_LEN: usize = 32;
/// Truncated HMAC-SHA256 confirmation tag length.
pub const TAG_LEN: usize = 8;
/// Derived AES-128 link key length.
pub const LINK_KEY_LEN: usize = 16;
/// JOIN_REQ payload: `VERSION ‖ E_node`.
pub const REQ_LEN: usize = 1 + PUBLIC_LEN;
/// JOIN_RESP payload: `VERSION ‖ E_gw ‖ tag_gw`.
pub const RESP_LEN: usize = 1 + PUBLIC_LEN + TAG_LEN;
/// JOIN_CONFIRM payload: `VERSION ‖ tag_node`.
pub const CONFIRM_LEN: usize = 1 + TAG_LEN;
/// Domain label: the HKDF info prefix and the transcript prefix.
pub const LABEL: &[u8; 16] = b"tower pairing v2";
/// Entropy an [`Ephemeral`] is derived from (bytes of hardware TRNG output).
pub const ENTROPY_LEN: usize = 64;
/// The SAS is a 6-digit decimal number: `0..SAS_MODULUS`.
pub const SAS_MODULUS: u32 = 1_000_000;
/// Domain label for hashing the TRNG entropy into an X25519 secret.
const EPHEMERAL_LABEL: &[u8] = b"tower pairing v2 ephemeral";

/// Which side computes or checks a confirmation tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The gateway (the host that opened the pairing window); its tag rides the JOIN_RESP.
    Gateway,
    /// The joining node; its tag rides the JOIN_CONFIRM.
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

/// One side's ephemeral X25519 key pair, used for **one** handshake answer (the firmware draws a
/// fresh one per JOIN_REQ it sends or answers). The secret is zeroized on drop.
pub struct Ephemeral {
    secret: [u8; 32],
    public: [u8; PUBLIC_LEN],
}

impl Ephemeral {
    /// Derive a key pair from [`ENTROPY_LEN`] bytes of hardware TRNG output: the secret is
    /// `SHA-256("tower pairing v2 ephemeral" ‖ entropy)` (a condensing step: twice the entropy
    /// the scalar needs, hashed, so a modestly biased TRNG word still yields a uniform scalar),
    /// clamped by X25519 itself. Costs one X25519 scalar multiplication (the public key).
    #[must_use]
    pub fn from_entropy(entropy: &[u8; ENTROPY_LEN]) -> Self {
        let mut h = Sha256::new();
        h.update(EPHEMERAL_LABEL);
        h.update(entropy);
        let mut secret: [u8; 32] = h.finalize().into();
        let e = Self::from_secret(&secret);
        secret.zeroize();
        e
    }

    /// A key pair from a given 32-byte X25519 secret (RFC 7748 test vectors; production code
    /// uses [`from_entropy`](Self::from_entropy)).
    #[must_use]
    pub fn from_secret(secret: &[u8; 32]) -> Self {
        Self {
            secret: *secret,
            public: x25519(*secret, X25519_BASEPOINT_BYTES),
        }
    }

    /// The public key `E` sent to the peer.
    #[must_use]
    pub fn public(&self) -> &[u8; PUBLIC_LEN] {
        &self.public
    }

    /// Run this side's half of the handshake against the peer's public key: the X25519 shared
    /// secret, the key schedule and both confirmation tags. `own_role` says which public key is
    /// `E_node` and which `E_gw`. `None` if the peer's key is a low-order point (the shared
    /// secret would be all zero, i.e. known to anyone: refused, never used as key material).
    #[must_use]
    pub fn session(
        &self,
        peer: &[u8; PUBLIC_LEN],
        own_role: Role,
        node_addr: u32,
        gw_addr: u32,
    ) -> Option<Session> {
        let mut shared = shared_secret(&self.secret, peer)?;
        let (e_node, e_gw) = match own_role {
            Role::Node => (&self.public, peer),
            Role::Gateway => (peer, &self.public),
        };
        let s = key_schedule(&shared, e_node, e_gw, node_addr, gw_addr);
        shared.zeroize();
        Some(s)
    }
}

impl Drop for Ephemeral {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
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

/// The key schedule (see the module docs) from an X25519 `shared` secret.
#[must_use]
pub fn key_schedule(
    shared: &[u8; 32],
    e_node: &[u8; PUBLIC_LEN],
    e_gw: &[u8; PUBLIC_LEN],
    node_addr: u32,
    gw_addr: u32,
) -> Session {
    let mut salt = [0u8; 2 * PUBLIC_LEN];
    salt[..PUBLIC_LEN].copy_from_slice(e_node);
    salt[PUBLIC_LEN..].copy_from_slice(e_gw);
    let mut info = [0u8; 24];
    info[..16].copy_from_slice(LABEL);
    info[16..20].copy_from_slice(&node_addr.to_le_bytes());
    info[20..].copy_from_slice(&gw_addr.to_le_bytes());

    let mut okm = [0u8; 40];
    Hkdf::<Sha256>::new(Some(&salt), shared)
        .expand(&info, &mut okm)
        .expect("40 B is far below HKDF-SHA256's 8160 B limit");

    let mut link_key = [0u8; LINK_KEY_LEN];
    link_key.copy_from_slice(&okm[..16]);
    let mut sas_raw = [0u8; 8];
    sas_raw.copy_from_slice(&okm[32..40]);
    // 64 bits reduced mod 10⁶: the bias is below 2⁻⁴⁴, irrelevant for a 6-digit comparison.
    let sas = (u64::from_le_bytes(sas_raw) % SAS_MODULUS as u64) as u32;
    let confirm_key = &okm[16..32];
    let tag = |role: Role| {
        let mut mac =
            <Hmac<Sha256> as KeyInit>::new_from_slice(confirm_key).expect("HMAC takes any key length");
        mac.update(role.label());
        mac.update(&info); // = LABEL ‖ node_addr ‖ gw_addr, the transcript's head
        mac.update(e_node);
        mac.update(e_gw);
        let full = mac.finalize().into_bytes();
        let mut t = [0u8; TAG_LEN];
        t.copy_from_slice(&full[..TAG_LEN]);
        t
    };
    let s = Session {
        link_key,
        sas,
        tag_gw: tag(Role::Gateway),
        tag_node: tag(Role::Node),
    };
    okm.zeroize();
    link_key.zeroize();
    sas_raw.zeroize();
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
        let alice = Ephemeral::from_secret(&h32(ALICE_SK));
        let bob = Ephemeral::from_secret(&h32(BOB_SK));
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

    /// The whole schedule pinned against an independent implementation (Python: RFC 7748 ladder
    /// + stdlib `hmac`/`hashlib`), node = Alice, gateway = Bob of RFC 7748 §6.1.
    #[test]
    fn key_schedule_golden() {
        let s = key_schedule(&h32(SHARED), &h32(ALICE_PK), &h32(BOB_PK), NODE, GW);
        assert_eq!(
            s.link_key(),
            &[
                0x8d, 0x1c, 0xea, 0x52, 0x73, 0xba, 0xa0, 0x25, 0x92, 0x96, 0x60, 0x53, 0x0c, 0x20, 0x07,
                0xf4
            ]
        );
        assert_eq!(s.sas(), 670_388);
        assert_eq!(
            s.tag(Role::Gateway),
            &[0xe7, 0x9b, 0xab, 0x39, 0xc4, 0xc4, 0xdf, 0x16]
        );
        assert_eq!(
            s.tag(Role::Node),
            &[0xd8, 0x73, 0x6b, 0x57, 0xaf, 0x43, 0x52, 0xdf]
        );
    }

    /// Both sides of a handshake (each with its own secret and the other's public key) agree on
    /// the link key, the SAS and both tags, and each verifies the other's tag.
    #[test]
    fn both_sides_agree() {
        let node = Ephemeral::from_secret(&h32(ALICE_SK));
        let gw = Ephemeral::from_secret(&h32(BOB_SK));
        let at_node = node.session(gw.public(), Role::Node, NODE, GW).unwrap();
        let at_gw = gw.session(node.public(), Role::Gateway, NODE, GW).unwrap();
        assert_eq!(at_node.link_key(), at_gw.link_key());
        assert_eq!(at_node.sas(), at_gw.sas());
        assert_eq!(at_node.sas(), 670_388, "same as the golden schedule");
        assert!(at_node.verify(Role::Gateway, at_gw.tag(Role::Gateway)));
        assert!(at_gw.verify(Role::Node, at_node.tag(Role::Node)));
        // The roles' tags differ, so one side's tag can't be reflected back as the other's.
        assert_ne!(at_gw.tag(Role::Gateway), at_gw.tag(Role::Node));
        assert!(!at_gw.verify(Role::Node, at_gw.tag(Role::Gateway)));
    }

    /// Transcript binding: flipping any single bit of any input (the shared secret, either public
    /// key, either address) changes the link key and both tags (and, for all but a ~10⁻⁶ chance
    /// per flip, the SAS; this deterministic set has no collision).
    #[test]
    fn every_transcript_byte_is_bound() {
        let (shared, e_node, e_gw) = (h32(SHARED), h32(ALICE_PK), h32(BOB_PK));
        let base = key_schedule(&shared, &e_node, &e_gw, NODE, GW);
        let differs = |s: &Session| {
            s.link_key() != base.link_key()
                && s.tag(Role::Gateway) != base.tag(Role::Gateway)
                && s.tag(Role::Node) != base.tag(Role::Node)
                && s.sas() != base.sas()
        };
        for byte in 0..32 {
            for bit in [0u8, 7] {
                let flip = |mut a: [u8; 32]| {
                    a[byte] ^= 1 << bit;
                    a
                };
                assert!(
                    differs(&key_schedule(&flip(shared), &e_node, &e_gw, NODE, GW)),
                    "shared[{byte}]"
                );
                assert!(
                    differs(&key_schedule(&shared, &flip(e_node), &e_gw, NODE, GW)),
                    "E_node[{byte}]"
                );
                assert!(
                    differs(&key_schedule(&shared, &e_node, &flip(e_gw), NODE, GW)),
                    "E_gw[{byte}]"
                );
            }
        }
        for bit in 0..32 {
            assert!(
                differs(&key_schedule(&shared, &e_node, &e_gw, NODE ^ (1 << bit), GW)),
                "node {bit}"
            );
            assert!(
                differs(&key_schedule(&shared, &e_node, &e_gw, NODE, GW ^ (1 << bit))),
                "gw {bit}"
            );
        }
        // The two public keys are not interchangeable (salt and transcript are ordered).
        assert!(differs(&key_schedule(&shared, &e_gw, &e_node, NODE, GW)));
        // Nor are the two addresses.
        assert!(differs(&key_schedule(&shared, &e_node, &e_gw, GW, NODE)));
    }

    /// A man in the middle that substitutes its own public key ends up with a different session
    /// on each side: the node rejects the gateway tag it relays, and the SAS values differ.
    #[test]
    fn substituted_key_fails_confirmation() {
        let node = Ephemeral::from_secret(&h32(ALICE_SK));
        let gw = Ephemeral::from_secret(&h32(BOB_SK));
        let mitm = Ephemeral::from_entropy(&[0x5a; ENTROPY_LEN]);
        // The gateway answered the attacker's JOIN_REQ; the node got the attacker's key.
        let at_gw = gw.session(mitm.public(), Role::Gateway, NODE, GW).unwrap();
        let at_node = node.session(mitm.public(), Role::Node, NODE, GW).unwrap();
        assert!(!at_node.verify(Role::Gateway, at_gw.tag(Role::Gateway)));
        assert_ne!(at_node.link_key(), at_gw.link_key());
        assert_ne!(at_node.sas(), at_gw.sas());
    }

    /// Low-order peer points give an all-zero shared secret: refused, never used as a key.
    #[test]
    fn low_order_points_refused() {
        let me = Ephemeral::from_secret(&h32(ALICE_SK));
        let mut one = [0u8; 32];
        one[0] = 1;
        // 0, 1 and the order-8 point from the curve25519 small-subgroup list.
        let order8 = h32("e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800");
        for p in [[0u8; 32], one, order8] {
            assert!(shared_secret(&h32(ALICE_SK), &p).is_none());
            assert!(me.session(&p, Role::Node, NODE, GW).is_none());
        }
    }

    /// The ephemeral derivation is the hash of the label and the entropy (pinned against the
    /// Python reference), and different entropy gives a different key.
    #[test]
    fn ephemeral_from_entropy() {
        let entropy: [u8; ENTROPY_LEN] = core::array::from_fn(|i| i as u8);
        let e = Ephemeral::from_entropy(&entropy);
        assert_eq!(
            e.public(),
            &h32("9e01df3e9f414dc43cc07282495309db3fda727f11c22c24e3c89b0ad4c44546")
        );
        let mut other = entropy;
        other[63] ^= 1;
        assert_ne!(Ephemeral::from_entropy(&other).public(), e.public());
    }

    /// `verify` needs the exact tag: wrong length, any flipped bit, or the other role's tag fail.
    #[test]
    fn verify_is_exact() {
        let s = key_schedule(&h32(SHARED), &h32(ALICE_PK), &h32(BOB_PK), NODE, GW);
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
}
