//! The wire fingerprint: a compile-time structural hash of the postcard schema.
//!
//! postcard is not self-describing, and the 3-bit [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION)
//! only protects a link if someone remembered to bump it. [`WIRE_FINGERPRINT`](crate::WIRE_FINGERPRINT)
//! closes that gap: it is derived from the schema itself, so two builds whose wire schemas
//! differ get different fingerprints even under the same `PROTOCOL_VERSION` (a forgotten
//! bump, or two consumers pinning different tower-protocol tags). The device announces its
//! fingerprint in [`Hello`](crate::msg::Hello) and in the [`DeviceInfo`](crate::mgmt::DeviceInfo)
//! `Describe` reply; a host compares it with its own and treats a difference exactly like a
//! version mismatch.
//!
//! # How it is computed
//!
//! Every wire type derives [`postcard_schema::Schema`], which reflects the type into a tree of
//! serde data-model nodes at compile time. [`schema_hash`] walks that tree with a `const fn`
//! FNV-1a 64 hasher over a **prefix-free** encoding of it:
//!
//! * one tag byte per data-model kind (`u8`, `i16`, `u32`, string, `Option`, sequence, tuple,
//!   struct, enum, ...), so integer widths and signedness, string vs byte sequence, and every
//!   kind of nesting are distinguished;
//! * an element count before the children of every struct, tuple and enum, then the children
//!   **in declaration order**, so field order, variant order, an added or removed field or
//!   variant, and each variant's shape (unit / newtype / tuple / struct and its contents) all
//!   change the hash;
//! * **no names at all**: type names, field names and variant names are skipped, because
//!   postcard never encodes them. A rename is wire-neutral and leaves the fingerprint unchanged
//!   (the v1.4.0 and v1.5.0 renames would have kept it).
//!
//! It is deliberately conservative: a few changes that happen to be byte-compatible still move
//! it (a struct turned into a tuple struct, `&[u8]` swapped for a serde byte string, a `u16`
//! widened to `u32` even though small values encode the same). Those are rare and erring
//! toward "mismatch" is the safe side.
//!
//! The 64-bit hash is xor-folded to 32 bits for the wire (`(h >> 32) ^ h`, truncated). 32 bits
//! are ample against accidental drift (about one chance in 4 * 10^9 that two different schemas
//! collide); this is an integrity check, not a defence against a deliberate forgery.
//!
//! # What is covered
//!
//! [`WIRE_FINGERPRINT`](crate::WIRE_FINGERPRINT) = [`combine`]`(`[`CONSOLE_HASH`]`, `[`RADIO_HASH`]`)`:
//!
//! * [`CONSOLE_HASH`], the console surface:
//!   - every [`MsgType`], in the fixed order of [`MsgType::ALL`] (ascending discriminant): its
//!     discriminant **value** (the low 5 bits of `ver_type` on the wire) paired with the schema
//!     of the payload type that `MsgType` carries ([`payload_schema`]);
//!   - the management surface, in this fixed order: [`MgmtOp`](crate::mgmt::MgmtOp) and every
//!     reply record streamed in `MgmtResponse.data`: [`DeviceInfo`](crate::mgmt::DeviceInfo),
//!     [`NodeEntry`](crate::mgmt::NodeEntry), [`NodeKey`](crate::mgmt::NodeKey),
//!     [`Paired`](crate::mgmt::Paired), [`QueueId`](crate::mgmt::QueueId),
//!     [`QueueEntry`](crate::mgmt::QueueEntry), [`ProvisionAck`](crate::mgmt::ProvisionAck),
//!     [`Joined`](crate::mgmt::Joined) (and, nested inside those, `Provision`, `DeviceRole`).
//! * [`RADIO_HASH`], the radio application schema: [`NodeMsg`](crate::radio::NodeMsg) and
//!   [`NodeCmd`](crate::radio::NodeCmd) (and, nested, `NodeInfo`, `NodeShellChunk`,
//!   `ButtonKind`, `AccelKind`).
//!
//! The two components are pinned separately, each next to the version constant that guards
//! it: `CONSOLE_HASH` in `tests/golden.rs` (`PROTOCOL_VERSION`), `RADIO_HASH` in
//! `tests/radio_golden.rs` (`RADIO_SCHEMA_VERSION`), so `tools/check_wire_bump.py` demands the
//! matching bump for a schema change on either surface. A radio-only schema change therefore
//! moves `WIRE_FINGERPRINT` without a `PROTOCOL_VERSION` bump (the console bytes did not
//! change); the exchanged fingerprint still flags a gateway built from a different tag.
//!
//! # What is NOT covered
//!
//! * Names (by design, see above).
//! * `heapless` capacities (the `N` of `Vec<T, N>`): not on the wire, since a sequence is
//!   length-prefixed. A receiver whose capacity is smaller than the sender's rejects a longer
//!   list as `Malformed` instead of mis-decoding it. String length limits (`MAX_NODE_NAME`,
//!   the documented byte caps) are likewise not part of the schema.
//! * Constant values and their meaning: `PROTOCOL_VERSION` and `RADIO_SCHEMA_VERSION` themselves
//!   (they ride on the wire on their own), the `mgmt::MGMT_*` result codes, `TX_*` outcomes,
//!   `NODE_FLAG_*`, `BAND_*`, the `LAST_SEEN_NEVER` / `RSSI_NONE` sentinels.
//! * The framing: COBS, the CRC-32 trailer, the `ver_type`/`seq` header, `MAX_FRAME` /
//!   `MAX_PAYLOAD`, the radio envelope's leading version byte, `MAX_RADIO_PAYLOAD`,
//!   `RADIO_SHELL_CHUNK`.
//! * Which mgmt op answers with which record (only the records' shapes are hashed).
//! * serde attributes that change the encoding (`skip`, `with`, `flatten`, `untagged`,
//!   `transparent`, ...): the `Schema` derive does not see them, so this crate uses none (only
//!   `borrow`, which does not affect the bytes). Adding one would desynchronize the fingerprint
//!   from the real encoding.
//!
//! # Stability
//!
//! The tag bytes and traversal order below are part of the contract: changing them changes
//! every fingerprint, which the golden pins report as a wire change. The value also depends on
//! how `postcard-schema` / `postcard-derive` reflect types, which is why both are pinned to an
//! exact version in `Cargo.toml` (each consumer recomputes the fingerprint in its own build).

