//! `protocol_frames`: the console receive path, end to end.
//!
//! The first input byte picks a mode:
//!
//! * **raw** (even): every remaining byte goes through one streaming [`FrameDecoder`], as a host
//!   reading a serial port would; each deframed inner buffer is checked by
//!   [`tower_fuzz::check_inner`] (`decode_frame` check order, `decode_msg`, re-encode round trip,
//!   nested radio / mgmt payloads).
//! * **framed** (odd): the rest is a list of `[type][seq lo][seq hi][len][payload; len]` chunks,
//!   each wrapped by the real producer (`encode_frame`, valid CRC + version) after an optional
//!   burst of line noise and a resync `0x00`. That spends the fuzzer's effort on the postcard
//!   payload decoders instead of on guessing CRCs, and checks that a valid frame decodes to
//!   exactly what was sent, whatever garbage preceded it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use serde::Serialize;
use serde::ser::SerializeTuple;
use tower_fuzz::{check_inner, deframe_one};
use tower_protocol::{Error, FrameDecoder, MAX_PAYLOAD, MAX_WIRE, MsgType, decode_frame, encode_frame};

/// Raw payload bytes: serializes as a postcard tuple, i.e. the bytes verbatim with no length
/// prefix, so `encode_frame` frames exactly the fuzzer's payload.
struct Raw<'a>(&'a [u8]);

impl Serialize for Raw<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut t = s.serialize_tuple(self.0.len())?;
        for b in self.0 {
            t.serialize_element(b)?;
        }
        t.end()
    }
}

fn raw_mode(data: &[u8]) {
    let mut dec = FrameDecoder::new();
    let mut pending = 0usize; // non-zero bytes since the last delimiter
    for &b in data {
        match dec.push(b) {
            Some(inner) => {
                assert_eq!(b, 0x00, "a frame can only complete on the delimiter");
                // COBS decoding never grows, and the deframer buffers at most MAX_WIRE bytes.
                assert!(inner.len() < pending.min(MAX_WIRE) + 1);
                check_inner(inner);
                pending = 0;
            }
            None => pending = if b == 0 { 0 } else { pending + 1 },
        }
    }
}

fn framed_mode(mut data: &[u8]) {
    let mut dec = FrameDecoder::new();
    while data.len() >= 5 {
        let (hdr, rest) = data.split_at(5);
        let t = MsgType::ALL[hdr[0] as usize % MsgType::ALL.len()];
        let seq = u16::from_le_bytes([hdr[1], hdr[2]]);
        let noise = (hdr[3] % 16) as usize;
        let len = (hdr[4] as usize).min(rest.len());
        let (payload, rest) = rest.split_at(len);
        data = rest;

        // Line noise before the frame (it may itself contain delimiters and frames), then the
        // resync delimiter: whatever the noise did, the next frame must come through intact.
        let (noise, rest) = data.split_at(noise.min(data.len()));
        data = rest;
        for &b in noise {
            if let Some(inner) = dec.push(b) {
                check_inner(inner);
            }
        }
        if let Some(inner) = dec.push(0x00) {
            check_inner(inner);
        }

        let mut wire = [0u8; MAX_WIRE];
        let n = match encode_frame(t, seq, &Raw(payload), &mut wire) {
            Ok(n) => n,
            Err(e) => {
                assert_eq!(e, Error::Encode);
                assert!(
                    payload.len() > MAX_PAYLOAD,
                    "an in-budget payload failed to encode"
                );
                continue;
            }
        };
        // The streaming decoder (dirty from the noise) and a fresh one must agree.
        let fresh = deframe_one(&wire[..n]);
        let mut got = None;
        for &b in &wire[..n] {
            if let Some(inner) = dec.push(b) {
                assert!(got.is_none(), "one wire frame yielded two inner frames");
                got = Some(inner.to_vec());
            }
        }
        let inner = got.expect("a valid frame after a resync delimiter must deframe");
        assert_eq!(inner, fresh);
        let (t2, seq2, p2) =
            decode_frame(&inner).expect("a producer-built frame must pass the framing checks");
        assert_eq!(
            (t2, seq2, p2),
            (t, seq, payload),
            "frame did not decode to what was sent"
        );
        check_inner(&inner);
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    if mode & 1 == 0 {
        raw_mode(rest);
    } else {
        framed_mode(rest);
    }
});
