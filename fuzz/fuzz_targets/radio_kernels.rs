//! `radio_kernels`: the pure radio timing / compliance / CCM logic (`crates/radio`).
//!
//! * [`DutyGovernor`] under an arbitrary sequence of refills (any `elapsed_ms`) and consumes (any
//!   airtime): never panics on overflow, matches a wide-integer model of its documented
//!   arithmetic (saturating refill, residue carry, cap clamp, all-or-nothing consume) exactly, and
//!   keeps the token-bucket regulatory bound: airtime spent never exceeds the cap plus the duty
//!   share of the wall-clock credited (EU 1 % / FCC §15.247 per-channel dwell).
//! * [`frame_toa_ms`] for ANY length: never panics, never under-counts the true airtime (the exact
//!   ceiling whenever it fits a `u32`), monotonic.
//! * [`hop_channel`] (the fixed FHSS permutation, FCC §15.247): for any seed, slots `0..N` map to
//!   a perfect permutation of the channels, identical in every cycle (so a channel recurs exactly
//!   one cycle apart). `i < N` is the documented precondition (the firmware passes `slot % N`).
//! * [`Ccm`] (AES-128-CCM, N=13 / M=8) against RustCrypto's independent `ccm` crate: same
//!   ciphertext and tag for any key / nonce / AAD (up to the documented `MAX_AAD`) / payload, open
//!   accepts exactly what the reference accepts (garbage and every single-bit tamper are refused),
//!   and the key-check value is `AES_K(0)`.
#![no_main]

use aes::Aes128;
use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
use arbitrary::Arbitrary;
use ccm::AeadInOut;
use ccm::consts::{U8, U13};
use libfuzzer_sys::fuzz_target;
use tower_radio::ccm::{AesBlock, Ccm, MAX_AAD, NONCE_LEN, TAG_LEN};
use tower_radio::{
    BITRATE, DutyGovernor, EU_CAP_MS, EU_PERMIL, FHSS_DWELL_BURST_MS, FHSS_PERMIL, frame_toa_ms, hop_channel,
};

#[derive(Arbitrary, Debug)]
enum Governor {
    Eu,
    Us915,
    Fhss,
    Custom { cap_ms: u32, permil: u32 },
}

#[derive(Arbitrary, Debug)]
enum GovOp {
    Refill(u32),
    Consume(u32),
    /// Consume the airtime of a frame of this many FIFO bytes (the firmware's `try_tx` shape).
    Frame(u8),
}

#[derive(Arbitrary, Debug)]
struct CcmCase<'a> {
    key: [u8; 16],
    nonce: [u8; NONCE_LEN],
    aad: &'a [u8],
    data: &'a [u8],
    /// Bit to flip for the tamper check (taken modulo the bits of nonce ‖ aad ‖ data ‖ tag).
    flip: u16,
    /// A forged tag for the garbage-open check.
    forged: [u8; TAG_LEN],
}

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    governor: Governor,
    ops: Vec<GovOp>,
    toa_lens: Vec<usize>,
    hop: (u32, u32, u32, u8),
    ccm: CcmCase<'a>,
}

/// Wide-integer model of `DutyGovernor`'s documented arithmetic.
struct Model {
    budget: u64,
    cap: u64,
    permil: u64,
    residue: u64,
}

impl Model {
    fn refill(&mut self, elapsed: u32) {
        // Saturating at u32::MAX, exactly like the u32 core (a saturated refill only ever credits
        // less, so the regulatory bound below still holds).
        let total = (elapsed as u64 * self.permil).min(u32::MAX as u64);
        let total = (total + self.residue).min(u32::MAX as u64);
        self.residue = total % 1000;
        self.budget = (self.budget + total / 1000).min(self.cap);
    }

    fn consume(&mut self, toa: u32) -> bool {
        if self.budget >= toa as u64 {
            self.budget -= toa as u64;
            true
        } else {
            false
        }
    }
}

