//! TOWER console wire protocol: the single source of truth for the host↔target
//! link, shared by the `tower` firmware SDK, the `tower` CLI (tower-cli) and the HIL
//! harness (tower-hil), which all build this crate by path from the one monorepo tree.
//!
//! Frame on the wire:
//! ```text
//! wire:   COBS( inner )  0x00
//! inner:  type(1) | tag(2, LE) | seq(2, LE) | payload(postcard) | crc32(4, LE)
//!         type  = the MsgType discriminant
//!         tag   = CONSOLE_TAG, the console schema's structural hash folded to 16 bits
//!         crc32 = CRC-32/IEEE over [type, tag, seq, payload...]
//! ```
//!
//! [`encode_frame`] builds the whole wire frame (used by every producer, including
//! the panic path). On receive, feed bytes to a [`FrameDecoder`] until it yields a
//! deframed inner buffer, then [`decode_frame`] checks length, **CRC first**, then the
//! schema tag and the type, and returns the `(MsgType, seq, payload)`; deserialize the
//! payload yourself or, preferably, use [`decode_msg`] for the typed one-call decode, which
//! also rejects trailing bytes that a bare `postcard::from_bytes` would silently ignore.
//!
//! ## `seq` semantics
//!
//! `seq` is a per-frame `u16` assigned by the **sender's writer** at transmit time: it
//! increments once per frame actually put on the wire (dropped-before-transmit messages
//! never consume a number) and wraps at `u16::MAX`. A receiver uses gaps to detect and
//! *count* wire loss, nothing more. It is not a dedup key, not an ordering guarantee,
//! and carries no meaning across a link reset (each side restarts at 0).
//!
//! ## Schema tags instead of versions
//!
//! postcard encodes structs by **field order** and enums by **variant index**; names
//! never reach the wire, and nothing in the bytes says which schema produced them. Every
//! end (the firmware, the CLI, the HIL harness) builds this one source tree by path, so a
//! *build* can never mix schemas. What can still differ at run time is an installed
//! artifact from another checkout: a board flashed earlier, a `tower` binary installed
//! earlier. The schema tags catch that, with nothing to bump by hand:
//!
//! * [`CONSOLE_TAG`] rides in every console frame. It is derived at compile time from
//!   [`fingerprint::CONSOLE_HASH`], the structural hash of every [`MsgType`] value with its
//!   payload schema and the [`mgmt`] op and record schemas. Any field or variant order,
//!   shape or width change moves it; a rename does not (names are not on the wire).
//! * [`radio::RADIO_TAG`] leads every radio application envelope, derived the same way from
//!   the radio schema ([`fingerprint::RADIO_HASH`]).
//!
//! A CRC-valid frame whose tag is not ours is [`Error::SchemaMismatch`]: the peer was built
//! from another checkout. The tag cannot say which side is older (it is a hash, not a
//! counter), so the remedy is always the same: rebuild both ends from the same checkout.
//! To change the wire, change the types and rebuild every end; the tags follow. There are no
//! version constants, no negotiation and no compatibility with older peers: a stale peer is
//! rejected by its tag, never half-understood.

#![no_std]

pub mod crc;
pub mod fingerprint;
pub mod mgmt;
pub mod msg;
pub mod radio;
pub mod ring;

pub use msg::*;
use serde::Serialize;

/// The console schema tag carried in every frame header: [`fingerprint::CONSOLE_HASH`] folded
/// to 16 bits ([`fingerprint::fold16`]). A receiver rejects a CRC-valid frame carrying any other
/// value as [`Error::SchemaMismatch`]. Two different schemas share a tag with probability
/// 2⁻¹⁶: an integrity check against a stale build, not a defence against forgery.
pub const CONSOLE_TAG: u16 = fingerprint::fold16(fingerprint::CONSOLE_HASH);

