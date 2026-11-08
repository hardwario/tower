//! Radio application schema: what node apps put **inside** their encrypted radio
//! frames, and what the host decodes back out of a forwarded
//! [`Uplink`](crate::msg::Uplink).
//!
//! The gateway is a *transparent bridge*: it authenticates/decrypts the radio frame
//! (net layer) and forwards the payload verbatim; it never decodes this schema. Only
//! the **node firmware** and the **host CLI** must agree on it, which is why it carries a
//! schema tag of its own: [`RADIO_TAG`] (the radio schema's structural hash folded to 16 bits,
//! [`fingerprint::RADIO_HASH`](crate::fingerprint::RADIO_HASH)) leads every envelope, apart from
//! the console's [`CONSOLE_TAG`](crate::CONSOLE_TAG).
//!
//! Envelope: `RADIO_TAG (2, LE) ‖ postcard(NodeMsg | NodeCmd)`, always ≤
//! [`MAX_RADIO_PAYLOAD`] bytes (the radio MTU; the firmware static-asserts equality
//! with its net-layer `MAX_PAYLOAD`).
//!
//! Evolution: change the types and rebuild every end from the same checkout; the tag follows.
//! A node or host built from another checkout is refused per envelope
//! ([`Error::SchemaMismatch`]), never half-understood.

use postcard_schema::Schema;
use serde::{Deserialize, Serialize};

use crate::{Error, from_bytes_exact};

/// The radio schema tag leading every envelope (little-endian):
/// [`fingerprint::RADIO_HASH`](crate::fingerprint::RADIO_HASH) folded to 16 bits.
pub const RADIO_TAG: u16 = crate::fingerprint::fold16(crate::fingerprint::RADIO_HASH);

/// Envelope header: the 2-byte [`RADIO_TAG`].
pub const ENVELOPE_HDR: usize = 2;

/// Max envelope size = the radio MTU (net-layer `MAX_PAYLOAD`). One `NodeMsg` /
/// `NodeCmd` always fits a single radio frame: there is no radio-level chunking;
/// [`NodeShellChunk`] chunks *content* instead.
pub const MAX_RADIO_PAYLOAD: usize = 74;

/// Shell text bytes per [`NodeShellChunk`] / longest [`NodeCmd::Shell`] line: the
/// MTU minus the worst-case envelope overhead, with margin. Pinned ≤ MTU by test.
pub const RADIO_SHELL_CHUNK: usize = 56;

/// The most property values one [`NodeMsg::Report`] carries.
pub const MAX_REPORT: usize = 8;

/// Node identity + health, sent at boot and every heartbeat period. The host derives the node's
/// kind from `firmware_name` (`radio_<kind>`, see [`props::kind_of_firmware`](crate::props::kind_of_firmware)),
/// auto-names unnamed nodes from it, and uses `session_id` to tell a counter reset (reboot) from a
/// wrapped counter.
#[derive(Serialize, Deserialize, Schema, Clone, PartialEq, Eq, Debug)]
pub struct NodeInfo<'a> {
    /// ≤ 24 bytes.
    pub firmware_name: &'a str,
    /// ≤ 8 bytes.
    pub firmware_version: &'a str,
    pub session_id: u32,
    /// The node sleeps between uplinks (downlinks must be queued on the gateway); `false` for a
    /// listening node, which keeps its receiver on.
    pub sleeping: bool,
    /// The node has no wall-clock time, or its last sync is old: the host answers with
    /// [`NodeCmd::Time`].
    pub wants_time: bool,
}

/// One chunk of a remote-shell response (mirrors the console `ShellResponse`
/// discipline: `result` authoritative on the `last` chunk, `chunk` indexes detect
/// a lost chunk mid-response).
#[derive(Serialize, Deserialize, Schema, Clone, PartialEq, Eq, Debug)]
pub struct NodeShellChunk<'a> {
    pub cmd_id: u16,
    pub result: u8,
    pub chunk: u16,
    pub last: bool,
    /// ≤ [`RADIO_SHELL_CHUNK`] bytes.
    pub text: &'a str,
}

