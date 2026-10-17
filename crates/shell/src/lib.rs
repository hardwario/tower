//! tower-shell (`crates/shell`): hardware-free, `no_std` pure kernels behind the firmware SDK's
//! device shell: the `addr` setting's value parser and `addr random` PRNG, the
//! response-pagination [`Window`], and the TAB-completion helpers ([`common_prefix`],
//! [`CompletionBudget`]). It is not the shell itself: that is the SDK's `tower::shell`, which
//! keeps the console I/O and delegates the pure logic here.
//!
//! Extracted from the no_std firmware `src/` (which can't `cargo test`) so they get host
//! coverage, the same split as `tower-net` / `tower-gateway`. The firmware keeps the
//! EEPROM I/O, the hardware TRNG and the wire encoding; the pure logic lives here.

#![no_std]

// Host test harness only: the `Window` tests use std collections (Vec/String).
#[cfg(test)]
extern crate std;

/// Parse a `Kind::Addr` setting value into the stored 32-bit radio address.
///
/// * `auto` (any case) → `Some(0)`, the sentinel `shell::radio_addr` resolves to the
///   chip-UID-derived address.
/// * a hex literal (`0x1a2b3c4d` / `0X…` / bare `1a2b3c4d`, 1..=8 hex digits) → its value.
/// * anything else → `None`.
///
/// The hex digits are validated explicitly, because `u32::from_str_radix(_, 16)` otherwise
/// silently accepts a leading `+` (`"+1"`, even `"0x+1"` → 1), which is not a valid address literal.
/// (`random` is resolved by the caller before this ever runs.)
pub fn parse_addr(value: &str) -> Option<u32> {
    if value.eq_ignore_ascii_case("auto") {
        return Some(0);
    }
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if hex.is_empty() || hex.len() > 8 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// One xorshift32 step over a seed forced non-zero (`| 1`).
///
/// The result is **never 0** (xorshift of a non-zero word stays non-zero), which is the
/// load-bearing property for an `addr` value (`0` is the `auto` sentinel). **Not
/// cryptographic**: the firmware mixes live entropy (chip UID, uptime tick, rolling state)
/// into `seed`; this is only the deterministic transform.
pub fn xorshift32_nonzero(seed: u32) -> u32 {
    let mut x = seed | 1;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// One window of a paginated byte stream: the kernel behind the shell's **windowed
/// re-run streaming**.
///
/// The shell dispatcher streams *any* command's response uniformly (no per-command
/// special-casing) by running the sync handler repeatedly and capturing one output
/// window per pass: the handler writes its full response as `&str` pieces; a `Window`
/// keeps only the char-aligned byte range `[skip, skip+cap)` and counts the total. The
/// driver sends the captured window, advances `skip` by [`captured`](Window::captured)
/// and re-runs until [`more`](Window::more) is false. A big dump therefore re-executes a
/// few times (fine: those commands are rare and read-only), while every ordinary
/// response fits one window and runs once.
///
/// `feed` returns the exact sub-slice to append to the caller's `cap`-sized buffer; all
/// slicing lands on UTF-8 boundaries, so a multi-byte char never straddles two windows
/// (which would make a frame's text an invalid `&str`).
///
/// **Re-run drift.** Paging assumes the handler reproduces the same byte stream on every pass.
/// The radio transport awaits a TX between passes, so a concurrent change (e.g. a console
/// `settings set` replacing a multi-byte value) can shift the stream and leave `skip` inside a
/// char of the new one. `feed` then skips that char's remaining bytes instead of slicing
/// mid-char (which would panic), and counts them in [`consumed`](Window::consumed) so paging
/// still advances and terminates; the output is as inconsistent as the drift made it, but
/// never invalid UTF-8 and never a panic.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    skip: usize,
    cap: usize,
    total: usize,
    captured: usize,
    /// Stream bytes at the window start skipped (not captured) because `skip` landed inside a
    /// char of a drifted stream, counted in [`consumed`](Window::consumed).
    skipped: usize,
}

impl Window {
    /// Capture the window `[skip, skip+cap)`. `skip` should be a char boundary of the
    /// stream (0, or a previous window's `skip + consumed`, char-aligned for a reproducible
    /// stream; a drifted one is tolerated, see the type docs).
    /// `cap` MUST be ≥ 4 (the max UTF-8 char length): a window narrower than the next
    /// char would capture nothing and the driver's re-run loop would never advance. The
    /// shell's chunk sizes (≥ 56) satisfy this trivially.
    #[must_use]
    pub fn new(skip: usize, cap: usize) -> Self {
        debug_assert!(cap >= 4, "window cap must fit any UTF-8 char");
        Self {
            skip,
            cap,
            total: 0,
            captured: 0,
            skipped: 0,
        }
    }

    /// Feed the next `&str` piece of the stream; returns the sub-slice of `s` that falls
    /// inside this window (empty if none). Append the returned slice to the out buffer.
    pub fn feed<'a>(&mut self, s: &'a str) -> &'a str {
        let start = self.total; // stream offset of this piece (char-aligned: whole pieces)
        self.total += s.len();
        let room = self.cap - self.captured;
        if room == 0 {
            return "";
        }
        // Intersect the piece [start, start+len) with the window [skip, skip+cap).
        let mut lo = self.skip.saturating_sub(start).min(s.len());
        // `lo` is a char boundary when the stream reproduced exactly (`start` is a piece
        // boundary and `skip` was a previous pass's char-aligned end). If the stream drifted
        // between passes, `skip` can land inside a char: skip forward to the next boundary
        // (slicing there would panic) and count those bytes as consumed so paging advances.
        while !s.is_char_boundary(lo) {
            lo += 1;
            self.skipped += 1;
        }
        let hi_win = (self.skip + self.cap).saturating_sub(start).min(s.len());
        // Clamp the high end to a char boundary AND to the remaining capacity so the out
        // buffer never overflows.
        let mut hi = hi_win.min(lo + room);
        while hi > lo && !s.is_char_boundary(hi) {
            hi -= 1;
        }
        if hi <= lo {
            return "";
        }
        let seg = &s[lo..hi];
        self.captured += seg.len();
        seg
    }

    /// Total bytes the handler wrote this pass (window-independent).
    #[must_use]
    pub fn total(&self) -> usize {
        self.total
    }

    /// Bytes captured into this window (≤ `cap`).
    #[must_use]
    pub fn captured(&self) -> usize {
        self.captured
    }

    /// Stream bytes this window accounts for: [`captured`](Window::captured) plus any
    /// partial-char bytes skipped at its start after a drift. The driver advances
    /// `skip += consumed()`; it is always > 0 while [`more`](Window::more) holds, so paging ends.
    #[must_use]
    pub fn consumed(&self) -> usize {
        self.captured + self.skipped
    }

    /// True if output exists beyond this window; re-run with `skip += consumed()`.
    #[must_use]
    pub fn more(&self) -> bool {
        self.skip + self.consumed() < self.total
    }
}

