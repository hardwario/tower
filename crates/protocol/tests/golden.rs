//! Golden wire-byte vectors — the anti-drift guard the round-trip tests can't provide.
//!
//! Round-trip tests encode and decode with the *same* schema, so they still pass if someone
//! reorders a struct field / enum variant WITHOUT bumping `PROTOCOL_VERSION` — the exact silent
//! mis-decode this crate exists to prevent (postcard encodes by field/variant order). These
//! assert the exact bytes on the wire for one instance of each representative message, so any
//! such change fails here and forces the author to consciously update BOTH the vectors AND
//! `PROTOCOL_VERSION`.
//!
//! ⚠️ If a change makes these vectors need updating, the wire format changed — you MUST bump
//! `PROTOCOL_VERSION` (and the crate version + tag) in the same change-set, and re-pin both
//! consumers. Do not "just fix the bytes".

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
        &Hello { protocol_version: PROTOCOL_VERSION, firmware_name: "app", firmware_version: "fw", session_id: 1 },
    );
    assert_eq!(
        got,
        [0x03, 0x60, 0x01, 0x0e, 0x03, 0x03, 0x61, 0x70, 0x70, 0x02, 0x66, 0x77, 0x01, 0x60, 0x44, 0x10, 0x0b, 0x00]
    );
}

#[test]
fn golden_log() {
    let got = frame(
        MsgType::Log,
        7,
        &Log { level: Level::Warn, uptime_us: 0x0102_0304, module: "m", message: "hi" },
    );
    assert_eq!(
        got,
        [
            0x03, 0x61, 0x07, 0x0f, 0x01, 0x84, 0x86, 0x88, 0x08, 0x01, 0x6d, 0x02, 0x68, 0x69,
            0x5d, 0xd6, 0xee, 0xd0, 0x00
        ]
    );
}

#[test]
fn golden_dropped() {
    let got = frame(MsgType::Dropped, 2, &Dropped { count: 300 });
    assert_eq!(got, [0x03, 0x66, 0x02, 0x07, 0xac, 0x02, 0xb2, 0x2f, 0xe5, 0x96, 0x00]);
}

#[test]
fn golden_shell_response() {
    let got = frame(
        MsgType::ShellResponse,
        9,
        &ShellResponse { cmd_id: 5, result: 0, chunk: 0, last: true, text: "ok" },
    );
    assert_eq!(
        got,
        [
            0x03, 0x64, 0x09, 0x02, 0x05, 0x01, 0x09, 0x01, 0x02, 0x6f, 0x6b, 0x33, 0x29, 0x20,
            0x46, 0x00
        ]
    );
}

// --- wire v3: the gateway link -------------------------------------------------

#[test]
fn golden_uplink() {
    let got = frame(
        MsgType::Uplink,
        4,
        &Uplink { src: 0x1122_3344, counter: 7, rssi_dbm: -67, lqi: 40, data: &[0xAA, 0xBB] },
    );
    assert_eq!(
        got,
        [
            0x03, 0x68, 0x04, 0x11, 0xc4, 0xe6, 0x88, 0x89, 0x01, 0x07, 0x85, 0x01, 0x28, 0x02,
            0xaa, 0xbb, 0xe1, 0x8f, 0x87, 0xf4, 0x00
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
                    0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac,
                    0xad, 0xae, 0xaf,
                ],
                name: "kitchen",
                flags: 1,
            },
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x72, 0x05, 0x23, 0x03, 0x02, 0x92, 0xd6, 0x02, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4,
            0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0x07, 0x6b, 0x69,
            0x74, 0x63, 0x68, 0x65, 0x6e, 0x01, 0xf2, 0xd6, 0x02, 0xc2, 0x00
        ]
    );
}

#[test]
fn golden_mgmt_response() {
    let got = frame(
        MsgType::MgmtResponse,
        6,
        &MgmtResponse { req_id: 3, result: 0, chunk: 0, last: true, data: &[0x01, 0x02, 0x03] },
    );
    assert_eq!(
        got,
        [
            0x03, 0x67, 0x06, 0x02, 0x03, 0x01, 0x0a, 0x01, 0x03, 0x01, 0x02, 0x03, 0xa2, 0x9a,
            0xd1, 0x14, 0x00
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
            ack_rssi_dbm: Some(-70),
        },
    );
    assert_eq!(
        got,
        [
            0x03, 0x69, 0x08, 0x06, 0x01, 0x92, 0xd6, 0x02, 0x02, 0x07, 0x01, 0xba, 0x24, 0x31,
            0x35, 0xc1, 0x00
        ]
    );
}

