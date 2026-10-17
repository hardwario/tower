//! The console framing: byte goldens of the frame layout (COBS, the type / tag / seq header,
//! the CRC-32 trailer), the frame budget for the largest real payloads, and an encode/decode
//! round trip of every message and every management op and record.
//!
//! The goldens are built with an explicit schema tag (`encode_frame_with_tag`) and payloads
//! that are plain serde values (a `&str`, a `u32`, a tuple), not the message structs, so they
//! pin the framing alone and never move when the schema does; the expected bytes come from an
//! independent reference (Python: `zlib.crc32` + a textbook COBS). The schema itself needs no
//! byte pins: it is identified by `CONSOLE_TAG`, which follows any change to it, and every end
//! rebuilds from the same checkout.

use tower_protocol::mgmt::*;
use tower_protocol::msg::*;
use tower_protocol::*;

/// The wire bytes of one frame with an explicit tag.
fn wire<T: serde::Serialize>(tag: u16, mt: MsgType, seq: u16, p: &T) -> Vec<u8> {
    let mut out = [0u8; MAX_WIRE];
    let n = encode_frame_with_tag(tag, mt, seq, p, &mut out).unwrap();
    out[..n].to_vec()
}

/// Feed wire bytes through a fresh decoder; the single deframed inner buffer.
fn deframe(wire: &[u8]) -> Vec<u8> {
    let mut dec = FrameDecoder::new();
    let mut frames: Vec<Vec<u8>> = wire
        .iter()
        .filter_map(|&b| dec.push(b).map(|s| s.to_vec()))
        .collect();
    assert_eq!(frames.len(), 1, "exactly one frame");
    frames.pop().unwrap()
}

// --- framing goldens (explicit tag) --------------------------------------------------------

/// A short text payload: header `type ‖ tag LE ‖ seq LE`, the postcard body, the CRC-32 LE, and
/// COBS around it all.
#[test]
fn golden_frame_layout() {
    let got = wire(0x1234, MsgType::Print, 1, &"ok");
    assert_eq!(
        got,
        [
            0x05, 0x02, 0x34, 0x12, 0x01, 0x08, 0x02, 0x6f, 0x6b, 0xa3, 0xc8, 0xd4, 0x33, 0x00
        ]
    );
    let inner = deframe(&got);
    assert_eq!(
        inner,
        [
            0x02, 0x34, 0x12, 0x01, 0x00, 0x02, 0x6f, 0x6b, 0xa3, 0xc8, 0xd4, 0x33
        ]
    );
    assert_eq!(
        decode_frame_with_tag(0x1234, &inner),
        Ok((MsgType::Print, 1, &[0x02, 0x6f, 0x6b][..]))
    );
}

/// `seq` 0 puts zero bytes in the header: COBS must still leave the trailing delimiter as the
/// only `0x00`.
#[test]
fn golden_frame_zero_seq() {
    let got = wire(0xA5C3, MsgType::Dropped, 0, &300u32);
    assert_eq!(
        got,
        [
            0x04, 0x06, 0xc3, 0xa5, 0x01, 0x07, 0xac, 0x02, 0x71, 0x04, 0xbb, 0x3d, 0x00
        ]
    );
    assert_eq!(
        decode_frame_with_tag(0xA5C3, &deframe(&got)),
        Ok((MsgType::Dropped, 0, &[0xac, 0x02][..]))
    );
}

/// A host→target type with a zero tag and a high seq byte.
#[test]
fn golden_frame_host_to_target() {
    let got = wire(0x0000, MsgType::ShellCommand, 0x0100, &(7u16, "/x"));
    assert_eq!(
        got,
        [
            0x02, 0x10, 0x01, 0x01, 0x0a, 0x01, 0x07, 0x02, 0x2f, 0x78, 0xdb, 0x52, 0x38, 0xa5, 0x00
        ]
    );
    assert_eq!(
        decode_frame_with_tag(0x0000, &deframe(&got)),
        Ok((MsgType::ShellCommand, 0x0100, &[0x07, 0x02, 0x2f, 0x78][..]))
    );
}