/// Max inner frame (type + tag + seq + payload + crc), pre-COBS.
pub const MAX_FRAME: usize = 256;
/// Max wire frame: COBS worst-case expansion of [`MAX_FRAME`] plus the `0x00`.
/// COBS adds `ceil(n/254)` overhead bytes; 272 leaves comfortable headroom.
pub const MAX_WIRE: usize = 272;

/// Frame header length: type(1) + tag(2) + seq(2).
const HDR: usize = 5;
const CRC_LEN: usize = 4;

/// Max **postcard payload** bytes one frame can carry: [`MAX_FRAME`] minus the 5-byte header
/// (type, tag, seq) and the trailing CRC, i.e. 247. This is the number consumers size owned
/// message buffers against: [`encode_frame`] rejects anything larger with [`Error::Encode`],
/// and the decode path guarantees ([`Error::TooLong`]) that no larger payload is ever surfaced.
pub const MAX_PAYLOAD: usize = MAX_FRAME - HDR - CRC_LEN;

/// Console message types: the first header byte. Target→host are 0..=15, host→target are
/// 16+. postcard never sees this enum (it's the raw type byte), so the discriminants are
/// the wire values.
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
    MgmtResponse = 7,
    Uplink = 8,
    RadioStat = 9,
    // host -> target
    ShellCommand = 16,
    ShellComplete = 17,
    MgmtRequest = 18,
}

impl MsgType {
    /// Every message type, in ascending discriminant order (the fixed order
    /// [`fingerprint::CONSOLE_HASH`] hashes them in).
    pub const ALL: [MsgType; 13] = [
        Self::Hello,
        Self::Log,
        Self::Print,
        Self::Event,
        Self::ShellResponse,
        Self::ShellCompletions,
        Self::Dropped,
        Self::MgmtResponse,
        Self::Uplink,
        Self::RadioStat,
        Self::ShellCommand,
        Self::ShellComplete,
        Self::MgmtRequest,
    ];

    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Hello,
            1 => Self::Log,
            2 => Self::Print,
            3 => Self::Event,
            4 => Self::ShellResponse,
            5 => Self::ShellCompletions,
            6 => Self::Dropped,
            7 => Self::MgmtResponse,
            8 => Self::Uplink,
            9 => Self::RadioStat,
            16 => Self::ShellCommand,
            17 => Self::ShellComplete,
            18 => Self::MgmtRequest,
            _ => return None,
        })
    }
}

