//! Message payloads (the postcard schema). Borrowed (`&str`) so they serialize
//! no-alloc on the target and decode zero-copy on the host. Both ends share these
//! exact definitions — postcard is not self-describing, so field order and variant
//! order are load-bearing (postcard encodes enums by variant **index**).

use heapless::Vec;
use serde::{Deserialize, Serialize};

/// Log severity. Order matches the `log` crate (Error highest). postcard encodes
/// this by variant index, so both ends must keep this order.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// Target → host: one-time announcement on boot / VBUS-present edge. The per-frame
/// `ver_type` is the real version guard; these fields carry the firmware identity for
/// display. `firmware_name` is the baked-in app/example name; `firmware_version` its
/// version string (e.g. `"v0.1.0"`). `session_id` is a per-boot id (a persisted boot
/// counter) so the host can tell a device **reboot** from a continuous link.
///
/// Field order is load-bearing (postcard encodes positionally); changing it is a wire
/// break — bump [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION).
#[derive(Serialize, Deserialize, Debug)]
pub struct Hello<'a> {
    pub protocol_version: u8,
    pub firmware_name: &'a str,
    pub firmware_version: &'a str,
    pub session_id: u32,
}

/// Target → host: a structured log record. The host prepends local time + colorizes.
#[derive(Serialize, Deserialize, Debug)]
pub struct Log<'a> {
    pub level: Level,
    pub uptime_us: u64,
    pub module: &'a str,
    pub message: &'a str,
}

/// Target → host: raw text from `print!`/`println!`, rendered verbatim.
#[derive(Serialize, Deserialize, Debug)]
pub struct Print<'a> {
    pub text: &'a str,
}

/// Target → host: a self-describing event (key=value pairs) so the host renders any
/// app's event without a shared per-app schema.
#[derive(Serialize, Deserialize, Debug)]
pub struct Event<'a> {
    #[serde(borrow)]
    pub name: &'a str,
    #[serde(borrow)]
    pub fields: Vec<(&'a str, &'a str), 8>,
}

/// Target → host: one chunk of a shell response. `result` is authoritative only on
/// the `last` chunk; `chunk` indexes detect a CRC-dropped chunk mid-response.
#[derive(Serialize, Deserialize, Debug)]
pub struct ShellResponse<'a> {
    pub cmd_id: u16,
    pub result: u8, // 0 = success
    pub chunk: u16,
    pub last: bool,
    pub text: &'a str,
}

/// What a completion candidate is — drives host coloring + the auto-insert separator.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum CandidateKind {
    Menu,
    Command,
    Arg,
    Value,
}

/// One completion candidate (text from the static command tree).
#[derive(Serialize, Deserialize, Debug)]
pub struct Candidate<'a> {
    pub text: &'a str,
    pub kind: CandidateKind,
}

/// Target → host: completion result for a `ShellComplete`. `token_start` is the byte
/// offset in the request line the host should replace; `more` flags >16 candidates.
#[derive(Serialize, Deserialize, Debug)]
pub struct ShellCompletions<'a> {
    pub req_id: u16,
    pub token_start: u16,
    pub common_prefix: &'a str,
    #[serde(borrow)]
    pub candidates: Vec<Candidate<'a>, 16>,
    pub more: bool,
}

/// Target → host: overflow marker — `count` log/print frames were dropped.
#[derive(Serialize, Deserialize, Debug)]
pub struct Dropped {
    pub count: u32,
}

/// Target → host: one decrypted, authenticated radio uplink, forwarded verbatim by
/// the gateway. The gateway does NOT interpret `data` — it is a radio-application
/// envelope (see the [`radio`](crate::radio) module) the **host** decodes; new node
/// app types therefore never require a gateway firmware change. `counter` is the
/// net-layer frame counter (dedup/diagnostics); `rssi_dbm`/`lqi` are the reception
/// metadata for the host's link table and graph.
#[derive(Serialize, Deserialize, Debug)]
pub struct Uplink<'a> {
    pub src: u32,
    pub counter: u32,
    pub rssi_dbm: i16,
    pub lqi: u8,
    /// ≤ 74 bytes (the radio MTU, [`radio::MAX_RADIO_PAYLOAD`](crate::radio::MAX_RADIO_PAYLOAD)).
    pub data: &'a [u8],
}

/// Target → host: one chunk of a management reply — see the [`mgmt`](crate::mgmt)
/// module for the op/record contract. Mirrors [`ShellResponse`]'s discipline:
/// `result` is authoritative only on the `last` chunk, `chunk` indexes detect a
/// CRC-dropped chunk mid-reply. The `data` of all chunks concatenate into a stream
/// of postcard records typed per-op.
#[derive(Serialize, Deserialize, Debug)]
pub struct MgmtResponse<'a> {
    pub req_id: u16,
    /// One of the `mgmt::MGMT_*` codes; [`MGMT_OK`](crate::mgmt::MGMT_OK) = success.
    pub result: u8,
    pub chunk: u16,
    pub last: bool,
    pub data: &'a [u8],
}

/// Target → host: one sample of the radio-diagnostics stream feeding the host's
/// running channel graph. Variant order is the wire — never reorder.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum RadioStat {
    /// Ambient RSSI of the current receive channel, sampled at the cadence set by
    /// `MgmtOp::StatsConfig` (or the gateway's persisted `stats-period` setting).
    Channel { channel: u8, rssi_dbm: i16 },
    /// One gateway TX attempt. `item` is the downlink-queue id it carried (0 = not
    /// a queue item); `outcome` is a `mgmt::TX_*` code; `ack_rssi_dbm` is the
    /// receiver-side RSSI the node reported inside its ACK (`None` = no ACK).
    Tx { dest: u32, item: u16, outcome: u8, ack_rssi_dbm: Option<i8> },
}

/// Host → target: a shell command line. `cmd_id` correlates the response.
#[derive(Serialize, Deserialize, Debug)]
pub struct ShellCommand<'a> {
    pub cmd_id: u16,
    pub line: &'a str,
}

/// Host → target: one management operation — see the [`mgmt`](crate::mgmt) module
/// for the op set and reply contract. Requests always fit a single frame; `req_id`
/// correlates the (possibly chunked, possibly delayed) [`MgmtResponse`].
#[derive(Serialize, Deserialize, Debug)]
pub struct MgmtRequest<'a> {
    pub req_id: u16,
    #[serde(borrow)]
    pub op: crate::mgmt::MgmtOp<'a>,
}

/// Host → target: a completion request. `req_id` correlates `ShellCompletions`.
#[derive(Serialize, Deserialize, Debug)]
pub struct ShellComplete<'a> {
    pub req_id: u16,
    pub line: &'a str,
    pub cursor: u16,
}
