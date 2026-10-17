//! The wire fingerprint (`WIRE_FINGERPRINT`, `src/fingerprint.rs`): its pinned value and the
//! properties of the structural hasher behind it.
//!
//! The two components are pinned where the wire-bump guard sees them (`CONSOLE_HASH` in
//! `tests/golden.rs`, `RADIO_HASH` in `tests/radio_golden.rs`); this file pins the combined
//! 32-bit value the devices actually announce, and proves the hasher's contract on test-only
//! types: names never matter, structure always does.

use postcard_schema::Schema;
use tower_protocol::fingerprint::{CONSOLE_HASH, RADIO_HASH, combine, fold32, payload_schema, schema_hash};
use tower_protocol::msg::Hello;
use tower_protocol::{MAX_WIRE, Msg, MsgType, PROTOCOL_VERSION, WIRE_FINGERPRINT, decode_msg, encode_frame};

/// The value every v1.6.0 build announces in `Hello` / `DeviceInfo`. If this moved, a component
/// pin moved too (and the guard demanded that component's version bump).
#[test]
fn wire_fingerprint_is_pinned() {
    assert_eq!(WIRE_FINGERPRINT, 0x3b04_c858);
    assert_eq!(WIRE_FINGERPRINT, combine(CONSOLE_HASH, RADIO_HASH));
}

#[test]
fn fold32_is_the_documented_xor_fold() {
    assert_eq!(fold32(0x0000_0001_0000_0000), 0x0000_0001);
    assert_eq!(fold32(0xFFFF_FFFF_0000_0000), 0xFFFF_FFFF);
    assert_eq!(fold32(0x1234_5678_1234_5678), 0);
}

/// `MsgType::ALL` is what the console hash iterates: it must hold every type `from_u8` knows,
/// exactly once, in ascending discriminant order.
#[test]
fn msg_type_all_is_complete_and_ascending() {
    for v in 0..=0x1Fu8 {
        let known = MsgType::from_u8(v);
        let listed = MsgType::ALL.iter().filter(|&&mt| mt as u8 == v).count();
        assert_eq!(listed, usize::from(known.is_some()), "discriminant {v:#04x}");
    }
    assert!(MsgType::ALL.windows(2).all(|w| (w[0] as u8) < (w[1] as u8)));
}

/// Every message type pairs with a payload schema (the match in `payload_schema` is exhaustive),
/// and `Hello` pairs with `Hello`.
#[test]
fn payload_schema_pairs_hello_with_hello() {
    assert_eq!(payload_schema(MsgType::Hello), <Hello<'static> as Schema>::SCHEMA);
}

/// The announce path end to end: a `Hello` carrying this build's fingerprint decodes back to it.
#[test]
fn hello_carries_the_fingerprint() {
    let h = Hello {
        protocol_version: PROTOCOL_VERSION,
        firmware_name: "app",
        firmware_version: "v0.1.0",
        session_id: 9,
        wire_fingerprint: WIRE_FINGERPRINT,
    };
    let mut out = [0u8; MAX_WIRE];
    let n = encode_frame(MsgType::Hello, 0, &h, &mut out).unwrap();
    let mut dec = tower_protocol::FrameDecoder::new();
    let inner: Vec<u8> = out[..n]
        .iter()
        .find_map(|&b| dec.push(b).map(|s| s.to_vec()))
        .unwrap();
    match decode_msg(&inner).unwrap().1 {
        Msg::Hello(back) => assert_eq!(back.wire_fingerprint, WIRE_FINGERPRINT),
        other => panic!("expected Hello, got {other:?}"),
    }
}

// --- the hasher's contract, on test-only types ----------------------------------------

/// Two families of types that differ ONLY in names (type, field and variant names).
mod named {
    #![allow(dead_code)]
    use postcard_schema::Schema;

    #[derive(Schema)]
    pub struct Reading<'a> {
        pub sensor: u8,
        pub label: &'a str,
        pub kind: Kind,
        pub extra: Option<u16>,
        pub blob: &'a [u8],
    }

    #[derive(Schema)]
    pub enum Kind {
        Idle,
        Value(u32),
        Pair { lo: i16, hi: i16 },
    }
}

mod renamed {
    #![allow(dead_code)]
    use postcard_schema::Schema;

    #[derive(Schema)]
    pub struct Sample<'a> {
        pub id: u8,
        pub text: &'a str,
        pub mode: Mode,
        pub more: Option<u16>,
        pub data: &'a [u8],
    }

    #[derive(Schema)]
    pub enum Mode {
        Off,
        Number(u32),
        Range { min: i16, max: i16 },
    }
}

