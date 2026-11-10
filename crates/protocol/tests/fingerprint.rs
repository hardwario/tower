//! The schema fingerprint behind the tags (`src/fingerprint.rs`): how the 16-bit tags are
//! derived, and the properties of the structural hasher.
//!
//! The tag values themselves are not pinned (they follow the schema by design: a pin would only
//! churn); what is pinned is how they are made and what the hasher sees: names never matter,
//! structure always does.

use postcard_schema::Schema;
use tower_protocol::fingerprint::{CONSOLE_HASH, RADIO_HASH, fold16, payload_schema, schema_hash};
use tower_protocol::msg::Hello;
use tower_protocol::radio::RADIO_TAG;
use tower_protocol::{CONSOLE_TAG, MsgType};

/// Each tag is its surface hash folded to 16 bits, and the two surfaces hash differently.
#[test]
fn tags_are_the_folded_surface_hashes() {
    assert_eq!(CONSOLE_TAG, fold16(CONSOLE_HASH));
    assert_eq!(RADIO_TAG, fold16(RADIO_HASH));
    assert_ne!(CONSOLE_HASH, RADIO_HASH);
}

#[test]
fn fold16_is_the_documented_xor_fold() {
    assert_eq!(fold16(0x0000_0000_0000_0001), 0x0001);
    assert_eq!(fold16(0x0001_0000_0000_0000), 0x0001);
    assert_eq!(fold16(0xFFFF_0000_0000_0000), 0xFFFF);
    assert_eq!(fold16(0x1234_1234_0000_0000), 0);
    assert_eq!(fold16(0x0001_0002_0004_0008), 0x000F);
    // A bit set in two of the four words cancels, whichever two they are (an OR would keep it).
    assert_eq!(fold16(0x0000_0000_0001_0001), 0);
    assert_eq!(fold16(0x0000_0001_0001_0000), 0);
    assert_eq!(fold16(0x0001_0001_0000_0000), 0);
    assert_eq!(fold16(0x0001_0000_0000_0001), 0);
    assert_eq!(fold16(0x8001_8001_8001_0001), 0x8000);
}

/// `MsgType::ALL` is what the console hash iterates: it must hold every type `from_u8` knows,
/// exactly once, in ascending discriminant order.
#[test]
fn msg_type_all_is_complete_and_ascending() {
    for v in 0..=u8::MAX {
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

    // Tuples: the elements are positional children, like fields.
    #[derive(Schema)]
    pub struct Pair16(pub u8, pub u16);
    #[derive(Schema)]
    pub struct Pair32(pub u8, pub u32);
    #[derive(Schema)]
    pub struct Riap16(pub u16, pub u8);
    #[derive(Schema)]
    pub struct InTuple16 {
        pub v: (u8, u16),
    }
    #[derive(Schema)]
    pub struct InTuple32 {
        pub v: (u8, u32),
    }
    #[derive(Schema)]
    pub enum TupleVariant16 {
        A(u8, u16),
    }
    #[derive(Schema)]
    pub enum TupleVariant32 {
        A(u8, u32),
    }

    // What comes BEFORE a nested struct, tuple or enum: the hash is one running state, so a
    // sibling ahead of a composite counts as much as one behind it.
    #[derive(Schema)]
    pub struct Inner {
        pub x: u8,
    }
    #[derive(Schema)]
    pub struct Lead8 {
        pub a: u8,
        pub inner: Inner,
    }
    #[derive(Schema)]
    pub struct Lead16 {
        pub a: u16,
        pub inner: Inner,
    }
    #[derive(Schema)]
    pub struct LeadTuple8 {
        pub a: u8,
        pub t: (u8, u8),
    }
    #[derive(Schema)]
    pub struct LeadTuple16 {
        pub a: u16,
        pub t: (u8, u8),
    }
    #[derive(Schema)]
    pub struct LeadEnum8 {
        pub a: u8,
        pub e: OneVariant,
    }
    #[derive(Schema)]
    pub struct LeadEnum16 {
        pub a: u16,
        pub e: OneVariant,
    }

    // The same leaves, grouped differently: only the element counts tell these apart.
    #[derive(Schema)]
    pub struct Split {
        pub x: (u8,),
        pub y: u8,
    }
    #[derive(Schema)]
    pub struct Joined {
        pub x: (u8, u8),
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

/// The elements of a tuple struct, of a tuple field and of a tuple variant are hashed like
/// fields: their types and their order.
#[test]
fn tuple_elements_change_the_hash() {
    assert_ne!(schema_hash::<Pair16>(), schema_hash::<Pair32>());
    assert_ne!(schema_hash::<Pair16>(), schema_hash::<Riap16>());
    assert_ne!(schema_hash::<InTuple16>(), schema_hash::<InTuple32>());
    assert_ne!(schema_hash::<TupleVariant16>(), schema_hash::<TupleVariant32>());
}

/// A field ahead of a nested struct, tuple or enum counts: entering a composite does not start
/// the hash over.
#[test]
fn a_field_before_a_nested_composite_changes_the_hash() {
    assert_ne!(schema_hash::<Lead8>(), schema_hash::<Lead16>());
    assert_ne!(schema_hash::<LeadTuple8>(), schema_hash::<LeadTuple16>());
    assert_ne!(schema_hash::<LeadEnum8>(), schema_hash::<LeadEnum16>());
}

/// The encoding is prefix-free: the same leaf types grouped differently (`((u8,), u8)` against
/// `((u8, u8),)`) differ by the element counts alone.
#[test]
fn grouping_changes_the_hash() {
    assert_ne!(schema_hash::<Split>(), schema_hash::<Joined>());
}

/// Documented NON-coverage: a `heapless::Vec` capacity never reaches the wire (sequences are
/// length-prefixed), so it does not move the tags.
#[test]
fn heapless_capacity_is_not_covered() {
    assert_eq!(schema_hash::<Cap4>(), schema_hash::<Cap8>());
}
