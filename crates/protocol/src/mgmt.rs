//! Management schema — the typed op/reply contract of the gateway link (wire v3).
//!
//! A host drives a device with [`MgmtRequest`](crate::msg::MgmtRequest) frames
//! (`{ req_id, op: MgmtOp }`; requests always fit a single frame). The device answers
//! with one or more [`MgmtResponse`](crate::msg::MgmtResponse) chunks carrying the same
//! `req_id`. The `data` fields of all chunks concatenate into a stream of postcard
//! records whose type depends on the op — reassemble, then decode with repeated
//! `postcard::take_from_bytes`. `result` is authoritative only on the `last` chunk;
//! a non-[`MGMT_OK`] result carries no records.
//!
//! | op               | served by | reply records       |
//! |------------------|-----------|---------------------|
//! | [`MgmtOp::Describe`]      | both      | one [`DeviceInfo`]  |
//! | [`MgmtOp::NodeList`]      | gateway   | [`NodeEntry`] × N (chunked) |
//! | [`MgmtOp::NodeAdd`]       | gateway   | none                |
//! | [`MgmtOp::NodeRemove`]    | gateway   | none                |
//! | [`MgmtOp::NodeUpdate`]    | gateway   | none                |
//! | [`MgmtOp::NodeRevealKey`] | gateway   | one [`NodeKey`]     |
//! | [`MgmtOp::PairingOpen`]   | gateway   | one [`Paired`] (delayed — see below) |
//! | [`MgmtOp::PairingCancel`] | gateway   | none                |
//! | [`MgmtOp::QueuePush`]     | gateway   | one [`QueueId`]     |
//! | [`MgmtOp::QueueList`]     | gateway   | [`QueueEntry`] × N (chunked) |
//! | [`MgmtOp::QueueDrop`]     | gateway   | none                |
//! | [`MgmtOp::StatsConfig`]   | gateway   | none                |
//! | [`MgmtOp::Provision`]     | node      | one [`ProvisionAck`] |
//! | [`MgmtOp::JoinOpen`]      | node      | one [`Joined`] (delayed) |
//!
//! **Role probing.** `Describe` doubles as the authoritative "is this a gateway?"
//! check: a v3 device answers with its [`DeviceRole`]; an op a device does not serve
//! gets [`MGMT_UNSUPPORTED`]; pre-v3 firmware never answers at all (the host times
//! out). This is stronger than matching `Hello.firmware_name`, which is display-only.
//!
//! **Delayed responses.** `PairingOpen` / `JoinOpen` reply when their radio window
//! *resolves* — a join commits, the window expires ([`MGMT_TIMEOUT`]), or it is
//! cancelled — which may be up to `window` seconds after the request. The `req_id`
//! correlation makes this safe; a second open while one runs answers [`MGMT_BUSY`].
//!
//! **Who mints AES keys: the host.** Device PRNGs here are deterministic and
//! explicitly non-cryptographic, so the key travels host→device in `PairingOpen`
//! (OTA) and `NodeAdd`/`Provision` (over-the-cable) — never the other way, except
//! for the deliberate, explicit [`MgmtOp::NodeRevealKey`].
//!
//! This enum set is **exhaustive and part of wire v3** — postcard encodes variants
//! by index, so appending an op later is a wire change (bump
//! [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION)). It is designed complete now.

use serde::{Deserialize, Serialize};

// --- result codes (MgmtResponse.result) ---------------------------------------

/// Op succeeded; the documented reply records (if any) are in `data`.
pub const MGMT_OK: u8 = 0;
/// The device does not serve this op (e.g. `NodeList` asked of a node).
pub const MGMT_UNSUPPORTED: u8 = 1;
/// An argument failed validation (name too long, bad band, zero id, …).
pub const MGMT_BAD_ARG: u8 = 2;
/// The referenced node / queue item does not exist.
pub const MGMT_NOT_FOUND: u8 = 3;
/// Capacity exhausted (registry slots, queue pool, per-node queue depth).
pub const MGMT_FULL: u8 = 4;
/// A conflicting operation is in flight (e.g. a pairing window is already open).
pub const MGMT_BUSY: u8 = 5;
/// Persisting to EEPROM failed; nothing was committed.
pub const MGMT_STORAGE: u8 = 6;
/// A window op resolved without an event (pairing window expired unjoined).
pub const MGMT_TIMEOUT: u8 = 7;