/// `encode_frame` is `encode_frame_with_tag(CONSOLE_TAG, ...)`: the header carries this build's
/// console tag, and only `decode_frame` (this build) accepts it.
#[test]
fn encode_frame_stamps_the_console_tag() {
    let mut out = [0u8; MAX_WIRE];
    let n = encode_frame(MsgType::Print, 0xBEEF, &"x", &mut out).unwrap();
    assert_eq!(out[..n], wire(CONSOLE_TAG, MsgType::Print, 0xBEEF, &"x"));
    let inner = deframe(&out[..n]);
    assert_eq!(inner[0], MsgType::Print as u8);
    assert_eq!(inner[1..3], CONSOLE_TAG.to_le_bytes());
    assert_eq!(inner[3..5], 0xBEEFu16.to_le_bytes());
    assert!(decode_frame(&inner).is_ok());
    let other = CONSOLE_TAG ^ 0x0101;
    assert_eq!(
        decode_frame_with_tag(other, &inner),
        Err(Error::SchemaMismatch { got: CONSOLE_TAG })
    );
}

/// The payload budget is the frame minus the 5-byte header and the 4-byte CRC (written out
/// independently of the crate's own constant).
#[test]
fn payload_budget_matches_the_frame_layout() {
    assert_eq!(MAX_PAYLOAD, 256 - 5 - 4);
    assert_eq!(MAX_FRAME, 256);
    // The largest payload fits exactly; one byte more is refused.
    let mut out = [0u8; MAX_WIRE];
    let max = [0x55u8; MAX_PAYLOAD - 2]; // a &[u8] costs its length varint (2 bytes here)
    assert!(encode_frame(MsgType::Print, 0, &&max[..], &mut out).is_ok());
    let over = [0x55u8; MAX_PAYLOAD - 1];
    assert_eq!(
        encode_frame(MsgType::Print, 0, &&over[..], &mut out),
        Err(Error::Encode)
    );
}

#[test]
fn decode_rejects_frame_over_max_frame() {
    // The receive path must reject an inner frame larger than MAX_FRAME so the encode-side
    // budget is a receive-side guarantee (a FrameDecoder can otherwise surface up to ~MAX_WIRE).
    let oversized = vec![0u8; MAX_FRAME + 1];
    assert_eq!(decode_frame(&oversized), Err(Error::TooLong));
    // A minimal valid-length frame is not rejected for length (it fails later, on the CRC,
    // which is checked before the tag and the type).
    let minimal = [0u8; 9];
    assert_eq!(decode_frame(&minimal), Err(Error::BadCrc));
}

// --- the frame budget holds for the largest real payloads --------------------------------

/// A `QueuePush` carrying a full radio-MTU downlink (74 bytes), the largest request, must fit a
/// single frame (requests are never chunked).
#[test]
fn max_queue_push_fits_one_frame() {
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

/// A `MgmtResponse` / `ShellResponse` chunk carrying the firmware's 192-byte chunk budget must
/// fit, and a worst-case `NodeEntry` record must stay small enough that a chunk always makes
/// progress (≥ 4 records per chunk keeps a 32-node list under 8 frames).
#[test]
fn chunking_budget_holds() {
    let chunk = [0xEEu8; 192];
    let rsp = MgmtResponse {
        req_id: u16::MAX,
        result: u8::MAX,
        chunk: u16::MAX,
        last: false,
        data: &chunk,
    };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::MgmtResponse, 0, &rsp, &mut out).expect("192-byte chunk must fit");
    let text = "x".repeat(192);
    let sr = ShellResponse {
        cmd_id: u16::MAX,
        result: u8::MAX,
        chunk: u16::MAX,
        last: false,
        text: &text,
    };
    encode_frame(MsgType::ShellResponse, 0, &sr, &mut out).expect("192-byte shell chunk must fit");

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

/// The largest `Log` the firmware can produce (192-char message, 24-char module, worst-case
/// uptime) must fit; guards the firmware's MAX_MSG.
#[test]
fn max_log_fits() {
    let message = "x".repeat(192);
    let module = "m".repeat(24);
    let l = Log {
        level: Level::Trace,
        uptime_us: u64::MAX,
        module: &module,
        message: &message,
    };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::Log, u16::MAX, &l, &mut out).expect("max log must fit the budget");
}

