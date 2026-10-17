//! Golden wire-byte vectors: the anti-drift guard the round-trip tests can't provide.
//!
//! Round-trip tests encode and decode with the *same* schema, so they still pass if someone
//! reorders a struct field / enum variant WITHOUT bumping `PROTOCOL_VERSION`, the exact silent
//! mis-decode this crate exists to prevent (postcard encodes by field/variant order). These
//! assert the exact bytes on the wire for one instance of each representative message, so any
//! such change fails here and forces the author to consciously update BOTH the vectors AND
//! `PROTOCOL_VERSION`.
//!
//! ⚠️ If a change makes these vectors need updating, the wire format changed: you MUST bump
//! `PROTOCOL_VERSION` (and the crate version + the `protocol-vX.Y.Z` tag) in the same commit,
//! and rebuild both ends. Do not "just fix the bytes".

use tower_protocol::msg::*;
use tower_protocol::*;

fn frame<T: serde::Serialize>(mt: MsgType, seq: u16, p: &T) -> Vec<u8> {
    let mut out = [0u8; MAX_WIRE];
    let n = encode_frame(mt, seq, p, &mut out).unwrap();
    out[..n].to_vec()
}

#[test]
fn golden_hello() {
    let got = frame(
        MsgType::Hello,
        1,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            firmware_name: "app",
            firmware_version: "fw",
            session_id: 1,
            wire_fingerprint: 0xA1B2_C3D4,
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x80, 0x01, 0x13, 0x04, 0x03, 0x61, 0x70, 0x70, 0x02, 0x66, 0x77, 0x01, 0xd4, 0x87, 0xcb,
            0x8d, 0x0a, 0x05, 0x22, 0xe9, 0xe7, 0x00
        ]
    );
}

#[test]
fn golden_log() {
    let got = frame(
        MsgType::Log,
        7,
        &Log {
            level: Level::Warn,
            uptime_us: 0x0102_0304,
            module: "m",
            message: "hi",
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x81, 0x07, 0x0f, 0x01, 0x84, 0x86, 0x88, 0x08, 0x01, 0x6d, 0x02, 0x68, 0x69, 0x14, 0x29,
            0x77, 0x7e, 0x00
        ]
    );
}

#[test]
fn golden_dropped() {
    let got = frame(MsgType::Dropped, 2, &Dropped { count: 300 });
    assert_eq!(
        got,
        [0x03, 0x86, 0x02, 0x07, 0xac, 0x02, 0x6d, 0xe8, 0x31, 0xbe, 0x00]
    );
}

#[test]
fn golden_shell_response() {
    let got = frame(
        MsgType::ShellResponse,
        9,
        &ShellResponse {
            cmd_id: 5,
            result: 0,
            chunk: 0,
            last: true,
            text: "ok",
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x84, 0x09, 0x02, 0x05, 0x01, 0x09, 0x01, 0x02, 0x6f, 0x6b, 0x42, 0x24, 0xc2, 0x71, 0x00
        ]
    );
}

// --- full field-layout coverage (1.5.1) ------------------------------------------
//
// Every console message type now has a byte golden, and each vector gives every field a value
// distinct from its siblings, so swapping two fields changes the bytes (a same-value vector
// such as `result: 0, chunk: 0` above cannot see that swap). Pure additions pinning today's
// layout; the vectors above are untouched (wire unchanged).

#[test]
fn golden_print() {
    let got = frame(MsgType::Print, 10, &Print { text: "hey" });
    assert_eq!(
        got,
        [
            0x03, 0x82, 0x0a, 0x09, 0x03, 0x68, 0x65, 0x79, 0xcf, 0x64, 0x5c, 0x46, 0x00
        ]
    );
}

#[test]
fn golden_event() {
    // Two pairs, key ≠ value everywhere: a (key, value) tuple flip or a pair reorder shows.
    let fields = heapless::Vec::from_slice(&[("t", "21"), ("rh", "40")]).unwrap();
    let got = frame(MsgType::Event, 11, &Event { name: "env", fields });
    assert_eq!(
        got,
        [
            0x03, 0x83, 0x0b, 0x15, 0x03, 0x65, 0x6e, 0x76, 0x02, 0x01, 0x74, 0x02, 0x32, 0x31, 0x02, 0x72,
            0x68, 0x02, 0x34, 0x30, 0x32, 0xbe, 0x9c, 0x80, 0x00
        ]
    );
}

#[test]
fn golden_shell_command() {
    let got = frame(
        MsgType::ShellCommand,
        12,
        &ShellCommand {
            cmd_id: 300,
            line: "/led on",
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x90, 0x0c, 0x0f, 0xac, 0x02, 0x07, 0x2f, 0x6c, 0x65, 0x64, 0x20, 0x6f, 0x6e, 0xe4, 0x20,
            0x33, 0x5a, 0x00
        ]
    );
}

