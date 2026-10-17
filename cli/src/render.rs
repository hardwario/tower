//! Host-side rendering of the decoded frame stream for the non-TUI commands (`logs`,
//! `events`, `monitor`). Owns the streaming read loop, sequence/decode-error accounting,
//! the schema mismatch banner, and the line formatting, which is deliberately
//! shared with the TUI (`log_line` / `event_fields` / `level_label`) so the two frontends
//! render a Log or Event identically.
//!
//! Output is either human text (optionally ANSI-colored) or NDJSON (`--json`): one JSON
//! object per line, so `tower logs --json | jq` works. JSON is emitted by hand with minimal
//! string escaping: the fields are small and known.
//!
//! Every device-supplied string is untrusted: the text renderer routes each one through
//! [`crate::term`] (escaping terminal controls) and the JSON renderer through [`json_str`]
//! (lossless `\u` escapes for the same set), so a frame can't drive the user's terminal.

use std::borrow::Cow;
use std::io::Write;

use clap::ValueEnum;

use tower_protocol::msg::{Dropped, Event, Level, Log, Print};
use tower_protocol::{FrameDecoder, Msg, decode_msg};

use crate::session::{FramingWatch, Mismatch, Transport, read_some};
use crate::term::{sanitize, sanitize_line};

/// Which entity stream to render.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum View {
    Logs,
    Events,
}

/// When to emit ANSI color escapes. `Auto` (the default) colors only when stdout is a
/// real terminal and `NO_COLOR` is unset, so `tower logs > file` / `| grep` stay clean.
#[derive(Clone, Copy, PartialEq, ValueEnum)]
pub(crate) enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    /// Resolve to a concrete on/off, honoring the `NO_COLOR` convention (<https://no-color.org>)
    /// and TTY detection for `Auto`.
    fn enabled(self) -> bool {
        use std::io::IsTerminal;
        match self {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal(),
        }
    }
}

/// Resolve the effective color choice: the deprecated `--no-colors` flag, if present, forces
/// off; otherwise `--color`'s auto/always/never decides (auto = TTY && `NO_COLOR` unset).
pub(crate) fn resolve_color(color: ColorMode, no_colors: bool) -> bool {
    if no_colors { false } else { color.enabled() }
}

/// How the streaming commands render frames: human-readable text (with or without color)
/// or one JSON object per line.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum OutputMode {
    Text { colors: bool },
    Json,
}

// ---- shared line formatting (also used by the TUI) ----

/// The uptime prefix `[   sss.mmm]` a Log line carries, from the device's microsecond clock.
pub(crate) fn uptime_prefix(uptime_us: u64) -> String {
    let secs = uptime_us / 1_000_000;
    let ms = (uptime_us % 1_000_000) / 1_000;
    format!("[{secs:>5}.{ms:03}]")
}

/// The fixed-width severity label for a level (padded so columns line up).
pub(crate) fn level_label(level: Level) -> &'static str {
    match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN ",
        Level::Info => "INFO ",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    }
}

/// The ANSI SGR color code for a level (used by the text renderer's `paint`).
fn level_ansi(level: Level) -> u8 {
    match level {
        Level::Error => 31,
        Level::Warn => 33,
        Level::Info => 32,
        Level::Debug => 36,
        Level::Trace => 90,
    }
}

/// Format a Log's body **without** the leading wall-clock time or color: `[uptime] LEVEL
/// module: message`. The caller prepends its own timestamp and applies its own coloring. A Log
/// is one record per line, so `module` and `message` are escaped as single-line fields: an
/// embedded newline can't forge a second, fake log record.
pub(crate) fn log_line(l: &Log) -> String {
    format!(
        "{} {} {}: {}",
        uptime_prefix(l.uptime_us),
        level_label(l.level),
        sanitize_line(l.module),
        sanitize_line(l.message)
    )
}

