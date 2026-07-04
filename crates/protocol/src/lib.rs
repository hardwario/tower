//! TOWER console wire protocol — the single source of truth for the host↔target
//! link, shared by the `tower` firmware and the `tower-cli` host.
//!
//! Frame on the wire:
//! ```text
//! wire:   COBS( inner )  0x00
//! inner:  ver_type(1) | seq(2, LE) | payload(postcard) | crc32(4, LE)
//!         ver_type = (PROTOCOL_VERSION << 5) | (msg_type & 0x1F)
//!         crc32    = CRC-32/IEEE over [ver_type, seq, payload...]
//! ```
//!
//! [`encode_frame`] builds the whole wire frame (used by every producer, including
//! the panic path). On receive, feed bytes to a [`FrameDecoder`] until it yields a
//! deframed inner buffer, then [`decode_frame`] checks version + CRC and returns the
//! `(MsgType, seq, payload)`; deserialize the payload with `postcard::from_bytes`.

#![no_std]

pub mod crc;
pub mod msg;

pub use msg::*;
use serde::Serialize;

/// Protocol version, carried in the top 3 bits of `ver_type` on every frame.
///
/// v2 (crate 1.1.0): [`Hello`] gained `firmware_name` and a per-boot `session_id`
/// (and `firmware_version` became a real version string). Field order is positional
/// under postcard, so this was a wire break.
pub const PROTOCOL_VERSION: u8 = 2;

// The version occupies only the top 3 bits of `ver_type` (`PROTOCOL_VERSION << 5`), so it must
// fit in 0..=7. Bumping past 7 would silently wrap in release builds and alias an old version —
// exactly the silent mis-decode this crate exists to prevent — so make it a compile error.
const _: () = assert!(
    PROTOCOL_VERSION < 8,
    "PROTOCOL_VERSION must fit in 3 bits (0..=7); the frame header has no room beyond that"
);

/// Max inner frame (ver_type + seq + payload + crc), pre-COBS.
pub const MAX_FRAME: usize = 256;
/// Max wire frame: COBS worst-case expansion of [`MAX_FRAME`] plus the `0x00`.
/// COBS adds `ceil(n/254)` overhead bytes; 272 leaves comfortable headroom.
pub const MAX_WIRE: usize = 272;

const HDR: usize = 3; // ver_type(1) + seq(2)
const CRC_LEN: usize = 4;

/// Console message types — the low 5 bits of `ver_type`. Target→host are 0..=15,
/// host→target are 16+. postcard never sees this enum (it's the raw `ver_type`
/// byte), so the discriminants are the wire values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MsgType {
    // target -> host
    Hello = 0,
    Log = 1,
    Print = 2,
    Event = 3,
    ShellResponse = 4,
    ShellCompletions = 5,
    Dropped = 6,
    // host -> target
    ShellCommand = 16,
    ShellComplete = 17,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Hello,
            1 => Self::Log,
            2 => Self::Print,
            3 => Self::Event,
            4 => Self::ShellResponse,
            5 => Self::ShellCompletions,
            6 => Self::Dropped,
            16 => Self::ShellCommand,
            17 => Self::ShellComplete,
            _ => return None,
        })
    }
}

/// Codec errors. All are "drop the frame" conditions on the receive side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The payload didn't fit / failed to serialize.
    Encode,
    /// The output buffer is too small for the COBS-encoded frame.
    Overflow,
    /// Inner frame shorter than the minimum (`ver_type + seq + crc`).
    TooShort,
    /// Inner frame longer than [`MAX_FRAME`]. The receive path (a [`FrameDecoder`] buffers up to
    /// [`MAX_WIRE`], which COBS-decodes to slightly more than `MAX_FRAME`) must reject these so a
    /// consumer that sizes buffers to the documented `MAX_FRAME`/payload budget can trust it — an
    /// oversized frame from a peer or an attacker on the wire is dropped, not mis-handled.
    TooLong,
    /// Protocol version in `ver_type` not understood; carries the version byte actually seen so a
    /// consumer can tell the user *which* version the peer speaks (a lockstep mismatch), rather
    /// than reporting generic "corrupt frame".
    BadVersion { got: u8 },
    /// Unknown message type.
    BadType,
    /// CRC mismatch (corruption on the wire).
    BadCrc,
    /// The frame passed version + CRC but its postcard payload did not deserialize into the
    /// expected type for its [`MsgType`] — a truncated or corrupt-but-CRC-valid body, or a
    /// producer bug. Only [`decode_msg`] returns this (it owns the deserialize step).
    Malformed,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Encode => f.write_str("payload too large or failed to serialize"),
            Error::Overflow => f.write_str("output buffer too small for the encoded frame"),
            Error::TooShort => f.write_str("frame shorter than the minimum header+crc"),
            Error::TooLong => f.write_str("frame longer than MAX_FRAME"),
            Error::BadVersion { got } => write!(
                f,
                "protocol version mismatch: peer speaks v{got}, this build speaks v{PROTOCOL_VERSION}"
            ),
            Error::BadType => f.write_str("unknown message type"),
            Error::BadCrc => f.write_str("CRC mismatch (wire corruption)"),
            Error::Malformed => f.write_str("payload failed to deserialize"),
        }
    }
}

impl core::error::Error for Error {}