#[test]
fn names_are_ignored() {
    assert_eq!(schema_hash::<named::Reading>(), schema_hash::<renamed::Sample>());
    assert_eq!(schema_hash::<named::Kind>(), schema_hash::<renamed::Mode>());
}

/// Pairs of test-only types that differ in exactly one structural respect.
mod shapes {
    #![allow(dead_code)]
    use postcard_schema::Schema;

    #[derive(Schema)]
    pub struct Ab {
        pub a: u8,
        pub b: u16,
    }
    #[derive(Schema)]
    pub struct Ba {
        pub b: u16,
        pub a: u8,
    }

    #[derive(Schema)]
    pub enum UnitFirst {
        A,
        B(u8),
    }
    #[derive(Schema)]
    pub enum NewtypeFirst {
        B(u8),
        A,
    }

    #[derive(Schema)]
    pub struct W16 {
        pub v: u16,
    }
    #[derive(Schema)]
    pub struct W32 {
        pub v: u32,
    }
    #[derive(Schema)]
    pub struct I32 {
        pub v: i32,
    }

    #[derive(Schema)]
    pub struct One {
        pub a: u8,
    }
    #[derive(Schema)]
    pub struct Two {
        pub a: u8,
        pub b: u8,
    }

    #[derive(Schema)]
    pub enum OneVariant {
        A,
    }
    #[derive(Schema)]
    pub enum TwoVariants {
        A,
        B,
    }

    #[derive(Schema)]
    pub enum UnitShape {
        A,
    }
    #[derive(Schema)]
    pub enum NewtypeShape {
        A(u8),
    }
    #[derive(Schema)]
    pub enum StructShape {
        A { x: u8, y: u8 },
    }

    #[derive(Schema)]
    pub struct Text<'a> {
        pub v: &'a str,
    }
    #[derive(Schema)]
    pub struct Bytes<'a> {
        pub v: &'a [u8],
    }

    #[derive(Schema)]
    pub struct Plain {
        pub v: u8,
    }
    #[derive(Schema)]
    pub struct Optional {
        pub v: Option<u8>,
    }
    #[derive(Schema)]
    pub struct Array {
        pub v: [u8; 4],
    }
    #[derive(Schema)]
    pub struct LongerArray {
        pub v: [u8; 5],
    }

    #[derive(Schema)]
    pub struct Cap4 {
        pub v: heapless::Vec<u8, 4>,
    }
    #[derive(Schema)]
    pub struct Cap8 {
        pub v: heapless::Vec<u8, 8>,
    }
}

use shapes::*;

#[test]
fn field_reorder_changes_the_hash() {
    assert_ne!(schema_hash::<Ab>(), schema_hash::<Ba>());
}

#[test]
fn variant_reorder_changes_the_hash() {
    assert_ne!(schema_hash::<UnitFirst>(), schema_hash::<NewtypeFirst>());
}

#[test]
fn integer_width_and_signedness_change_the_hash() {
    assert_ne!(schema_hash::<W16>(), schema_hash::<W32>());
    assert_ne!(schema_hash::<W32>(), schema_hash::<I32>());
}

#[test]
fn an_added_field_or_variant_changes_the_hash() {
    assert_ne!(schema_hash::<One>(), schema_hash::<Two>());
    assert_ne!(schema_hash::<OneVariant>(), schema_hash::<TwoVariants>());
}

#[test]
fn variant_shape_changes_the_hash() {
    assert_ne!(schema_hash::<UnitShape>(), schema_hash::<NewtypeShape>());
    assert_ne!(schema_hash::<NewtypeShape>(), schema_hash::<StructShape>());
    assert_ne!(schema_hash::<UnitShape>(), schema_hash::<StructShape>());
}

#[test]
fn string_vs_bytes_and_nesting_change_the_hash() {
    assert_ne!(schema_hash::<Text>(), schema_hash::<Bytes>());
    assert_ne!(schema_hash::<Plain>(), schema_hash::<Optional>());
    // A fixed array is a tuple on the wire (no length prefix), so its length is structure.
    assert_ne!(schema_hash::<Array>(), schema_hash::<LongerArray>());
}

/// Documented NON-coverage: a `heapless::Vec` capacity never reaches the wire (sequences are
/// length-prefixed), so it does not move the fingerprint.
#[test]
fn heapless_capacity_is_not_covered() {
    assert_eq!(schema_hash::<Cap4>(), schema_hash::<Cap8>());
}