/// The firmware's worst-case `Event` (name 24, 6 fields of key 12 / value 20) must fit.
#[test]
fn max_event_fits() {
    let (name, k, v) = ("n".repeat(24), "k".repeat(12), "v".repeat(20));
    let mut fields: heapless::Vec<(&str, &str), 8> = heapless::Vec::new();
    for _ in 0..6 {
        fields.push((&k, &v)).unwrap();
    }
    let e = Event { name: &name, fields };
    let mut out = [0u8; MAX_WIRE];
    encode_frame(MsgType::Event, 0, &e, &mut out).expect("max event must fit the budget");
}

// --- every message round-trips ----------------------------------------------------------

/// Encode `p` as `mt`, deframe it byte by byte and decode it with `decode_msg`; the decoded
/// message must print exactly like the original (the payload types are borrow-heavy and do not
/// all implement `PartialEq`; their `Debug` shows every field).
fn roundtrip<T: serde::Serialize + core::fmt::Debug>(mt: MsgType, seq: u16, p: &T) {
    let mut out = [0u8; MAX_WIRE];
    let n = encode_frame(mt, seq, p, &mut out).unwrap();
    assert_eq!(
        out[..n - 1].iter().filter(|&&b| b == 0).count(),
        0,
        "no interior zero"
    );
    let inner = deframe(&out[..n]);
    let (rseq, msg) = decode_msg(&inner).unwrap();
    assert_eq!(rseq, seq);
    let expect = format!("{mt:?}({p:?})");
    assert_eq!(format!("{msg:?}"), expect);
}

#[test]
fn every_message_roundtrips() {
    roundtrip(
        MsgType::Hello,
        0,
        &Hello {
            firmware_name: "radio_push_button",
            firmware_version: "v0.1.0",
            session_id: 0xABCD_0123,
        },
    );
    roundtrip(
        MsgType::Log,
        1,
        &Log {
            level: Level::Warn,
            uptime_us: 1_234_567,
            module: "power",
            message: "USB connected",
        },
    );
    roundtrip(MsgType::Print, 2, &Print { text: "raw text" });
    let mut fields = heapless::Vec::new();
    fields.push(("temp", "23.5")).unwrap();
    fields.push(("rh", "41")).unwrap();
    roundtrip(
        MsgType::Event,
        3,
        &Event {
            name: "measurement",
            fields,
        },
    );
    roundtrip(
        MsgType::ShellResponse,
        4,
        &ShellResponse {
            cmd_id: 300,
            result: 4,
            chunk: 2,
            last: false,
            text: "hi",
        },
    );
    let mut candidates = heapless::Vec::new();
    for (text, kind) in [
        ("system", CandidateKind::Menu),
        ("reboot", CandidateKind::Command),
        ("name=", CandidateKind::Arg),
        ("on", CandidateKind::Value),
    ] {
        candidates.push(Candidate { text, kind }).unwrap();
    }
    roundtrip(
        MsgType::ShellCompletions,
        5,
        &ShellCompletions {
            req_id: 42,
            token_start: 3,
            common_prefix: "se",
            candidates,
            more: true,
        },
    );
    roundtrip(MsgType::Dropped, 6, &Dropped { count: 300 });
    roundtrip(
        MsgType::MgmtResponse,
        7,
        &MgmtResponse {
            req_id: 9,
            result: MGMT_TIMEOUT,
            chunk: 1,
            last: true,
            data: &[1, 2, 3],
        },
    );
    roundtrip(
        MsgType::Uplink,
        8,
        &Uplink {
            src: 0x0000_00BB,
            counter: 77,
            rssi: -91,
            lqi: 12,
            data: &[0xAA; 74],
        },
    );
    roundtrip(
        MsgType::RadioStat,
        9,
        &RadioStat::Channel {
            channel: 2,
            rssi: -117,
        },
    );
    roundtrip(
        MsgType::RadioStat,
        10,
        &RadioStat::Tx {
            dest: 5,
            item: 3,
            outcome: TX_NOT_DELIVERED,
            ack_rssi: Some(-80),
        },
    );
    roundtrip(
        MsgType::ShellCommand,
        11,
        &ShellCommand {
            cmd_id: 3,
            line: "/system identity print",
        },
    );
    roundtrip(
        MsgType::ShellComplete,
        12,
        &ShellComplete {
            req_id: 11,
            line: "/sys",
            cursor: 4,
        },
    );
    roundtrip(
        MsgType::MgmtRequest,
        13,
        &MgmtRequest {
            req_id: u16::MAX,
            op: MgmtOp::PairingOpen { window: 60 },
        },
    );
}

