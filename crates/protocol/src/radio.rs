//! Radio application schema — what node apps put **inside** their encrypted radio
//! frames, and what the host decodes back out of a forwarded
//! [`Uplink`](crate::msg::Uplink).
//!
//! The gateway is a *transparent bridge*: it authenticates/decrypts the radio frame
//! (net layer) and forwards the payload verbatim — it never decodes this schema. Only
//! the **node firmware** and the **host CLI** must agree on it, which is why it is
//! versioned independently of the console framing: [`RADIO_SCHEMA_VERSION`] is the
//! leading byte of every envelope, and a schema change re-pins node firmware + CLI
//! while the gateway firmware stays put.
//!
//! Envelope: `[RADIO_SCHEMA_VERSION] ‖ postcard(NodeMsg | NodeCmd)`, always ≤
//! [`MAX_RADIO_PAYLOAD`] bytes (the radio MTU; the firmware static-asserts equality
//! with its net-layer `MAX_PAYLOAD`).
//!
//! Evolution rules are the crate's usual: never reorder fields/variants; appending
//! is still a schema change; any change bumps [`RADIO_SCHEMA_VERSION`] and regenerates
//! `tests/radio_golden.rs` (guarded by `tools/check_wire_bump.py`, independently of
//! `PROTOCOL_VERSION`).

use serde::{Deserialize, Serialize};

use crate::Error;

/// Leading byte of every radio application envelope.
pub const RADIO_SCHEMA_VERSION: u8 = 1;

/// Max envelope size = the radio MTU (net-layer `MAX_PAYLOAD`). One `NodeMsg` /
/// `NodeCmd` always fits a single radio frame — there is no radio-level chunking;
/// [`NodeShellChunk`] chunks *content* instead.
pub const MAX_RADIO_PAYLOAD: usize = 74;

/// Shell text bytes per [`NodeShellChunk`] / longest [`NodeCmd::Shell`] line: the
/// MTU minus the worst-case envelope overhead, with margin. Pinned ≤ MTU by test.
pub const RADIO_SHELL_CHUNK: usize = 56;

/// Button event kinds. Variant order pinned by test.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonKind {
    Press,
    Release,
    Click,
    Hold,
}

/// Accelerometer event kinds. Variant order pinned by test.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccelKind {
    /// Wake-on-motion (tilt interrupt) fired.
    Motion,
    /// The resting orientation changed; `face` says which side is up.
    Orientation,
}

/// Node identity + health, sent at boot and every heartbeat period. The host uses
/// `firmware_name` to auto-name unnamed nodes, and `session_id` to disambiguate a
/// counter reset (reboot) from a wrapped counter.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct NodeInfo<'a> {
    /// ≤ 24 bytes.
    pub firmware_name: &'a str,
    /// ≤ 8 bytes.
    pub firmware_version: &'a str,
    pub session_id: u32,
    /// The node sleeps between uplinks (downlinks must be queued on the gateway).
    pub sleeping: bool,
    /// Reserved until the SDK grows an ADC block; encode `None`.
    pub battery_mv: Option<u16>,
}

/// One chunk of a remote-shell response (mirrors the console `ShellResponse`
/// discipline: `result` authoritative on the `last` chunk, `chunk` indexes detect
/// a lost chunk mid-response).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct NodeShellChunk<'a> {
    pub cmd_id: u16,
    pub result: u8,
    pub chunk: u16,
    pub last: bool,
    /// ≤ [`RADIO_SHELL_CHUNK`] bytes.
    pub text: &'a str,
}

/// Node → host application messages. Variant order is the wire — never reorder.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum NodeMsg<'a> {
    #[serde(borrow)]
    Info(NodeInfo<'a>),
    /// One button event with the per-kind running count since boot (RAM-only —
    /// a reset means the node rebooted; see [`NodeInfo::session_id`]).
    Button { kind: ButtonKind, count: u32 },
    /// Temperature in millidegrees Celsius.
    Temperature { millic: i32 },
    /// `face` is the die side facing up (1..=6), 0 = unknown/moving.
    Accel { kind: AccelKind, face: u8 },
    #[serde(borrow)]
    Shell(NodeShellChunk<'a>),
}

/// Host → node application messages, delivered from the gateway's downlink queue
/// on the node's next uplink. Variant order is the wire — never reorder.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum NodeCmd<'a> {
    /// Run `line` in the node's shell dispatcher; respond with
    /// [`NodeMsg::Shell`] chunks correlated by `cmd_id`.
    Shell { cmd_id: u16, line: &'a str },
}

fn encode_envelope<T: Serialize>(payload: &T, out: &mut [u8]) -> Result<usize, Error> {
    if out.is_empty() {
        return Err(Error::Overflow);
    }
    out[0] = RADIO_SCHEMA_VERSION;
    let used = postcard::to_slice(payload, &mut out[1..])
        .map_err(|_| Error::Encode)?
        .len();
    let total = 1 + used;
    if total > MAX_RADIO_PAYLOAD {
        // Reject rather than hand the net layer an over-MTU payload it would refuse
        // anyway — the error should point at the message, not the radio.
        return Err(Error::Encode);
    }
    Ok(total)
}

fn split_envelope(data: &[u8]) -> Result<&[u8], Error> {
    let (&ver, body) = data.split_first().ok_or(Error::TooShort)?;
    if ver != RADIO_SCHEMA_VERSION {
        // Same shape as the console framing's version guard: the host can report
        // "node speaks radio schema vN, this build speaks vRADIO_SCHEMA_VERSION"
        // instead of silently mis-decoding.
        return Err(Error::BadVersion { got: ver });
    }
    Ok(body)
}

/// Encode a [`NodeMsg`] envelope into `out`; returns the byte count (≤
/// [`MAX_RADIO_PAYLOAD`]). Pure computation.
pub fn encode_node_msg(msg: &NodeMsg<'_>, out: &mut [u8]) -> Result<usize, Error> {
    encode_envelope(msg, out)
}

/// Decode a [`NodeMsg`] envelope (e.g. the `data` of a forwarded `Uplink`).
pub fn decode_node_msg(data: &[u8]) -> Result<NodeMsg<'_>, Error> {
    postcard::from_bytes(split_envelope(data)?).map_err(|_| Error::Malformed)
}

/// Encode a [`NodeCmd`] envelope into `out` (the bytes a host hands to the
/// gateway's downlink queue); returns the byte count (≤ [`MAX_RADIO_PAYLOAD`]).
pub fn encode_node_cmd(cmd: &NodeCmd<'_>, out: &mut [u8]) -> Result<usize, Error> {
    encode_envelope(cmd, out)
}

/// Decode a [`NodeCmd`] envelope (what a node receives in its downlink window).
pub fn decode_node_cmd(data: &[u8]) -> Result<NodeCmd<'_>, Error> {
    postcard::from_bytes(split_envelope(data)?).map_err(|_| Error::Malformed)
}
