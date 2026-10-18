//! edge_frame_limits: MTU + malformed/forged-frame rejection KAT (docs/radio.md).
//! Single board, no radio link: drives the frame codec + AES-CCM at its
//! boundaries and asserts each accept/reject, printing one verdict (`all checks PASS`, or a
//! FAIL line per failed check and `some checks FAIL`).
//!
//!   just flash example edge_frame_limits
//!
//! Covers: payload MTU (1 B / 74 B accept, 75 B reject; bulk 64 B accept, 65 B
//! reject), and the receive-side drops (other format marker, unknown type, truncated,
//! tampered ciphertext, tampered tag, wrong key): each must yield the right error.

#![no_std]
#![no_main]

use log::{error, info};
use tower::radio::ccm::Ccm;
use tower::radio::frame::{self, FrameError, FrameType, Header, MAX_FRAME, flags};
use tower::{app, board::Board};

const KEY: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
];
const KEY2: [u8; 16] = [0xA5; 16];

fn data_hdr(counter: u32) -> Header {
    Header {
        frame_type: FrameType::Data,
        flags: flags::CONFIRMED,
        src: 0x1111_1111,
        dest: 0x2222_2222,
        counter,
        bulk_index: None,
    }
}
fn bulk_hdr(counter: u32, idx: u32) -> Header {
    Header {
        frame_type: FrameType::BulkData,
        flags: 0,
        src: 0x1111_1111,
        dest: 0x2222_2222,
        counter,
        bulk_index: Some(idx),
    }
}

async fn run(_b: Board) {
    let mut ccm = Ccm::new();
    let mut pass = true;
    // Quiet on success: this KAT fires ~13 checks back-to-back with no `.await` between
    // them, so the writer task never runs to drain the console queue mid-burst. Logging
    // every ✓ would overflow the depth-4 TX queue (drop-newest) and silently drop the
    // final verdict line. We only log *failures* (rare) plus the single verdict at the
    // end. The verdict is the answer; per-check detail appears only when something breaks.
    let mut check = |name: &str, ok: bool| {
        if !ok {
            error!(target: "edge", "{} FAIL", name);
        }
        pass &= ok;
    };

    let mut buf = [0u8; MAX_FRAME];

    // --- MTU accept/reject (seal side) ---
    check(
        "seal of a 1 B payload",
        frame::seal_frame(&mut ccm, &KEY, &data_hdr(1), &[0x5A], &mut buf).is_ok(),
    );
    let p74 = [0xA5u8; 74];
    check(
        "seal of a 74 B payload",
        frame::seal_frame(&mut ccm, &KEY, &data_hdr(2), &p74, &mut buf).is_ok(),
    );
    let p75 = [0xA5u8; 75];
    check(
        "refusal of a 75 B payload as too long",
        frame::seal_frame(&mut ccm, &KEY, &data_hdr(3), &p75, &mut buf) == Err(FrameError::PayloadTooLong),
    );
    let c64 = [0x33u8; 64];
    check(
        "seal of a 64 B bulk chunk",
        frame::seal_frame(&mut ccm, &KEY, &bulk_hdr(4, 0), &c64, &mut buf).is_ok(),
    );
    let c65 = [0x33u8; 65];
    check(
        "refusal of a 65 B bulk chunk as too long",
        frame::seal_frame(&mut ccm, &KEY, &bulk_hdr(5, 0), &c65, &mut buf) == Err(FrameError::PayloadTooLong),
    );

    // --- Receive-side drops (open side). Build one good frame, then corrupt it. ---
    let payload = *b"edge-case-payload";
    let good_len = frame::seal_frame(&mut ccm, &KEY, &data_hdr(42), &payload, &mut buf).unwrap();

    // Valid frame opens and round-trips.
    {
        let mut b = buf;
        let r = frame::open_frame(&mut ccm, &KEY, &mut b[..good_len]);
        let ok = matches!(&r, Ok((h, range)) if h.counter == 42 && b[range.clone()] == payload[..]);
        check("open and round-trip of a genuine frame", ok);
    }
    // Another format marker: flip a marker bit (bits[7:5]).
    {
        let mut b = buf;
        b[0] ^= 0x20;
        check(
            "refusal of another format marker",
            frame::open_frame(&mut ccm, &KEY, &mut b[..good_len]) == Err(FrameError::BadFormat),
        );
    }
    // Unknown type: set type field (bits[4:0]) to 10. JoinReveal=9 is the max valid type,
    // so 10 is the first unused discriminant: `FrameType::from_u8` returns None → BadType.
    {
        let mut b = buf;
        b[0] = (b[0] & 0xE0) | 0x0A;
        check(
            "refusal of an unknown frame type",
            frame::open_frame(&mut ccm, &KEY, &mut b[..good_len]) == Err(FrameError::BadType),
        );
    }
    // Truncated below header+tag.
    {
        let mut b = buf;
        check(
            "refusal of a truncated frame as too short",
            frame::open_frame(&mut ccm, &KEY, &mut b[..8]) == Err(FrameError::TooShort),
        );
    }
    // Tampered ciphertext byte (a payload byte after the 14-byte header).
    {
        let mut b = buf;
        b[16] ^= 0x01;
        check(
            "refusal of a tampered ciphertext",
            frame::open_frame(&mut ccm, &KEY, &mut b[..good_len]) == Err(FrameError::AuthFail),
        );
    }
    // Tampered tag (last byte).
    {
        let mut b = buf;
        b[good_len - 1] ^= 0x80;
        check(
            "refusal of a tampered tag",
            frame::open_frame(&mut ccm, &KEY, &mut b[..good_len]) == Err(FrameError::AuthFail),
        );
    }
    // Wrong key.
    {
        let mut b = buf;
        check(
            "refusal under a wrong key",
            frame::open_frame(&mut ccm, &KEY2, &mut b[..good_len]) == Err(FrameError::AuthFail),
        );
    }

    if pass {
        info!(target: "edge", "frame limits, all checks PASS");
    } else {
        error!(target: "edge", "frame limits, some checks FAIL");
    }
    loop {
        embassy_time::Timer::after_secs(5).await;
    }
}

app!(run);
