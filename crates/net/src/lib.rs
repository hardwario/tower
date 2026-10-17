//! tower-net (`crates/net`): the hardware-free, `no_std` decision core of the TOWER radio
//! network layer's security invariants (firmware/docs/radio.md). It is not the network layer:
//! that is the firmware SDK's `tower::radio::net`, which keeps all radio, timing and EEPROM I/O
//! and delegates each accept/reject decision to the kernels here.
//!
//! Extracted from `firmware/src/radio/net/{mod,fhss,pairing}.rs` and
//! `firmware/src/radio/frame.rs` so the
//! security-critical accept/reject arithmetic (the replay rule, the TX-counter reserve-ahead
//! watermark (CCM nonce anti-reuse), ACK delivery resolution, the FHSS beacon-epoch acceptance
//! machine, the pairing admission/confirm rules, the reserved-address rules, and the CCM nonce
//! construction) can be **unit-tested on the host** (`cargo test`). The firmware itself is
//! `no_std` with a thumbv6m default target and has no libtest, exactly the reason
//! `crates/kv` and `crates/radio` were split out; this follows that precedent.
//!
//! Everything here is `no_std`, dependency-free, and free of any real-time, radio, or storage
//! dependency: each kernel is a small struct holding counters/windows whose methods take
//! "persisted value" / "elapsed time" / "received fields" as arguments and return *decisions*.
//! The firmware keeps all I/O, async, radio and EEPROM flow and delegates each decision here,
//! so there is **zero** behavioural change on target and no external API change.
//!
//! The contracts these kernels encode are argued in firmware/docs/radio.md; see "Security model
//! (AES-128-CCM): the nonce-uniqueness argument" in its Design rationale section. In
//! particular: a CCM nonce counter is NEVER reused, even across power loss (the counter
//! resumes *at* the persisted watermark), and near u32 exhaustion the link fails **closed**.

#![no_std]

pub mod ack;
pub mod addr;
pub mod epoch;
pub mod handshake;
pub mod nonce;
pub mod pairing;
pub mod replay;
pub mod txctr;