/// Format an Event's `key=value` fields into one space-joined string (shared CLI/TUI). Keys
/// and values are single-line fields (escaped), so a value can't fake another `key=value`
/// column or line.
pub(crate) fn event_fields(e: &Event) -> String {
    e.fields
        .iter()
        .map(|(k, v)| format!("{}={}", sanitize_line(k), sanitize_line(v)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Free-form multi-line device text (`print!` output, shell responses) made terminal-safe. The
/// firmware ends lines with CRLF (its `println!` and shell output are terminal-friendly), so a CR
/// that closes a line is folded into the LF first; only a *bare* CR (the one that can rewind
/// the cursor and overprint an earlier line) is left for [`sanitize`] to escape.
pub(crate) fn device_text(s: &str) -> Cow<'_, str> {
    if s.contains("\r\n") {
        Cow::Owned(sanitize(&s.replace("\r\n", "\n")).into_owned())
    } else {
        sanitize(s)
    }
}

/// Wrap `s` in an ANSI SGR color if `colors`, else return it plain.
fn paint(s: &str, code: u8, colors: bool) -> String {
    if colors {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

// ---- JSON (NDJSON) ----

/// Minimal JSON string escaping for the small, known fields we emit. Beyond the C0 controls
/// JSON requires escaping, DEL, the C1 block (U+0080-U+009F; 0x9B is an 8-bit CSI) and the
/// bidi controls are `\u`-escaped too: that's still the *same* string to any JSON parser (no
/// data is altered, unlike the text renderer's visible escapes), but `tower logs --json` read
/// straight off a terminal can't be made to execute a control sequence. (serde_json would only
/// escape C0, so it isn't used here.)
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- streaming read loop ----

/// Per-stream render state: sequence tracking, plus mismatch/decode-error accounting so a
/// protocol drift is surfaced loudly rather than as garbled output.
#[derive(Default)]
pub(crate) struct RenderState {
    pub(crate) last_seq: Option<u16>,
    /// Frames the codec refused to decode (`SchemaMismatch`/`BadType`/`BadCrc`/…). A burst of
    /// these on an otherwise-live link is the classic symptom of a stale peer.
    pub(crate) decode_failures: u64,
    /// The mismatch seen so far, if any (the device's identity once its `Hello` was seen).
    mismatch: Option<Mismatch>,
    /// Whether we've already shouted about a lockstep mismatch (warn once per stream).
    warned_mismatch: bool,
    /// Every frame failing its CRC: a framing difference the tags cannot see (warn once).
    framing: FramingWatch,
    /// The `session_id` from the last `Hello`. The console re-emits `Hello` on every USB
    /// plug-in with the SAME id within a boot, so a *changed* id means the device rebooted.
    last_session: Option<u32>,
}

#[cfg(test)]
impl RenderState {
    /// Whether the framing diagnosis was printed (tests).
    fn framing_reported(&self) -> bool {
        self.framing.reported()
    }
}

/// Emit the loud, persistent lockstep-mismatch banner (stderr): the device's frames carry
/// another console schema tag, so it was built from another checkout and postcard would
/// silently mis-decode it. The tag can't say which side is older: the remedy is to rebuild both.
pub(crate) fn warn_mismatch(m: &Mismatch) {
    eprintln!("[tower] ============================================================");
    eprintln!(
        "[tower] SCHEMA MISMATCH: {} speaks console schema {:#06x}, this `tower` {:#06x}.",
        m.device_label(),
        m.tag,
        tower_protocol::CONSOLE_TAG
    );
    eprintln!("[tower] The two were built from different checkouts. postcard is NOT self-describing,");
    eprintln!("[tower] so none of its frames are interpreted. Rebuild both `tower` and the firmware");
    eprintln!("[tower] from the SAME checkout (`just install-cli`, then reflash the board).");
    eprintln!("[tower] ============================================================");
}

/// One connection's worth of stream rendering: the deframer plus the render state, so bytes
/// can be fed from more than one source in order (the boot transcript captured while waiting
/// for the `Hello` (`logs --reset --send`), then the live port) without a seam between them.
pub(crate) struct Renderer {
    dec: FrameDecoder,
    st: RenderState,
    mode: OutputMode,
    view: View,
}

impl Renderer {
    pub(crate) fn new(mode: OutputMode, view: View) -> Self {
        Renderer {
            dec: FrameDecoder::new(),
            st: RenderState::default(),
            mode,
            view,
        }
    }

    /// Deframe and render `bytes` (any split; frames may straddle calls).
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if let Some(inner) = self.dec.push(b) {
                render(inner, self.mode, self.view, &mut self.st);
            }
        }
    }
}

/// Stream the framed link to stdout through `r` until the port fails, and return that failure
/// (a live link never ends the loop; the caller decides whether to reconnect or give up).
pub(crate) fn read_loop(sp: &mut (impl Transport + ?Sized), r: &mut Renderer) -> std::io::Error {
    let mut buf = [0u8; 512];
    loop {
        match read_some(sp, &mut buf) {
            Ok(n) => r.feed(&buf[..n]),
            Err(e) => return e,
        }
    }
}

/// Decode one frame and render it per `view`/`mode`, updating sequence + error accounting.
/// (Kept `pub(crate)` so unit tests can drive it directly with synthesized frames.)
pub(crate) fn render(inner: &[u8], mode: OutputMode, view: View, st: &mut RenderState) {
    // `decode_msg` does the CRC + tag check (like `decode_frame`) AND deserializes the payload
    // into the typed `Msg` in one step: the consumer no longer hand-writes a `MsgType` match +
    // `from_bytes` per arm. A CRC-valid body that fails to deserialize surfaces as
    // `Error::Malformed` here, same drop path as a tag/CRC failure.
    let decoded = decode_msg(inner);
    if st.framing.observe(&decoded) {
        eprintln!("[tower] {}", FramingWatch::MESSAGE);
    }
    let (seq, msg) = match decoded {
        Ok(t) => t,
        Err(e) => {
            st.decode_failures += 1;
            // A CRC-valid frame with another schema tag IS the lockstep mismatch: escalate to the
            // full remediation banner once, as soon as the device is known (its Hello names it)
            // or at the first such frame. Every later one of its frames is dropped here too.
            if let tower_protocol::Error::SchemaMismatch { got } = e {
                Mismatch::note(&mut st.mismatch, Mismatch::from_frame(got, inner));
                if !st.warned_mismatch
                    && let Some(m) = &st.mismatch
                {
                    warn_mismatch(m);
                    st.warned_mismatch = true;
                }
            }
            // A tagged codec error is the smoking gun for a stale peer; surface the running count
            // so a wall of them reads as one diagnosable cause, not noise. In JSON mode make it a
            // first-class NDJSON record (like device-side `Dropped`) so `| jq` consumers see it on
            // stdout, not just humans on stderr.
            match mode {
                OutputMode::Json => println!(
                    "{{\"type\":\"decode_error\",\"error\":{},\"count\":{}}}",
                    json_str(&format!("{e:?}")),
                    st.decode_failures
                ),
                OutputMode::Text { .. } => eprintln!(
                    "[tower] dropped a frame: {e} ({} undecodable frame(s))",
                    st.decode_failures
                ),
            }
            return;
        }
    };
    // A `Hello` marks a new device session: the firmware resets its per-session `seq`
    // (the dynamic console re-emits `Hello` on every USB plug-in), so re-baseline our
    // tracking on it rather than reporting a spurious gap across the reconnect.
    if matches!(msg, Msg::Hello(_)) {
        st.last_seq = None;
    }
    if let Some(prev) = st.last_seq {
        let expected = prev.wrapping_add(1);
        if seq != expected {
            match mode {
                OutputMode::Json => {
                    println!("{{\"type\":\"seq_gap\",\"expected\":{expected},\"got\":{seq}}}")
                }
                OutputMode::Text { .. } => {
                    eprintln!("[tower] seq gap: expected {expected}, got {seq}")
                }
            }
        }
    }
    st.last_seq = Some(seq);

    match msg {
        Msg::Hello(h) => {
            // A changed session_id since the last Hello means the device rebooted (the console
            // re-emits Hello with the SAME id on a mere USB re-plug within one boot).
            let rebooted = st.last_session.is_some_and(|s| s != h.session_id);
            st.last_session = Some(h.session_id);
            // A Hello with our tag: the device (re)flashed from this checkout; a later mismatch
            // is a new event worth its own banner.
            st.mismatch = None;
            st.warned_mismatch = false;
            eprintln!(
                "[tower] hello: {} {} (schema {:#06x}, session {}){}",
                sanitize_line(h.firmware_name),
                sanitize_line(h.firmware_version),
                tower_protocol::CONSOLE_TAG,
                h.session_id,
                if rebooted { "; device rebooted" } else { "" }
            );
        }
        Msg::Log(l) if view == View::Logs => emit_log(&l, mode),
        Msg::Print(p) if view == View::Logs => emit_print(&p, mode),
        Msg::Dropped(d) if view == View::Logs => emit_dropped(&d, mode),
        Msg::Event(e) if view == View::Events => emit_event(&e, mode),
        _ => {} // frames not relevant to this view (or host→device types)
    }
}

fn emit_log(l: &Log, mode: OutputMode) {
    match mode {
        OutputMode::Text { colors } => {
            // Re-color just the level label inside the shared layout.
            let body = log_line(l);
            let label = level_label(l.level);
            let colored = body.replacen(label, &paint(label, level_ansi(l.level), colors), 1);
            println!("{} {colored}", now());
        }
        OutputMode::Json => {
            println!(
                "{{\"type\":\"log\",\"uptime_us\":{},\"level\":{},\"module\":{},\"message\":{}}}",
                l.uptime_us,
                json_str(level_label(l.level).trim()),
                json_str(l.module),
                json_str(l.message)
            );
        }
    }
}

fn emit_print(p: &Print, mode: OutputMode) {
    match mode {
        OutputMode::Text { .. } => {
            print!("{}", device_text(p.text));
            let _ = std::io::stdout().flush();
        }
        OutputMode::Json => println!("{{\"type\":\"print\",\"text\":{}}}", json_str(p.text)),
    }
}

fn emit_dropped(d: &Dropped, mode: OutputMode) {
    match mode {
        OutputMode::Text { colors } => eprintln!(
            "{} {} log frame(s) dropped (device queue full)",
            paint("⚠", 33, colors),
            d.count
        ),
        OutputMode::Json => println!("{{\"type\":\"dropped\",\"count\":{}}}", d.count),
    }
}

fn emit_event(e: &Event, mode: OutputMode) {
    match mode {
        OutputMode::Text { colors } => println!(
            "{} {} {}  {}",
            now(),
            paint("EVENT", 35, colors),
            sanitize_line(e.name),
            event_fields(e)
        ),
        OutputMode::Json => {
            let mut fields = String::from("{");
            for (i, (k, v)) in e.fields.iter().enumerate() {
                if i > 0 {
                    fields.push(',');
                }
                fields.push_str(&format!("{}:{}", json_str(k), json_str(v)));
            }
            fields.push('}');
            println!(
                "{{\"type\":\"event\",\"name\":{},\"fields\":{fields}}}",
                json_str(e.name)
            );
        }
    }
}

// ---- monitor (transport debugging) ----

/// Hex-dump a byte slice as contiguous lowercase pairs (no separators).
pub(crate) fn hexline(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_protocol::msg::Hello;
    use tower_protocol::{MsgType, encode_frame};

    const TEXT: OutputMode = OutputMode::Text { colors: false };

    fn frame<T: serde::Serialize>(mt: MsgType, seq: u16, payload: &T) -> Vec<u8> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(mt, seq, payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn hello_frame() -> Vec<u8> {
        frame(
            MsgType::Hello,
            0,
            &Hello {
                firmware_name: "app",
                firmware_version: "test",
                session_id: 1,
            },
        )
    }

    /// The tag of a device built from another checkout.
    const FOREIGN: u16 = tower_protocol::CONSOLE_TAG ^ 0x00FF;

    /// A frame from a device built from another checkout (another schema tag).
    fn foreign<T: serde::Serialize>(mt: MsgType, seq: u16, payload: &T) -> Vec<u8> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = tower_protocol::encode_frame_with_tag(FOREIGN, mt, seq, payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn log_frame(seq: u16, msg: &str) -> Vec<u8> {
        frame(
            MsgType::Log,
            seq,
            &Log {
                level: Level::Info,
                uptime_us: 0,
                module: "t",
                message: msg,
            },
        )
    }

    /// Feed whole wire frames through a fresh decoder into `render`, accumulating state.
    fn feed_render(st: &mut RenderState, frames: &[u8]) {
        let mut d = FrameDecoder::new();
        for &b in frames {
            if let Some(inner) = d.push(b) {
                render(inner, TEXT, View::Logs, st);
            }
        }
    }

    #[test]
    fn render_rebaselines_seq_on_hello() {
        // A Hello resets seq tracking, so a fresh session starting at seq 0 after a prior
        // stream at seq 100 is NOT reported as a gap.
        let mut st = RenderState::default();
        feed_render(&mut st, &log_frame(100, "a"));
        assert_eq!(st.last_seq, Some(100));
        feed_render(&mut st, &hello_frame());
        // Hello re-baselined: last_seq is the Hello's own seq (0), no spurious gap.
        assert_eq!(st.last_seq, Some(0));
        assert_eq!(st.decode_failures, 0);
    }

    #[test]
    fn render_drops_frames_from_another_schema() {
        // A device built from another checkout: its Hello names it, the banner fires once, and
        // every one of its frames is counted as a decode failure instead of being rendered (it
        // would decode against the wrong definitions). A device reflashed from this checkout
        // (a Hello with our tag) is rendered again.
        let mut st = RenderState::default();
        let h = Hello {
            firmware_name: "radio_push_button",
            firmware_version: "v0.1.0",
            session_id: 9,
        };
        feed_render(&mut st, &foreign(MsgType::Hello, 0, &h));
        assert!(st.warned_mismatch);
        let m = st.mismatch.clone().expect("a mismatch");
        assert_eq!(m.tag, FOREIGN);
        assert_eq!(m.device.as_deref(), Some("radio_push_button v0.1.0"));
        let l = Log {
            level: Level::Info,
            uptime_us: 0,
            module: "t",
            message: "mis-decoded",
        };
        feed_render(&mut st, &foreign(MsgType::Log, 1, &l));
        feed_render(&mut st, &foreign(MsgType::Log, 2, &l));
        assert_eq!(st.decode_failures, 3);
        assert_eq!(st.last_seq, None, "refused frames don't advance seq tracking");
        // A later non-Hello frame keeps the identity the Hello gave.
        assert_eq!(
            st.mismatch.as_ref().unwrap().device.as_deref(),
            Some("radio_push_button v0.1.0")
        );
        feed_render(&mut st, &hello_frame());
        assert!(st.mismatch.is_none() && !st.warned_mismatch);
        feed_render(&mut st, &log_frame(1, "fine"));
        assert_eq!(st.decode_failures, 3);
        assert_eq!(st.last_seq, Some(1));
    }

    #[test]
    fn render_counts_decode_failures() {
        // A frame that COBS-decodes but fails CRC must be counted as a decode failure. Flip the
        // CRC byte just before the trailing 0x00: it stays a data byte in the COBS stream, so
        // COBS still decodes, but the CRC check then fails. XOR 0x80 avoids making a 0x00.
        let mut st = RenderState::default();
        let mut bytes = log_frame(0, "ok");
        let last = bytes.len() - 2;
        bytes[last] ^= 0x80;
        feed_render(&mut st, &bytes);
        assert_eq!(st.decode_failures, 1);
        assert!(!st.warned_mismatch, "a corrupt frame is never a mismatch");
    }

    #[test]
    fn render_reports_a_framing_difference_once() {
        // Every frame failing its CRC (here: frames whose CRC is computed differently) is the
        // framing heuristic's pattern: reported once at the threshold, not per frame, and a
        // frame that passes its CRC resets the run.
        let corrupt = |seq| {
            let mut b = log_frame(seq, "x");
            let last = b.len() - 2;
            b[last] ^= 0x80;
            b
        };
        let mut st = RenderState::default();
        for seq in 0..(FramingWatch::THRESHOLD as u16 - 1) {
            feed_render(&mut st, &corrupt(seq));
        }
        assert!(!st.framing_reported());
        feed_render(&mut st, &corrupt(100));
        assert!(st.framing_reported());
        assert_eq!(st.decode_failures, u64::from(FramingWatch::THRESHOLD));
    }

    /// Like [`feed_render`] but in an explicit [`OutputMode`], for the JSON-diagnostics paths.
    fn feed_render_mode(st: &mut RenderState, mode: OutputMode, frames: &[u8]) {
        let mut d = FrameDecoder::new();
        for &b in frames {
            if let Some(inner) = d.push(b) {
                render(inner, mode, View::Logs, st);
            }
        }
    }

    #[test]
    fn json_mode_emits_and_counts_decode_error() {
        // In JSON mode a corrupt frame becomes a `decode_error` NDJSON record on stdout (like
        // the device-side `dropped` record) rather than only a human stderr line; here we pin
        // that it still runs the JSON branch and keeps the running count.
        let mut st = RenderState::default();
        let mut bytes = log_frame(0, "ok");
        let last = bytes.len() - 2;
        bytes[last] ^= 0x80; // break the CRC (stays COBS-decodable)
        feed_render_mode(&mut st, OutputMode::Json, &bytes);
        assert_eq!(st.decode_failures, 1);
    }

    #[test]
    fn json_mode_detects_seq_gap() {
        // A seq discontinuity in JSON mode emits a `seq_gap` NDJSON record on stdout; assert the
        // gap is detected (state advances to the latest seq) without a Hello re-baseline.
        let mut st = RenderState::default();
        feed_render_mode(&mut st, OutputMode::Json, &log_frame(1, "a"));
        feed_render_mode(&mut st, OutputMode::Json, &log_frame(5, "b")); // gap 2..=4
        assert_eq!(st.last_seq, Some(5));
        assert_eq!(st.decode_failures, 0);
    }

    #[test]
    fn color_mode_never_and_always() {
        assert!(!ColorMode::Never.enabled());
        assert!(ColorMode::Always.enabled());
    }

    #[test]
    fn resolve_color_no_colors_forces_off() {
        // The deprecated --no-colors flag always wins, even over --color always.
        assert!(!resolve_color(ColorMode::Always, true));
        assert!(resolve_color(ColorMode::Always, false));
    }

    #[test]
    fn json_escapes_control_and_quotes() {
        assert_eq!(json_str("a\"b\\c\n"), r#""a\"b\\c\n""#);
    }

    #[test]
    fn log_line_layout_is_stable() {
        let l = Log {
            level: Level::Info,
            uptime_us: 1_234_567,
            module: "radio",
            message: "up",
        };
        assert_eq!(log_line(&l), "[    1.234] INFO  radio: up");
    }

    #[test]
    fn hexline_is_contiguous_lowercase() {
        assert_eq!(hexline(&[0x00, 0xAB, 0x0f]), "00ab0f");
    }

    // ---- terminal safety: device strings can't drive the terminal (CLI-6) ----

    #[test]
    fn device_text_folds_crlf_but_escapes_bare_cr() {
        // The firmware's CRLF line endings are plain newlines, not an attack…
        assert_eq!(device_text("a: 1\r\nb: 2\r\n"), "a: 1\nb: 2\n");
        assert!(matches!(device_text("plain\n\tok"), Cow::Borrowed(_)));
        // …but a bare CR (overprint an earlier line) and an OSC title write are escaped.
        assert_eq!(device_text("ok\r[tower] fake\r\n"), "ok\\x0d[tower] fake\n");
        assert_eq!(device_text("\x1b]0;pwn\x07"), "\\x1b]0;pwn\\x07");
    }

    #[test]
    fn text_formatters_escape_device_controls() {
        // A Log is one record per line: module + message can't clear the screen or forge a
        // second record with an embedded newline.
        let l = Log {
            level: Level::Info,
            uptime_us: 0,
            module: "m\x1b[2J",
            message: "a\n00:00:00.000 [    0.000] ERROR auth: fake",
        };
        let line = log_line(&l);
        assert!(!line.contains('\x1b') && !line.contains('\n'), "{line}");
        assert!(line.contains("m\\x1b[2J: a\\n00:00"), "{line}");
        // Event keys/values are single-line fields too.
        let mut fields: heapless::Vec<(&str, &str), 8> = heapless::Vec::new();
        fields.push(("k\u{9b}", "v\nx=1")).unwrap();
        let e = Event { name: "n", fields };
        assert_eq!(event_fields(&e), "k\\u{9b}=v\\nx=1");
    }

    #[test]
    fn json_escapes_c1_del_and_bidi_losslessly() {
        // Beyond JSON's mandatory C0 escapes, DEL / C1 / bidi controls are \u-escaped too…
        assert_eq!(json_str("a\u{9b}b\x7fc\u{202e}d"), r#""a\u009bb\u007fc\u202ed""#);
        // …which a JSON parser decodes back to the identical string: nothing is altered.
        let raw = "x\u{9b}\x1b[31m\u{2066}y\r\n";
        let back: String = serde_json::from_str(&json_str(raw)).unwrap();
        assert_eq!(back, raw);
    }

    // ---- Renderer: one deframer across feeds (CLI-7 replay seam) ----

    #[test]
    fn renderer_carries_a_frame_across_feeds() {
        // The boot transcript and the live port are fed separately; a frame straddling the two
        // must still decode (no seam, no spurious decode failure).
        let mut bytes = hello_frame();
        bytes.extend(log_frame(1, "boot"));
        let cut = bytes.len() - 4;
        let mut r = Renderer::new(TEXT, View::Logs);
        r.feed(&bytes[..cut]);
        assert_eq!(r.st.last_seq, Some(0)); // the Hello so far
        r.feed(&bytes[cut..]);
        assert_eq!(r.st.last_seq, Some(1));
        assert_eq!(r.st.decode_failures, 0);
    }
}
