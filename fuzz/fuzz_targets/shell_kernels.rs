//! `shell_kernels`: the pure kernels behind the device shell (`crates/shell`).
//!
//! * [`parse_addr`] on arbitrary text (valid UTF-8 as given, and the raw input bytes decoded
//!   lossily, the way a non-UTF-8 line reaches a `&str`): agrees with an independent reference
//!   implementation of its documented grammar, and every accepted literal re-parses from its
//!   canonical hex forms. [`xorshift32_nonzero`] never yields the `auto` sentinel 0.
//! * [`Window`] pagination: the shell's re-run loop over a stable stream reassembles it exactly
//!   (no loss, no duplication, every window within `cap`); over a stream that DRIFTS between
//!   passes (a concurrent settings change), it still never splits a char, never exceeds `cap`,
//!   and terminates. Each pass captures exactly the documented byte range.
//! * [`common_prefix`]: a char-boundary prefix of every candidate, and the longest such.
//! * [`CompletionBudget`]: the candidate set the firmware's completer admits under the budget
//!   really fits: the real postcard encoding of the `ShellCompletions` reply (worst-case u16
//!   fields) is within `used()`, and with the firmware's budget (`MAX_PAYLOAD`) the reply encodes
//!   into one console frame.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tower_protocol::{
    Candidate, CandidateKind, MAX_PAYLOAD, MAX_WIRE, MsgType, ShellCompletions, encode_frame,
};
use tower_shell::{CompletionBudget, Window, common_prefix, parse_addr, xorshift32_nonzero};

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    addr: &'a str,
    addr_raw: &'a [u8],
    seed: u32,
    pieces: Vec<&'a str>,
    drift_pieces: Vec<&'a str>,
    drift_at: u8,
    cap: u16,
    candidates: Vec<(&'a str, u8)>,
    budget_cap: u16,
}

/// The documented grammar of `parse_addr`, written independently: `auto` (ASCII case-insensitive)
/// is 0; otherwise an optional `0x`/`0X` prefix and 1..=8 ASCII hex digits, nothing else.
fn reference_addr(s: &str) -> Option<u32> {
    if s.len() == 4 && s.to_ascii_lowercase() == "auto" {
        return Some(0);
    }
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if digits.is_empty() || digits.len() > 8 {
        return None;
    }
    let mut v = 0u32;
    for c in digits.chars() {
        v = (v << 4) | c.to_digit(16)?;
    }
    Some(v)
}

fn check_addr(s: &str) {
    let got = parse_addr(s);
    assert_eq!(got, reference_addr(s), "parse_addr({s:?})");
    if let Some(v) = got {
        if v != 0 || !s.eq_ignore_ascii_case("auto") {
            for form in [format!("{v:x}"), format!("0x{v:08X}"), format!("0X{v:X}")] {
                assert_eq!(parse_addr(&form), Some(v), "canonical form {form:?} of {s:?}");
            }
        }
    }
}

/// One pass of the shell's re-run loop over `stream`, checking the captured window against the
/// documented range. Returns `(chunk, consumed, more)`.
fn pass(stream: &[&str], skip: usize, cap: usize) -> (String, usize, bool) {
    let whole: String = stream.concat();
    let mut w = Window::new(skip, cap);
    let mut chunk = String::new();
    for p in stream {
        chunk.push_str(w.feed(p));
    }
    assert_eq!(w.total(), whole.len());
    assert!(chunk.len() <= cap, "window exceeded cap");
    assert_eq!(w.captured(), chunk.len());
    assert!(w.consumed() >= w.captured());
    assert_eq!(w.more(), skip + w.consumed() < whole.len());
    if w.more() {
        assert!(w.consumed() > 0, "a pass that reports `more` must advance");
    }
    if skip < whole.len() {
        // Exactly `[skip + skipped, skip + consumed)` of the stream, `skipped` being the tail of
        // a char that `skip` landed inside (only after a drift).
        let start = skip + w.consumed() - w.captured();
        assert_eq!(
            chunk,
            whole[start..skip + w.consumed()],
            "window captured the wrong range"
        );
    } else {
        assert!(chunk.is_empty() && w.consumed() == 0);
    }
    (chunk, w.consumed(), w.more())
}

