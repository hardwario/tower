//! Shared oracles for the `protocol_frames` and `protocol_radio` fuzz targets.
//!
//! Every check here is a property the crate documents (or a consumer relies on), asserted with a
//! panic so libFuzzer records the input:
//!
//! * [`check_inner`]: `decode_frame`'s load-bearing check order (length, CRC, version, type),
//!   `decode_msg` agreeing with it, and a decode / re-encode / decode round trip through the real
//!   COBS framing ([`tower_protocol::encode_frame`] + [`FrameDecoder`]).
//! * [`check_node_msg`] / [`check_node_cmd`]: the radio envelope decoders, plus re-encoding within
//!   the radio MTU.
//! * [`check_records`]: the management reply record stream, decoded exactly like the host CLI's
//!   `parse_records` (repeated `postcard::take_from_bytes`), which must always make progress.
//!
//! The harness is std (it runs on the host), so it allocates freely; the crates under test are
//! no_std and never do.

use tower_protocol::mgmt::{
    DeviceInfo, Joined, MgmtOp, NodeEntry, NodeKey, Paired, Provision, ProvisionAck, QueueEntry, QueueId,
};
use tower_protocol::radio::{self, MAX_RADIO_PAYLOAD, NodeCmd, NodeMsg, RADIO_SCHEMA_VERSION};
use tower_protocol::{
    Error, FrameDecoder, MAX_FRAME, MAX_PAYLOAD, MAX_WIRE, Msg, MsgType, PROTOCOL_VERSION, crc, decode_frame,
    decode_msg, encode_frame,
};

/// Header (`ver_type` + `seq`) and CRC trailer sizes of an inner frame (see the crate docs).
const HDR: usize = 3;
const CRC_LEN: usize = 4;

/// The [`MsgType`] a decoded [`Msg`] variant belongs to.
fn type_of(m: &Msg<'_>) -> MsgType {
    match m {
        Msg::Hello(_) => MsgType::Hello,
        Msg::Log(_) => MsgType::Log,
        Msg::Print(_) => MsgType::Print,
        Msg::Event(_) => MsgType::Event,
        Msg::ShellResponse(_) => MsgType::ShellResponse,
        Msg::ShellCompletions(_) => MsgType::ShellCompletions,
        Msg::Dropped(_) => MsgType::Dropped,
        Msg::MgmtResponse(_) => MsgType::MgmtResponse,
        Msg::Uplink(_) => MsgType::Uplink,
        Msg::RadioStat(_) => MsgType::RadioStat,
        Msg::ShellCommand(_) => MsgType::ShellCommand,
        Msg::ShellComplete(_) => MsgType::ShellComplete,
        Msg::MgmtRequest(_) => MsgType::MgmtRequest,
    }
}

/// Re-encode a decoded message as a full wire frame (COBS + `0x00`) into `out`.
fn reencode(seq: u16, m: &Msg<'_>, out: &mut [u8]) -> Result<usize, Error> {
    let t = type_of(m);
    match m {
        Msg::Hello(x) => encode_frame(t, seq, x, out),
        Msg::Log(x) => encode_frame(t, seq, x, out),
        Msg::Print(x) => encode_frame(t, seq, x, out),
        Msg::Event(x) => encode_frame(t, seq, x, out),
        Msg::ShellResponse(x) => encode_frame(t, seq, x, out),
        Msg::ShellCompletions(x) => encode_frame(t, seq, x, out),
        Msg::Dropped(x) => encode_frame(t, seq, x, out),
        Msg::MgmtResponse(x) => encode_frame(t, seq, x, out),
        Msg::Uplink(x) => encode_frame(t, seq, x, out),
        Msg::RadioStat(x) => encode_frame(t, seq, x, out),
        Msg::ShellCommand(x) => encode_frame(t, seq, x, out),
        Msg::ShellComplete(x) => encode_frame(t, seq, x, out),
        Msg::MgmtRequest(x) => encode_frame(t, seq, x, out),
    }
}

/// Push a whole wire frame through a fresh [`FrameDecoder`]: it must yield exactly one inner
/// frame, on the final `0x00` and on no earlier byte. Returns that inner frame (copied).
pub fn deframe_one(wire: &[u8]) -> Vec<u8> {
    let (&last, body) = wire.split_last().expect("a wire frame is never empty");
    assert_eq!(last, 0x00, "a wire frame ends with the 0x00 delimiter");
    assert!(
        !body.contains(&0x00),
        "COBS output must contain no 0x00 before the delimiter"
    );
    let mut dec = FrameDecoder::new();
    for &b in body {
        assert!(
            dec.push(b).is_none(),
            "the deframer yielded a frame before the delimiter"
        );
    }
    dec.push(0x00)
        .expect("a well-formed wire frame must deframe")
        .to_vec()
}

