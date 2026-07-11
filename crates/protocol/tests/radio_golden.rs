//! Golden vectors for the **radio application schema** (`src/radio.rs`) — the same
//! anti-drift role `tests/golden.rs` plays for the console framing, but guarded by
//! `RADIO_SCHEMA_VERSION` instead of `PROTOCOL_VERSION` (the schema rides opaquely
//! through the gateway; only node firmware + the host CLI must agree on it).
//!
//! ⚠️ If a change makes these vectors need updating, the radio schema changed — you
//! MUST bump `RADIO_SCHEMA_VERSION` in `src/radio.rs` (and the crate version + tag)
//! and re-pin node firmware + tower-cli. `tools/check_wire_bump.py` enforces this
//! mechanically. Do not "just fix the bytes".

use tower_protocol::radio::*;

fn enc_msg(m: &NodeMsg) -> Vec<u8> {
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    let n = encode_node_msg(m, &mut buf).unwrap();
    buf[..n].to_vec()
}

fn enc_cmd(c: &NodeCmd) -> Vec<u8> {
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    let n = encode_node_cmd(c, &mut buf).unwrap();
    buf[..n].to_vec()
}

#[test]
fn golden_node_info() {
    let got = enc_msg(&NodeMsg::Info(NodeInfo {
        firmware_name: "radio_push_button",
        firmware_version: "v0.1.0",
        session_id: 3,
        sleeping: true,
        battery_mv: None,
    }));
    assert_eq!(
        got,
        [
            0x01, 0x00, 0x11, 0x72, 0x61, 0x64, 0x69, 0x6f, 0x5f, 0x70, 0x75, 0x73, 0x68, 0x5f,
            0x62, 0x75, 0x74, 0x74, 0x6f, 0x6e, 0x06, 0x76, 0x30, 0x2e, 0x31, 0x2e, 0x30, 0x03,
            0x01, 0x00
        ]
    );
}

#[test]
fn golden_node_button() {
    let got = enc_msg(&NodeMsg::Button { kind: ButtonKind::Click, count: 42 });
    assert_eq!(got, [0x01, 0x01, 0x02, 0x2a]);
}

#[test]
fn golden_node_temperature() {
    let got = enc_msg(&NodeMsg::Temperature { millic: 21_375 });
    assert_eq!(got, [0x01, 0x02, 0xfe, 0xcd, 0x02]);
}

#[test]
fn golden_node_accel() {
    let got = enc_msg(&NodeMsg::Accel { kind: AccelKind::Orientation, face: 3 });
    assert_eq!(got, [0x01, 0x03, 0x01, 0x03]);
}

#[test]
fn golden_node_shell() {
    let got = enc_msg(&NodeMsg::Shell(NodeShellChunk {
        cmd_id: 9,
        result: 0,
        chunk: 0,
        last: true,
        text: "ok",
    }));
    assert_eq!(got, [0x01, 0x04, 0x09, 0x00, 0x00, 0x01, 0x02, 0x6f, 0x6b]);
}

#[test]
fn golden_node_cmd_shell() {
    let got = enc_cmd(&NodeCmd::Shell { cmd_id: 9, line: "/system reboot" });
    assert_eq!(
        got,
        [
            0x01, 0x00, 0x09, 0x0e, 0x2f, 0x73, 0x79, 0x73, 0x74, 0x65, 0x6d, 0x20, 0x72, 0x65,
            0x62, 0x6f, 0x6f, 0x74
        ]
    );
}

// --- variant-order pins ----------------------------------------------------------

#[test]
fn button_kind_variant_order_is_stable() {
    for (i, kind) in [ButtonKind::Press, ButtonKind::Release, ButtonKind::Click, ButtonKind::Hold]
        .into_iter()
        .enumerate()
    {
        let mut buf = [0u8; 4];
        let n = postcard::to_slice(&kind, &mut buf).unwrap().len();
        assert_eq!(n, 1);
        assert_eq!(buf[0], i as u8, "ButtonKind variant order changed — schema break");
    }
}

#[test]
fn accel_kind_variant_order_is_stable() {
    for (i, kind) in [AccelKind::Motion, AccelKind::Orientation].into_iter().enumerate() {
        let mut buf = [0u8; 4];
        let n = postcard::to_slice(&kind, &mut buf).unwrap().len();
        assert_eq!(n, 1);
        assert_eq!(buf[0], i as u8, "AccelKind variant order changed — schema break");
    }
}