// --- TX outcome codes (RadioStat::Tx.outcome) ----------------------------------

/// Confirmed delivery: the node ACKed.
pub const TX_DELIVERED: u8 = 0;
/// All retransmissions exhausted without an ACK.
pub const TX_NOT_DELIVERED: u8 = 1;
/// Radio busy (CSMA / mode); will be retried on the node's next uplink.
pub const TX_BUSY: u8 = 2;
/// Blocked by the duty-cycle governor; retried later.
pub const TX_DUTY_LIMITED: u8 = 3;
/// Radio / crypto error.
pub const TX_ERROR: u8 = 4;
/// Queue item expired (TTL) before it could be delivered; never transmitted.
pub const TX_EXPIRED: u8 = 5;

// --- node entry flags -----------------------------------------------------------

/// The node is battery powered and sleeps between uplinks (downlinks are queued).
pub const NODE_FLAG_SLEEPING: u8 = 1 << 0;
/// The node has no operator-assigned name yet; the host may auto-name it from the
/// first `NodeInfo` uplink (see the `radio` module) via [`MgmtOp::NodeUpdate`].
pub const NODE_FLAG_UNNAMED: u8 = 1 << 1;

/// `NodeEntry.last_seen` when the node has not been heard since gateway boot.
pub const LAST_SEEN_NEVER: u32 = u32::MAX;
/// `NodeEntry.rssi` when no uplink RSSI has been captured yet.
pub const RSSI_NONE: i8 = i8::MAX;

/// `Provision.band` / `DeviceInfo.band` values.
pub const BAND_EU868: u8 = 0;
pub const BAND_US915: u8 = 1;

/// Longest node name accepted by `NodeAdd` / `NodeUpdate` (bytes, UTF-8).
pub const MAX_NODE_NAME: usize = 16;

// --- ops -------------------------------------------------------------------------

/// One management operation. Variant order is the wire format (postcard encodes by
/// variant index) — never reorder; appending is a wire change.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum MgmtOp<'a> {
    /// Identify the device: role, radio schema, network parameters. Both roles.
    Describe,
    /// Stream the registry as [`NodeEntry`] records. Gateway.
    NodeList,
    /// Install a node paired over the cable: persist `(addr, key, name, flags)` and
    /// start accepting its traffic. The host minted `key` and provisioned the node
    /// on its own serial port. Gateway.
    NodeAdd { addr: u32, key: [u8; 16], name: &'a str, flags: u8 },
    /// Forget a node: registry, RAM peer slot, and its queued downlinks. Gateway.
    NodeRemove { addr: u32 },
    /// Update mutable metadata; `None` keeps the current value. Gateway.
    NodeUpdate { addr: u32, name: Option<&'a str>, flags: Option<u8> },
    /// Return the node's AES key as a [`NodeKey`] record — the only path that ever
    /// discloses a stored key. Gateway.
    NodeRevealKey { addr: u32 },
    /// Open the OTA pairing window for `window` seconds with the host-minted key
    /// to hand out. Replies (delayed) [`Paired`] or [`MGMT_TIMEOUT`]. Gateway.
    PairingOpen { window: u16, key: [u8; 16] },
    /// Close an open pairing window; its pending response resolves [`MGMT_TIMEOUT`].
    PairingCancel,
    /// Enqueue an opaque downlink (a `radio::NodeCmd` envelope built by the host)
    /// for delivery on the node's next uplink. Replies [`QueueId`]. Gateway.
    QueuePush { node_addr: u32, ttl: u16, data: &'a [u8] },
    /// Stream pending downlinks as [`QueueEntry`] records; `node_addr = 0` = all. Gateway.
    QueueList { node_addr: u32 },
    /// Drop one queued item (`Some(item)`) or a node's whole queue (`None`). Gateway.
    QueueDrop { node_addr: u32, item: Option<u16> },
    /// Set the ambient channel-RSSI sampling cadence (`0` = off). RAM-only override
    /// of the persisted `stats-period` setting. Gateway.
    StatsConfig { channel_period_ms: u32 },
    /// Over-the-cable provisioning: persist the network credentials on a node.
    /// Node (served only while on USB — which is exactly the cable-pairing case).
    Provision(Provision),
    /// Ask a node to run its OTA join for `window` seconds. Replies (delayed)
    /// [`Joined`] or [`MGMT_TIMEOUT`]. Node.
    JoinOpen { window: u16 },
}

/// Network credentials installed on a node by [`MgmtOp::Provision`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Provision {
    /// `Some` overrides the node's UID-derived radio address; `None` keeps it.
    pub addr: Option<u32>,
    pub gw_addr: u32,
    pub key: [u8; 16],
    /// [`BAND_EU868`] / [`BAND_US915`].
    pub band: u8,
    pub channel: u8,
}

/// What kind of device answered [`MgmtOp::Describe`]. Variant order pinned by test.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceRole {
    Gateway,
    Node,
    Other,
}