/// Longest common prefix of `texts` (empty for none), cut at a **char boundary** of the
/// first text. Completion candidates include app-defined enum values, which may be
/// non-ASCII (`°C` / `°F` share a 1-byte prefix of a 2-byte char): a byte-wise prefix
/// sliced as `&str` would panic on TAB.
#[must_use]
pub fn common_prefix<'a>(mut texts: impl Iterator<Item = &'a str>) -> &'a str {
    let Some(first) = texts.next() else {
        return "";
    };
    let mut len = first.len();
    for t in texts {
        let common = first.bytes().zip(t.bytes()).take_while(|(a, b)| a == b).count();
        len = len.min(common);
    }
    while !first.is_char_boundary(len) {
        len -= 1;
    }
    &first[..len]
}

/// postcard's varint (LEB128) length of `n`: the length prefix of every `&str`/`Vec` and
/// the encoding of every `u16`/`u32` field.
#[must_use]
pub const fn varint_len(mut n: usize) -> usize {
    let mut l = 1;
    while n >= 0x80 {
        n >>= 7;
        l += 1;
    }
    l
}

/// Byte budget for one `ShellCompletions` reply, so it always fits **one** wire frame.
///
/// The reply is a single frame (the host matches it by `req_id`); 16 candidates averaging
/// more than ~12 bytes overflow the ~249-byte payload, the encode fails, the reply is
/// dropped and the host's TAB just times out. The completer therefore admits candidates
/// only while this budget holds, and flags `more` when it stops early.
///
/// Accounting mirrors postcard exactly, with worst cases for the values only known after
/// the candidate list is final: `req_id` + `token_start` (u16 varints, ≤ 3 bytes each),
/// the candidates' `Vec` length (1 byte, ≤ 16 items), `more` (1 byte), each candidate as
/// `str` (varint length + bytes) + `kind` (a 1-byte unit-variant index), and the
/// `common_prefix` reserved at the **first** candidate's full length (the prefix can't be
/// longer than any candidate).
#[derive(Debug, Clone, Copy)]
pub struct CompletionBudget {
    used: usize,
    cap: usize,
    any: bool,
}