/// Pin the `NodeMsg` / `NodeCmd` variant indices via the envelope's second byte
/// (byte 0 is `RADIO_SCHEMA_VERSION`).
#[test]
fn node_msg_variant_order_is_stable() {
    let msgs: [NodeMsg; 5] = [
        NodeMsg::Info(NodeInfo {
            firmware_name: "n",
            firmware_version: "v",
            session_id: 1,
            sleeping: false,
            battery_mv: None,
        }),
        NodeMsg::Button { kind: ButtonKind::Press, count: 1 },
        NodeMsg::Temperature { millic: 0 },
        NodeMsg::Accel { kind: AccelKind::Motion, face: 0 },
        NodeMsg::Shell(NodeShellChunk { cmd_id: 0, result: 0, chunk: 0, last: true, text: "" }),
    ];
    for (i, m) in msgs.iter().enumerate() {
        let bytes = enc_msg(m);
        assert_eq!(bytes[0], RADIO_SCHEMA_VERSION);
        assert_eq!(bytes[1], i as u8, "NodeMsg variant order changed — schema break");
    }
    let cmd = enc_cmd(&NodeCmd::Shell { cmd_id: 0, line: "" });
    assert_eq!(cmd[1], 0, "NodeCmd variant order changed — schema break");
}

// --- capacity: everything fits the radio MTU -------------------------------------

/// A shell chunk at the full `RADIO_SHELL_CHUNK` budget, worst-case metadata.
#[test]
fn max_shell_chunk_fits_the_mtu() {
    let text: String = "x".repeat(RADIO_SHELL_CHUNK);
    let m = NodeMsg::Shell(NodeShellChunk {
        cmd_id: u16::MAX,
        result: u8::MAX,
        chunk: u16::MAX,
        last: false,
        text: &text,
    });
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    encode_node_msg(&m, &mut buf).expect("max shell chunk must fit the radio MTU");
}

/// A `NodeCmd::Shell` line at the `RADIO_SHELL_CHUNK` budget.
#[test]
fn max_shell_line_fits_the_mtu() {
    let line: String = "y".repeat(RADIO_SHELL_CHUNK);
    let c = NodeCmd::Shell { cmd_id: u16::MAX, line: &line };
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    encode_node_cmd(&c, &mut buf).expect("max shell line must fit the radio MTU");
}

/// A `NodeInfo` at the documented name/version budgets (24 + 8 bytes).
#[test]
fn max_node_info_fits_the_mtu() {
    let name: String = "n".repeat(24);
    let ver: String = "v".repeat(8);
    let m = NodeMsg::Info(NodeInfo {
        firmware_name: &name,
        firmware_version: &ver,
        session_id: u32::MAX,
        sleeping: true,
        battery_mv: Some(u16::MAX),
    });
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    encode_node_msg(&m, &mut buf).expect("max NodeInfo must fit the radio MTU");
}

// --- envelope guards --------------------------------------------------------------

/// A schema-version mismatch surfaces as `BadVersion { got }` — never a mis-decode.
#[test]
fn wrong_schema_version_is_rejected() {
    let mut bytes = enc_msg(&NodeMsg::Temperature { millic: 1 });
    bytes[0] = RADIO_SCHEMA_VERSION + 1;
    assert_eq!(
        decode_node_msg(&bytes),
        Err(tower_protocol::Error::BadVersion { got: RADIO_SCHEMA_VERSION + 1 })
    );
    assert_eq!(decode_node_msg(&[]), Err(tower_protocol::Error::TooShort));
}

/// An oversize encode is rejected, not truncated — even into a roomier buffer.
#[test]
fn over_mtu_encode_is_rejected() {
    let text: String = "z".repeat(MAX_RADIO_PAYLOAD); // envelope overhead pushes it past
    let m = NodeMsg::Shell(NodeShellChunk { cmd_id: 0, result: 0, chunk: 0, last: true, text: &text });
    let mut big = [0u8; 256];
    assert_eq!(encode_node_msg(&m, &mut big), Err(tower_protocol::Error::Encode));
}

/// Round-trip through encode → decode for each variant (the golden vectors pin the
/// bytes; this pins the decode path against them).
#[test]
fn node_msgs_roundtrip() {
    let msgs = [
        NodeMsg::Button { kind: ButtonKind::Hold, count: 7 },
        NodeMsg::Temperature { millic: -12_345 },
        NodeMsg::Accel { kind: AccelKind::Motion, face: 0 },
    ];
    for m in &msgs {
        let mut buf = [0u8; MAX_RADIO_PAYLOAD];
        let n = encode_node_msg(m, &mut buf).unwrap();
        assert_eq!(&decode_node_msg(&buf[..n]).unwrap(), m);
    }
    let c = NodeCmd::Shell { cmd_id: 3, line: "/led on" };
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    let n = encode_node_cmd(&c, &mut buf).unwrap();
    assert_eq!(decode_node_cmd(&buf[..n]).unwrap(), c);
}
