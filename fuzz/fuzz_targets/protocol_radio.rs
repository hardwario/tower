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
use tower_protocol::radio::{MAX_REPORT, NodeCmd, NodeInfo, NodeMsg, NodeShellChunk};

#[derive(Arbitrary, Debug)]
enum MsgSpec<'a> {
    Info {
        firmware_name: &'a str,
        firmware_version: &'a str,
        session_id: u32,
        sleeping: bool,
        wants_time: bool,
    },
    Report {
        values: Vec<(u8, i32)>,
    },
    Event {
        id: u8,
        value: i32,
        count: u32,
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
                wants_time,
            } => NodeMsg::Info(NodeInfo {
                firmware_name,
                firmware_version,
                session_id,
                sleeping,
                wants_time,
            }),
            // A node never packs more than MAX_REPORT values into one Report.
            MsgSpec::Report { ref values } => {
                NodeMsg::Report(values.iter().copied().take(MAX_REPORT).collect())
            }
            MsgSpec::Event { id, value, count } => NodeMsg::Event { id, value, count },
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

#[derive(Arbitrary, Debug)]
enum CmdSpec<'a> {
    Shell { epoch: u32, cmd_id: u16, line: &'a str },
    Set { id: u8, value: i32 },
    Time { unix: u32, utc_offset_min: i16 },
    Describe,
}

impl<'a> CmdSpec<'a> {
    fn build(&self) -> NodeCmd<'a> {
        match *self {
            CmdSpec::Shell { epoch, cmd_id, line } => NodeCmd::Shell { epoch, cmd_id, line },
            CmdSpec::Set { id, value } => NodeCmd::Set { id, value },
            CmdSpec::Time { unix, utc_offset_min } => NodeCmd::Time { unix, utc_offset_min },
            CmdSpec::Describe => NodeCmd::Describe,
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
        let Ok(cmd) = CmdSpec::arbitrary(&mut u) else {
            break;
        };
        check_encode_node_cmd(&cmd.build());
        if u.is_empty() {
            break;
        }
    }
});
