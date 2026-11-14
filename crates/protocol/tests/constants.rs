//! The wire values the schema tags cannot see.
//!
//! `CONSOLE_TAG` and `RADIO_TAG` follow the *shape* of the schema (docs/protocol.md): a field
//! added, a variant moved. A constant carried in a `u8` field has no shape: change `MGMT_FULL`
//! from 4 to 5 and both tags stay where they are, the frames still decode, and the two ends
//! read the same byte as two different things. So these values are pinned here, written out as
//! numbers: changing one is a wire change like any other (every end rebuilt from the same
//! checkout), and it has to be made on purpose.

use tower_protocol::MsgType;
use tower_protocol::mgmt::*;
use tower_protocol::radio::{ENVELOPE_HDR, MAX_RADIO_PAYLOAD, MAX_REPORT, RADIO_SHELL_CHUNK};

/// `MgmtResponse.result`.
#[test]
fn management_result_codes() {
    assert_eq!(
        [
            MGMT_OK,
            MGMT_UNSUPPORTED,
            MGMT_BAD_ARG,
            MGMT_NOT_FOUND,
            MGMT_FULL,
            MGMT_BUSY,
            MGMT_STORAGE,
            MGMT_TIMEOUT
        ],
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
}

/// `RadioStat::Tx.outcome`.
#[test]
fn transmit_outcome_codes() {
    assert_eq!(
        [
            TX_DELIVERED,
            TX_NOT_DELIVERED,
            TX_BUSY,
            TX_DUTY_LIMITED,
            TX_ERROR,
            TX_EXPIRED
        ],
        [0, 1, 2, 3, 4, 5]
    );
}

/// `NodeEntry.flags` (one bit each), the "never" sentinels of a registry row, the bands and the
/// limits a request is checked against.
#[test]
fn registry_flags_sentinels_and_limits() {
    assert_eq!((NODE_FLAG_SLEEPING, NODE_FLAG_UNNAMED), (0x01, 0x02));
    assert_eq!(LAST_SEEN_NEVER, 0xFFFF_FFFF);
    assert_eq!(RSSI_NONE, 127);
    assert_eq!((BAND_EU868, BAND_US915), (0, 1));
    assert_eq!(MAX_CHANNEL, 2);
    assert_eq!(MAX_NODE_NAME, 16);
}

/// The type byte of every console frame: target to host from 0, host to target from 16.
#[test]
fn message_type_bytes() {
    let bytes: Vec<(MsgType, u8)> = MsgType::ALL.iter().map(|&t| (t, t as u8)).collect();
    assert_eq!(
        bytes,
        [
            (MsgType::Hello, 0),
            (MsgType::Log, 1),
            (MsgType::Print, 2),
            (MsgType::Event, 3),
            (MsgType::ShellResponse, 4),
            (MsgType::ShellCompletions, 5),
            (MsgType::Dropped, 6),
            (MsgType::MgmtResponse, 7),
            (MsgType::Uplink, 8),
            (MsgType::RadioStat, 9),
            (MsgType::ShellCommand, 16),
            (MsgType::ShellComplete, 17),
            (MsgType::MgmtRequest, 18),
        ]
    );
    for (t, byte) in bytes {
        assert_eq!(MsgType::from_u8(byte), Some(t));
    }
}

/// The radio envelope's budget: what a node and the host size their buffers and chunk their
/// shell output by.
#[test]
fn radio_envelope_budget() {
    assert_eq!(ENVELOPE_HDR, 2);
    assert_eq!(MAX_RADIO_PAYLOAD, 74);
    assert_eq!(RADIO_SHELL_CHUNK, 56);
    assert_eq!(MAX_REPORT, 10);
}