fn check_governor(kind: &Governor, ops: &[GovOp]) {
    let (mut g, cap, permil) = match *kind {
        Governor::Eu => (DutyGovernor::eu(), EU_CAP_MS, EU_PERMIL),
        Governor::Us915 => (DutyGovernor::us915(), EU_CAP_MS, 1000),
        Governor::Fhss => (DutyGovernor::fhss_channel(), FHSS_DWELL_BURST_MS, FHSS_PERMIL),
        Governor::Custom { cap_ms, permil } => (DutyGovernor::new(cap_ms, permil), cap_ms, permil),
    };
    let mut m = Model {
        budget: cap as u64,
        cap: cap as u64,
        permil: permil as u64,
        residue: 0,
    };
    let (mut spent, mut credited_permil_ms) = (0u128, 0u128);
    for op in ops {
        match *op {
            GovOp::Refill(e) => {
                g.refill_ms(e);
                m.refill(e);
                credited_permil_ms += e as u128 * permil as u128;
            }
            GovOp::Consume(t) => {
                let ok = g.try_consume(t);
                assert_eq!(ok, m.consume(t), "try_consume({t}) disagrees with the model");
                if ok {
                    spent += t as u128;
                }
            }
            GovOp::Frame(n) => {
                let t = frame_toa_ms(n as usize);
                let ok = g.try_consume(t);
                assert_eq!(ok, m.consume(t), "frame consume disagrees with the model");
                if ok {
                    spent += t as u128;
                }
            }
        }
        assert_eq!(g.budget_ms() as u64, m.budget, "budget diverged from the model");
        assert!(g.budget_ms() <= cap, "bucket above its cap");
        // The token-bucket bound the regulatory sizing relies on: spend <= cap + duty * time.
        assert!(
            spent <= cap as u128 + credited_permil_ms / 1000,
            "airtime above the duty bound"
        );
    }
}

fn check_toa(len: usize) {
    let toa = frame_toa_ms(len);
    let bits = (len as u128 + 4 + 4 + 1 + 2) * 8;
    let exact_ceil = (bits * 1000).div_ceil(BITRATE as u128);
    if exact_ceil <= u32::MAX as u128 {
        assert_eq!(
            toa as u128, exact_ceil,
            "frame_toa_ms({len}) is not the exact ceiling"
        );
    } else {
        assert_eq!(toa, u32::MAX, "frame_toa_ms({len}) must saturate, never wrap low");
    }
    if let Some(next) = len.checked_add(1) {
        assert!(frame_toa_ms(next) >= toa, "frame_toa_ms not monotonic at {len}");
    }
}

fn check_hop<const N: usize>(seed: u32, cycle_a: u32, cycle_b: u32) {
    let mut seen = [false; N];
    for i in 0..N {
        let ch = hop_channel::<N>(seed, cycle_a, i as u8);
        assert!((ch as usize) < N, "channel {ch} out of range for N={N}");
        assert!(
            !seen[ch as usize],
            "channel {ch} repeated: not a permutation (N={N}, seed {seed})"
        );
        seen[ch as usize] = true;
        assert_eq!(
            hop_channel::<N>(seed, cycle_b, i as u8),
            ch,
            "permutation changed across cycles: breaks the §15.247 spacing"
        );
    }
}

/// Pure-Rust AES-128 as the CCM block cipher (the device plugs in the L0 hardware AES).
struct SoftAes;

impl AesBlock for SoftAes {
    fn encrypt_block(&mut self, key: &[u8; 16], block: &mut [u8; 16]) {
        let cipher = Aes128::new(&Array(*key));
        let mut b = Array(*block);
        cipher.encrypt_block(&mut b);
        block.copy_from_slice(&b.0);
    }
}

type RefCcm = ccm::Ccm<Aes128, U8, U13>;

fn ref_open(r: &RefCcm, nonce: &[u8; NONCE_LEN], aad: &[u8], data: &mut [u8], tag: &[u8; TAG_LEN]) -> bool {
    r.decrypt_inout_detached(&Array(*nonce), aad, data.into(), &Array(*tag))
        .is_ok()
}