fn check_window(pieces: &[&str], drift: &[&str], drift_at: usize, cap: usize) {
    // Stable stream: lossless reassembly.
    let whole: String = pieces.concat();
    let mut out = String::new();
    let mut skip = 0;
    let mut passes = 0;
    loop {
        let (chunk, consumed, more) = pass(pieces, skip, cap);
        out.push_str(&chunk);
        passes += 1;
        assert!(passes <= whole.len() + 1, "paging did not terminate");
        if !more {
            break;
        }
        skip += consumed;
    }
    assert_eq!(out, whole, "pagination lost or duplicated bytes");

    // Drifting stream: `pieces` for the first `drift_at` passes, then `drift`.
    let bound = whole.len().max(drift.concat().len()) + 1;
    let mut skip = 0;
    for n in 0.. {
        assert!(n <= bound, "paging over a drifting stream did not terminate");
        let stream = if n < drift_at { pieces } else { drift };
        let (_, consumed, more) = pass(stream, skip, cap);
        if !more {
            break;
        }
        skip += consumed;
    }
}

fn check_common_prefix(texts: &[&str]) {
    let p = common_prefix(texts.iter().copied());
    let Some(first) = texts.first() else {
        assert_eq!(p, "");
        return;
    };
    assert!(first.is_char_boundary(p.len()));
    for t in texts {
        assert!(t.starts_with(p), "{p:?} is not a prefix of {t:?}");
    }
    // Maximal: the next char of the first text is not shared by all.
    if let Some(c) = first[p.len()..].chars().next() {
        let longer = &first[..p.len() + c.len_utf8()];
        assert!(
            !texts.iter().all(|t| t.starts_with(longer)),
            "{p:?} is not the longest"
        );
    }
}

fn check_budget(cands: &[(&str, u8)], cap: usize) {
    const KINDS: [CandidateKind; 4] = [
        CandidateKind::Menu,
        CandidateKind::Command,
        CandidateKind::Arg,
        CandidateKind::Value,
    ];
    // Admission exactly as the firmware's completer does it (16 slots, budget first-fit).
    let mut budget = CompletionBudget::new(cap);
    let mut admitted: heapless::Vec<Candidate<'_>, 16> = heapless::Vec::new();
    let mut more = false;
    for &(text, kind) in cands {
        let c = Candidate {
            text,
            kind: KINDS[kind as usize % 4],
        };
        if !(admitted.len() < admitted.capacity() && budget.try_add(text.len()) && admitted.push(c).is_ok()) {
            more = true;
            break;
        }
    }
    let reply = ShellCompletions {
        req_id: u16::MAX,
        token_start: u16::MAX,
        common_prefix: common_prefix(admitted.iter().map(|c| c.text)),
        candidates: admitted,
        more,
    };
    let mut buf = vec![0u8; budget.used() + 64];
    let enc = postcard::to_slice(&reply, &mut buf).expect("reply encodes").len();
    assert!(
        enc <= budget.used(),
        "budget under-counted: {enc} > {}",
        budget.used()
    );
    if !reply.candidates.is_empty() {
        assert!(budget.used() <= cap, "budget admitted past its cap");
    }
    if cap == MAX_PAYLOAD {
        let mut wire = [0u8; MAX_WIRE];
        encode_frame(MsgType::ShellCompletions, 0, &reply, &mut wire)
            .expect("budgeted reply must fit one frame");
    }
}

fuzz_target!(|input: Input<'_>| {
    check_addr(input.addr);
    check_addr(&String::from_utf8_lossy(input.addr_raw));
    if let Ok(s) = core::str::from_utf8(input.addr_raw) {
        check_addr(s);
    }
    assert_ne!(xorshift32_nonzero(input.seed), 0, "0 is the `auto` sentinel");

    // `cap` >= 4 is `Window::new`'s documented precondition (any UTF-8 char must fit).
    let cap = 4 + input.cap as usize % 300;
    check_window(&input.pieces, &input.drift_pieces, input.drift_at as usize, cap);

    let texts: Vec<&str> = input.candidates.iter().map(|c| c.0).collect();
    check_common_prefix(&texts);
    let lossy: Vec<String> = input
        .addr_raw
        .split(|&b| b == b'\n')
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect();
    check_common_prefix(&lossy.iter().map(String::as_str).collect::<Vec<_>>());

    // The firmware's budget, and an arbitrary one.
    check_budget(&input.candidates, MAX_PAYLOAD);
    check_budget(&input.candidates, input.budget_cap as usize);
});