// --- wire v3: the mgmt *reply records* (MgmtResponse.data payloads) --------------
//
// These are postcard-encoded bare (not framed) and streamed inside `MgmtResponse.data`,
// typed per op. The enum *variants* are order-pinned above; these pin each RECORD's field
// layout — a `DeviceInfo`/`NodeEntry`/… field reorder is otherwise a silent mis-decode with
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
    };
    let bytes = rec(&d);
    assert_eq!(
        bytes,
        [
            0x00, 0x01, 0xc4, 0xe6, 0x88, 0x89, 0x01, 0x01, 0x02, 0x10, 0x03, 0x01, 0x88, 0xef,
            0x99, 0xab, 0x05, 0x02, 0x67, 0x77
        ]
    );
    let back: DeviceInfo = postcard::from_bytes(&bytes).unwrap();
    assert_eq!((back.role, back.addr, back.gw_addr), (DeviceRole::Gateway, 0x1122_3344, 0x5566_7788));
    assert_eq!((back.node_capacity, back.node_count, back.provisioned), (16, 3, true));
    assert_eq!(back.firmware_name, "gw");
}

#[test]
fn golden_mgmt_node_entry() {
    use tower_protocol::mgmt::*;
    let e = NodeEntry { addr: 0x0000_AB12, name: "kitchen", flags: 0x03, last_seen_s: 42, rssi_dbm: -70, uplinks: 7, queued: 2 };
    let bytes = rec(&e);
    assert_eq!(
        bytes,
        [0x92, 0xd6, 0x02, 0x07, 0x6b, 0x69, 0x74, 0x63, 0x68, 0x65, 0x6e, 0x03, 0x2a, 0xba, 0x07, 0x02]
    );
    let back: NodeEntry = postcard::from_bytes(&bytes).unwrap();
    assert_eq!((back.addr, back.name, back.flags), (0x0000_AB12, "kitchen", 0x03));
    assert_eq!((back.last_seen_s, back.rssi_dbm, back.uplinks, back.queued), (42, -70, 7, 2));
}