use postcard_schema::Schema;
use postcard_schema::schema::{DataModelType, DataModelVariant, NamedType, NamedValue, NamedVariant};

use crate::{MsgType, mgmt, msg, radio};

const FNV_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

// Data-model kind tags. Any distinct values work; these are fixed forever (see "Stability").
const T_BOOL: u8 = 0x01;
const T_I8: u8 = 0x02;
const T_U8: u8 = 0x03;
const T_I16: u8 = 0x04;
const T_I32: u8 = 0x05;
const T_I64: u8 = 0x06;
const T_I128: u8 = 0x07;
const T_U16: u8 = 0x08;
const T_U32: u8 = 0x09;
const T_U64: u8 = 0x0A;
const T_U128: u8 = 0x0B;
const T_USIZE: u8 = 0x0C;
const T_ISIZE: u8 = 0x0D;
const T_F32: u8 = 0x0E;
const T_F64: u8 = 0x0F;
const T_CHAR: u8 = 0x10;
const T_STRING: u8 = 0x11;
const T_BYTE_ARRAY: u8 = 0x12;
const T_OPTION: u8 = 0x13;
const T_UNIT: u8 = 0x14;
const T_UNIT_STRUCT: u8 = 0x15;
const T_NEWTYPE_STRUCT: u8 = 0x16;
const T_SEQ: u8 = 0x17;
const T_TUPLE: u8 = 0x18;
const T_TUPLE_STRUCT: u8 = 0x19;
const T_MAP: u8 = 0x1A;
const T_STRUCT: u8 = 0x1B;
const T_ENUM: u8 = 0x1C;
const T_SCHEMA: u8 = 0x1D;
// Enum variant shape tags.
const V_UNIT: u8 = 0x21;
const V_NEWTYPE: u8 = 0x22;
const V_TUPLE: u8 = 0x23;
const V_STRUCT: u8 = 0x24;
// Section tags for the top-level composition.
const S_MSG: u8 = 0x40;
const S_MGMT: u8 = 0x41;
const S_RADIO: u8 = 0x42;

const fn feed(mut state: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        state ^= bytes[i] as u64;
        state = state.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    state
}

const fn feed_u8(state: u64, byte: u8) -> u64 {
    feed(state, &[byte])
}