fn check_ccm(c: &CcmCase<'_>) {
    // `aad.len() <= MAX_AAD` is the documented precondition (the stack's AAD is the <= 17 B
    // frame header); the radio MTU bounds the payload far below CCM's L=2 limit.
    let aad = &c.aad[..c.aad.len().min(MAX_AAD)];
    let data = &c.data[..c.data.len().min(512)];
    let mut ccm = Ccm::new(SoftAes);
    let r = RefCcm::new(&Array(c.key));

    // Seal: identical to the reference.
    let mut ct = data.to_vec();
    let tag = ccm.seal(&c.key, &c.nonce, aad, &mut ct);
    let mut ref_ct = data.to_vec();
    let ref_tag = r
        .encrypt_inout_detached(&Array(c.nonce), aad, ref_ct.as_mut_slice().into())
        .expect("reference seal");
    assert_eq!(ct, ref_ct, "ciphertext differs from the reference CCM");
    assert_eq!(&tag[..], &ref_tag[..], "tag differs from the reference CCM");

    // Open: round trip.
    let mut pt = ct.clone();
    assert!(
        ccm.open(&c.key, &c.nonce, aad, &mut pt, &tag),
        "open refused its own seal"
    );
    assert_eq!(pt, data);

    // Single-bit tamper anywhere (nonce, AAD, ciphertext, tag) must be refused, like the reference.
    let mut nonce = c.nonce;
    let mut aad_t = aad.to_vec();
    let mut ct_t = ct.clone();
    let mut tag_t = tag;
    let total_bits = (NONCE_LEN + aad_t.len() + ct_t.len() + TAG_LEN) * 8;
    let bit = c.flip as usize % total_bits;
    let (byte, mask) = (bit / 8, 1u8 << (bit % 8));
    if byte < NONCE_LEN {
        nonce[byte] ^= mask;
    } else if byte < NONCE_LEN + aad_t.len() {
        aad_t[byte - NONCE_LEN] ^= mask;
    } else if byte < NONCE_LEN + aad_t.len() + ct_t.len() {
        ct_t[byte - NONCE_LEN - aad_t.len()] ^= mask;
    } else {
        tag_t[byte - NONCE_LEN - aad_t.len() - ct_t.len()] ^= mask;
    }
    let mut ref_buf = ct_t.clone();
    assert!(!ref_open(&r, &nonce, &aad_t, &mut ref_buf, &tag_t));
    assert!(
        !ccm.open(&c.key, &nonce, &aad_t, &mut ct_t, &tag_t),
        "tampered frame accepted"
    );

    // Garbage in (the payload as ciphertext, a forged tag): same verdict as the reference.
    let mut g1 = data.to_vec();
    let mut g2 = data.to_vec();
    assert_eq!(
        ccm.open(&c.key, &c.nonce, aad, &mut g1, &c.forged),
        ref_open(&r, &c.nonce, aad, &mut g2, &c.forged),
        "open verdict differs from the reference on forged input"
    );

    // Key-check value: the first 4 bytes (LE) of AES_K(0^128).
    let mut zero = [0u8; 16];
    SoftAes.encrypt_block(&c.key, &mut zero);
    assert_eq!(
        ccm.key_check(&c.key),
        u32::from_le_bytes([zero[0], zero[1], zero[2], zero[3]])
    );
}

fuzz_target!(|input: Input<'_>| {
    check_governor(&input.governor, &input.ops);

    for &len in &input.toa_lens {
        check_toa(len);
    }

    let (seed, cycle_a, cycle_b, sel) = input.hop;
    // The firmware's channel count (`config::FHSS_N` = 80), plus edge sizes of the generic kernel.
    check_hop::<80>(seed, cycle_a, cycle_b);
    match sel % 5 {
        0 => check_hop::<1>(seed, cycle_a, cycle_b),
        1 => check_hop::<2>(seed, cycle_a, cycle_b),
        2 => check_hop::<7>(seed, cycle_a, cycle_b),
        3 => check_hop::<64>(seed, cycle_a, cycle_b),
        _ => check_hop::<256>(seed, cycle_a, cycle_b),
    }

    check_ccm(&input.ccm);
});