/// Codec errors. All are "drop the frame" conditions on the receive side.
///
/// `#[non_exhaustive]`: new failure modes may be added without a wire change, so consumers
/// must keep a catch-all arm. (The message enums ([`Msg`], [`MsgType`], and the payload
/// types) are deliberately **exhaustive**: under the lockstep model a new message variant
/// SHOULD break consumer matches at compile time, so every consumer consciously handles it.)
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The payload didn't fit / failed to serialize.
    Encode,
    /// The output buffer is too small for the COBS-encoded frame.
    Overflow,
    /// Inner frame shorter than the minimum (header + crc), or a radio envelope shorter than
    /// its tag.
    TooShort,
    /// Inner frame longer than [`MAX_FRAME`]. The receive path (a [`FrameDecoder`] buffers up to
    /// [`MAX_WIRE`], which COBS-decodes to slightly more than `MAX_FRAME`) must reject these so a
    /// consumer that sizes buffers to the documented `MAX_FRAME`/payload budget can trust it; an
    /// oversized frame from a peer or an attacker on the wire is dropped, not mis-handled.
    TooLong,
    /// A **CRC-valid** frame whose schema tag is not [`CONSOLE_TAG`]: the peer was built from
    /// another checkout (another schema). Carries the tag actually seen. It cannot say which side
    /// is older; the remedy is to rebuild both ends from the same checkout.
    ///
    /// The tag is checked only *after* the CRC passes, so line noise, a torn or partial frame,
    /// or a bit-flip in the header surfaces as [`Error::BadCrc`], never as a spurious
    /// `SchemaMismatch`. A consumer may therefore treat even a single `SchemaMismatch` as a
    /// genuine mismatch (the host CLI does: mismatch banner / dedicated exit code). (From the
    /// radio envelope ([`radio::decode_node_msg`] / [`radio::decode_node_cmd`]), it means a tag
    /// other than [`radio::RADIO_TAG`]; that payload was already authenticated by the net layer.)
    SchemaMismatch { got: u16 },
    /// Unknown message type in a CRC-valid frame carrying our tag: a producer bug.
    BadType,
    /// CRC mismatch: corruption on the wire, a torn/partial frame, or line noise. Checked
    /// before the tag and the type, so every frame that fails integrity lands here whatever its
    /// header bytes happen to claim.
    BadCrc,
    /// The frame passed CRC + tag but its postcard payload did not deserialize into the
    /// expected type for its [`MsgType`]: a truncated or corrupt-but-CRC-valid body, a body
    /// with **trailing bytes** after the value, or a producer bug. Returned by [`decode_msg`]
    /// and the radio [`radio::decode_node_msg`] / [`radio::decode_node_cmd`] (they own the
    /// deserialize step).
    ///
    /// Trailing bytes: `postcard::from_bytes` silently ignores them, but a CRC-valid body longer
    /// than its type means the producer's schema is not ours; half-reading it would be exactly
    /// the silent mis-decode this crate exists to prevent, so it is rejected instead.
    Malformed,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Encode => f.write_str("payload too large or failed to serialize"),
            Error::Overflow => f.write_str("output buffer too small for the encoded frame"),
            Error::TooShort => f.write_str("frame shorter than its header and trailer"),
            Error::TooLong => f.write_str("frame longer than MAX_FRAME"),
            Error::SchemaMismatch { got } => write!(
                f,
                "schema tag mismatch: the peer sent {got:#06x} (built from another checkout)"
            ),
            Error::BadType => f.write_str("unknown message type"),
            Error::BadCrc => f.write_str("CRC mismatch (wire corruption)"),
            Error::Malformed => f.write_str("payload failed to deserialize (or has trailing bytes)"),
        }
    }
}

impl core::error::Error for Error {}

/// Build a complete wire frame `COBS(type ‖ CONSOLE_TAG ‖ seq ‖ postcard(payload) ‖ crc) ‖ 0x00`
/// into `out`; returns the byte count written. Pure computation, safe from any context (IRQ,
/// panic handler).
pub fn encode_frame<T: Serialize>(
    msg_type: MsgType,
    seq: u16,
    payload: &T,
    out: &mut [u8],
) -> Result<usize, Error> {
    encode_frame_with_tag(CONSOLE_TAG, msg_type, seq, payload, out)
}

/// Serialize a message payload (postcard) into `out` on its own, for framing later with
/// [`encode_frame_bytes`]: the device's console queue ([`ring::PayloadRing`]) stores messages this
/// way. Returns the byte count; [`Error::Encode`] past [`MAX_PAYLOAD`] (what one frame carries)
/// or past `out`.
pub fn encode_payload<T: Serialize>(payload: &T, out: &mut [u8]) -> Result<usize, Error> {
    let room = out.len().min(MAX_PAYLOAD);
    Ok(postcard::to_slice(payload, &mut out[..room])
        .map_err(|_| Error::Encode)?
        .len())
}

