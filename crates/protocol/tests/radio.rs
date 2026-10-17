//! The radio application envelope (`src/radio.rs`): its layout (`RADIO_TAG` then the postcard
//! body), the MTU budget of the largest messages, the guards (a foreign tag, a tail, an over-MTU
//! encode), and a round trip of every `NodeMsg` / `NodeCmd` variant.
//!
//! No byte goldens of the schema: it is identified by `RADIO_TAG`, which follows any change to
//! it (only the node firmware and the host decode it; every end rebuilds from the same checkout).

use tower_protocol::Error;
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

/// An envelope is the 2-byte tag (little-endian) followed by exactly the postcard body.
#[test]
fn envelope_is_the_tag_then_the_postcard_body() {
    let m = NodeMsg::Button {
        kind: ButtonKind::Click,
        count: 42,
    };
    let bytes = enc_msg(&m);
    assert_eq!(ENVELOPE_HDR, 2);
    assert_eq!(bytes[..2], RADIO_TAG.to_le_bytes());
    let mut body = [0u8; MAX_RADIO_PAYLOAD];
    let body = postcard::to_slice(&m, &mut body).unwrap();
    assert_eq!(&bytes[2..], &body[..]);

    let c = NodeCmd::Shell {
        epoch: 7,
        cmd_id: 9,
        line: "/system reboot",
    };
    let bytes = enc_cmd(&c);
    assert_eq!(bytes[..2], RADIO_TAG.to_le_bytes());
    let mut body = [0u8; MAX_RADIO_PAYLOAD];
    let body = postcard::to_slice(&c, &mut body).unwrap();
    assert_eq!(&bytes[2..], &body[..]);
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
    let c = NodeCmd::Shell {
        epoch: u32::MAX,
        cmd_id: u16::MAX,
        line: &line,
    };
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
    });
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    encode_node_msg(&m, &mut buf).expect("max NodeInfo must fit the radio MTU");
}

// --- envelope guards --------------------------------------------------------------

/// A foreign tag surfaces as `SchemaMismatch { got }`, never a mis-decode; too short for the tag
/// is `TooShort`.
#[test]
fn foreign_tag_is_rejected() {
    let mut bytes = enc_msg(&NodeMsg::Temperature { millic: 1 });
    let forged = RADIO_TAG ^ 0x0100;
    bytes[..2].copy_from_slice(&forged.to_le_bytes());
    assert_eq!(
        decode_node_msg(&bytes),
        Err(Error::SchemaMismatch { got: forged })
    );
    let mut cmd = enc_cmd(&NodeCmd::Shell {
        epoch: 1,
        cmd_id: 2,
        line: "x",
    });
    cmd[..2].copy_from_slice(&forged.to_le_bytes());
    assert_eq!(decode_node_cmd(&cmd), Err(Error::SchemaMismatch { got: forged }));
    assert_eq!(decode_node_msg(&[]), Err(Error::TooShort));
    assert_eq!(
        decode_node_msg(&RADIO_TAG.to_le_bytes()[..1]),
        Err(Error::TooShort)
    );
    let mut tiny = [0u8; 1];
    assert_eq!(
        encode_node_msg(&NodeMsg::Temperature { millic: 0 }, &mut tiny),
        Err(Error::Overflow)
    );
}

/// An envelope from the tree that led with a one-byte schema version (protocol-v1.6.0; a
/// `NodeMsg::Button { Click, 42 }` was `[0x02, 0x01, 0x02, 0x2a]`) is refused on its tag.
#[test]
fn a_version_byte_envelope_from_an_older_tree_is_refused() {
    let old = [0x02, 0x01, 0x02, 0x2a];
    assert_ne!(RADIO_TAG, 0x0102);
    assert_eq!(decode_node_msg(&old), Err(Error::SchemaMismatch { got: 0x0102 }));
}

/// Trailing bytes after a valid body are `Malformed` for both directions: postcard's
/// `from_bytes` would silently ignore them, half-reading a node built from another schema. The
/// net layer delivers the exact payload length, so a legitimate envelope never has a tail.
#[test]
fn trailing_bytes_are_malformed() {
    let mut msg = enc_msg(&NodeMsg::Button {
        kind: ButtonKind::Hold,
        count: 7,
    });
    assert!(decode_node_msg(&msg).is_ok());
    msg.push(0);
    assert_eq!(decode_node_msg(&msg), Err(Error::Malformed));

    let mut cmd = enc_cmd(&NodeCmd::Shell {
        epoch: 42,
        cmd_id: 3,
        line: "/led on",
    });
    assert!(decode_node_cmd(&cmd).is_ok());
    cmd.push(0);
    assert_eq!(decode_node_cmd(&cmd), Err(Error::Malformed));
}

/// An oversize encode is rejected, not truncated, even into a roomier buffer.
#[test]
fn over_mtu_encode_is_rejected() {
    let text: String = "z".repeat(MAX_RADIO_PAYLOAD); // envelope overhead pushes it past
    let m = NodeMsg::Shell(NodeShellChunk {
        cmd_id: 0,
        result: 0,
        chunk: 0,
        last: true,
        text: &text,
    });
    let mut big = [0u8; 256];
    assert_eq!(encode_node_msg(&m, &mut big), Err(Error::Encode));
}

/// Every variant round-trips through encode → decode.
#[test]
fn every_variant_roundtrips() {
    let msgs = [
        NodeMsg::Info(NodeInfo {
            firmware_name: "radio_push_button",
            firmware_version: "v0.1.0",
            session_id: 3,
            sleeping: true,
        }),
        NodeMsg::Info(NodeInfo {
            firmware_name: "radio_climate",
            firmware_version: "v1.2.3",
            session_id: 77,
            sleeping: false,
        }),
        NodeMsg::Button {
            kind: ButtonKind::Press,
            count: 1,
        },
        NodeMsg::Button {
            kind: ButtonKind::Release,
            count: 2,
        },
        NodeMsg::Button {
            kind: ButtonKind::Click,
            count: 3,
        },
        NodeMsg::Button {
            kind: ButtonKind::Hold,
            count: 7,
        },
        NodeMsg::Temperature { millic: -12_345 },
        NodeMsg::Accel {
            kind: AccelKind::Motion,
            face: 0,
        },
        NodeMsg::Accel {
            kind: AccelKind::Orientation,
            face: 6,
        },
        NodeMsg::Shell(NodeShellChunk {
            cmd_id: 300,
            result: 4,
            chunk: 2,
            last: false,
            text: "hi",
        }),
    ];
    for m in &msgs {
        let mut buf = [0u8; MAX_RADIO_PAYLOAD];
        let n = encode_node_msg(m, &mut buf).unwrap();
        assert_eq!(&decode_node_msg(&buf[..n]).unwrap(), m);
    }
    let c = NodeCmd::Shell {
        epoch: 42,
        cmd_id: 3,
        line: "/led on",
    };
    let mut buf = [0u8; MAX_RADIO_PAYLOAD];
    let n = encode_node_cmd(&c, &mut buf).unwrap();
    assert_eq!(decode_node_cmd(&buf[..n]).unwrap(), c);
}