#[test]
fn golden_shell_complete() {
    // req_id ≠ cursor, so swapping the two u16s is caught.
    let got = frame(
        MsgType::ShellComplete,
        14,
        &ShellComplete {
            req_id: 13,
            line: "/sy",
            cursor: 2,
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x91, 0x0e, 0x0b, 0x0d, 0x03, 0x2f, 0x73, 0x79, 0x02, 0x1d, 0xf3, 0x46, 0x6b, 0x00
        ]
    );
}

#[test]
fn golden_shell_completions() {
    // Two candidates of different (non-zero) kinds pin `Candidate { text, kind }` in context;
    // req_id ≠ token_start, and `more: true` is the bool's non-default value.
    let candidates = heapless::Vec::from_iter([
        Candidate {
            text: "set",
            kind: CandidateKind::Command,
        },
        Candidate {
            text: "sel",
            kind: CandidateKind::Value,
        },
    ]);
    let c = ShellCompletions {
        req_id: 21,
        token_start: 4,
        common_prefix: "se",
        candidates,
        more: true,
    };
    let got = frame(MsgType::ShellCompletions, 15, &c);
    assert_eq!(
        got,
        [
            0x03, 0x85, 0x0f, 0x16, 0x15, 0x04, 0x02, 0x73, 0x65, 0x02, 0x03, 0x73, 0x65, 0x74, 0x01, 0x03,
            0x73, 0x65, 0x6c, 0x03, 0x01, 0xe5, 0xc2, 0x6c, 0xcf, 0x00
        ]
    );
}

/// `golden_shell_response` uses `result: 0, chunk: 0`; swapping those two fields leaves its
/// bytes unchanged. Here every field differs (and `last` is false), so any swap shows.
#[test]
fn golden_shell_response_distinct_fields() {
    let got = frame(
        MsgType::ShellResponse,
        16,
        &ShellResponse {
            cmd_id: 300,
            result: 3,
            chunk: 5,
            last: false,
            text: "err",
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x84, 0x10, 0x05, 0xac, 0x02, 0x03, 0x05, 0x09, 0x03, 0x65, 0x72, 0x72, 0xd1, 0x06, 0xd9,
            0x68, 0x00
        ]
    );
}

// --- wire v3: the gateway link -------------------------------------------------

#[test]
fn golden_uplink() {
    let got = frame(
        MsgType::Uplink,
        4,
        &Uplink {
            src: 0x1122_3344,
            counter: 7,
            rssi: -67,
            lqi: 40,
            data: &[0xAA, 0xBB],
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x88, 0x04, 0x11, 0xc4, 0xe6, 0x88, 0x89, 0x01, 0x07, 0x85, 0x01, 0x28, 0x02, 0xaa, 0xbb,
            0xb9, 0x3a, 0x3e, 0xf8, 0x00
        ]
    );
}

#[test]
fn golden_mgmt_request_node_add() {
    let got = frame(
        MsgType::MgmtRequest,
        5,
        &MgmtRequest {
            req_id: 3,
            op: tower_protocol::mgmt::MgmtOp::NodeAdd {
                addr: 0x0000_AB12,
                key: [
                    0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae,
                    0xaf,
                ],
                name: "kitchen",
                flags: 1,
            },
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x92, 0x05, 0x23, 0x03, 0x02, 0x92, 0xd6, 0x02, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6,
            0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0x07, 0x6b, 0x69, 0x74, 0x63, 0x68, 0x65,
            0x6e, 0x01, 0x48, 0xd7, 0xc6, 0x03, 0x00
        ]
    );
}

#[test]
fn golden_mgmt_response() {
    let got = frame(
        MsgType::MgmtResponse,
        6,
        &MgmtResponse {
            req_id: 3,
            result: 0,
            chunk: 0,
            last: true,
            data: &[0x01, 0x02, 0x03],
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x87, 0x06, 0x02, 0x03, 0x01, 0x0a, 0x01, 0x03, 0x01, 0x02, 0x03, 0x05, 0x39, 0xe4, 0x33,
            0x00
        ]
    );
}

#[test]
fn golden_radio_stat_tx() {
    let got = frame(
        MsgType::RadioStat,
        8,
        &RadioStat::Tx {
            dest: 0x0000_AB12,
            item: 2,
            outcome: tower_protocol::mgmt::TX_DELIVERED,
            ack_rssi: Some(-70),
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x89, 0x08, 0x06, 0x01, 0x92, 0xd6, 0x02, 0x02, 0x05, 0x01, 0xba, 0x83, 0x92, 0x02, 0xe6,
            0x00
        ]
    );
}