#[test]
fn golden_mgmt_node_key() {
    use tower_protocol::mgmt::*;
    let k = NodeKey {
        addr: 0x0000_AB12,
        key: [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf],
    };
    let bytes = rec(&k);
    assert_eq!(
        bytes,
        [
            0x92, 0xd6, 0x02, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa,
            0xab, 0xac, 0xad, 0xae, 0xaf
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
    assert_eq!(rec(&ProvisionAck { addr: 0x88a4_e90d }), [0x8d, 0xd2, 0x93, 0xc5, 0x08]);
    assert_eq!(rec(&Joined { gw_addr: 0x1122_3344 }), [0xc4, 0xe6, 0x88, 0x89, 0x01]);
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
    let q = QueueEntry { node_addr: 0x0000_AB12, item: 5, age_s: 10, ttl_s: 3600, data: &[0x01, 0x02] };
    let bytes = rec(&q);
    assert_eq!(bytes, [0x92, 0xd6, 0x02, 0x05, 0x0a, 0x90, 0x1c, 0x02, 0x01, 0x02]);
    let back: QueueEntry = postcard::from_bytes(&bytes).unwrap();
    assert_eq!((back.node_addr, back.item, back.age_s, back.ttl_s), (0x0000_AB12, 5, 10, 3600));
    assert_eq!(back.data, &[0x01, 0x02]);
}

/// The documented `MgmtResponse.data` stream contract: reply records concatenate across
/// chunks and decode back with repeated `take_from_bytes`. Exercises the real host
/// consumption path (a `NodeList` split over two frames) end to end.
#[test]
fn mgmt_response_record_stream_reassembles() {
    use tower_protocol::mgmt::{LAST_SEEN_NEVER, NodeEntry};
    let entries = [
        NodeEntry { addr: 1, name: "a", flags: 0, last_seen_s: 0, rssi_dbm: -50, uplinks: 1, queued: 0 },
        NodeEntry { addr: 2, name: "bb", flags: 1, last_seen_s: LAST_SEEN_NEVER, rssi_dbm: -70, uplinks: 9, queued: 2 },
        NodeEntry { addr: 3, name: "ccc", flags: 3, last_seen_s: 5, rssi_dbm: -90, uplinks: 0, queued: 1 },
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
        let r = MgmtResponse { req_id: 9, result: 0, chunk: i as u16, last: i == 1, data: part };
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
            .push(Candidate { text: "settings", kind: CandidateKind::Menu })
            .unwrap();
    }
    let c = ShellCompletions { req_id: 42, token_start: 3, common_prefix: "se", candidates, more: false };
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
    let e = Event { name: "measurement", fields };
    let (_, _, payload) = redecode(MsgType::Event, 3, &e);
    let back: Event = postcard::from_bytes(&payload).unwrap();
    assert_eq!(back.name, "measurement");
    assert_eq!(back.fields.len(), 8);
}

#[test]
fn shell_complete_roundtrips() {
    let sc = ShellComplete { req_id: 11, line: "/sys", cursor: 4 };
    let (_, _, payload) = redecode(MsgType::ShellComplete, 5, &sc);
    let back: ShellComplete = postcard::from_bytes(&payload).unwrap();
    assert_eq!((back.req_id, back.line, back.cursor), (11, "/sys", 4));
}

#[test]
fn level_variant_order_is_stable() {
    // golden_log implicitly pins Warn = 1; this pins ALL five indices, so reordering the
    // tail variants (Debug/Trace) — which no byte-vector covers — still fails loudly.
    for (i, level) in [Level::Error, Level::Warn, Level::Info, Level::Debug, Level::Trace]
        .into_iter()
        .enumerate()
    {
        let mut buf = [0u8; 4];
        let n = postcard::to_slice(&level, &mut buf).unwrap().len();
        assert_eq!(n, 1);
        assert_eq!(buf[0], i as u8, "Level variant order changed — wire break");
    }
}

#[test]
fn candidate_kind_variant_order_is_stable() {
    // postcard encodes an enum by variant index; pin the four kinds' indices.
    for (i, kind) in [
        CandidateKind::Menu,
        CandidateKind::Command,
        CandidateKind::Arg,
        CandidateKind::Value,
    ]
    .into_iter()
    .enumerate()
    {
        let mut buf = [0u8; 4];
        let n = postcard::to_slice(&kind, &mut buf).unwrap().len();
        assert_eq!(n, 1);
        assert_eq!(buf[0], i as u8, "CandidateKind variant order changed — wire break");
    }
}

// --- wire v3 variant-order pins (postcard encodes enums by variant index) -------

/// Pin ALL 14 `MgmtOp` indices. Data-carrying variants encode their index as the
/// first byte (varint < 128), so asserting byte 0 pins the order without caring
/// about the field bytes behind it.
#[test]
fn mgmt_op_variant_order_is_stable() {
    use tower_protocol::mgmt::*;
    let key = [0u8; 16];
    let ops: [MgmtOp; 14] = [
        MgmtOp::Describe,
        MgmtOp::NodeList,
        MgmtOp::NodeAdd { addr: 1, key, name: "n", flags: 0 },
        MgmtOp::NodeRemove { addr: 1 },
        MgmtOp::NodeUpdate { addr: 1, name: None, flags: None },
        MgmtOp::NodeRevealKey { addr: 1 },
        MgmtOp::PairingOpen { window_s: 60, key },
        MgmtOp::PairingCancel,
        MgmtOp::QueuePush { node_addr: 1, ttl_s: 60, data: &[0] },
        MgmtOp::QueueList { node_addr: 0 },
        MgmtOp::QueueDrop { node_addr: 1, item: None },
        MgmtOp::StatsConfig { channel_period_ms: 1000 },
        MgmtOp::Provision(Provision { addr: None, gw_addr: 1, key, band: BAND_EU868, channel: 0 }),
        MgmtOp::JoinOpen { window_s: 60 },
    ];
    for (i, op) in ops.iter().enumerate() {
        let mut buf = [0u8; 64];
        let used = postcard::to_slice(op, &mut buf).unwrap().len();
        assert!(used >= 1);
        assert_eq!(buf[0], i as u8, "MgmtOp variant order changed — wire break");
    }
}

#[test]
fn device_role_variant_order_is_stable() {
    use tower_protocol::mgmt::DeviceRole;
    for (i, role) in [DeviceRole::Gateway, DeviceRole::Node, DeviceRole::Other]
        .into_iter()
        .enumerate()
    {
        let mut buf = [0u8; 4];
        let n = postcard::to_slice(&role, &mut buf).unwrap().len();
        assert_eq!(n, 1);
        assert_eq!(buf[0], i as u8, "DeviceRole variant order changed — wire break");
    }
}

#[test]
fn radio_stat_variant_order_is_stable() {
    let stats = [
        RadioStat::Channel { channel: 0, rssi_dbm: -100 },
        RadioStat::Tx { dest: 1, item: 0, outcome: 0, ack_rssi_dbm: None },
    ];
    for (i, stat) in stats.iter().enumerate() {
        let mut buf = [0u8; 16];
        let used = postcard::to_slice(stat, &mut buf).unwrap().len();
        assert!(used >= 1);
        assert_eq!(buf[0], i as u8, "RadioStat variant order changed — wire break");
    }
}

// --- wire v3 capacity: the frame budget holds for the new payloads ---------------

/// A `QueuePush` carrying a full radio-MTU downlink (74 bytes) — the largest
/// request — must fit a single frame (requests are never chunked).
#[test]
fn max_queue_push_fits_one_frame() {
    use tower_protocol::mgmt::MgmtOp;
    let data = [0xEEu8; 74];
    let req = MgmtRequest { req_id: u16::MAX, op: MgmtOp::QueuePush { node_addr: u32::MAX, ttl_s: u16::MAX, data: &data } };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::MgmtRequest, 0, &req, &mut out).expect("max QueuePush must fit");
}

/// An `Uplink` carrying a full radio-MTU payload with worst-case metadata must fit.
#[test]
fn max_uplink_fits_one_frame() {
    let data = [0xEEu8; 74];
    let up = Uplink { src: u32::MAX, counter: u32::MAX, rssi_dbm: i16::MIN, lqi: u8::MAX, data: &data };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::Uplink, u16::MAX, &up, &mut out).expect("max Uplink must fit");
}

