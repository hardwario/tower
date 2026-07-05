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
        [0x03, 0x40, 0x01, 0x0e, 0x02, 0x03, 0x61, 0x70, 0x70, 0x02, 0x66, 0x77, 0x01, 0xdb, 0x8b, 0x5a, 0x04, 0x00]
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
            0x03, 0x41, 0x07, 0x0f, 0x01, 0x84, 0x86, 0x88, 0x08, 0x01, 0x6d, 0x02, 0x68, 0x69,
            0xa8, 0x9d, 0x90, 0x63, 0x00
        ]
    );
}

#[test]
fn golden_dropped() {
    let got = frame(MsgType::Dropped, 2, &Dropped { count: 300 });
    assert_eq!(got, [0x03, 0x46, 0x02, 0x04, 0xac, 0x02, 0xb6, 0x03, 0x24, 0x57, 0x00]);
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
            0x03, 0x44, 0x09, 0x02, 0x05, 0x01, 0x09, 0x01, 0x02, 0x6f, 0x6b, 0xe3, 0x2a, 0x7d,
            0x09, 0x00
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