/// Build a complete wire frame `COBS(ver_type ‖ seq ‖ postcard(payload) ‖ crc) ‖ 0x00`
/// into `out`; returns the byte count written. Pure computation — safe from any
/// context (IRQ, panic handler).
pub fn encode_frame<T: Serialize>(
    msg_type: MsgType,
    seq: u16,
    payload: &T,
    out: &mut [u8],
) -> Result<usize, Error> {
    let mut inner = [0u8; MAX_FRAME];
    inner[0] = (PROTOCOL_VERSION << 5) | (msg_type as u8 & 0x1F);
    inner[1..3].copy_from_slice(&seq.to_le_bytes());

    let used = postcard::to_slice(payload, &mut inner[HDR..])
        .map_err(|_| Error::Encode)?
        .len();
    let body = HDR + used;
    if body + CRC_LEN > MAX_FRAME {
        return Err(Error::Encode);
    }
    let crc = crc::crc32_ieee(&inner[..body]);
    inner[body..body + CRC_LEN].copy_from_slice(&crc.to_le_bytes());
    let inner_len = body + CRC_LEN;

    if cobs::max_encoding_length(inner_len) + 1 > out.len() {
        return Err(Error::Overflow);
    }
    let enc = cobs::encode(&inner[..inner_len], out);
    out[enc] = 0x00;
    Ok(enc + 1)
}

/// Validate version + CRC on a deframed inner buffer and split it into
/// `(msg_type, seq, payload)`. Deserialize `payload` with `postcard::from_bytes`.
pub fn decode_frame(inner: &[u8]) -> Result<(MsgType, u16, &[u8]), Error> {
    if inner.len() < HDR + CRC_LEN {
        return Err(Error::TooShort);
    }
    if inner.len() > MAX_FRAME {
        // The COBS deframer buffers up to MAX_WIRE, which decodes to slightly more than
        // MAX_FRAME; reject the excess so the encode-side budget is a receive-side guarantee.
        return Err(Error::TooLong);
    }
    let ver_type = inner[0];
    if (ver_type >> 5) != PROTOCOL_VERSION {
        return Err(Error::BadVersion { got: ver_type >> 5 });
    }
    let msg_type = MsgType::from_u8(ver_type & 0x1F).ok_or(Error::BadType)?;
    let seq = u16::from_le_bytes([inner[1], inner[2]]);
    let body = inner.len() - CRC_LEN;
    let stored = u32::from_le_bytes([inner[body], inner[body + 1], inner[body + 2], inner[body + 3]]);
    if crc::crc32_ieee(&inner[..body]) != stored {
        return Err(Error::BadCrc);
    }
    Ok((msg_type, seq, &inner[HDR..body]))
}

/// A decoded frame's payload, deserialized into its concrete type. The variant set mirrors
/// [`MsgType`]; borrowed fields point into the buffer passed to [`decode_msg`].
#[derive(Debug)]
pub enum Msg<'a> {
    // target -> host
    Hello(Hello<'a>),
    Log(Log<'a>),
    Print(Print<'a>),
    Event(Event<'a>),
    ShellResponse(ShellResponse<'a>),
    ShellCompletions(ShellCompletions<'a>),
    Dropped(Dropped),
    // host -> target
    ShellCommand(ShellCommand<'a>),
    ShellComplete(ShellComplete<'a>),
}

/// Decode a deframed inner buffer all the way into `(seq, Msg)`: validate version + CRC via
/// [`decode_frame`], then deserialize the postcard payload into the type for its [`MsgType`].
/// One call instead of `decode_frame` + a hand-written `match` + `from_bytes` at every
/// consumer — the borrow ties `Msg` to `inner`. Payloads that pass CRC but fail to
/// deserialize return [`Error::Malformed`].
pub fn decode_msg(inner: &[u8]) -> Result<(u16, Msg<'_>), Error> {
    let (msg_type, seq, payload) = decode_frame(inner)?;
    // Generic (not a closure): each arm deserializes into a different type.
    fn de<'a, T: serde::Deserialize<'a>>(p: &'a [u8]) -> Result<T, Error> {
        postcard::from_bytes(p).map_err(|_| Error::Malformed)
    }
    let msg = match msg_type {
        MsgType::Hello => Msg::Hello(de(payload)?),
        MsgType::Log => Msg::Log(de(payload)?),
        MsgType::Print => Msg::Print(de(payload)?),
        MsgType::Event => Msg::Event(de(payload)?),
        MsgType::ShellResponse => Msg::ShellResponse(de(payload)?),
        MsgType::ShellCompletions => Msg::ShellCompletions(de(payload)?),
        MsgType::Dropped => Msg::Dropped(de(payload)?),
        MsgType::ShellCommand => Msg::ShellCommand(de(payload)?),
        MsgType::ShellComplete => Msg::ShellComplete(de(payload)?),
    };
    Ok((seq, msg))
}

/// Byte-fed COBS deframer. Feed received bytes one at a time; on the `0x00`
/// delimiter it COBS-decodes the accumulated frame **in place** and returns the
/// inner bytes (pass them to [`decode_frame`]). A frame larger than [`MAX_WIRE`] or
/// a COBS error yields `None` (dropped); the next `0x00` resynchronizes.
pub struct FrameDecoder {
    buf: [u8; MAX_WIRE],
    len: usize,
    overflow: bool,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub const fn new() -> Self {
        Self {
            buf: [0; MAX_WIRE],
            len: 0,
            overflow: false,
        }
    }

    /// Discard any partial frame (e.g. on reconnect / after a gap).
    pub fn reset(&mut self) {
        self.len = 0;
        self.overflow = false;
    }

    /// Feed one byte. Returns the deframed inner bytes when `b` completes a frame.
    pub fn push(&mut self, b: u8) -> Option<&[u8]> {
        if b != 0 {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            } else {
                self.overflow = true;
            }
            return None;
        }
        // Frame boundary.
        let len = self.len;
        let overflow = self.overflow;
        self.len = 0;
        self.overflow = false;
        if len == 0 || overflow {
            return None;
        }
        match cobs::decode_in_place(&mut self.buf[..len]) {
            Ok(n) => Some(&self.buf[..n]),
            Err(_) => None,
        }
    }
}