/// Node → host application messages. Variant order is the wire; never reorder.
#[derive(Serialize, Deserialize, Schema, Clone, PartialEq, Eq, Debug)]
pub enum NodeMsg<'a> {
    #[serde(borrow)]
    Info(NodeInfo<'a>),
    /// Current values of properties, `(id, value)` (see [`props`](crate::props)): what changed,
    /// and every state and setting on a heartbeat.
    Report(heapless::Vec<(u8, i32), MAX_REPORT>),
    /// The event property `id` happened; `value` is its detail, `count` the running count of this
    /// event since boot (RAM-only: a reset means the node rebooted, see [`NodeInfo::session_id`]),
    /// so the host can see events lost on the air.
    Event { id: u8, value: i32, count: u32 },
    #[serde(borrow)]
    Shell(NodeShellChunk<'a>),
}

/// Host → node application messages: from the gateway's downlink queue on a sleeping node's next
/// uplink, or at once to a listening node. Variant order is the wire; never reorder.
#[derive(Serialize, Deserialize, Schema, Clone, PartialEq, Eq, Debug)]
pub enum NodeCmd<'a> {
    /// Run `line` in the node's shell dispatcher; respond with [`NodeMsg::Shell`]
    /// chunks correlated by `cmd_id`. `epoch` is a per-host-process random tag: the
    /// node de-duplicates the gateway's at-least-once re-deliveries on `(epoch,
    /// cmd_id)`, so a fresh host that reuses a low `cmd_id` can never be mistaken for
    /// a re-delivery a node already ran (the "empty checkmark" bug). The reply chunks
    /// carry only `cmd_id`; correlation is within one host process.
    Shell { epoch: u32, cmd_id: u16, line: &'a str },
    /// Set the control or setting property `id` to `value`. The node answers with a
    /// [`NodeMsg::Report`] of the value it now holds (idempotent, so a re-delivery is harmless).
    Set { id: u8, value: i32 },
    /// Wall-clock time: Unix seconds (UTC) when the host sent it, and the host's UTC offset in
    /// minutes then (local time = `unix + 60 * utc_offset_min`).
    Time { unix: u32, utc_offset_min: i16 },
    /// Send [`NodeMsg::Info`] and a full [`NodeMsg::Report`] (a host that does not know the
    /// node's kind yet, after its own restart, asks rather than wait for the next heartbeat).
    Describe,
}

fn encode_envelope<T: Serialize>(payload: &T, out: &mut [u8]) -> Result<usize, Error> {
    if out.len() < ENVELOPE_HDR {
        return Err(Error::Overflow);
    }
    out[..ENVELOPE_HDR].copy_from_slice(&RADIO_TAG.to_le_bytes());
    let used = postcard::to_slice(payload, &mut out[ENVELOPE_HDR..])
        .map_err(|_| Error::Encode)?
        .len();
    let total = ENVELOPE_HDR + used;
    if total > MAX_RADIO_PAYLOAD {
        // Reject rather than hand the net layer an over-MTU payload it would refuse
        // anyway; the error should point at the message, not the radio.
        return Err(Error::Encode);
    }
    Ok(total)
}

fn split_envelope(data: &[u8]) -> Result<&[u8], Error> {
    if data.len() < ENVELOPE_HDR {
        return Err(Error::TooShort);
    }
    let (tag, body) = data.split_at(ENVELOPE_HDR);
    // No integrity check to order this against (unlike the console's CRC-before-tag in
    // `decode_frame`): the envelope carries no CRC of its own because it is carved out of a
    // net-layer payload that AES-CCM already authenticated, so the tag is never line noise,
    // and a mismatch here is always a real schema mismatch (a node or host built from another
    // checkout), reported instead of silently mis-decoded.
    let got = u16::from_le_bytes([tag[0], tag[1]]);
    if got != RADIO_TAG {
        return Err(Error::SchemaMismatch { got });
    }
    Ok(body)
}

/// Encode a [`NodeMsg`] envelope into `out`; returns the byte count (≤
/// [`MAX_RADIO_PAYLOAD`]). Pure computation.
pub fn encode_node_msg(msg: &NodeMsg<'_>, out: &mut [u8]) -> Result<usize, Error> {
    encode_envelope(msg, out)
}

/// Decode a [`NodeMsg`] envelope (e.g. the `data` of a forwarded `Uplink`). A body that
/// fails to parse **or carries trailing bytes** is [`Error::Malformed`] (the net layer hands
/// over the exact payload length, so a tail means a node built from another schema).
pub fn decode_node_msg(data: &[u8]) -> Result<NodeMsg<'_>, Error> {
    from_bytes_exact(split_envelope(data)?)
}

/// Encode a [`NodeCmd`] envelope into `out` (the bytes a host hands to the
/// gateway's downlink queue); returns the byte count (≤ [`MAX_RADIO_PAYLOAD`]).
pub fn encode_node_cmd(cmd: &NodeCmd<'_>, out: &mut [u8]) -> Result<usize, Error> {
    encode_envelope(cmd, out)
}

/// Decode a [`NodeCmd`] envelope (what a node receives in its downlink window). A body that
/// fails to parse or carries trailing bytes is [`Error::Malformed`].
pub fn decode_node_cmd(data: &[u8]) -> Result<NodeCmd<'_>, Error> {
    from_bytes_exact(split_envelope(data)?)
}