/// `golden_mgmt_response` uses `result: 0, chunk: 0` (swap-blind); every field differs here.
#[test]
fn golden_mgmt_response_distinct_fields() {
    let got = frame(
        MsgType::MgmtResponse,
        19,
        &MgmtResponse {
            req_id: 400,
            result: tower_protocol::mgmt::MGMT_FULL,
            chunk: 6,
            last: false,
            data: &[0xd0, 0xd1],
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x87, 0x13, 0x05, 0x90, 0x03, 0x04, 0x06, 0x08, 0x02, 0xd0, 0xd1, 0xf2, 0x41, 0x15, 0x80,
            0x00
        ]
    );
}

#[test]
fn golden_radio_stat_channel() {
    let got = frame(
        MsgType::RadioStat,
        17,
        &RadioStat::Channel {
            channel: 5,
            rssi: -90,
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x89, 0x11, 0x01, 0x08, 0x05, 0xb3, 0x01, 0x67, 0xae, 0x4c, 0xf6, 0x00
        ]
    );
}

/// The `Tx` arm again with the `None` ack and a non-zero outcome (`golden_radio_stat_tx`
/// covers `Some` + `TX_DELIVERED = 0`).
#[test]
fn golden_radio_stat_tx_no_ack() {
    let got = frame(
        MsgType::RadioStat,
        18,
        &RadioStat::Tx {
            dest: 0x0102_0304,
            item: 9,
            outcome: tower_protocol::mgmt::TX_DUTY_LIMITED,
            ack_rssi: None,
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x89, 0x12, 0x08, 0x01, 0x84, 0x86, 0x88, 0x08, 0x09, 0x03, 0x05, 0x69, 0x69, 0xb4, 0x5c,
            0x00
        ]
    );
}

// --- wire v3: the mgmt *reply records* (MgmtResponse.data payloads) --------------
//
// These are postcard-encoded bare (not framed) and streamed inside `MgmtResponse.data`,
// typed per op. The enum *variants* are order-pinned above; these pin each RECORD's field
// layout: a `DeviceInfo`/`NodeEntry`/… field reorder is otherwise a silent mis-decode with
// no failing test (exactly the drift the golden.rs discipline guards). Each pins the bytes
// AND decodes back, so a reorder trips the vector and a decode regression trips the fields.

/// Encode a bare mgmt record (an `MgmtResponse.data` payload) to its wire bytes.
fn rec<T: serde::Serialize>(p: &T) -> Vec<u8> {
    let mut out = [0u8; 128];
    let n = postcard::to_slice(p, &mut out).unwrap().len();
    out[..n].to_vec()
}

#[test]
fn golden_mgmt_device_info() {
    use tower_protocol::mgmt::*;
    // Distinct addr (0x11223344) vs gw_addr (0x55667788) so swapping the two u32s is caught.
    let d = DeviceInfo {
        role: DeviceRole::Gateway,
        radio_schema_version: 1,
        addr: 0x1122_3344,
        band: BAND_US915,
        channel: 2,
        node_capacity: 16,
        node_count: 3,
        provisioned: true,
        gw_addr: 0x5566_7788,
        firmware_name: "gw",
        wire_fingerprint: 0xA1B2_C3D4,
        firmware_version: "v1.2.3",
    };
    let bytes = rec(&d);
    assert_eq!(
        bytes,
        [
            0x00, 0x01, 0xc4, 0xe6, 0x88, 0x89, 0x01, 0x01, 0x02, 0x10, 0x03, 0x01, 0x88, 0xef, 0x99, 0xab,
            0x05, 0x02, 0x67, 0x77, 0xd4, 0x87, 0xcb, 0x8d, 0x0a, 0x06, 0x76, 0x31, 0x2e, 0x32, 0x2e, 0x33
        ]
    );
    let back: DeviceInfo = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(
        (back.role, back.addr, back.gw_addr),
        (DeviceRole::Gateway, 0x1122_3344, 0x5566_7788)
    );
    assert_eq!(
        (back.node_capacity, back.node_count, back.provisioned),
        (16, 3, true)
    );
    assert_eq!(back.firmware_name, "gw");
    assert_eq!(
        (back.wire_fingerprint, back.firmware_version),
        (0xA1B2_C3D4, "v1.2.3")
    );
}

