//! Terminal-safe rendering of untrusted text on the plain (non-TUI) output paths.
//!
//! Everything a device or an MQTT peer hands us (log lines, event keys/values, Hello fields,
//! shell output, node names, retained topic payloads) is attacker-influenced text. Written
//! raw to stdout/stderr, an embedded ESC/CSI/OSC sequence is *executed* by the user's terminal:
//! it can rewrite the window title, emit OSC 52 clipboard writes, plant hyperlinks, move the
//! cursor to overwrite earlier lines, or fake a `[tower]` status line with a bare `\r`. The TUI
//! is immune (ratatui drops control chars when it fills its buffer); this module is the single
//! choke point for every other path, so the two can't drift.

use std::borrow::Cow;
use std::fmt::Write as _;

/// Does `c` need escaping? C0 controls (incl. ESC, CR, BEL) except `\n`/`\t`, DEL, the C1 block
/// (U+0080-U+009F; 0x9B is a single-byte CSI on terminals that honour 8-bit controls), and the
/// bidi embedding/override/isolate controls that can visually reorder a line ("Trojan Source").
fn needs_escape(c: char) -> bool {
    match c {
        '\n' | '\t' => false,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => true,
        c => c.is_control(),
    }
}

/// `s` with every dangerous character replaced by a visible escape (`\x1b`, `\u{9b}`, …).
/// Borrows when nothing needed escaping: the common case costs one scan, no allocation.
pub(crate) fn sanitize(s: &str) -> Cow<'_, str> {
    if !s.chars().any(needs_escape) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if !needs_escape(c) {
            out.push(c);
        } else if (c as u32) < 0x80 {
            let _ = write!(out, "\\x{:02x}", c as u32);
        } else {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        }
    }
    Cow::Owned(out)
}

/// [`sanitize`] for single-line fields (names, ids, keys): a newline or tab is escaped too, so
/// a value can't break out of its column or forge an extra output line.
pub(crate) fn sanitize_line(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c == '\n' || c == '\t' || needs_escape(c)) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push_str(&sanitize(c.encode_utf8(&mut [0; 4]))),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_borrows() {
        assert!(matches!(sanitize("temp=21.5 °C ✓"), Cow::Borrowed(_)));
        assert!(matches!(sanitize("a\tb\nc"), Cow::Borrowed(_)));
        assert!(matches!(sanitize_line("node-1"), Cow::Borrowed(_)));
    }

    #[test]
    fn escapes_terminal_controls() {
        // OSC 52 clipboard write, CSI cursor-up, bare CR, BEL, DEL, 8-bit CSI, RTL override.
        assert_eq!(sanitize("\x1b]52;c;aGk=\x07"), "\\x1b]52;c;aGk=\\x07");
        assert_eq!(
            sanitize("ok\x1b[1A\r[tower] fake"),
            "ok\\x1b[1A\\x0d[tower] fake"
        );
        assert_eq!(sanitize("a\x7fb"), "a\\x7fb");
        assert_eq!(sanitize("a\u{9b}31mb"), "a\\u{9b}31mb");
        assert_eq!(sanitize("txt\u{202E}gpj.exe"), "txt\\u{202e}gpj.exe");
    }

    #[test]
    fn line_variant_also_escapes_newline_and_tab() {
        assert_eq!(sanitize_line("evil\n[tower] ok\t"), "evil\\n[tower] ok\\t");
        assert_eq!(sanitize_line("x\x1by"), "x\\x1by");
    }
}