impl CompletionBudget {
    /// Fixed overhead of an empty reply: `req_id` (≤3) + `token_start` (≤3) + the empty
    /// `common_prefix` (1) + the `Vec` length (1) + `more` (1).
    const BASE: usize = 3 + 3 + 1 + 1 + 1;

    /// A budget for a payload of at most `cap` bytes (`tower_protocol::MAX_PAYLOAD`).
    #[must_use]
    pub const fn new(cap: usize) -> Self {
        Self {
            used: Self::BASE,
            cap,
            any: false,
        }
    }

    /// Account one candidate of `text_len` bytes; `false` (nothing accounted) if the reply
    /// would no longer fit; the caller stops adding and sets `more`.
    pub fn try_add(&mut self, text_len: usize) -> bool {
        let mut cost = varint_len(text_len) + text_len + 1;
        if !self.any {
            // The common prefix, worst case = this first candidate (its empty-prefix byte is
            // already in BASE, so add the difference).
            cost += varint_len(text_len) - 1 + text_len;
        }
        if self.used + cost > self.cap {
            return false;
        }
        self.used += cost;
        self.any = true;
        true
    }

    /// Worst-case encoded payload size of the candidates admitted so far.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used
    }
}

/// Drive a [`Window`] over deterministic `pieces` (a test helper): returns each
/// captured window, so a round-trip proves the pagination loses and duplicates nothing.
#[cfg(test)]
fn paginate(pieces: &[&str], cap: usize) -> std::vec::Vec<std::string::String> {
    use std::string::String;
    use std::vec::Vec;
    let mut windows: Vec<String> = Vec::new();
    let mut skip = 0;
    loop {
        let mut w = Window::new(skip, cap);
        let mut chunk = String::new();
        for p in pieces {
            chunk.push_str(w.feed(p));
        }
        windows.push(chunk);
        if !w.more() {
            break;
        }
        skip += w.consumed();
        assert!(skip <= w.total(), "skip overran the stream");
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    #[test]
    fn parse_addr_auto_is_the_zero_sentinel() {
        assert_eq!(parse_addr("auto"), Some(0));
        assert_eq!(parse_addr("AUTO"), Some(0));
        assert_eq!(parse_addr("Auto"), Some(0));
    }

    #[test]
    fn parse_addr_hex_forms_agree() {
        assert_eq!(parse_addr("0x1a2b3c4d"), Some(0x1a2b_3c4d));
        assert_eq!(parse_addr("0X1A2B3C4D"), Some(0x1a2b_3c4d));
        assert_eq!(parse_addr("1a2b3c4d"), Some(0x1a2b_3c4d));
        assert_eq!(parse_addr("1"), Some(1));
        assert_eq!(parse_addr("ffffffff"), Some(0xffff_ffff));
    }

    #[test]
    fn parse_addr_length_bounds() {
        assert_eq!(parse_addr(""), None, "empty");
        assert_eq!(parse_addr("0x"), None, "prefix only");
        assert_eq!(parse_addr("123456789"), None, "9 hex digits > 32-bit");
        assert_eq!(parse_addr("0x123456789"), None, "9 digits after the prefix");
    }

    #[test]
    fn parse_addr_rejects_non_hex_and_signs() {
        // `u32::from_str_radix` would silently accept the sign forms; parse_addr must not.
        assert_eq!(parse_addr("+1"), None, "leading + is not an address literal");
        assert_eq!(parse_addr("0x+1"), None);
        assert_eq!(parse_addr("-1"), None);
        assert_eq!(parse_addr("12 34"), None, "embedded whitespace");
        assert_eq!(parse_addr("0xghij"), None, "non-hex letters");
        assert_eq!(
            parse_addr("random"),
            None,
            "resolved by the caller, never reaches here"
        );
    }

    #[test]
    fn xorshift_is_never_zero_even_from_a_zero_seed() {
        for seed in [0u32, 1, 0xffff_ffff, 0x8000_0000, 0x88a4_e90d] {
            assert_ne!(xorshift32_nonzero(seed), 0, "0 collides with the auto sentinel");
        }
    }

    #[test]
    fn xorshift_diverges_across_seeds() {
        // Distinct seeds → distinct outputs (the point of mixing fresh entropy per call).
        assert_ne!(xorshift32_nonzero(1), xorshift32_nonzero(2));
        assert_ne!(xorshift32_nonzero(0x1000), xorshift32_nonzero(0x2000));
    }

    #[test]
    fn xorshift_transform_is_pinned() {
        // Pin the exact transform (seed|1, then <<13 / >>17 / <<5) so an accidental
        // constant tweak is caught; the firmware's rand_u32_sw depends on this word.
        assert_eq!(xorshift32_nonzero(0), 270_369);
    }

    // ---- Window (shell response pagination) -------------------------------------

    /// The core invariant: paginating and reassembling loses/duplicates nothing, and
    /// no window exceeds `cap`.
    fn assert_roundtrip(pieces: &[&str], cap: usize) {
        let expected: std::string::String = pieces.concat();
        let windows = paginate(pieces, cap);
        for w in &windows {
            assert!(w.len() <= cap, "window {:?} exceeds cap {cap}", w);
        }
        let joined: std::string::String = windows.concat();
        assert_eq!(joined, expected, "pieces={pieces:?} cap={cap}");
    }

    #[test]
    fn window_paginates_ascii_without_loss() {
        // "/export"-shaped output split across many small windows.
        let pieces = &[
            "identity = tower\r\n",
            "addr = 0x88a4e90d\r\n",
            "therm-period = 60\r\n",
            "therm-delta = 50\r\n",
        ];
        for cap in [4, 7, 16, 20, 1000] {
            assert_roundtrip(pieces, cap);
        }
    }

    #[test]
    fn window_single_pass_when_it_fits() {
        let mut w = Window::new(0, 64);
        assert_eq!(w.feed("hello"), "hello");
        assert_eq!(w.feed(" world"), " world");
        assert!(!w.more(), "fits one window → no re-run");
        assert_eq!(w.total(), 11);
        assert_eq!(w.captured(), 11);
    }

    #[test]
    fn window_never_splits_a_multibyte_char() {
        // A 3-byte '✓' (and 2-byte 'αβγ') straddling every window boundary must move
        // whole. `cap` starts at 4, the documented minimum (a narrower window than the
        // next char captures nothing and the driver can't advance).
        let pieces = &["ok ✓ done ✓✓ αβγ end"];
        for cap in 4..24 {
            let windows = paginate(pieces, cap);
            for w in &windows {
                // Each window is itself valid UTF-8 (would panic on a split char).
                assert!(std::str::from_utf8(w.as_bytes()).is_ok());
            }
            assert_roundtrip(pieces, cap);
        }
    }

    #[test]
    fn window_empty_and_exact_multiple() {
        assert_roundtrip(&[""], 8);
        assert_roundtrip(&["", "", ""], 8);
        // Exactly two full windows, nothing left over.
        assert_roundtrip(&["abcdefgh", "ijklmnop"], 8);
    }

    /// Drive the shell's re-run loop over a handler whose output CHANGES between passes
    /// (`stream(pass)`), as a concurrent `settings set` can make it over the radio transport.
    /// Returns the windows; panics (test failure) on a mid-char slice or a non-terminating loop.
    fn paginate_drifting(
        stream: impl Fn(usize) -> std::vec::Vec<&'static str>,
        cap: usize,
    ) -> std::vec::Vec<std::string::String> {
        let mut windows = std::vec::Vec::new();
        let mut skip = 0;
        for pass in 0.. {
            assert!(pass < 1000, "paging must terminate");
            let mut w = Window::new(skip, cap);
            let mut chunk = std::string::String::new();
            for p in stream(pass) {
                chunk.push_str(w.feed(p));
            }
            assert!(chunk.len() <= cap);
            windows.push(chunk);
            if !w.more() {
                break;
            }
            assert!(w.consumed() > 0, "a pass that reports `more` must advance");
            skip += w.consumed();
        }
        windows
    }

    #[test]
    fn window_survives_a_stream_that_drifts_mid_char() {
        // Pass 0 emits an ASCII value; later passes a multi-byte one shifted by one byte, so
        // the second pass's `skip` lands INSIDE a char of the new stream. Before the fix this
        // sliced `&s[lo..]` mid-char and panicked.
        let a = "name = abcdef\r\nnext = 1\r\n";
        let b = "name = ✓✓✓✓✓✓\r\nnext = 1\r\n"; // '✓' is 3 bytes
        for cap in 4..20 {
            for drift_at in 0..3 {
                let w = paginate_drifting(|pass| if pass <= drift_at { vec![a] } else { vec![b] }, cap);
                for s in &w {
                    assert!(std::str::from_utf8(s.as_bytes()).is_ok());
                }
            }
        }
        // Piece-split variant: the drifted char straddles the window edge in a later piece.
        for cap in 4..12 {
            let w = paginate_drifting(
                |pass| {
                    if pass == 0 {
                        vec!["ab", "cdefgh", "ij"]
                    } else {
                        vec!["ab", "αβγδ", "ij"]
                    }
                },
                cap,
            );
            assert!(!w.is_empty());
        }
        // A stream that SHRINKS below `skip` ends cleanly too.
        let w = paginate_drifting(|pass| if pass == 0 { vec!["0123456789"] } else { vec!["01"] }, 4);
        assert_eq!(w[0], "0123");
    }

    #[test]
    fn window_consumed_counts_skipped_partial_char() {
        // Aligned skip: consumed == captured.
        let mut w = Window::new(2, 4);
        assert_eq!(w.feed("ab✓cdef"), "✓c");
        assert_eq!((w.captured(), w.consumed()), (4, 4));
        // skip=3 lands inside '✓' (bytes 2..5): its last 2 bytes are skipped, not sliced.
        let mut w = Window::new(3, 4);
        assert_eq!(w.feed("ab✓cdef"), "cd");
        assert_eq!((w.captured(), w.consumed()), (2, 4));
        assert!(w.more());
    }

    // ---- completion helpers ------------------------------------------------------

    #[test]
    fn common_prefix_is_char_safe() {
        assert_eq!(common_prefix([].into_iter()), "");
        assert_eq!(common_prefix(["print"].into_iter()), "print");
        assert_eq!(common_prefix(["settings", "set", "seed"].into_iter()), "se");
        // '°C' / '°F' share the whole 2-byte '°'.
        assert_eq!(common_prefix(["°C", "°F"].into_iter()), "°");
        // 'é' (C3 A9) vs 'è' (C3 A8): a byte-wise prefix would be the lone C3 (mid-char).
        assert_eq!(common_prefix(["é", "è"].into_iter()), "");
        assert_eq!(common_prefix(["xé1", "xè2"].into_iter()), "x");
        assert_eq!(common_prefix(["✓a", "✗b"].into_iter()), ""); // E2 9C 93 vs E2 9C 97
    }

    #[test]
    fn varint_len_matches_leb128() {
        assert_eq!(varint_len(0), 1);
        assert_eq!(varint_len(127), 1);
        assert_eq!(varint_len(128), 2);
        assert_eq!(varint_len(16_383), 2);
        assert_eq!(varint_len(16_384), 3);
        assert_eq!(varint_len(65_535), 3);
    }

    /// Exact postcard size of a `ShellCompletions` (worst-case u16 fields), computed
    /// independently of `CompletionBudget`'s incremental bookkeeping.
    fn encoded_size(cands: &[&str], prefix: &str) -> usize {
        3 + 3
            + varint_len(prefix.len())
            + prefix.len()
            + varint_len(cands.len())
            + cands
                .iter()
                .map(|c| varint_len(c.len()) + c.len() + 1)
                .sum::<usize>()
            + 1
    }

    #[test]
    fn completion_budget_keeps_every_reply_in_one_frame() {
        const PAYLOAD: usize = 249; // tower_protocol::MAX_PAYLOAD
        // 16 candidates of 20 chars (~22 B each): the case that used to overflow the frame.
        let long: std::vec::Vec<std::string::String> = (0..16)
            .map(|i| std::format!("very-long-setting-{i:02}"))
            .collect();
        let mut b = CompletionBudget::new(PAYLOAD);
        let mut admitted: std::vec::Vec<&str> = std::vec::Vec::new();
        for c in &long {
            if !b.try_add(c.len()) {
                break;
            }
            admitted.push(c);
        }
        assert!(admitted.len() < 16, "the budget must stop early (more = true)");
        let prefix = common_prefix(admitted.iter().copied());
        assert!(
            encoded_size(&admitted, prefix) <= b.used(),
            "accounting is worst-case"
        );
        assert!(b.used() <= PAYLOAD, "admitted set fits one frame");
        // Maximal: one more candidate would not have fit even with the real (shorter) prefix
        // reserved at its worst case.
        let next = &long[admitted.len()];
        assert!(b.used() + varint_len(next.len()) + next.len() + 1 > PAYLOAD);

        // Short names (the SDK tree) all fit: 16 × "print"-sized candidates.
        let mut b = CompletionBudget::new(PAYLOAD);
        assert!((0..16).all(|_| b.try_add(8)));
    }
}