/// `golden_mgmt_device_info` has `radio_schema_version: 1`, `band: 1` and `provisioned: true`:
/// three one-byte fields holding the same byte, so swapping any two is invisible there. Here
/// all seven one-byte fields differ (and `provisioned` is false).
#[test]
fn golden_mgmt_device_info_distinct_fields() {
    use tower_protocol::mgmt::*;
    let d = DeviceInfo {
        role: DeviceRole::Other,
        radio_schema_version: 3,
        addr: 0x0A0B_0C0D,
        band: BAND_US915,
        channel: 5,
        node_capacity: 16,
        node_count: 7,
        provisioned: false,
        gw_addr: 0x5566_7788,
        firmware_name: "node",
        wire_fingerprint: 0x0102_0304,
        firmware_version: "v9",
    };
    let bytes = rec(&d);
    assert_eq!(
        bytes,
        [
            0x02, 0x03, 0x8d, 0x98, 0xac, 0x50, 0x01, 0x05, 0x10, 0x07, 0x00, 0x88, 0xef, 0x99, 0xab, 0x05,
            0x04, 0x6e, 0x6f, 0x64, 0x65, 0x84, 0x86, 0x88, 0x08, 0x02, 0x76, 0x39
        ]
    );
    let back: DeviceInfo = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back, d);
}

#[test]
fn golden_mgmt_node_entry() {
    use tower_protocol::mgmt::*;
    let e = NodeEntry {
        addr: 0x0000_AB12,
        name: "kitchen",
        flags: 0x03,
        last_seen: 42,
        rssi: -70,
        uplinks: 7,
        queued: 2,
    };
    let bytes = rec(&e);
    assert_eq!(
        bytes,
        [
            0x92, 0xd6, 0x02, 0x07, 0x6b, 0x69, 0x74, 0x63, 0x68, 0x65, 0x6e, 0x03, 0x2a, 0xba, 0x07, 0x02
        ]
    );
    let back: NodeEntry = postcard::from_bytes(&bytes).unwrap();
    assert_eq!((back.addr, back.name, back.flags), (0x0000_AB12, "kitchen", 0x03));
    assert_eq!(
        (back.last_seen, back.rssi, back.uplinks, back.queued),
        (42, -70, 7, 2)
    );
}

#[test]
fn golden_mgmt_node_key() {
    use tower_protocol::mgmt::*;
    let k = NodeKey {
        addr: 0x0000_AB12,
        key: [
            0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
        ],
    };
    let bytes = rec(&k);
    assert_eq!(
        bytes,
        [
            0x92, 0xd6, 0x02, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac,
            0xad, 0xae, 0xaf
        ]
    );
    let back: NodeKey = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back.addr, 0x0000_AB12);
    assert_eq!(back.key[15], 0xaf);
}

#[test]
fn golden_mgmt_small_records() {
    use tower_protocol::mgmt::*;
    assert_eq!(rec(&Paired { addr: 0x0000_AB12 }), [0x92, 0xd6, 0x02]);
    assert_eq!(rec(&QueueId { item: 5 }), [0x05]);
    assert_eq!(
        rec(&ProvisionAck { addr: 0x88a4_e90d }),
        [0x8d, 0xd2, 0x93, 0xc5, 0x08]
    );
    assert_eq!(
        rec(&Joined { gw_addr: 0x1122_3344 }),
        [0xc4, 0xe6, 0x88, 0x89, 0x01]
    );
    // Decode back (these are the delayed-pairing / provisioning acks the host waits on).
    let p: Paired = postcard::from_bytes(&rec(&Paired { addr: 0x0000_AB12 })).unwrap();
    assert_eq!(p.addr, 0x0000_AB12);
    let j: Joined = postcard::from_bytes(&rec(&Joined { gw_addr: 0x1122_3344 })).unwrap();
    assert_eq!(j.gw_addr, 0x1122_3344);
    let a: ProvisionAck = postcard::from_bytes(&rec(&ProvisionAck { addr: 0x88a4_e90d })).unwrap();
    assert_eq!(a.addr, 0x88a4_e90d);
}

#[test]
fn golden_mgmt_queue_entry() {
    use tower_protocol::mgmt::*;
    let q = QueueEntry {
        node_addr: 0x0000_AB12,
        item: 5,
        age: 10,
        ttl: 3600,
        data: &[0x01, 0x02],
    };
    let bytes = rec(&q);
    assert_eq!(
        bytes,
        [0x92, 0xd6, 0x02, 0x05, 0x0a, 0x90, 0x1c, 0x02, 0x01, 0x02]
    );
    let back: QueueEntry = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(
        (back.node_addr, back.item, back.age, back.ttl),
        (0x0000_AB12, 5, 10, 3600)
    );
    assert_eq!(back.data, &[0x01, 0x02]);
}

// --- wire v3: every MgmtOp variant's field layout (1.5.1) -------------------------
//
// `golden_mgmt_request_node_add` pins the `MgmtRequest { req_id, op }` frame; these pin each
// remaining op's bytes bare (its postcard encoding inside that frame) and decode them back.
// Fields hold pairwise-distinct values, so a field swap within an op changes the bytes.

/// Encode one op to its bare wire bytes, asserting it decodes back to itself.
fn op_bytes(op: &tower_protocol::mgmt::MgmtOp) -> Vec<u8> {
    let bytes = rec(op);
    let back: tower_protocol::mgmt::MgmtOp = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(&back, op, "MgmtOp decode-back");
    bytes
}