/// [`encode_frame`] for a payload that is already postcard-encoded ([`encode_payload`]): the same
/// bytes on the wire.
pub fn encode_frame_bytes(
    msg_type: MsgType,
    seq: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Result<usize, Error> {
    if payload.len() > MAX_PAYLOAD {
        return Err(Error::Encode);
    }
    let mut inner = [0u8; MAX_FRAME];
    let body = header(&mut inner, CONSOLE_TAG, msg_type, seq) + payload.len();
    inner[HDR..body].copy_from_slice(payload);
    seal(&mut inner, body, out)
}

/// [`encode_frame`] with an explicit schema `tag` in place of [`CONSOLE_TAG`]: for framing
/// tests whose bytes must not move with the schema, and for simulating a peer built from
/// another checkout. Production code uses [`encode_frame`].
pub fn encode_frame_with_tag<T: Serialize>(
    tag: u16,
    msg_type: MsgType,
    seq: u16,
    payload: &T,
    out: &mut [u8],
) -> Result<usize, Error> {
    let mut inner = [0u8; MAX_FRAME];
    let hdr = header(&mut inner, tag, msg_type, seq);
    let used = postcard::to_slice(payload, &mut inner[hdr..])
        .map_err(|_| Error::Encode)?
        .len();
    let body = hdr + used;
    if body + CRC_LEN > MAX_FRAME {
        return Err(Error::Encode);
    }
    seal(&mut inner, body, out)
}

/// Write the frame header (type, tag, seq) at the start of `inner`; returns its length.
fn header(inner: &mut [u8; MAX_FRAME], tag: u16, msg_type: MsgType, seq: u16) -> usize {
    inner[0] = msg_type as u8;
    inner[1..3].copy_from_slice(&tag.to_le_bytes());
    inner[3..5].copy_from_slice(&seq.to_le_bytes());
    HDR
}

/// Append the CRC to the `body` bytes of `inner` (header and payload), COBS-encode the result
/// into `out` and terminate it; returns the byte count written.
fn seal(inner: &mut [u8; MAX_FRAME], body: usize, out: &mut [u8]) -> Result<usize, Error> {
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

/// Validate a deframed inner buffer and split it into `(msg_type, seq, payload)`.
///
/// Check order (load-bearing): length ([`Error::TooShort`] / [`Error::TooLong`]), then the
/// **CRC** ([`Error::BadCrc`]), and only then the schema tag ([`Error::SchemaMismatch`]) and
/// the type ([`Error::BadType`]). Deserialize `payload` yourself, or use [`decode_msg`], which
/// also rejects trailing bytes (`postcard::from_bytes` silently ignores them).
pub fn decode_frame(inner: &[u8]) -> Result<(MsgType, u16, &[u8]), Error> {
    decode_frame_with_tag(CONSOLE_TAG, inner)
}

/// [`decode_frame`] expecting the schema `tag` instead of [`CONSOLE_TAG`] (the counterpart of
/// [`encode_frame_with_tag`], for tests). Production code uses [`decode_frame`].
pub fn decode_frame_with_tag(tag: u16, inner: &[u8]) -> Result<(MsgType, u16, &[u8]), Error> {
    if inner.len() < HDR + CRC_LEN {
        return Err(Error::TooShort);
    }
    if inner.len() > MAX_FRAME {
        // The COBS deframer buffers up to MAX_WIRE, which decodes to slightly more than
        // MAX_FRAME; reject the excess so the encode-side budget is a receive-side guarantee.
        return Err(Error::TooLong);
    }
    // CRC BEFORE the tag and the type. A corrupt or partial frame's header bytes are noise, and
    // consumers treat a single SchemaMismatch as a stale peer (the CLI's mismatch banner / exit
    // code), so only a CRC-valid frame may report one. The CRC trailer (CRC-32/IEEE over
    // everything before it) is part of the framing, not of the schema: a peer from another
    // checkout still passes it, which is what makes its tag readable.
    let body = inner.len() - CRC_LEN;
    let stored = u32::from_le_bytes([inner[body], inner[body + 1], inner[body + 2], inner[body + 3]]);
    if crc::crc32_ieee(&inner[..body]) != stored {
        return Err(Error::BadCrc);
    }
    let got = u16::from_le_bytes([inner[1], inner[2]]);
    if got != tag {
        return Err(Error::SchemaMismatch { got });
    }
    let msg_type = MsgType::from_u8(inner[0]).ok_or(Error::BadType)?;
    let seq = u16::from_le_bytes([inner[3], inner[4]]);
    Ok((msg_type, seq, &inner[HDR..body]))
}

/// Best effort, for a mismatch message only: the [`Hello`] inside a CRC-valid frame that carries
/// another schema tag (a peer built from another checkout), read with this build's `Hello`
/// layout. `None` unless the frame is a `Hello` whose body reads as exactly one. Its fields are
/// unverified (the other schema's `Hello` may be laid out differently), so a consumer shows them
/// as a hint ("the device announced ...") and never acts on them.
pub fn decode_foreign_hello(inner: &[u8]) -> Option<Hello<'_>> {
    let Err(Error::SchemaMismatch { got }) = decode_frame(inner) else {
        return None;
    };
    let (msg_type, _, payload) = decode_frame_with_tag(got, inner).ok()?;
    if msg_type != MsgType::Hello {
        return None;
    }
    from_bytes_exact(payload).ok()
}

/// Deserialize exactly one `T` from `bytes`: a body that fails to parse, *or* leaves trailing
/// bytes behind, is [`Error::Malformed`]. `postcard::from_bytes` silently ignores a tail, which
/// would let a producer whose schema grew be half-read as ours, so every typed decode in this
/// crate goes through here instead.
pub(crate) fn from_bytes_exact<'a, T: serde::Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    match postcard::take_from_bytes(bytes) {
        Ok((value, [])) => Ok(value),
        _ => Err(Error::Malformed),
    }
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
    MgmtResponse(MgmtResponse<'a>),
    Uplink(Uplink<'a>),
    RadioStat(RadioStat),
    // host -> target
    ShellCommand(ShellCommand<'a>),
    ShellComplete(ShellComplete<'a>),
    MgmtRequest(MgmtRequest<'a>),
}

/// Decode a deframed inner buffer all the way into `(seq, Msg)`: validate CRC + tag via
/// [`decode_frame`], then deserialize the postcard payload into the type for its [`MsgType`].
/// One call instead of `decode_frame` + a hand-written `match` + `from_bytes` at every
/// consumer; the borrow ties `Msg` to `inner`. Payloads that pass CRC but fail to
/// deserialize (or carry trailing bytes past the value) return [`Error::Malformed`].
pub fn decode_msg(inner: &[u8]) -> Result<(u16, Msg<'_>), Error> {
    let (msg_type, seq, payload) = decode_frame(inner)?;
    // Generic (not a closure): each arm deserializes into a different type.
    fn de<'a, T: serde::Deserialize<'a>>(p: &'a [u8]) -> Result<T, Error> {
        from_bytes_exact(p)
    }
    let msg = match msg_type {
        MsgType::Hello => Msg::Hello(de(payload)?),
        MsgType::Log => Msg::Log(de(payload)?),
        MsgType::Print => Msg::Print(de(payload)?),
        MsgType::Event => Msg::Event(de(payload)?),
        MsgType::ShellResponse => Msg::ShellResponse(de(payload)?),
        MsgType::ShellCompletions => Msg::ShellCompletions(de(payload)?),
        MsgType::Dropped => Msg::Dropped(de(payload)?),
        MsgType::MgmtResponse => Msg::MgmtResponse(de(payload)?),
        MsgType::Uplink => Msg::Uplink(de(payload)?),
        MsgType::RadioStat => Msg::RadioStat(de(payload)?),
        MsgType::ShellCommand => Msg::ShellCommand(de(payload)?),
        MsgType::ShellComplete => Msg::ShellComplete(de(payload)?),
        MsgType::MgmtRequest => Msg::MgmtRequest(de(payload)?),
    };
    Ok((seq, msg))
}

/// Byte-fed COBS deframer. Feed received bytes one at a time; on the `0x00`
/// delimiter it COBS-decodes the accumulated frame **in place** and returns the
/// inner bytes (pass them to [`decode_frame`] / [`decode_msg`]). A frame larger than
/// [`MAX_WIRE`] or a COBS error yields `None` (dropped); the next `0x00` resynchronizes.
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