/// The documented outcome of [`decode_frame`] for `inner`, computed independently of it:
/// length first, then the CRC, then version, then type (`Ok` carries `(type, seq)`).
fn expected_frame(inner: &[u8]) -> Result<(MsgType, u16), Error> {
    if inner.len() < HDR + CRC_LEN {
        return Err(Error::TooShort);
    }
    if inner.len() > MAX_FRAME {
        return Err(Error::TooLong);
    }
    let body = inner.len() - CRC_LEN;
    let stored = u32::from_le_bytes(inner[body..].try_into().unwrap());
    if crc::crc32_ieee(&inner[..body]) != stored {
        return Err(Error::BadCrc);
    }
    let ver = inner[0] >> 5;
    if ver != PROTOCOL_VERSION {
        return Err(Error::BadVersion { got: ver });
    }
    let t = MsgType::from_u8(inner[0] & 0x1F).ok_or(Error::BadType)?;
    Ok((t, u16::from_le_bytes([inner[1], inner[2]])))
}

/// Check one deframed inner buffer (whatever a [`FrameDecoder`] yielded, or a frame the harness
/// built): the decode is total (never panics), follows the documented check order, and anything
/// that decodes re-encodes into a frame that decodes to the same value.
pub fn check_inner(inner: &[u8]) {
    // The deframer buffers at most MAX_WIRE bytes and COBS-decoding only shrinks.
    assert!(inner.len() <= MAX_WIRE, "inner frame longer than MAX_WIRE");

    let frame = decode_frame(inner);
    let expected = expected_frame(inner);
    match (&frame, &expected) {
        (Ok((t, seq, payload)), Ok((et, eseq))) => {
            assert_eq!((t, seq), (et, eseq), "decode_frame header mismatch");
            assert_eq!(payload.len(), inner.len() - HDR - CRC_LEN);
            assert!(payload.len() <= MAX_PAYLOAD, "payload above MAX_PAYLOAD surfaced");
        }
        (Err(e), Err(ee)) => assert_eq!(e, ee, "decode_frame error / check order mismatch"),
        _ => panic!("decode_frame {frame:?} but the documented check order says {expected:?}"),
    }

    match (decode_msg(inner), frame) {
        (Err(e), Err(fe)) => assert_eq!(e, fe, "decode_msg must surface decode_frame's error"),
        (Err(e), Ok(_)) => assert_eq!(e, Error::Malformed, "a framing-valid frame can only be Malformed"),
        (Ok(_), Err(fe)) => panic!("decode_msg accepted a frame decode_frame rejects ({fe:?})"),
        (Ok((seq, msg)), Ok((t, fseq, _))) => {
            assert_eq!(seq, fseq);
            assert_eq!(
                type_of(&msg),
                t,
                "decode_msg picked a payload type that does not match"
            );
            roundtrip(seq, &msg);
            chain(&msg);
        }
    }
}

/// decode -> re-encode -> deframe -> decode: same value (compared through `Debug`, the console
/// payload types do not implement `PartialEq`), and re-encoding is deterministic.
fn roundtrip(seq: u16, msg: &Msg<'_>) {
    let mut wire = [0u8; MAX_WIRE];
    // postcard's canonical encoding is never longer than any encoding it accepts (non-minimal
    // varints decode too), so a decoded payload always fits a frame again.
    let n = reencode(seq, msg, &mut wire).expect("a decoded message must re-encode");
    let wire = &wire[..n];

    // A too-small output buffer is an error, never a panic or a partial write reported as Ok.
    let mut short = vec![0u8; n - 1];
    assert_eq!(reencode(seq, msg, &mut short), Err(Error::Overflow));

    let inner2 = deframe_one(wire);
    let (seq2, msg2) = decode_msg(&inner2).expect("a re-encoded frame must decode");
    assert_eq!(seq2, seq);
    assert_eq!(
        format!("{msg:?}"),
        format!("{msg2:?}"),
        "round trip changed the value"
    );

    let mut wire2 = [0u8; MAX_WIRE];
    let n2 = reencode(seq2, &msg2, &mut wire2).expect("re-encode of the round-tripped value");
    assert_eq!(&wire2[..n2], wire, "encoding is not deterministic");
}

/// Payloads that carry a nested schema are decoded the way their consumer decodes them.
fn chain(msg: &Msg<'_>) {
    match msg {
        // The host decodes a forwarded uplink's `data` as a radio NodeMsg envelope.
        Msg::Uplink(u) => check_node_msg(u.data),
        // A queued downlink is a NodeCmd envelope the node decodes after the gateway forwards it.
        Msg::MgmtRequest(r) => {
            if let MgmtOp::QueuePush { data, .. } = &r.op {
                check_node_cmd(data);
            }
        }
        // A reply chunk's data is (part of) a record stream.
        Msg::MgmtResponse(r) => check_records(r.data),
        _ => {}
    }
}

/// The documented failure of a radio envelope decode: empty input is `TooShort`, a foreign
/// leading byte is `BadVersion { got }`, anything else that fails is `Malformed`.
fn check_envelope_error(data: &[u8], e: Error) {
    match e {
        Error::TooShort => assert!(data.is_empty()),
        Error::BadVersion { got } => {
            assert_eq!(Some(&got), data.first());
            assert_ne!(got, RADIO_SCHEMA_VERSION);
        }
        Error::Malformed => assert_eq!(data.first(), Some(&RADIO_SCHEMA_VERSION)),
        other => panic!("unexpected radio envelope error {other:?}"),
    }
}