#[test]
fn golden_mgmt_op_unit_variants() {
    use tower_protocol::mgmt::MgmtOp;
    assert_eq!(op_bytes(&MgmtOp::Describe), [0x00]);
    assert_eq!(op_bytes(&MgmtOp::NodeList), [0x01]);
    assert_eq!(op_bytes(&MgmtOp::PairingCancel), [0x07]);
}

#[test]
fn golden_mgmt_op_node_remove_and_reveal_key() {
    use tower_protocol::mgmt::MgmtOp;
    assert_eq!(
        op_bytes(&MgmtOp::NodeRemove { addr: 0x0000_AB12 }),
        [0x03, 0x92, 0xd6, 0x02]
    );
    assert_eq!(
        op_bytes(&MgmtOp::NodeRevealKey { addr: 0x0000_CD34 }),
        [0x05, 0xb4, 0x9a, 0x03]
    );
}

#[test]
fn golden_mgmt_op_node_update() {
    use tower_protocol::mgmt::MgmtOp;
    // Both / only-flags / only-name: pins the two Options' order and their None/Some tags.
    assert_eq!(
        op_bytes(&MgmtOp::NodeUpdate {
            addr: 0x0000_AB12,
            name: Some("porch"),
            flags: Some(2)
        }),
        [
            0x04, 0x92, 0xd6, 0x02, 0x01, 0x05, 0x70, 0x6f, 0x72, 0x63, 0x68, 0x01, 0x02
        ]
    );
    assert_eq!(
        op_bytes(&MgmtOp::NodeUpdate {
            addr: 0x0000_AB12,
            name: None,
            flags: Some(3)
        }),
        [0x04, 0x92, 0xd6, 0x02, 0x00, 0x01, 0x03]
    );
    assert_eq!(
        op_bytes(&MgmtOp::NodeUpdate {
            addr: 0x0000_AB12,
            name: Some("shed"),
            flags: None
        }),
        [0x04, 0x92, 0xd6, 0x02, 0x01, 0x04, 0x73, 0x68, 0x65, 0x64, 0x00]
    );
}

#[test]
fn golden_mgmt_op_pairing_open() {
    use tower_protocol::mgmt::MgmtOp;
    let key = [
        0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf,
    ];
    assert_eq!(
        op_bytes(&MgmtOp::PairingOpen { window: 120, key }),
        [
            0x06, 0x78, 0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd,
            0xbe, 0xbf
        ]
    );
}

#[test]
fn golden_mgmt_op_queue_push() {
    use tower_protocol::mgmt::MgmtOp;
    assert_eq!(
        op_bytes(&MgmtOp::QueuePush {
            node_addr: 0x0000_AB12,
            ttl: 600,
            data: &[0xde, 0xad, 0xbe]
        }),
        [0x08, 0x92, 0xd6, 0x02, 0xd8, 0x04, 0x03, 0xde, 0xad, 0xbe]
    );
}

#[test]
fn golden_mgmt_op_queue_list_and_drop() {
    use tower_protocol::mgmt::MgmtOp;
    assert_eq!(
        op_bytes(&MgmtOp::QueueList {
            node_addr: 0x0000_AB12
        }),
        [0x09, 0x92, 0xd6, 0x02]
    );
    assert_eq!(op_bytes(&MgmtOp::QueueList { node_addr: 0 }), [0x09, 0x00]); // 0 = all nodes
    assert_eq!(
        op_bytes(&MgmtOp::QueueDrop {
            node_addr: 0x0000_AB12,
            item: Some(4)
        }),
        [0x0a, 0x92, 0xd6, 0x02, 0x01, 0x04]
    );
    assert_eq!(
        op_bytes(&MgmtOp::QueueDrop {
            node_addr: 0x0000_AB12,
            item: None
        }),
        [0x0a, 0x92, 0xd6, 0x02, 0x00]
    );
}

#[test]
fn golden_mgmt_op_stats_config_and_join_open() {
    use tower_protocol::mgmt::MgmtOp;
    assert_eq!(
        op_bytes(&MgmtOp::StatsConfig {
            channel_period_ms: 5000
        }),
        [0x0b, 0x88, 0x27]
    );
    assert_eq!(op_bytes(&MgmtOp::JoinOpen { window: 45 }), [0x0d, 0x2d]);
}