/// The borrow-heavy nested types at capacity: 16 completion candidates and 8 event fields.
#[test]
fn full_capacity_lists_roundtrip() {
    let mut candidates: heapless::Vec<Candidate, 16> = heapless::Vec::new();
    for _ in 0..16 {
        candidates
            .push(Candidate {
                text: "settings",
                kind: CandidateKind::Menu,
            })
            .unwrap();
    }
    roundtrip(
        MsgType::ShellCompletions,
        1,
        &ShellCompletions {
            req_id: 42,
            token_start: 3,
            common_prefix: "se",
            candidates,
            more: false,
        },
    );
    let mut fields: heapless::Vec<(&str, &str), 8> = heapless::Vec::new();
    for _ in 0..8 {
        fields.push(("k", "v")).unwrap();
    }
    roundtrip(
        MsgType::Event,
        3,
        &Event {
            name: "measurement",
            fields,
        },
    );
}

/// Bare postcard round trip of one value (an op inside a `MgmtRequest`, or a record inside
/// `MgmtResponse.data`), consuming the bytes exactly.
fn rec_roundtrip<'a, T>(v: &T, buf: &'a mut [u8]) -> T
where
    T: serde::Serialize + serde::Deserialize<'a> + PartialEq + core::fmt::Debug,
{
    let n = postcard::to_slice(v, buf).unwrap().len();
    let (back, rest) = postcard::take_from_bytes::<T>(&buf[..n]).unwrap();
    assert!(rest.is_empty());
    back
}

#[test]
fn every_mgmt_op_roundtrips() {
    let key: [u8; 16] = core::array::from_fn(|i| i as u8);
    let ops = [
        MgmtOp::Describe,
        MgmtOp::NodeList,
        MgmtOp::NodeAdd {
            addr: 0x1234_5678,
            key,
            name: "kitchen",
            flags: NODE_FLAG_SLEEPING,
        },
        MgmtOp::NodeRemove { addr: 7 },
        MgmtOp::NodeUpdate {
            addr: 8,
            name: Some("hall"),
            flags: None,
        },
        MgmtOp::NodeUpdate {
            addr: 9,
            name: None,
            flags: Some(NODE_FLAG_UNNAMED),
        },
        MgmtOp::NodeRevealKey { addr: 10 },
        MgmtOp::PairingOpen { window: 60 },
        MgmtOp::PairingCancel,
        MgmtOp::QueuePush {
            node_addr: 11,
            ttl: 600,
            data: &[1, 2, 3],
        },
        MgmtOp::QueueList { node_addr: 0 },
        MgmtOp::QueueDrop {
            node_addr: 12,
            item: Some(4),
        },
        MgmtOp::QueueDrop {
            node_addr: 12,
            item: None,
        },
        MgmtOp::StatsConfig {
            channel_period_ms: 5000,
        },
        MgmtOp::Provision(Provision {
            addr: Some(0x0000_00BB),
            gw_addr: 0x2222_2222,
            key,
            band: BAND_US915,
            channel: 2,
        }),
        MgmtOp::Provision(Provision {
            addr: None,
            gw_addr: 1,
            key,
            band: BAND_EU868,
            channel: 0,
        }),
        MgmtOp::JoinOpen { window: 30 },
    ];
    for op in &ops {
        let mut buf = [0u8; 128];
        assert_eq!(&rec_roundtrip(op, &mut buf), op);
    }
}

