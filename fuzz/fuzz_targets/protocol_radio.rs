//! `protocol_radio`: the radio application schema and the management record stream.
//!
//! * The whole input is decoded as a `NodeMsg` envelope (what the host does with a forwarded
//!   `Uplink.data`) and as a `NodeCmd` envelope (what a node does with a downlink), and as a
//!   management reply record stream of every record type (the host's `parse_records`). Decodes
//!   must be total, fail with the documented error, and round-trip whatever they accept.
//! * The same bytes also drive [`arbitrary`] to BUILD `NodeMsg` / `NodeCmd` values the way node
//!   firmware and the host do, checking the encoders never emit an over-MTU envelope and that
//!   everything they emit decodes back to the same value.
#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use tower_fuzz::{
    check_encode_node_cmd, check_encode_node_msg, check_node_cmd, check_node_msg, check_records,
};
use tower_protocol::radio::{AccelKind, ButtonKind, NodeCmd, NodeInfo, NodeMsg, NodeShellChunk};

#[derive(Arbitrary, Debug)]
enum MsgSpec<'a> {
    Info {
        firmware_name: &'a str,
        firmware_version: &'a str,
        session_id: u32,
        sleeping: bool,
    },
    Button {
        kind: u8,
        count: u32,
    },
    Temperature {
        millic: i32,
    },
    Accel {
        orientation: bool,
        face: u8,
    },
    Shell {
        cmd_id: u16,
        result: u8,
        chunk: u16,
        last: bool,
        text: &'a str,
    },
}

impl<'a> MsgSpec<'a> {
    fn build(&self) -> NodeMsg<'a> {
        match *self {
            MsgSpec::Info {
                firmware_name,
                firmware_version,
                session_id,
                sleeping,
            } => NodeMsg::Info(NodeInfo {
                firmware_name,
                firmware_version,
                session_id,
                sleeping,
            }),
            MsgSpec::Button { kind, count } => NodeMsg::Button {
                kind: [
                    ButtonKind::Press,
                    ButtonKind::Release,
                    ButtonKind::Click,
                    ButtonKind::Hold,
                ][kind as usize % 4],
                count,
            },
            MsgSpec::Temperature { millic } => NodeMsg::Temperature { millic },
            MsgSpec::Accel { orientation, face } => NodeMsg::Accel {
                kind: if orientation {
                    AccelKind::Orientation
                } else {
                    AccelKind::Motion
                },
                face,
            },
            MsgSpec::Shell {
                cmd_id,
                result,
                chunk,
                last,
                text,
            } => NodeMsg::Shell(NodeShellChunk {
                cmd_id,
                result,
                chunk,
                last,
                text,
            }),
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // Decoder side: the same bytes under every interpretation a consumer may apply.
    check_node_msg(data);
    check_node_cmd(data);
    check_records(data);

    // Encoder side: values built from the bytes.
    let mut u = Unstructured::new(data);
    while let Ok(spec) = MsgSpec::arbitrary(&mut u) {
        check_encode_node_msg(&spec.build());
        let Ok((epoch, cmd_id, line)) = <(u32, u16, &str)>::arbitrary(&mut u) else {
            break;
        };
        check_encode_node_cmd(&NodeCmd::Shell { epoch, cmd_id, line });
        if u.is_empty() {
            break;
        }
    }
});