/// `Provision` with `band ≠ channel` (the old variant pin used `band: 0, channel: 0`, so a
/// band/channel swap was invisible), `addr` ≠ `gw_addr`, and both arms of `addr: Option`.
#[test]
fn golden_mgmt_op_provision() {
    use tower_protocol::mgmt::*;
    let key = [
        0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf,
    ];
    let own = Provision {
        addr: Some(0x0000_AB12),
        gw_addr: 0x1122_3344,
        key,
        band: BAND_US915,
        channel: 6,
    };
    assert_eq!(
        op_bytes(&MgmtOp::Provision(own)),
        [
            0x0c, 0x01, 0x92, 0xd6, 0x02, 0xc4, 0xe6, 0x88, 0x89, 0x01, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5,
            0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf, 0x01, 0x06
        ]
    );
    let uid = Provision {
        addr: None,
        gw_addr: 0x1122_3344,
        key,
        band: BAND_EU868,
        channel: 9,
    };
    assert_eq!(
        op_bytes(&MgmtOp::Provision(uid)),
        [
            0x0c, 0x00, 0xc4, 0xe6, 0x88, 0x89, 0x01, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8,
            0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf, 0x00, 0x09
        ]
    );
}

/// The documented `MgmtResponse.data` stream contract: reply records concatenate across
/// chunks and decode back with repeated `take_from_bytes`. Exercises the real host
/// consumption path (a `NodeList` split over two frames) end to end.
#[test]
fn mgmt_response_record_stream_reassembles() {
    use tower_protocol::mgmt::{LAST_SEEN_NEVER, NodeEntry};
    let entries = [
        NodeEntry {
            addr: 1,
            name: "a",
            flags: 0,
            last_seen: 0,
            rssi: -50,
            uplinks: 1,
            queued: 0,
        },
        NodeEntry {
            addr: 2,
            name: "bb",
            flags: 1,
            last_seen: LAST_SEEN_NEVER,
            rssi: -70,
            uplinks: 9,
            queued: 2,
        },
        NodeEntry {
            addr: 3,
            name: "ccc",
            flags: 3,
            last_seen: 5,
            rssi: -90,
            uplinks: 0,
            queued: 1,
        },
    ];
    let mut stream = Vec::new();
    for e in &entries {
        stream.extend_from_slice(&rec(e));
    }
    // Split the record stream across two MgmtResponse chunks (chunk 0 not-last, 1 last),
    // each riding the real frame codec, then reassemble the payloads.
    let mid = stream.len() / 2;
    let mut reassembled = Vec::new();
    for (i, part) in [&stream[..mid], &stream[mid..]].into_iter().enumerate() {
        let r = MgmtResponse {
            req_id: 9,
            result: 0,
            chunk: i as u16,
            last: i == 1,
            data: part,
        };
        let (_, _, payload) = redecode(MsgType::MgmtResponse, i as u16, &r);
        let back: MgmtResponse = postcard::from_bytes(&payload).unwrap();
        reassembled.extend_from_slice(back.data);
    }
    // Decode the reassembled stream record by record.
    let mut rest: &[u8] = &reassembled;
    let mut decoded = Vec::new();
    while !rest.is_empty() {
        let (e, tail) = postcard::take_from_bytes::<NodeEntry>(rest).unwrap();
        decoded.push((e.addr, e.uplinks, e.queued));
        rest = tail;
    }
    assert_eq!(decoded, vec![(1, 1, 0), (2, 9, 2), (3, 0, 1)]);
}

// --- round-trip coverage for the message types the roundtrip suite doesn't exercise ---

/// Encode then deframe+decode, returning (msg_type, seq, payload bytes) for reserialization.
fn redecode<T: serde::Serialize>(mt: MsgType, seq: u16, p: &T) -> (MsgType, u16, Vec<u8>) {
    let wire = frame(mt, seq, p);
    let mut dec = FrameDecoder::new();
    let inner: Vec<u8> = wire
        .iter()
        .find_map(|&b| dec.push(b).map(|s| s.to_vec()))
        .expect("one frame");
    let (m, s, payload) = decode_frame(&inner).unwrap();
    (m, s, payload.to_vec())
}

#[test]
fn shell_completions_full_16_candidates_roundtrips() {
    // The borrow-heavy nested type at capacity: 16 candidates. Confirms it fits a frame and
    // decodes field-for-field (a serde-attribute or heapless bump would break this).
    let mut candidates: heapless::Vec<Candidate, 16> = heapless::Vec::new();
    for _ in 0..16 {
        candidates
            .push(Candidate {
                text: "settings",
                kind: CandidateKind::Menu,
            })
            .unwrap();
    }
    let c = ShellCompletions {
        req_id: 42,
        token_start: 3,
        common_prefix: "se",
        candidates,
        more: false,
    };
    let (mt, seq, payload) = redecode(MsgType::ShellCompletions, 1, &c);
    assert_eq!((mt, seq), (MsgType::ShellCompletions, 1));
    let back: ShellCompletions = postcard::from_bytes(&payload).unwrap();
    assert_eq!(back.req_id, 42);
    assert_eq!(back.token_start, 3);
    assert_eq!(back.candidates.len(), 16);
    assert_eq!(back.candidates[0].kind, CandidateKind::Menu);
}