#[test]
fn every_mgmt_record_roundtrips() {
    let mut buf = [0u8; 128];
    for role in [DeviceRole::Gateway, DeviceRole::Node, DeviceRole::Other] {
        let info = DeviceInfo {
            role,
            addr: 0x2222_2222,
            band: BAND_US915,
            channel: 1,
            node_capacity: 16,
            node_count: 3,
            provisioned: true,
            gw_addr: 0x2222_2222,
            firmware_name: "radio_dongle_gateway",
            firmware_version: "v0.1.0",
        };
        assert_eq!(rec_roundtrip(&info, &mut buf), info);
    }
    let entry = NodeEntry {
        addr: 5,
        name: "button",
        flags: NODE_FLAG_SLEEPING | NODE_FLAG_UNNAMED,
        last_seen: LAST_SEEN_NEVER,
        rssi: RSSI_NONE,
        uplinks: 99,
        queued: 2,
    };
    assert_eq!(rec_roundtrip(&entry, &mut buf), entry);
    let nk = NodeKey {
        addr: 6,
        key: [0xA5; 16],
    };
    assert_eq!(rec_roundtrip(&nk, &mut buf), nk);
    assert_eq!(rec_roundtrip(&Paired { addr: 7 }, &mut buf), Paired { addr: 7 });
    assert_eq!(rec_roundtrip(&QueueId { item: 8 }, &mut buf), QueueId { item: 8 });
    let qe = QueueEntry {
        node_addr: 9,
        item: 10,
        age: 11,
        ttl: 12,
        data: &[3, 2, 1],
    };
    assert_eq!(rec_roundtrip(&qe, &mut buf), qe);
    assert_eq!(
        rec_roundtrip(&ProvisionAck { addr: 13 }, &mut buf),
        ProvisionAck { addr: 13 }
    );
    assert_eq!(
        rec_roundtrip(&Joined { gw_addr: 14 }, &mut buf),
        Joined { gw_addr: 14 }
    );
}

/// The documented `MgmtResponse.data` stream contract: reply records concatenate across chunks
/// and decode back with repeated `take_from_bytes`. Exercises the real host consumption path (a
/// `NodeList` split over two frames) end to end.
#[test]
fn mgmt_response_record_stream_reassembles() {
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
        let mut buf = [0u8; 64];
        stream.extend_from_slice(postcard::to_slice(e, &mut buf).unwrap());
    }
    // Split the record stream across two MgmtResponse chunks (chunk 0 not-last, 1 last), each
    // riding the real frame codec, then reassemble the payloads.
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
        let mut out = [0u8; MAX_WIRE];
        let n = encode_frame(MsgType::MgmtResponse, i as u16, &r, &mut out).unwrap();
        let inner = deframe(&out[..n]);
        match decode_msg(&inner).unwrap().1 {
            Msg::MgmtResponse(back) => reassembled.extend_from_slice(back.data),
            other => panic!("expected MgmtResponse, got {other:?}"),
        }
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