/// An element count, so a node's children are delimited (the encoding stays prefix-free).
const fn feed_count(state: u64, count: usize) -> u64 {
    feed(state, &(count as u32).to_le_bytes())
}

const fn hash_types(state: u64, tag: u8, types: &[&NamedType]) -> u64 {
    let mut state = feed_count(feed_u8(state, tag), types.len());
    let mut i = 0;
    while i < types.len() {
        state = hash_type(state, types[i]);
        i += 1;
    }
    state
}

const fn hash_fields(state: u64, tag: u8, fields: &[&NamedValue]) -> u64 {
    let mut state = feed_count(feed_u8(state, tag), fields.len());
    let mut i = 0;
    while i < fields.len() {
        // `fields[i].name` is deliberately NOT hashed: postcard encodes fields by position.
        state = hash_type(state, fields[i].ty);
        i += 1;
    }
    state
}

const fn hash_variants(state: u64, variants: &[&NamedVariant]) -> u64 {
    let mut state = feed_count(feed_u8(state, T_ENUM), variants.len());
    let mut i = 0;
    while i < variants.len() {
        // `variants[i].name` is deliberately NOT hashed: postcard encodes the variant index,
        // which is the position in this list.
        state = match variants[i].ty {
            DataModelVariant::UnitVariant => feed_u8(state, V_UNIT),
            DataModelVariant::NewtypeVariant(inner) => hash_type(feed_u8(state, V_NEWTYPE), inner),
            DataModelVariant::TupleVariant(types) => hash_types(state, V_TUPLE, types),
            DataModelVariant::StructVariant(fields) => hash_fields(state, V_STRUCT, fields),
        };
        i += 1;
    }
    state
}

/// Fold one type's schema into a running FNV-1a 64 `state`. The type's own name is ignored
/// (as are every nested field and variant name); only its data-model structure counts.
const fn hash_type(state: u64, ty: &NamedType) -> u64 {
    match ty.ty {
        DataModelType::Bool => feed_u8(state, T_BOOL),
        DataModelType::I8 => feed_u8(state, T_I8),
        DataModelType::U8 => feed_u8(state, T_U8),
        DataModelType::I16 => feed_u8(state, T_I16),
        DataModelType::I32 => feed_u8(state, T_I32),
        DataModelType::I64 => feed_u8(state, T_I64),
        DataModelType::I128 => feed_u8(state, T_I128),
        DataModelType::U16 => feed_u8(state, T_U16),
        DataModelType::U32 => feed_u8(state, T_U32),
        DataModelType::U64 => feed_u8(state, T_U64),
        DataModelType::U128 => feed_u8(state, T_U128),
        DataModelType::Usize => feed_u8(state, T_USIZE),
        DataModelType::Isize => feed_u8(state, T_ISIZE),
        DataModelType::F32 => feed_u8(state, T_F32),
        DataModelType::F64 => feed_u8(state, T_F64),
        DataModelType::Char => feed_u8(state, T_CHAR),
        DataModelType::String => feed_u8(state, T_STRING),
        DataModelType::ByteArray => feed_u8(state, T_BYTE_ARRAY),
        DataModelType::Option(inner) => hash_type(feed_u8(state, T_OPTION), inner),
        DataModelType::Unit => feed_u8(state, T_UNIT),
        DataModelType::UnitStruct => feed_u8(state, T_UNIT_STRUCT),
        DataModelType::NewtypeStruct(inner) => hash_type(feed_u8(state, T_NEWTYPE_STRUCT), inner),
        DataModelType::Seq(inner) => hash_type(feed_u8(state, T_SEQ), inner),
        DataModelType::Tuple(types) => hash_types(state, T_TUPLE, types),
        DataModelType::TupleStruct(types) => hash_types(state, T_TUPLE_STRUCT, types),
        DataModelType::Map { key, val } => hash_type(hash_type(feed_u8(state, T_MAP), key), val),
        DataModelType::Struct(fields) => hash_fields(state, T_STRUCT, fields),
        DataModelType::Enum(variants) => hash_variants(state, variants),
        DataModelType::Schema => feed_u8(state, T_SCHEMA),
    }
}

/// The 64-bit structural hash of `T`'s schema (names ignored; see the module docs). Usable in
/// `const` context.
pub const fn schema_hash<T: Schema + ?Sized>() -> u64 {
    hash_type(FNV_BASIS, T::SCHEMA)
}