#[test]
fn event_at_full_capacity_roundtrips() {
    let mut fields: heapless::Vec<(&str, &str), 8> = heapless::Vec::new();
    for _ in 0..8 {
        fields.push(("k", "v")).unwrap();
    }
    let e = Event {
        name: "measurement",
        fields,
    };
    let (_, _, payload) = redecode(MsgType::Event, 3, &e);
    let back: Event = postcard::from_bytes(&payload).unwrap();
    assert_eq!(back.name, "measurement");
    assert_eq!(back.fields.len(), 8);
}

#[test]
fn shell_complete_roundtrips() {
    let sc = ShellComplete {
        req_id: 11,
        line: "/sys",
        cursor: 4,
    };
    let (_, _, payload) = redecode(MsgType::ShellComplete, 5, &sc);
    let back: ShellComplete = postcard::from_bytes(&payload).unwrap();
    assert_eq!((back.req_id, back.line, back.cursor), (11, "/sys", 4));
}

// --- variant-order pins (postcard encodes enums by variant index) ---------------
//
// Each pin pairs a variant with its expected index written as a HEX LITERAL, never
// `i as u8` from an enumerate(). The wire-bump guard fingerprints every golden fn by its
// literals and `Type::Variant` paths, so reordering an enum and then "fixing" this table
// (whether by editing the bytes or by reshuffling the variants) changes the fingerprint and
// fails CI unless the version constant was bumped. (With `i as u8`, reorder-the-enum +
// reorder-the-array passed silently.)

#[test]
fn level_variant_order_is_stable() {
    // golden_log implicitly pins Warn = 1; this pins ALL five indices, so reordering the
    // tail variants (Debug/Trace), which no byte-vector covers, still fails loudly.
    let pins: [(Level, u8); 5] = [
        (Level::Error, 0x00),
        (Level::Warn, 0x01),
        (Level::Info, 0x02),
        (Level::Debug, 0x03),
        (Level::Trace, 0x04),
    ];
    for (level, byte) in pins {
        assert_eq!(rec(&level), [byte], "Level variant order changed: wire break");
    }
}

#[test]
fn candidate_kind_variant_order_is_stable() {
    let pins: [(CandidateKind, u8); 4] = [
        (CandidateKind::Menu, 0x00),
        (CandidateKind::Command, 0x01),
        (CandidateKind::Arg, 0x02),
        (CandidateKind::Value, 0x03),
    ];
    for (kind, byte) in pins {
        assert_eq!(
            rec(&kind),
            [byte],
            "CandidateKind variant order changed: wire break"
        );
    }
}

/// Pin ALL 14 `MgmtOp` indices. Data-carrying variants encode their index as the
/// first byte (varint < 128), so asserting byte 0 pins the order without caring
/// about the field bytes behind it (those are pinned by the `golden_mgmt_op_*` vectors).
#[test]
fn mgmt_op_variant_order_is_stable() {
    use tower_protocol::mgmt::*;
    let key = [0u8; 16];
    let pins: [(MgmtOp, u8); 14] = [
        (MgmtOp::Describe, 0x00),
        (MgmtOp::NodeList, 0x01),
        (
            MgmtOp::NodeAdd {
                addr: 1,
                key,
                name: "n",
                flags: 0,
            },
            0x02,
        ),
        (MgmtOp::NodeRemove { addr: 1 }, 0x03),
        (
            MgmtOp::NodeUpdate {
                addr: 1,
                name: None,
                flags: None,
            },
            0x04,
        ),
        (MgmtOp::NodeRevealKey { addr: 1 }, 0x05),
        (MgmtOp::PairingOpen { window: 60, key }, 0x06),
        (MgmtOp::PairingCancel, 0x07),
        (
            MgmtOp::QueuePush {
                node_addr: 1,
                ttl: 60,
                data: &[0],
            },
            0x08,
        ),
        (MgmtOp::QueueList { node_addr: 0 }, 0x09),
        (
            MgmtOp::QueueDrop {
                node_addr: 1,
                item: None,
            },
            0x0a,
        ),
        (
            MgmtOp::StatsConfig {
                channel_period_ms: 1000,
            },
            0x0b,
        ),
        (
            MgmtOp::Provision(Provision {
                addr: None,
                gw_addr: 1,
                key,
                band: BAND_EU868,
                channel: 0,
            }),
            0x0c,
        ),
        (MgmtOp::JoinOpen { window: 60 }, 0x0d),
    ];
    for (op, byte) in &pins {
        assert_eq!(rec(op)[0], *byte, "MgmtOp variant order changed: wire break");
    }
}