/// A `MgmtResponse` chunk carrying the firmware's 192-byte chunk budget must fit —
/// and a worst-case `NodeEntry` record must stay small enough that a chunk always
/// makes progress (≥ 4 records per chunk keeps a 32-node list under 8 frames).
#[test]
fn mgmt_chunking_budget_holds() {
    use tower_protocol::mgmt::{NodeEntry, LAST_SEEN_NEVER};
    let chunk = [0xEEu8; 192];
    let rsp = MgmtResponse { req_id: u16::MAX, result: 0, chunk: u16::MAX, last: false, data: &chunk };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::MgmtResponse, 0, &rsp, &mut out).expect("192-byte chunk must fit");

    let entry = NodeEntry {
        addr: u32::MAX,
        name: "sixteen-byte-nam", // MAX_NODE_NAME
        flags: 0xFF,
        last_seen_s: LAST_SEEN_NEVER,
        rssi_dbm: i8::MIN,
        uplinks: u32::MAX,
        queued: u8::MAX,
    };
    let mut buf = [0u8; 64];
    let used = postcard::to_slice(&entry, &mut buf).unwrap().len();
    assert!(used <= 48, "worst-case NodeEntry grew past the chunking assumption: {used}");
}

#[test]
fn decode_rejects_frame_over_max_frame() {
    // C9: the receive path must reject an inner frame larger than MAX_FRAME so the encode-side
    // budget is a receive-side guarantee (a FrameDecoder can otherwise surface up to ~MAX_WIRE).
    let oversized = vec![0u8; MAX_FRAME + 1];
    assert_eq!(decode_frame(&oversized), Err(Error::TooLong));
    // A minimal valid-length frame is not rejected for length (it fails later on version/crc).
    let minimal = [0u8; 7];
    assert!(!matches!(decode_frame(&minimal), Err(Error::TooLong)));
}