/// Xor-fold a 64-bit hash to the 32 bits carried on the wire: `((h >> 32) ^ h) as u32`.
pub const fn fold32(hash: u64) -> u32 {
    ((hash >> 32) ^ hash) as u32
}

/// Combine the two surface hashes into the 32-bit wire value:
/// FNV-1a 64 over `console` then `radio` (each little-endian), then [`fold32`].
pub const fn combine(console: u64, radio: u64) -> u32 {
    fold32(feed(
        feed(FNV_BASIS, &console.to_le_bytes()),
        &radio.to_le_bytes(),
    ))
}

/// The payload type each [`MsgType`] carries (the type [`decode_msg`](crate::decode_msg)
/// deserializes for it). The match is exhaustive, so a new `MsgType` does not compile until it
/// is paired here and so covered by the fingerprint.
pub const fn payload_schema(msg_type: MsgType) -> &'static NamedType {
    match msg_type {
        MsgType::Hello => <msg::Hello<'static> as Schema>::SCHEMA,
        MsgType::Log => <msg::Log<'static> as Schema>::SCHEMA,
        MsgType::Print => <msg::Print<'static> as Schema>::SCHEMA,
        MsgType::Event => <msg::Event<'static> as Schema>::SCHEMA,
        MsgType::ShellResponse => <msg::ShellResponse<'static> as Schema>::SCHEMA,
        MsgType::ShellCompletions => <msg::ShellCompletions<'static> as Schema>::SCHEMA,
        MsgType::Dropped => <msg::Dropped as Schema>::SCHEMA,
        MsgType::MgmtResponse => <msg::MgmtResponse<'static> as Schema>::SCHEMA,
        MsgType::Uplink => <msg::Uplink<'static> as Schema>::SCHEMA,
        MsgType::RadioStat => <msg::RadioStat as Schema>::SCHEMA,
        MsgType::ShellCommand => <msg::ShellCommand<'static> as Schema>::SCHEMA,
        MsgType::ShellComplete => <msg::ShellComplete<'static> as Schema>::SCHEMA,
        MsgType::MgmtRequest => <msg::MgmtRequest<'static> as Schema>::SCHEMA,
    }
}

/// The management surface, in its fixed hashing order: the op enum, then every reply record.
const MGMT_TYPES: [&NamedType; 9] = [
    <mgmt::MgmtOp<'static> as Schema>::SCHEMA,
    <mgmt::DeviceInfo<'static> as Schema>::SCHEMA,
    <mgmt::NodeEntry<'static> as Schema>::SCHEMA,
    <mgmt::NodeKey as Schema>::SCHEMA,
    <mgmt::Paired as Schema>::SCHEMA,
    <mgmt::QueueId as Schema>::SCHEMA,
    <mgmt::QueueEntry<'static> as Schema>::SCHEMA,
    <mgmt::ProvisionAck as Schema>::SCHEMA,
    <mgmt::Joined as Schema>::SCHEMA,
];

/// The radio application surface, in its fixed hashing order.
const RADIO_TYPES: [&NamedType; 2] = [
    <radio::NodeMsg<'static> as Schema>::SCHEMA,
    <radio::NodeCmd<'static> as Schema>::SCHEMA,
];

const fn console_hash() -> u64 {
    let mut state = FNV_BASIS;
    let mut i = 0;
    while i < MsgType::ALL.len() {
        let msg_type = MsgType::ALL[i];
        state = feed(state, &[S_MSG, msg_type as u8]);
        state = hash_type(state, payload_schema(msg_type));
        i += 1;
    }
    hash_types(state, S_MGMT, &MGMT_TYPES)
}

/// 64-bit structural hash of the console surface: every `MsgType` value paired with its payload
/// schema, plus the mgmt op and reply-record schemas. Pinned in `tests/golden.rs`, so changing
/// it is a console wire change that needs a `PROTOCOL_VERSION` bump.
pub const CONSOLE_HASH: u64 = console_hash();

/// 64-bit structural hash of the radio application schema (`NodeMsg`, `NodeCmd`). Pinned in
/// `tests/radio_golden.rs`, so changing it needs a `RADIO_SCHEMA_VERSION` bump.
pub const RADIO_HASH: u64 = hash_types(FNV_BASIS, S_RADIO, &RADIO_TYPES);