#[test]
fn device_role_variant_order_is_stable() {
    use tower_protocol::mgmt::DeviceRole;
    let pins: [(DeviceRole, u8); 3] = [
        (DeviceRole::Gateway, 0x00),
        (DeviceRole::Node, 0x01),
        (DeviceRole::Other, 0x02),
    ];
    for (role, byte) in pins {
        assert_eq!(rec(&role), [byte], "DeviceRole variant order changed: wire break");
    }
}

#[test]
fn radio_stat_variant_order_is_stable() {
    let pins: [(RadioStat, u8); 2] = [
        (
            RadioStat::Channel {
                channel: 0,
                rssi: -100,
            },
            0x00,
        ),
        (
            RadioStat::Tx {
                dest: 1,
                item: 0,
                outcome: 0,
                ack_rssi: None,
            },
            0x01,
        ),
    ];
    for (stat, byte) in pins {
        assert_eq!(rec(&stat)[0], byte, "RadioStat variant order changed: wire break");
    }
}

// --- wire v3 capacity: the frame budget holds for the new payloads ---------------

/// A `QueuePush` carrying a full radio-MTU downlink (74 bytes), the largest
/// request, must fit a single frame (requests are never chunked).
#[test]
fn max_queue_push_fits_one_frame() {
    use tower_protocol::mgmt::MgmtOp;
    let data = [0xEEu8; 74];
    let req = MgmtRequest {
        req_id: u16::MAX,
        op: MgmtOp::QueuePush {
            node_addr: u32::MAX,
            ttl: u16::MAX,
            data: &data,
        },
    };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::MgmtRequest, 0, &req, &mut out).expect("max QueuePush must fit");
}

/// An `Uplink` carrying a full radio-MTU payload with worst-case metadata must fit.
#[test]
fn max_uplink_fits_one_frame() {
    let data = [0xEEu8; 74];
    let up = Uplink {
        src: u32::MAX,
        counter: u32::MAX,
        rssi: i16::MIN,
        lqi: u8::MAX,
        data: &data,
    };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::Uplink, u16::MAX, &up, &mut out).expect("max Uplink must fit");
}

/// A `MgmtResponse` chunk carrying the firmware's 192-byte chunk budget must fit,
/// and a worst-case `NodeEntry` record must stay small enough that a chunk always
/// makes progress (≥ 4 records per chunk keeps a 32-node list under 8 frames).
#[test]
fn mgmt_chunking_budget_holds() {
    use tower_protocol::mgmt::{LAST_SEEN_NEVER, NodeEntry};
    let chunk = [0xEEu8; 192];
    let rsp = MgmtResponse {
        req_id: u16::MAX,
        result: 0,
        chunk: u16::MAX,
        last: false,
        data: &chunk,
    };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::MgmtResponse, 0, &rsp, &mut out).expect("192-byte chunk must fit");

    let entry = NodeEntry {
        addr: u32::MAX,
        name: "sixteen-byte-nam", // MAX_NODE_NAME
        flags: 0xFF,
        last_seen: LAST_SEEN_NEVER,
        rssi: i8::MIN,
        uplinks: u32::MAX,
        queued: u8::MAX,
    };
    let mut buf = [0u8; 64];
    let used = postcard::to_slice(&entry, &mut buf).unwrap().len();
    assert!(
        used <= 48,
        "worst-case NodeEntry grew past the chunking assumption: {used}"
    );
}

#[test]
fn decode_rejects_frame_over_max_frame() {
    // C9: the receive path must reject an inner frame larger than MAX_FRAME so the encode-side
    // budget is a receive-side guarantee (a FrameDecoder can otherwise surface up to ~MAX_WIRE).
    let oversized = vec![0u8; MAX_FRAME + 1];
    assert_eq!(decode_frame(&oversized), Err(Error::TooLong));
    // A minimal valid-length frame is not rejected for length (it fails later, on the CRC,
    // which since 1.5.1 is checked before version/type).
    let minimal = [0u8; 7];
    assert_eq!(decode_frame(&minimal), Err(Error::BadCrc));
}

// --- v4: the console half of the wire fingerprint ------------------------------
//
// `fingerprint::CONSOLE_HASH` is the structural hash (names ignored) of every `MsgType` value
// paired with its payload schema, plus the mgmt op and reply-record schemas. Pinning it here
// turns ANY console schema change into a golden change, so the wire-bump guard demands a
// `PROTOCOL_VERSION` bump even when no byte vector above happens to exercise the changed
// field. The radio half is pinned in `tests/radio_golden.rs` (guarded by
// `RADIO_SCHEMA_VERSION`); `WIRE_FINGERPRINT` combines the two (`tests/fingerprint.rs`).

#[test]
fn golden_console_schema_hash() {
    assert_eq!(tower_protocol::fingerprint::CONSOLE_HASH, 0x7b3a_6ab4_066f_930b);
}