/// Decode `data` as a node-to-host envelope; whatever decodes must re-encode within the MTU
/// (when the input itself fit it) and round-trip to an equal value.
pub fn check_node_msg(data: &[u8]) {
    match radio::decode_node_msg(data) {
        Ok(msg) => {
            let mut buf = vec![0u8; data.len() + 16];
            match radio::encode_node_msg(&msg, &mut buf) {
                Ok(n) => {
                    assert!(n <= MAX_RADIO_PAYLOAD && n <= data.len());
                    assert_eq!(radio::decode_node_msg(&buf[..n]).as_ref(), Ok(&msg));
                }
                Err(e) => {
                    assert_eq!(e, Error::Encode);
                    assert!(
                        data.len() > MAX_RADIO_PAYLOAD,
                        "an MTU-sized envelope must re-encode"
                    );
                }
            }
        }
        Err(e) => check_envelope_error(data, e),
    }
}

/// Decode `data` as a host-to-node envelope (what a node receives in its downlink window).
pub fn check_node_cmd(data: &[u8]) {
    match radio::decode_node_cmd(data) {
        Ok(cmd) => {
            let mut buf = vec![0u8; data.len() + 16];
            match radio::encode_node_cmd(&cmd, &mut buf) {
                Ok(n) => {
                    assert!(n <= MAX_RADIO_PAYLOAD && n <= data.len());
                    assert_eq!(radio::decode_node_cmd(&buf[..n]).as_ref(), Ok(&cmd));
                }
                Err(e) => {
                    assert_eq!(e, Error::Encode);
                    assert!(
                        data.len() > MAX_RADIO_PAYLOAD,
                        "an MTU-sized envelope must re-encode"
                    );
                }
            }
        }
        Err(e) => check_envelope_error(data, e),
    }
}

/// Round-trip every encoder the harness can build from a decoded radio value. Used by
/// `protocol_radio` for values constructed directly (the producer side).
pub fn check_encode_node_msg(msg: &NodeMsg<'_>) {
    let mut buf = [0u8; 256];
    match radio::encode_node_msg(msg, &mut buf) {
        Ok(n) => {
            assert!(n <= MAX_RADIO_PAYLOAD, "encoder emitted an over-MTU envelope");
            assert_eq!(radio::decode_node_msg(&buf[..n]).as_ref(), Ok(msg));
        }
        Err(e) => assert_eq!(e, Error::Encode),
    }
}

/// See [`check_encode_node_msg`].
pub fn check_encode_node_cmd(cmd: &NodeCmd<'_>) {
    let mut buf = [0u8; 256];
    match radio::encode_node_cmd(cmd, &mut buf) {
        Ok(n) => {
            assert!(n <= MAX_RADIO_PAYLOAD, "encoder emitted an over-MTU envelope");
            assert_eq!(radio::decode_node_cmd(&buf[..n]).as_ref(), Ok(cmd));
        }
        Err(e) => assert_eq!(e, Error::Encode),
    }
}

/// Decode `data` as a stream of `$t` records exactly like the host CLI's `parse_records` (stop at
/// the first undecodable tail). Every record must consume at least one byte (a zero-length record
/// would spin that loop forever) and re-encode to an equal value. A macro rather than a generic fn:
/// the re-decoded copy borrows from a local buffer, which a borrowed record type's single
/// lifetime parameter cannot express.
macro_rules! records {
    ($t:ty, $data:expr) => {{
        let mut data: &[u8] = $data;
        while !data.is_empty() {
            match postcard::take_from_bytes::<$t>(data) {
                Ok((rec, rest)) => {
                    assert!(rest.len() < data.len(), "a record decoded from zero bytes");
                    let used = data.len() - rest.len();
                    let mut buf = vec![0u8; used + 16];
                    let enc = postcard::to_slice(&rec, &mut buf).expect("a decoded record must re-encode");
                    assert!(enc.len() <= used, "canonical re-encoding grew");
                    let (back, tail) =
                        postcard::take_from_bytes::<$t>(enc).expect("re-encoded record decodes");
                    assert!(tail.is_empty());
                    assert_eq!(back, rec, "record round trip changed the value");
                    data = rest;
                }
                Err(_) => break,
            }
        }
    }};
}

/// Every management record type (and the op enum itself) over the same bytes: a host decodes a
/// reassembled reply with the record type its op implies, so any of them can meet any bytes.
pub fn check_records(data: &[u8]) {
    records!(DeviceInfo<'_>, data);
    records!(NodeEntry<'_>, data);
    records!(NodeKey, data);
    records!(Paired, data);
    records!(QueueId, data);
    records!(QueueEntry<'_>, data);
    records!(ProvisionAck, data);
    records!(Joined, data);
    records!(MgmtOp<'_>, data);
    records!(Provision, data);
}