/// Reply record for [`MgmtOp::Describe`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct DeviceInfo<'a> {
    pub role: DeviceRole,
    /// The `radio` module schema version this firmware encodes/decodes.
    pub radio_schema_version: u8,
    /// The device's own radio address (a gateway's coordinator address; a node's own address).
    pub addr: u32,
    pub band: u8,
    pub channel: u8,
    /// Registry slots (gateway); 0 on a node.
    pub node_capacity: u8,
    pub node_count: u8,
    /// Node: has a persisted gateway + key; gateway: always `true`.
    pub provisioned: bool,
    /// Node: the gateway it is paired to (`0` = none); gateway: equals `addr`.
    pub gw_addr: u32,
    pub firmware_name: &'a str,
}

/// Reply record for [`MgmtOp::NodeList`] — one registered node.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct NodeEntry<'a> {
    pub addr: u32,
    /// ≤ [`MAX_NODE_NAME`] bytes; empty while [`NODE_FLAG_UNNAMED`].
    pub name: &'a str,
    /// [`NODE_FLAG_SLEEPING`] | [`NODE_FLAG_UNNAMED`].
    pub flags: u8,
    /// Seconds since the last uplink; [`LAST_SEEN_NEVER`] = not since gateway boot.
    /// RAM-only on the device (per-uplink EEPROM writes would burn the part).
    pub last_seen: u32,
    /// Last uplink RSSI; [`RSSI_NONE`] = none captured yet.
    pub rssi: i8,
    /// Uplinks since gateway boot.
    pub uplinks: u32,
    /// Downlink items currently queued for this node.
    pub queued: u8,
}

/// Reply record for [`MgmtOp::NodeRevealKey`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct NodeKey {
    pub addr: u32,
    pub key: [u8; 16],
}

/// Delayed reply record for [`MgmtOp::PairingOpen`] — a node joined the window.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Paired {
    pub addr: u32,
}

/// Reply record for [`MgmtOp::QueuePush`] — the handle for `QueueDrop` and the
/// `item` echoed in `RadioStat::Tx` reports. Monotonic from 1 per gateway boot.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct QueueId {
    pub item: u16,
}

/// Reply record for [`MgmtOp::QueueList`] — one pending downlink.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct QueueEntry<'a> {
    pub node_addr: u32,
    pub item: u16,
    pub age: u16,
    pub ttl: u16,
    pub data: &'a [u8],
}

/// Reply record for [`MgmtOp::Provision`] — the node's effective radio address.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProvisionAck {
    pub addr: u32,
}

/// Delayed reply record for [`MgmtOp::JoinOpen`] — the node joined a gateway.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Joined {
    pub gw_addr: u32,
}
