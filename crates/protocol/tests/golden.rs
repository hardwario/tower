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
                id: 0x0000_AB12,
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
        MgmtOp::NodeAdd { id: 1, key, name: "n", flags: 0 },
        MgmtOp::NodeRemove { id: 1 },
        MgmtOp::NodeUpdate { id: 1, name: None, flags: None },
        MgmtOp::NodeRevealKey { id: 1 },
        MgmtOp::PairingOpen { window_s: 60, key },
        MgmtOp::PairingCancel,
        MgmtOp::QueuePush { node: 1, ttl_s: 60, data: &[0] },
        MgmtOp::QueueList { node: 0 },
        MgmtOp::QueueDrop { node: 1, item: None },
        MgmtOp::StatsConfig { channel_period_ms: 1000 },
        MgmtOp::Provision(Provision { my_id: None, gw_id: 1, key, band: BAND_EU868, channel: 0 }),
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
    let req = MgmtRequest { req_id: u16::MAX, op: MgmtOp::QueuePush { node: u32::MAX, ttl_s: u16::MAX, data: &data } };
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
        id: u32::MAX,
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
