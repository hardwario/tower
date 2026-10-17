//! `tower console`: the ratatui TUI.
//!
//! One synchronous loop owns the serial port: it polls the keyboard (short timeout),
//! drains all incoming frames through one `FrameDecoder`, and redraws. Everything is
//! async-via-drain: TAB sends a `ShellComplete` and Enter sends a `ShellCommand`; the
//! responses (`ShellCompletions` / `ShellResponse`) are handled when they arrive in the
//! drain, so nothing blocks. Layout/keys follow `docs/console.md` (in the firmware repo).
//!
//! Caveat: a TUI needs a real terminal; this is build- + clippy-verified, but NOT
//! interactively driven here (no TTY available). The completion/command round-trips
//! reuse the protocol paths proven in the non-TUI commands, so behaviour is by
//! construction; run `tower console` on a real terminal to confirm the UI.

use std::collections::VecDeque;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use tower_protocol::msg::{
    Dropped, Event as EvMsg, Hello, Level, Log, Print, ShellCommand, ShellComplete, ShellCompletions,
    ShellResponse,
};
use tower_protocol::{FrameDecoder, MsgType, decode_frame, encode_frame};

use crate::session::{FramingWatch, MAX_RESPONSE_BYTES, Mismatch, completion_start, read_some};

const CAP: usize = 5000; // scrollback per pane

/// How long an in-flight command may wait with no response activity before it's closed with a
/// "no response … (timed out)" marker. The same idle budget as `exec`/`shell`'s default
/// `--timeout` (sized above the firmware's ~5.2 s EEPROM-compaction stall), so a slow but
/// healthy command is never written off early.
const PENDING_IDLE: Duration = Duration::from_millis(crate::DEFAULT_TIMEOUT_MS);

/// A boot `Hello` orphans the commands sent to the *previous* device session, but a command
/// typed in the same instant may already have reached the new one: the Hello is read up to a
/// drain tick (+ UART transit) after the device emitted it. Only commands older than this when
/// the Hello is processed are written off; younger ones wait for their answer (or the timeout).
const HELLO_RACE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    /// The merged SSH-style shell: scrollback (echo + responses) + a "> " prompt + hints.
    Shell,
    Events,
    Logs,
}

struct App {
    port_name: String,
    sp: Option<Box<dyn serialport::SerialPort>>,
    dec: FrameDecoder,
    /// Device log lines as (colored prefix `clock [uptime] LEVEL`, rest, level color); only
    /// the prefix is tinted, so message text stays readable at every severity.
    logs: VecDeque<(String, String, Color)>,
    events: VecDeque<String>,
    responses: VecDeque<String>,
    input: String,
    cursor: usize,
    history: Vec<String>,
    hist_idx: Option<usize>,
    focus: Pane,
    zoom: bool,
    paused: bool,
    scroll: [usize; 3], // [logs, events, responses] lines scrolled up from bottom
    cmd_id: u16,
    req_id: u16,
    seq: u16,
    /// In-flight commands, oldest first. A queue, not a single slot, so a slow response never
    /// blocks typing/sending the next command: its lines simply keep landing in the scrollback,
    /// right above the prompt, whenever they arrive. Responses match by `cmd_id` ANYWHERE in the
    /// queue (see [`on_shell_response`]); an entry that will never be answered (superseded by
    /// a later answer, idle past [`PENDING_IDLE`], orphaned by a device restart or a lost link)
    /// is closed with a visible marker instead of silently wedging everything behind it.
    pending: VecDeque<PendingCmd>,
    /// The last scrollback line is an incomplete (no trailing newline yet) piece of the
    /// front in-flight response; the next chunk continues it in place. Cleared by an echo
    /// (a new command visually closes the line) and by completion/truncation.
    resp_partial: bool,
    pending_req: Option<u16>,
    hint: String, // transient completion / status hint
    /// Whether we've already warned about a schema mismatch (warn once per mismatch; a Hello
    /// with our tag re-arms it).
    warned_mismatch: bool,
    /// Last frame `seq` seen (any type), for gap detection; re-baselined on every `Hello`.
    last_seq: Option<u16>,
    /// Frames the codec refused to decode (`SchemaMismatch`/`BadCrc`/`BadType`/…). A burst of
    /// these on a live link is the classic stale-peer symptom: shown in the header, not swallowed.
    decode_failures: u64,
    /// Count of `seq` discontinuities (a decoded frame whose `seq` wasn't `prev + 1`).
    seq_gaps: u64,
    /// Frames that decoded at the frame layer but whose postcard payload failed to parse.
    payload_errors: u64,
    /// The device's frames carry another schema tag (it was built from another checkout): the
    /// tag and, once its `Hello` was seen, its firmware, for the header hint. None of its frames
    /// is interpreted. Cleared by a `Hello` with our tag.
    mismatch: Option<Mismatch>,
    /// Every frame failing its CRC: a framing difference the tags cannot see (shown once).
    framing: FramingWatch,
    /// Last failed (re)connect error, shown next to the reconnect indicator. `None` while
    /// connected or before the first failed attempt.
    last_open_error: Option<String>,
    last_open_attempt: Instant,
    /// Set when the session opened with `--reset`: the moment the pulse released. Cleared by
    /// the first `Hello`; while set, the header animates "booting…" so a slow boot (a fallback
    /// EEPROM compaction can hold the chip ~5 s; firmware/docs/storage.md) reads as
    /// progress, not a dead UI. Past the CLI's 8 s Hello ceiling it escalates to a warning.
    boot_wait: Option<Instant>,
    /// High-water mark of hint rows ever rendered under the prompt: once TAB completion
    /// allocates rows they STAY allocated (blank when no hint), so the prompt never jumps
    /// up and down as hints come and go. Reset by `Shift-F8`.
    hint_rows_reserved: u16,
    quit: bool,
}

/// One in-flight shell command awaiting its (possibly multi-chunk) response.
struct PendingCmd {
    cmd_id: u16,
    /// The command line as sent, named in the marker if the command is abandoned.
    line: String,
    /// Expected `chunk` index of the next `ShellResponse` frame; a gap means a middle chunk
    /// was CRC-dropped, so the streamed output is truncated.
    next_chunk: u16,
    /// Response bytes streamed so far, capped at [`MAX_RESPONSE_BYTES`] (a device that never
    /// sends `last` mustn't grow the scrollback, or one open partial line, without bound).
    received: usize,
    /// When the command went out (decides which session a `Hello` orphans it from).
    sent: Instant,
    /// Start of the current idle stretch: the send, restarted whenever the device streams a
    /// chunk for any in-flight command (it serializes, so the ones queued behind a busy
    /// command are progressing too). [`PENDING_IDLE`] past this, the command is written off.
    idle_since: Instant,
}

impl PendingCmd {
    fn new(cmd_id: u16, line: String, now: Instant) -> Self {
        PendingCmd {
            cmd_id,
            line,
            next_chunk: 0,
            received: 0,
            sent: now,
            idle_since: now,
        }
    }
}

impl App {
    fn new(port_name: String) -> Self {
        App {
            port_name,
            sp: None,
            dec: FrameDecoder::new(),
            logs: VecDeque::new(),
            events: VecDeque::new(),
            responses: VecDeque::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_idx: None,
            focus: Pane::Shell,
            zoom: false,
            paused: false,
            scroll: [0; 3],
            cmd_id: 1,
            req_id: 1,
            seq: 0,
            pending: VecDeque::new(),
            resp_partial: false,
            pending_req: None,
            hint: String::new(),
            warned_mismatch: false,
            last_seq: None,
            decode_failures: 0,
            seq_gaps: 0,
            payload_errors: 0,
            mismatch: None,
            framing: FramingWatch::default(),
            last_open_error: None,
            last_open_attempt: Instant::now() - Duration::from_secs(10),
            boot_wait: None,
            hint_rows_reserved: 0,
            quit: false,
        }
    }

    fn connected(&self) -> bool {
        self.sp.is_some()
    }
}

fn push_cap<T>(buf: &mut VecDeque<T>, item: T) {
    if buf.len() >= CAP {
        buf.pop_front();
    }
    buf.push_back(item);
}

/// Append to Device Logs. While paused (F5), bump the scroll offset so the visible window
/// stays anchored: capture continues, only the view is frozen.
fn push_log(app: &mut App, item: (String, String, Color)) {
    push_cap(&mut app.logs, item);
    if app.paused {
        app.scroll[0] = app.scroll[0].saturating_add(1);
    }
}

/// Append to Device Events: same paused-anchor rule as [`push_log`].
fn push_event(app: &mut App, item: String) {
    push_cap(&mut app.events, item);
    if app.paused {
        app.scroll[1] = app.scroll[1].saturating_add(1);
    }
}

/// Close the oldest in-flight command without (the rest of) its response, leaving a visible
/// marker in the scrollback; `why` is an optional ` (reason)` suffix. A command dropped
/// silently is exactly how the shell used to look dead.
fn abandon_front(app: &mut App, why: &str) {
    let Some(p) = app.pending.pop_front() else {
        return;
    };
    app.resp_partial = false; // whatever it streamed so far ends here
    let what = if p.next_chunk == 0 {
        "no response to"
    } else {
        "incomplete response to"
    };
    push_cap(&mut app.responses, format!("[tower] {what} `{}`{why}", p.line));
}

/// Close every in-flight command (see [`abandon_front`]).
fn abandon_all(app: &mut App, why: &str) {
    while !app.pending.is_empty() {
        abandon_front(app, why);
    }
}

/// Write off in-flight commands that have gone [`PENDING_IDLE`] with no response activity.
/// Commands are queued in send order and activity restarts every clock at once, so the queue
/// stays ordered by `idle_since` and expiry only ever peels from the front.
fn expire_pending(app: &mut App, now: Instant) {
    while app
        .pending
        .front()
        .is_some_and(|p| now.saturating_duration_since(p.idle_since) >= PENDING_IDLE)
    {
        abandon_front(app, " (timed out)");
    }
}

/// The port failed: drop the handle (the next tick reconnects) and close every in-flight
/// command and completion: their remaining frames can never arrive on a new connection.
fn drop_link(app: &mut App) {
    app.sp = None;
    abandon_all(app, " (link lost)");
    app.pending_req = None;
}

/// Stream a response chunk's text into the shell scrollback line by line, continuing the
/// in-flight partial line in place when the previous chunk ended mid-line. A trailing piece
/// without a newline stays open (`resp_partial`) for the next chunk to extend.
fn append_response_text(app: &mut App, text: &str) {
    let mut cur = if app.resp_partial {
        app.responses.pop_back().unwrap_or_default()
    } else {
        String::new()
    };
    app.resp_partial = false;
    let mut pieces = text.split('\n').peekable();
    while let Some(piece) = pieces.next() {
        cur.push_str(piece.trim_end_matches('\r'));
        if pieces.peek().is_some() {
            push_cap(&mut app.responses, core::mem::take(&mut cur));
        } else if !cur.is_empty() {
            push_cap(&mut app.responses, core::mem::take(&mut cur));
            app.resp_partial = true;
        }
    }
}

pub fn run(port: String, reset: bool) -> Result<()> {
    // The FIRST open is fatal (same contract as the streaming commands): a bad `--device`
    // (typo, EBUSY, EACCES) must exit 1 with the real OS error, not silently spin in the
    // reconnect loop behind an empty four-pane UI. Open BEFORE ratatui grabs the terminal so
    // the error prints normally. `--reset` is consumed here, on the initial attach; the
    // reconnect path never re-resets (a flaky link mustn't become a reboot loop).
    let sp = crate::port::open_console_responsive(&port, reset)?;
    let mut app = App::new(port);
    app.sp = Some(sp);
    if reset {
        app.boot_wait = Some(Instant::now());
    }
    let mut terminal = ratatui::init(); // raw mode + alt screen + panic-restore hook
    let res = run_loop(&mut terminal, app);
    ratatui::restore();
    res
}

fn run_loop(terminal: &mut DefaultTerminal, mut app: App) -> Result<()> {
    while !app.quit {
        ensure_connected(&mut app);
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(Duration::from_millis(33))?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            handle_key(&mut app, key.code, key.modifiers);
        }
        drain_serial(&mut app);
        expire_pending(&mut app, Instant::now());
    }
    Ok(())
}

fn ensure_connected(app: &mut App) {
    if app.sp.is_some() {
        return;
    }
    if app.last_open_attempt.elapsed() < Duration::from_millis(800) {
        return;
    }
    app.last_open_attempt = Instant::now();
    // Reconnect without re-resetting (never re-reset on a flaky link). Unlike the fatal first
    // open in `run`, a later failure isn't fatal (we keep retrying), but we no longer drop it
    // on the floor: stash it so the header can show *why* (EBUSY, EACCES, ENOENT, …) next to
    // the reconnect indicator instead of a bare "reconnecting…" with a green-looking UI.
    match crate::port::open_console_responsive(&app.port_name, false) {
        Ok(sp) => {
            app.sp = Some(sp);
            app.last_open_error = None;
            app.dec.reset();
            // Nothing from the previous connection can still be answered (`drop_link` already
            // closed it all when the port failed; this is belt-and-braces). `cmd_id`
            // deliberately keeps counting across reconnects rather than restarting at 1, so a
            // late answer to an old command can never be mistaken for a new one's.
            abandon_all(app, " (link lost)");
            app.resp_partial = false;
            app.pending_req = None;
        }
        Err(e) => app.last_open_error = Some(format!("{e:#}")),
    }
}

fn drain_serial(app: &mut App) {
    let mut buf = [0u8; 512];
    let Some(mut sp) = app.sp.take() else {
        return;
    };
    // `read_some` folds the idle read timeout and a signal-interrupted poll (a terminal resize
    // delivers SIGWINCH) into "nothing yet"; only a real port failure drops the link.
    match read_some(&mut *sp, &mut buf) {
        Ok(n) => {
            app.sp = Some(sp);
            for &b in &buf[..n] {
                // Copy the deframed bytes out so the `app.dec` borrow ends before we
                // borrow `app` mutably in `handle_frame`.
                if let Some(frame) = app.dec.push(b).map(|inner| inner.to_vec()) {
                    handle_frame(app, &frame);
                }
            }
        }
        Err(_) => drop_link(app), // `sp` is dropped here → reconnect on the next tick
    }
}

fn handle_frame(app: &mut App, inner: &[u8]) {
    // Capture the decode error instead of dropping the frame silently: a wall of these behind a
    // green "connected" dot is exactly the stale-peer symptom that used to show four empty
    // panes. Count them (shown in the header) and, on the first frame carrying another schema
    // tag, shout a red line into the log pane naming the device and the remedy.
    let decoded = decode_frame(inner);
    if app.framing.observe(&decoded) {
        push_cap(
            &mut app.logs,
            (format!("✖ {}", FramingWatch::MESSAGE), String::new(), Color::Red),
        );
    }
    let (mt, seq, payload) = match decoded {
        Ok(t) => t,
        Err(e) => {
            app.decode_failures += 1;
            if let tower_protocol::Error::SchemaMismatch { got } = e {
                let seen = Mismatch::from_frame(got, inner);
                // Name the device as soon as its Hello does (the first frame may not be one).
                let named_now =
                    seen.device.is_some() && app.mismatch.as_ref().is_some_and(|m| m.device.is_none());
                Mismatch::note(&mut app.mismatch, seen);
                if (!app.warned_mismatch || named_now)
                    && let Some(m) = &app.mismatch
                {
                    push_cap(
                        &mut app.logs,
                        (
                            format!(
                                "✖ SCHEMA MISMATCH: {} speaks console schema {:#06x}, this \
                                 `tower` {:#06x}: built from different checkouts, so none of its \
                                 frames is interpreted. {}.",
                                m.device_label(),
                                m.tag,
                                tower_protocol::CONSOLE_TAG,
                                m.remedy()
                            ),
                            String::new(),
                            Color::Red,
                        ),
                    );
                    app.warned_mismatch = true;
                }
            }
            return;
        }
    };

    // Sequence-gap accounting (mirrors the plain-CLI renderer): a `Hello` marks a new device
    // session and re-baselines `seq` (the console re-emits `Hello` on every USB plug-in), so a
    // fresh 0 isn't a spurious gap. Done before the pause check because frames still arrive
    // while paused; only their *display* is frozen.
    if matches!(mt, MsgType::Hello) {
        app.boot_wait = None;
        app.last_seq = None;
        // A new device session: commands sent to the old one died with it (the shell
        // restarted), so close them now instead of letting them sit out the idle timeout.
        // Commands younger than the Hello race window may already belong to the new session.
        let now = Instant::now();
        while app
            .pending
            .front()
            .is_some_and(|p| now.saturating_duration_since(p.sent) > HELLO_RACE)
        {
            abandon_front(app, " (device restarted)");
        }
    }
    if let Some(prev) = app.last_seq {
        // Wrapping u16 distance minus one: nonzero means at least one frame went missing.
        if seq.wrapping_sub(prev).wrapping_sub(1) != 0 {
            app.seq_gaps += 1;
        }
    }
    app.last_seq = Some(seq);

    match mt {
        // A Hello with our tag: the device runs a firmware from this checkout (again), so any
        // earlier mismatch is over (a later one gets its own warning).
        MsgType::Hello => match postcard::from_bytes::<Hello>(payload) {
            Ok(_) => {
                app.mismatch = None;
                app.warned_mismatch = false;
            }
            Err(_) => app.payload_errors += 1,
        },
        MsgType::Log => match postcard::from_bytes::<Log>(payload) {
            // Reuse the CLI's shared layout (`[uptime] LEVEL module: message`) so both
            // frontends render a Log identically; the TUI prepends its own clock + color.
            Ok(l) => {
                // Split so only `clock [uptime] LEVEL` carries the severity tint; the
                // `module: message` text stays default-colored (readable at every level).
                let prefix = format!(
                    "{} {} {}",
                    now(),
                    crate::render::uptime_prefix(l.uptime_us),
                    crate::render::level_label(l.level)
                );
                let rest = format!(" {}: {}", l.module, l.message);
                push_log(app, (prefix, rest, level_color(l.level)));
            }
            Err(_) => app.payload_errors += 1,
        },
        MsgType::Print => match postcard::from_bytes::<Print>(payload) {
            Ok(p) => push_log(app, (String::new(), p.text.trim_end().to_string(), Color::Reset)),
            Err(_) => app.payload_errors += 1,
        },
        MsgType::Event => match postcard::from_bytes::<EvMsg>(payload) {
            Ok(e) => push_event(
                app,
                format!("{} {}  {}", now(), e.name, crate::render::event_fields(&e)),
            ),
            Err(_) => app.payload_errors += 1,
        },
        MsgType::Dropped => match postcard::from_bytes::<Dropped>(payload) {
            Ok(d) => push_log(
                app,
                (
                    format!("⚠ {} log frame(s) dropped", d.count),
                    String::new(),
                    Color::Yellow,
                ),
            ),
            Err(_) => app.payload_errors += 1,
        },
        // Chunks (`chunk`/`last`) stream into the scrollback line by line as they arrive; a
        // chunk boundary mid-line continues the open line in place (see `append_response_text`).
        MsgType::ShellResponse => match postcard::from_bytes::<ShellResponse>(payload) {
            Ok(r) => on_shell_response(app, &r),
            Err(_) => app.payload_errors += 1,
        },
        MsgType::ShellCompletions => match postcard::from_bytes::<ShellCompletions>(payload) {
            Ok(c) if Some(c.req_id) == app.pending_req => {
                apply_completion(app, &c);
                app.pending_req = None;
            }
            Ok(_) => {} // a completion for a stale req_id: ignore
            Err(_) => app.payload_errors += 1,
        },
        _ => {}
    }
}

/// Route one `ShellResponse` chunk to its in-flight command, streaming its text into the
/// scrollback as it arrives (no wait for `last`).
///
/// Matched by `cmd_id` ANYWHERE in the queue, not just at the front. The device's shell
/// serializes, so an answer for entry `i` proves every older entry is finished: its command was
/// lost on the way in, its `last` chunk was CRC-dropped, or the device reset mid-command.
/// Matching only the front let one such stale entry swallow every later response: the shell
/// went permanently mute. Now the stale ones are closed with a marker and the stream carries
/// on. An unknown `cmd_id` (a late chunk of an abandoned command) is ignored.
fn on_shell_response(app: &mut App, r: &ShellResponse) {
    let Some(idx) = app.pending.iter().position(|p| p.cmd_id == r.cmd_id) else {
        return;
    };
    for _ in 0..idx {
        abandon_front(app, "");
    }
    // The device is demonstrably working through the queue: restart every waiter's idle clock
    // (the commands behind this one can't be answered before it finishes).
    let now = Instant::now();
    for p in &mut app.pending {
        p.idle_since = now;
    }
    let Some(front) = app.pending.front_mut() else {
        return;
    };
    let expected = front.next_chunk;
    front.next_chunk = r.chunk.wrapping_add(1);
    front.received += r.text.len();
    let over_cap = front.received > MAX_RESPONSE_BYTES;
    // A `chunk` gap means a middle chunk was CRC-dropped (the decoder silently discards
    // corrupt frames): flag the truncation in place.
    if r.chunk != expected {
        app.resp_partial = false;
        push_cap(
            &mut app.responses,
            format!(
                "[tower] response chunk dropped (expected #{}, got #{}); output truncated",
                expected, r.chunk
            ),
        );
    }
    if over_cap {
        // A device streaming one response without end: stop showing it (bounded memory: the
        // open partial line can't grow forever either) and let the queue move on. Its
        // remaining chunks match nothing any more and are ignored.
        if let Some(p) = app.pending.pop_front() {
            app.resp_partial = false;
            push_cap(
                &mut app.responses,
                format!(
                    "[tower] response to `{}` exceeded {} KiB; truncated",
                    p.line,
                    MAX_RESPONSE_BYTES / 1024
                ),
            );
        }
        return;
    }
    append_response_text(app, r.text);
    if r.last {
        app.resp_partial = false;
        if r.result != 0 {
            push_cap(&mut app.responses, crate::session::result_tag(r.result));
        }
        app.pending.pop_front();
    }
}

fn apply_completion(app: &mut App, c: &ShellCompletions) {
    // `token_start` is device-supplied: the shared validation (`tower shell` uses it too) keeps
    // it on a char boundary and not past the cursor before it reaches `replace_range`, which
    // would panic. `app.cursor` is always kept on a boundary by the key handlers.
    let Some(start) = completion_start(&app.input, app.cursor, c.token_start) else {
        return;
    };
    if c.candidates.len() == 1 {
        let cand = &c.candidates[0];
        let sep = match cand.kind {
            tower_protocol::msg::CandidateKind::Menu => "/",
            tower_protocol::msg::CandidateKind::Command => " ",
            tower_protocol::msg::CandidateKind::Arg => "=",
            tower_protocol::msg::CandidateKind::Value => "",
        };
        let repl = format!("{}{sep}", cand.text);
        app.input.replace_range(start..app.cursor, &repl);
        app.cursor = start + repl.len();
        app.hint.clear();
    } else if c.candidates.is_empty() {
        app.hint = "(no completions)".into();
    } else {
        if !c.common_prefix.is_empty() {
            app.input.replace_range(start..app.cursor, c.common_prefix);
            app.cursor = start + c.common_prefix.len();
        }
        let list: Vec<String> = c.candidates.iter().map(|cd| cd.text.to_string()).collect();
        app.hint = list.join("  ");
        if c.more {
            app.hint.push_str("  …");
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    // Ctrl+C always quits (any focus). F10 works too, but many terminals swallow F10, so a
    // universally-available quit key matters, and without this, Ctrl+C used to fall through
    // to `handle_command_key` and insert a literal 'c'.
    if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }
    // Global function keys (any focus).
    match code {
        KeyCode::F(10) => {
            app.quit = true;
            return;
        }
        KeyCode::F(3) => {
            app.zoom = !app.zoom;
            return;
        }
        KeyCode::F(5) => {
            // Pause freezes the streaming VIEWPORTS only: frames keep arriving and keep
            // being captured (the paused-anchor in push_log/push_event holds the view
            // still). Unpausing jumps back to the live tail.
            app.paused = !app.paused;
            if !app.paused {
                app.scroll[0] = 0;
                app.scroll[1] = 0;
            }
            return;
        }
        KeyCode::F(8) => {
            if mods.contains(KeyModifiers::SHIFT) {
                // Shift-F8: clear every text area at once (and release the hint rows).
                app.logs.clear();
                app.events.clear();
                app.responses.clear();
                app.scroll = [0; 3];
                app.hint_rows_reserved = 0;
            } else {
                match app.focus {
                    Pane::Logs => app.logs.clear(),
                    Pane::Events => app.events.clear(),
                    Pane::Shell => app.responses.clear(),
                }
            }
            return;
        }
        // F1 walks the panes clockwise; Shift-F1 counter-clockwise. (Some
        // terminals report Shift-F1 as F13; accept both.)
        KeyCode::F(1) if !mods.contains(KeyModifiers::SHIFT) => {
            app.focus = match app.focus {
                Pane::Shell => Pane::Events,
                Pane::Events => Pane::Logs,
                Pane::Logs => Pane::Shell,
            };
            return;
        }
        KeyCode::F(1) | KeyCode::F(13) => {
            app.focus = match app.focus {
                Pane::Shell => Pane::Logs,
                Pane::Logs => Pane::Events,
                Pane::Events => Pane::Shell,
            };
            return;
        }
        _ => {}
    }

    match app.focus {
        Pane::Shell => handle_command_key(app, code, mods),
        pane => handle_scroll_key(app, pane, code),
    }
}

fn handle_scroll_key(app: &mut App, pane: Pane, code: KeyCode) {
    let idx = match pane {
        Pane::Logs => 0,
        Pane::Events => 1,
        Pane::Shell => return, // shell scrolling is handled in handle_command_key
    };
    match code {
        KeyCode::PageUp => app.scroll[idx] = app.scroll[idx].saturating_add(10),
        KeyCode::PageDown => app.scroll[idx] = app.scroll[idx].saturating_sub(10),
        KeyCode::Up => app.scroll[idx] = app.scroll[idx].saturating_add(1),
        KeyCode::Down => app.scroll[idx] = app.scroll[idx].saturating_sub(1),
        // In a non-input pane, bare 'q' quits (a common TUI convention); the command pane
        // needs the character, so it's handled here rather than globally.
        KeyCode::Char('q') => app.quit = true,
        _ => {}
    }
}

fn handle_command_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    // AltGr is text, not a command. Windows reports it as Ctrl+Alt, so on CZ/DE/PL/… layouts
    // `@ { } [ ] \ | ~ €` all arrive with both modifiers set; dropping them made those
    // characters untypeable. AltGr yields a printable char that is never a plain ASCII
    // letter/digit (a real Ctrl+Alt+<key> chord reports the bare key), which tells the two apart.
    let altgr_text = mods.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && matches!(code, KeyCode::Char(c) if !c.is_control() && !c.is_ascii_alphanumeric());
    // Other Control/Alt-modified keys are commands, not text: handle the readline-style editing
    // bindings and drop the rest, so e.g. a stray Ctrl+<x> can't insert a literal control
    // character into the command line. (Plain Shift is fine; it just gives capitals.)
    if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && !altgr_text {
        match code {
            KeyCode::Char('u') => kill_to_start(app), // Ctrl+U: clear to line start
            KeyCode::Char('w') => kill_word(app),     // Ctrl+W: delete previous word
            KeyCode::Char('a') => app.cursor = 0,     // Ctrl+A: line start
            KeyCode::Char('e') => app.cursor = app.input.len(), // Ctrl+E: line end
            _ => {}
        }
        return;
    }
    match code {
        // Scroll the shell scrollback without leaving the prompt (arrows stay for
        // history/cursor; paging is free).
        KeyCode::PageUp => app.scroll[2] = app.scroll[2].saturating_add(10),
        KeyCode::PageDown => app.scroll[2] = app.scroll[2].saturating_sub(10),
        KeyCode::Char(ch) => {
            app.input.insert(app.cursor, ch);
            app.cursor += ch.len_utf8();
            app.hint.clear();
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let prev = app.input[..app.cursor]
                    .chars()
                    .next_back()
                    .map(|c| c.len_utf8())
                    .unwrap_or(1);
                app.cursor -= prev;
                app.input.remove(app.cursor);
            }
        }
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(prev_char(&app.input, app.cursor)),
        KeyCode::Right => {
            if app.cursor < app.input.len() {
                app.cursor += next_char(&app.input, app.cursor);
            }
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.len(),
        KeyCode::Up => history_prev(app),
        KeyCode::Down => history_next(app),
        KeyCode::Tab => send_complete(app),
        KeyCode::Enter => send_command(app),
        _ => {}
    }
}

/// Ctrl+U: delete everything left of the cursor.
fn kill_to_start(app: &mut App) {
    app.input.replace_range(0..app.cursor, "");
    app.cursor = 0;
    app.hint.clear();
}

/// Ctrl+W: delete the whitespace-delimited word before the cursor (trailing spaces first,
/// then the word). Operates on char boundaries so multibyte input can't panic.
fn kill_word(app: &mut App) {
    let mut start = app.cursor;
    // Skip any spaces immediately left of the cursor.
    while start > 0 {
        let p = prev_char(&app.input, start);
        if app.input[start - p..start].chars().all(char::is_whitespace) {
            start -= p;
        } else {
            break;
        }
    }
    // Then skip the word itself.
    while start > 0 {
        let p = prev_char(&app.input, start);
        if app.input[start - p..start].chars().any(char::is_whitespace) {
            break;
        }
        start -= p;
    }
    app.input.replace_range(start..app.cursor, "");
    app.cursor = start;
    app.hint.clear();
}

fn prev_char(s: &str, cur: usize) -> usize {
    s[..cur].chars().next_back().map(|c| c.len_utf8()).unwrap_or(0)
}
fn next_char(s: &str, cur: usize) -> usize {
    s[cur..].chars().next().map(|c| c.len_utf8()).unwrap_or(0)
}

fn history_prev(app: &mut App) {
    if app.history.is_empty() {
        return;
    }
    let i = match app.hist_idx {
        None => app.history.len() - 1,
        Some(0) => 0,
        Some(i) => i - 1,
    };
    app.hist_idx = Some(i);
    app.input = app.history[i].clone();
    app.cursor = app.input.len();
}

fn history_next(app: &mut App) {
    match app.hist_idx {
        Some(i) if i + 1 < app.history.len() => {
            app.hist_idx = Some(i + 1);
            app.input = app.history[i + 1].clone();
            app.cursor = app.input.len();
        }
        _ => {
            app.hist_idx = None;
            app.input.clear();
            app.cursor = 0;
        }
    }
}

fn send_complete(app: &mut App) {
    let req_id = app.req_id;
    app.req_id = app.req_id.wrapping_add(1);
    let line = app.input.clone();
    let cursor = app.cursor as u16;
    if send_frame(
        app,
        MsgType::ShellComplete,
        &ShellComplete {
            req_id,
            line: &line,
            cursor,
        },
    ) {
        app.pending_req = Some(req_id);
    }
}

fn send_command(app: &mut App) {
    let line = app.input.trim().to_string();
    if line.is_empty() {
        return;
    }
    let cmd_id = app.cmd_id;
    // Try to send FIRST; only commit UI state (echo, arm reassembly, history, clear the input)
    // if the frame actually went out. A too-long line fails to encode and a dropped link fails
    // to write; in either case keep the typed line so the user can edit/retry, don't arm a
    // reassembly for a command that never left, and explain why in the hint.
    if !send_frame(app, MsgType::ShellCommand, &ShellCommand { cmd_id, line: &line }) {
        app.hint = if app.connected() {
            format!("line too long (max ~{} bytes)", tower_protocol::MAX_FRAME - 12)
        } else {
            "send failed: link lost; reconnecting…".to_string()
        };
        return;
    }
    app.cmd_id = app.cmd_id.wrapping_add(1);
    // Queue (don't replace): a slow response must not block the next command. Bounded so a
    // dead device can't grow it without limit; the evicted oldest gets its marker, which
    // lands just above this command's echo.
    if app.pending.len() >= 8 {
        abandon_front(app, " (too many in flight)");
    }
    // The echo closes any in-flight partial line; a late response continues on a fresh line
    // under this echo, right above the prompt, in arrival order.
    app.resp_partial = false;
    push_cap(&mut app.responses, format!("> {line}"));
    app.pending
        .push_back(PendingCmd::new(cmd_id, line.clone(), Instant::now()));
    if app.history.last().map(|h| h.as_str()) != Some(line.as_str()) {
        app.history.push(line);
    }
    app.hist_idx = None;
    app.input.clear();
    app.cursor = 0;
    app.hint.clear();
}

fn send_frame<T: serde::Serialize>(app: &mut App, mt: MsgType, payload: &T) -> bool {
    let Some(sp) = app.sp.as_mut() else {
        return false;
    };
    let mut buf = [0u8; tower_protocol::MAX_WIRE];
    let Ok(n) = encode_frame(mt, app.seq, payload, &mut buf) else {
        return false;
    };
    app.seq = app.seq.wrapping_add(1);
    if sp.write_all(&buf[..n]).and_then(|_| sp.flush()).is_err() {
        drop_link(app); // trigger reconnect
        return false;
    }
    true
}

// ---- rendering ----

/// The header's decode/seq/payload diagnostics string, or `None` when everything's clean.
/// e.g. `"✖ schema 0x1234 ≠ 0xfd85 · 37 bad frames · 4 seq gaps · 2 payload errs"`.
fn header_diagnostics(app: &App) -> Option<String> {
    if app.decode_failures == 0 && app.seq_gaps == 0 && app.payload_errors == 0 && app.mismatch.is_none() {
        return None;
    }
    let plural = |n: u64| if n == 1 { "" } else { "s" };
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = &app.mismatch {
        parts.push(format!(
            "schema {:#06x} ≠ {:#06x}",
            m.tag,
            tower_protocol::CONSOLE_TAG
        ));
    }
    if app.decode_failures > 0 {
        let mut d = format!("{} bad frame{}", app.decode_failures, plural(app.decode_failures));
        if app.framing.reported() && app.mismatch.is_none() {
            d.push_str(" (framing?)");
        }
        parts.push(d);
    }
    if app.seq_gaps > 0 {
        parts.push(format!("{} seq gap{}", app.seq_gaps, plural(app.seq_gaps)));
    }
    if app.payload_errors > 0 {
        parts.push(format!(
            "{} payload err{}",
            app.payload_errors,
            plural(app.payload_errors)
        ));
    }
    Some(format!("  ✖ {}", parts.join(" · ")))
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    let bar = Style::new().bg(Color::Gray).fg(Color::Black);

    let rows =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).split(area);

    // Header. Built as spans so the connection dot, a reconnect error, and the decode/seq
    // diagnostics can each carry their own color over the gray bar.
    let on_bar = |fg: Color| Style::new().bg(Color::Gray).fg(fg);
    let mut spans = vec![Span::styled(
        format!(
            " HARDWARIO TOWER Console v{} ({}) ",
            env!("CARGO_PKG_VERSION"),
            app.port_name
        ),
        bar,
    )];
    if app.connected() {
        spans.push(Span::styled("●", on_bar(Color::Green)));
    } else {
        spans.push(Span::styled("○ reconnecting…", bar));
        // Surface *why* the (re)connect is failing instead of an endless bare "reconnecting…".
        if let Some(err) = &app.last_open_error {
            spans.push(Span::styled(
                format!("  {}: {err}", app.port_name),
                on_bar(Color::Red),
            ));
        }
    }
    // Post-reset boot wait: animated while the device is (possibly slowly) booting, loud once
    // it exceeds the 8 s worst case the CLI tolerates (a fallback EEPROM compaction can
    // legitimately hold the chip ~5 s; see firmware/docs/storage.md).
    if let Some(t0) = app.boot_wait {
        let el = t0.elapsed();
        if el < Duration::from_secs(8) {
            const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let c = SPIN[(el.as_millis() / 100) as usize % SPIN.len()];
            spans.push(Span::styled(
                format!("  {c} booting… {:.1}s", el.as_secs_f32()),
                on_bar(Color::Yellow),
            ));
        } else {
            spans.push(Span::styled(
                format!(
                    "  ✗ no boot Hello after {:.0}s; wedged or console-less?",
                    el.as_secs_f32()
                ),
                on_bar(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
    }
    // Decode-failure / seq-gap / payload-error tally: the tag-mismatch smoking gun, kept in
    // view so a live-but-garbled link can't masquerade as healthy.
    if let Some(diag) = header_diagnostics(app) {
        spans.push(Span::styled(
            diag,
            on_bar(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(bar), rows[0]);

    if app.zoom {
        render_zoom(f, app, rows[1]);
    } else {
        render_split(f, app, rows[1]);
    }

    // Footer. Toggleable items render as chips: gray when off, yellow with white bold text
    // when active (fg-yellow-on-gray was unreadable).
    let chip = |label: &str, on: bool| {
        if on {
            Span::styled(
                format!(" {label} "),
                Style::new()
                    .fg(Color::White)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                format!(" {label} "),
                Style::new().fg(Color::Black).bg(Color::Gray),
            )
        }
    };
    let footer = Line::from(vec![
        Span::raw(" <[Shift-]F1> Focus  "),
        chip("<F3> Zoom", app.zoom),
        Span::raw("  "),
        chip("<F5> Pause", app.paused),
        Span::raw("  <F8> Clear  <Shift-F8> Clear All  <PgUp/PgDn> Scroll  <F10>/<^C> Quit"),
    ]);
    let footer_area = rows[2];
    f.render_widget(Paragraph::new(footer).style(bar), footer_area);
    // Clock, right-aligned.
    let clock = now_date();
    let cw = clock.len() as u16;
    if footer_area.width > cw + 1 {
        let clock_rect = Rect::new(footer_area.x + footer_area.width - cw - 1, footer_area.y, cw, 1);
        f.render_widget(Paragraph::new(clock).style(bar), clock_rect);
    }
}

fn render_split(f: &mut ratatui::Frame, app: &mut App, body: Rect) {
    let cols = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(body);
    let left = Layout::vertical([Constraint::Percentage(25), Constraint::Min(0)]).split(cols[0]);

    let plain = |s: &String| Line::raw(s.clone());
    render_text_pane(
        f,
        left[0],
        "Device Events",
        app.focus == Pane::Events,
        &app.events,
        app.scroll[1],
        plain,
    );
    render_shell(f, left[1], app);
    render_text_pane(
        f,
        cols[1],
        "Device Logs",
        app.focus == Pane::Logs,
        &app.logs,
        app.scroll[0],
        log_line_spans,
    );
}

fn render_zoom(f: &mut ratatui::Frame, app: &mut App, body: Rect) {
    let plain = |s: &String| Line::raw(s.clone());
    match app.focus {
        Pane::Logs => render_text_pane(f, body, "", false, &app.logs, app.scroll[0], log_line_spans),
        Pane::Events => render_text_pane(f, body, "", false, &app.events, app.scroll[1], plain),
        Pane::Shell => render_shell(f, body, app),
    }
}

/// A device-log line: only the `clock [uptime] LEVEL` prefix carries the severity tint.
fn log_line_spans((prefix, rest, c): &(String, String, Color)) -> Line<'static> {
    Line::from(vec![
        Span::styled(prefix.clone(), Style::new().fg(*c)),
        Span::raw(rest.clone()),
    ])
}

/// How many terminal rows a line of display width `w` occupies once wrapped to `width`
/// columns (matching `Wrap { trim: false }`): `ceil(w / width)`, at least one row.
fn wrapped_rows(w: usize, width: usize) -> usize {
    if width == 0 { 1 } else { w.div_ceil(width).max(1) }
}

/// Render a bottom-anchored scrollback pane.
///
/// The window is anchored by **visual rows, not item count**: with `Wrap { trim: false }` a
/// long item spans several rows, so counting items would push the newest lines below the
/// clip and hide them (they were only reachable by scrolling *up*). Instead we walk items
/// from the back, summing each one's wrapped row count, until we've gathered `inner_h`
/// rows (plus the user's `scrollback`), then render that slice with a Paragraph `scroll`
/// offset that trims the partial item at the top so the newest line sits exactly on the
/// bottom edge. Only the visible items are materialized, not the whole `CAP`-deep deque.
fn render_text_pane<T>(
    f: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    focused: bool,
    items: &VecDeque<T>,
    scrollback: usize,
    to_line: impl Fn(&T) -> Line<'static>,
) {
    let block = if title.is_empty() {
        Block::default()
    } else {
        let style = if focused {
            Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        Block::bordered().title(title).border_style(style)
    };
    let bordered = !title.is_empty();
    let inner_h = area.height.saturating_sub(if bordered { 2 } else { 0 }) as usize;
    // Wrap width = inner content width (both borders removed when bordered).
    let inner_w = area.width.saturating_sub(if bordered { 2 } else { 0 }) as usize;
    if inner_h == 0 {
        f.render_widget(Paragraph::new(Vec::<Line>::new()).block(block), area);
        return;
    }

    // Materialize each candidate line lazily from the back, accumulating its wrapped row
    // count until we've covered the visible window plus the requested scroll-up. `want` can
    // exceed what exists (short buffer); that's fine, we just render everything.
    let want = inner_h + scrollback;
    let mut collected: Vec<Line> = Vec::new();
    let mut rows_by_line: Vec<usize> = Vec::new();
    let mut total_rows = 0usize;
    for item in items.iter().rev() {
        let line = to_line(item);
        let rows = wrapped_rows(line.width(), inner_w);
        total_rows += rows;
        rows_by_line.push(rows);
        collected.push(line);
        if total_rows >= want {
            break;
        }
    }
    collected.reverse();
    rows_by_line.reverse();

    // Bottom of the collected block sits at row `total_rows`; the visible window's top row
    // (from the top of `collected`) is `total_rows - inner_h - scrollback`, clamped to 0.
    let top_row = total_rows.saturating_sub(inner_h + scrollback);
    // Scrolling by whole rows can land mid-item; Paragraph's `scroll` skips exactly that many
    // rendered rows, so we can pass `top_row` directly (it trims the partial top item).
    let p = Paragraph::new(collected)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((top_row as u16, 0));
    f.render_widget(p, area);
}

fn render_shell(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Pane::Shell;
    let style = if focused {
        Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let block = Block::bordered().title("Interactive Shell").border_style(style);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // Bottom-up allocation, SSH-style: the prompt is always the last row (plus hint rows
    // right under it); everything above is scrollback. Hint rows are a sticky HIGH-WATER
    // mark: once TAB completion needs them they stay allocated (blank when no hint), so
    // the prompt doesn't jump up and down as hints come and go. <Shift-F8> resets it.
    let needed: u16 = if app.hint.is_empty() {
        0
    } else {
        let w = inner.width.max(1) as usize;
        (app.hint.chars().count().div_ceil(w) as u16).min(3)
    };
    app.hint_rows_reserved = app.hint_rows_reserved.max(needed);
    let hint_rows = app.hint_rows_reserved.min(inner.height.saturating_sub(1));
    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(hint_rows),
    ])
    .split(inner);

    // Scrollback: the command echoes ("> …") and responses, syntax-highlighted, borderless
    // (the surrounding block is already drawn).
    render_text_pane(
        f,
        rows[0],
        "",
        false,
        &app.responses,
        app.scroll[2],
        |s: &String| crate::highlight::response(s),
    );

    // Prompt line: "> " + the input (highlighted), or the placeholder on an empty line.
    let prompt = Span::styled(
        "> ".to_string(),
        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
    );
    let line = if app.input.is_empty() {
        Line::from(vec![
            prompt,
            Span::styled(
                "Enter your command here… (starts with \"/\"; supports <TAB> completions)",
                Style::new().fg(Color::DarkGray),
            ),
        ])
    } else {
        let mut spans = vec![prompt];
        spans.extend(crate::highlight::command(&app.input));
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(line), rows[1]);

    if hint_rows > 0 {
        f.render_widget(
            Paragraph::new(app.hint.clone())
                .style(Style::new().fg(Color::DarkGray))
                .wrap(Wrap { trim: false }),
            rows[2],
        );
    }

    if focused {
        let cx = rows[1].x + 2 + app.input[..app.cursor].chars().count() as u16;
        f.set_cursor_position((cx.min(rows[1].x + rows[1].width.saturating_sub(1)), rows[1].y));
    }
}

// ---- shell syntax highlighting --------------------------------------------
// One color per syntactic class (shared by the input line and response lines):
// tree paths cyan (separators dim), bare command words yellow, keys magenta,
// values green, punctuation dark gray.
/// The ratatui color a Log line is tinted with, by severity. (The label text itself comes
/// from the shared `render::level_label` via `render::log_line`, so it can't drift.)
fn level_color(l: Level) -> Color {
    match l {
        Level::Error => Color::Red,
        Level::Warn => Color::Yellow,
        Level::Info => Color::Green,
        Level::Debug => Color::Cyan,
        Level::Trace => Color::DarkGray,
    }
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}
fn now_date() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

// ===========================================================================
// Tests
//
// The TUI can't be driven interactively here (no TTY), but its two pure halves *can* be:
// `ui()` renders into ratatui's `TestBackend` (an in-memory cell grid we can read back as
// text), and `handle_key`/`handle_frame` are ordinary state transitions over `App`. These
// cover the input-handling fixes (C22 char-boundary completion, C24 modifier handling) and
// the frame-reassembly fixes (C18 cmd_id filtering, C19 chunk-gap flagging) without hardware.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tower_protocol::encode_frame;
    use tower_protocol::msg::{Candidate, CandidateKind, ShellResponse};

    fn test_app() -> App {
        App::new("/dev/mock".to_string())
    }

    /// A freshly sent in-flight command `cmd_id` (its line is `/c<cmd_id>`).
    fn pending(cmd_id: u16) -> PendingCmd {
        PendingCmd::new(cmd_id, format!("/c{cmd_id}"), Instant::now())
    }

    /// The deframed inner bytes of a well-formed boot `Hello`.
    fn hello_inner() -> Vec<u8> {
        let payload = postcard::to_allocvec(&Hello {
            firmware_name: "app",
            firmware_version: "test",
            session_id: 2,
        })
        .unwrap();
        valid_inner(MsgType::Hello, 0, &payload)
    }

    /// The console schema tag of a device built from another checkout.
    const FOREIGN: u16 = tower_protocol::CONSOLE_TAG ^ 0x0100;

    /// A deframed inner frame carrying schema `tag`, CRC intact, with a caller-supplied raw
    /// payload: `tag` = ours makes `decode_frame` succeed and exercises the postcard parse (e.g.
    /// an empty payload for a `Log` fails to parse → a payload error, not a frame error); another
    /// tag is what a device built from another checkout puts on the wire (`SchemaMismatch`).
    fn tagged_inner(tag: u16, mt: MsgType, seq: u16, payload: &[u8]) -> Vec<u8> {
        let mut inner = vec![mt as u8];
        inner.extend_from_slice(&tag.to_le_bytes());
        inner.extend_from_slice(&seq.to_le_bytes());
        inner.extend_from_slice(payload);
        let crc = tower_protocol::crc::crc32_ieee(&inner);
        inner.extend_from_slice(&crc.to_le_bytes());
        inner
    }

    /// [`tagged_inner`] with our tag.
    fn valid_inner(mt: MsgType, seq: u16, payload: &[u8]) -> Vec<u8> {
        tagged_inner(tower_protocol::CONSOLE_TAG, mt, seq, payload)
    }

    /// The boot `Hello` of a device built from another checkout (it still names the firmware).
    fn foreign_hello_inner() -> Vec<u8> {
        let payload = postcard::to_allocvec(&Hello {
            firmware_name: "app",
            firmware_version: "v0.0.9",
            session_id: 3,
        })
        .unwrap();
        tagged_inner(FOREIGN, MsgType::Hello, 0, &payload)
    }

    /// The deframed inner bytes of a well-formed `Log` frame at sequence `seq`.
    fn log_inner(seq: u16) -> Vec<u8> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(
            MsgType::Log,
            seq,
            &Log {
                level: Level::Info,
                uptime_us: 0,
                module: "t",
                message: "m",
            },
            &mut buf,
        )
        .unwrap();
        let mut dec = FrameDecoder::new();
        let mut inner = Vec::new();
        for &b in &buf[..n] {
            if let Some(f) = dec.push(b) {
                inner = f.to_vec();
            }
        }
        inner
    }

    /// Render `ui()` into an 80x24 test terminal and return the whole screen as text (rows
    /// joined by newlines), trailing spaces trimmed: enough to assert on visible content.
    fn render_to_text(app: &mut App) -> String {
        render_to_text_sized(app, 80, 24)
    }

    /// Like [`render_to_text`] but at an explicit size; some assertions (the footer hint)
    /// need a wide terminal so the right-aligned clock doesn't overwrite the region.
    fn render_to_text_sized(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    // ---- ui() snapshots for canned states ----

    #[test]
    fn ui_disconnected_shows_reconnecting() {
        let mut app = test_app(); // no sp → disconnected
        let text = render_to_text(&mut app);
        assert!(text.contains("HARDWARIO TOWER Console"));
        assert!(text.contains("reconnecting"));
        assert!(text.contains("/dev/mock"));
        // The footer lists the key bindings.
        assert!(text.contains("<F8> Clear"));
    }

    #[test]
    fn ui_split_shows_all_pane_titles() {
        let mut app = test_app();
        push_cap(
            &mut app.logs,
            ("boot ok".to_string(), String::new(), Color::Green),
        );
        push_cap(&mut app.events, "sensor temp=21".to_string());
        push_cap(&mut app.responses, "> /help".to_string());
        let text = render_to_text(&mut app);
        assert!(text.contains("Device Logs"));
        assert!(text.contains("Device Events"));
        assert!(text.contains("Interactive Shell"));
        assert!(text.contains("boot ok"));
    }

    #[test]
    fn ui_zoom_shows_only_focused_pane() {
        let mut app = test_app();
        app.zoom = true;
        app.focus = Pane::Logs;
        push_cap(
            &mut app.logs,
            ("zoomed log line".to_string(), String::new(), Color::Reset),
        );
        let text = render_to_text(&mut app);
        assert!(text.contains("zoomed log line"));
        // In zoom, the events/responses pane borders aren't drawn.
        assert!(!text.contains("Device Events"));
    }

    #[test]
    fn pause_keeps_capturing_and_anchors_the_view() {
        // F5 freezes the VIEWPORT only: frames keep landing in the buffer, and the scroll
        // offset grows so the visible window stays put; unpausing jumps back to the tail.
        let mut app = test_app();
        app.paused = true;
        let before = app.logs.len();
        handle_frame(&mut app, &log_inner(1));
        assert_eq!(app.logs.len(), before + 1, "capture must continue while paused");
        assert_eq!(app.scroll[0], 1, "view must stay anchored while paused");
        handle_key(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        assert!(!app.paused);
        assert_eq!(app.scroll[0], 0, "unpausing resumes the live tail");
    }

    #[test]
    fn shift_f8_clears_all_panes() {
        let mut app = test_app();
        push_cap(&mut app.logs, ("x".into(), String::new(), Color::Reset));
        push_cap(&mut app.events, "e".to_string());
        push_cap(&mut app.responses, "r".to_string());
        handle_key(&mut app, KeyCode::F(8), KeyModifiers::SHIFT);
        assert!(app.logs.is_empty() && app.events.is_empty() && app.responses.is_empty());
    }

    #[test]
    fn ui_empty_input_shows_placeholder() {
        let mut app = test_app();
        let text = render_to_text_sized(&mut app, 140, 24);
        assert!(text.contains("Enter your command here"));
    }

    #[test]
    fn ui_paused_and_hint_render() {
        let mut app = test_app();
        app.paused = true;
        app.hint = "system  radio  gpio".to_string();
        // Wide terminal so nothing truncates the hint line inside the command pane.
        let text = render_to_text_sized(&mut app, 140, 24);
        assert!(text.contains("<F5> Pause"));
        // The hint renders INSIDE the Shell Command pane (second inner line), not the footer.
        assert!(text.contains("system  radio  gpio"));
    }

    #[test]
    fn ui_keeps_newest_line_visible_when_lines_wrap() {
        // Regression for C6: with a pane full of over-wide lines, the *newest* line must be
        // on screen (previously wrapped items pushed it below the clip).
        let mut app = test_app();
        app.zoom = true;
        app.focus = Pane::Logs;
        let wide = "X".repeat(200); // wraps to several rows at width 80
        for i in 0..40 {
            push_cap(
                &mut app.logs,
                (format!("{i}-{wide}"), String::new(), Color::Reset),
            );
        }
        push_cap(
            &mut app.logs,
            ("NEWEST-MARKER".to_string(), String::new(), Color::Reset),
        );
        let text = render_to_text(&mut app);
        assert!(
            text.contains("NEWEST-MARKER"),
            "the most recent line must stay visible even when earlier lines wrap"
        );
    }

    // ---- handle_key: multibyte input, modifiers (C24) ----

    #[test]
    fn typing_multibyte_chars_keeps_cursor_on_boundary() {
        let mut app = test_app();
        for ch in "über→π".chars() {
            handle_key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(app.input, "über→π");
        assert_eq!(app.cursor, app.input.len());
        assert!(app.input.is_char_boundary(app.cursor));
    }

    #[test]
    fn ctrl_c_quits_from_any_pane() {
        let mut app = test_app();
        handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit);
        // And it must NOT have inserted a literal 'c' into the command line.
        assert_eq!(app.input, "");
    }

    #[test]
    fn ctrl_modified_char_is_not_inserted() {
        let mut app = test_app();
        handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(app.input, ""); // Ctrl+X is not text
    }

    #[test]
    fn q_quits_only_in_non_command_panes() {
        let mut app = test_app();
        app.focus = Pane::Logs;
        handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.quit);

        let mut app2 = test_app(); // focus defaults to Command
        handle_key(&mut app2, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app2.quit);
        assert_eq!(app2.input, "q"); // in the command pane, 'q' is just text
    }

    #[test]
    fn ctrl_u_and_ctrl_w_edit_the_line() {
        let mut app = test_app();
        for ch in "/system radio".chars() {
            handle_key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        handle_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL); // kill "radio"
        assert_eq!(app.input, "/system ");
        handle_key(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL); // kill to start
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
    }

    // ---- apply_completion: device-supplied token_start can't panic (C22) ----

    fn completions(
        token_start: u16,
        prefix: &str,
        cands: &[(&str, CandidateKind)],
    ) -> ShellCompletions<'static> {
        // Leak the strings so the returned struct is 'static for the test (small, one-shot).
        let mut v: heapless::Vec<Candidate<'static>, 16> = heapless::Vec::new();
        for (t, k) in cands {
            v.push(Candidate {
                text: Box::leak(t.to_string().into_boxed_str()),
                kind: *k,
            })
            .unwrap();
        }
        ShellCompletions {
            req_id: 1,
            token_start,
            common_prefix: Box::leak(prefix.to_string().into_boxed_str()),
            candidates: v,
            more: false,
        }
    }

    #[test]
    fn completion_with_mid_char_token_start_is_ignored() {
        // "π" is 2 bytes; token_start=1 falls inside it. apply_completion must bail, not panic.
        let mut app = test_app();
        app.input = "π".to_string();
        app.cursor = app.input.len();
        let c = completions(1, "", &[("system", CandidateKind::Command)]);
        apply_completion(&mut app, &c); // must not panic
        assert_eq!(app.input, "π"); // unchanged
    }

    #[test]
    fn completion_past_cursor_is_ignored() {
        let mut app = test_app();
        app.input = "ab".to_string();
        app.cursor = 1;
        // token_start beyond the cursor is nonsensical → ignore.
        let c = completions(2, "", &[("x", CandidateKind::Command)]);
        apply_completion(&mut app, &c);
        assert_eq!(app.input, "ab");
    }

    #[test]
    fn single_completion_is_inserted_with_separator() {
        let mut app = test_app();
        app.input = "/sys".to_string();
        app.cursor = app.input.len();
        let c = completions(1, "system", &[("system", CandidateKind::Menu)]);
        apply_completion(&mut app, &c);
        assert_eq!(app.input, "/system/"); // Menu kind appends '/'
        assert_eq!(app.cursor, app.input.len());
    }

    // ---- handle_frame: cmd_id filtering (C18) + chunk-gap flag (C19) ----

    fn resp_frame(cmd_id: u16, chunk: u16, last: bool, text: &str) -> Vec<u8> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(
            MsgType::ShellResponse,
            0,
            &ShellResponse {
                cmd_id,
                result: 0,
                chunk,
                last,
                text,
            },
            &mut buf,
        )
        .unwrap();
        // decode_frame in handle_frame wants the *inner* bytes; feed via a decoder to deframe.
        let mut dec = FrameDecoder::new();
        let mut inner = Vec::new();
        for &b in &buf[..n] {
            if let Some(f) = dec.push(b) {
                inner = f.to_vec();
            }
        }
        inner
    }

    #[test]
    fn handle_frame_ignores_wrong_cmd_id() {
        let mut app = test_app();
        app.pending.push_back(pending(5));
        // A response for a different command must not populate this one (C18).
        handle_frame(&mut app, &resp_frame(9, 0, true, "stale"));
        assert!(app.responses.is_empty());
        assert!(!app.pending.is_empty(), "the real command is still awaited");
    }

    #[test]
    fn handle_frame_reassembles_matching_cmd_id() {
        let mut app = test_app();
        app.pending.push_back(pending(5));
        handle_frame(&mut app, &resp_frame(5, 0, false, "line one\n"));
        // Chunks stream in as they arrive: line one is visible BEFORE `last`.
        assert_eq!(app.responses.back().unwrap(), "line one");
        handle_frame(&mut app, &resp_frame(5, 1, true, "line two"));
        let joined: Vec<String> = app.responses.iter().cloned().collect();
        assert_eq!(joined, vec!["line one".to_string(), "line two".to_string()]);
        assert!(app.pending.is_empty()); // cleared on `last`
    }

    #[test]
    fn slow_response_lands_under_the_newer_echo() {
        // SSH flow: cmd A is sent, then cmd B is typed before A's response arrives. A's
        // (and then B's) lines land in arrival order right above the prompt; nothing
        // blocks, nothing is dropped.
        let mut app = test_app();
        app.pending.push_back(pending(1));
        push_cap(&mut app.responses, "> first".to_string());
        push_cap(&mut app.responses, "> second".to_string()); // typed before A answered
        app.pending.push_back(pending(2));
        handle_frame(&mut app, &resp_frame(1, 0, true, "answer A"));
        handle_frame(&mut app, &resp_frame(2, 0, true, "answer B"));
        let joined: Vec<String> = app.responses.iter().cloned().collect();
        assert_eq!(joined, vec!["> first", "> second", "answer A", "answer B"]);
        assert!(app.pending.is_empty());
    }

    #[test]
    fn partial_chunk_line_is_continued_in_place() {
        let mut app = test_app();
        app.pending.push_back(pending(3));
        handle_frame(&mut app, &resp_frame(3, 0, false, "hello "));
        assert_eq!(app.responses.back().unwrap(), "hello ");
        handle_frame(&mut app, &resp_frame(3, 1, true, "world"));
        assert_eq!(app.responses.back().unwrap(), "hello world");
        assert_eq!(app.responses.len(), 1);
    }

    #[test]
    fn handle_frame_flags_chunk_gap() {
        let mut app = test_app();
        app.pending.push_back(pending(2));
        handle_frame(&mut app, &resp_frame(2, 0, false, "a"));
        handle_frame(&mut app, &resp_frame(2, 2, true, "c")); // chunk 1 missing → gap (C19)
        let joined: String = app.responses.iter().cloned().collect::<Vec<_>>().join("\n");
        assert!(joined.contains("chunk dropped"));
    }

    // ---- pending queue: one lost answer must not mute the shell (CLI-1) ----

    fn scrollback(app: &App) -> Vec<String> {
        app.responses.iter().cloned().collect()
    }

    #[test]
    fn answer_for_a_later_command_closes_the_stale_one() {
        // cmd 1's answer never arrives (lost command / CRC-dropped frame). cmd 2's answer used
        // to be dropped because only the FRONT entry could match; the shell went mute for
        // good. Now cmd 2 is answered and cmd 1 is closed with a visible marker.
        let mut app = test_app();
        app.pending.push_back(pending(1));
        app.pending.push_back(pending(2));
        handle_frame(&mut app, &resp_frame(2, 0, true, "answer B"));
        assert_eq!(scrollback(&app), vec!["[tower] no response to `/c1`", "answer B"]);
        assert!(app.pending.is_empty());
        // And the shell keeps working afterwards.
        app.pending.push_back(pending(3));
        handle_frame(&mut app, &resp_frame(3, 0, true, "answer C"));
        assert_eq!(app.responses.back().unwrap(), "answer C");
    }

    #[test]
    fn stale_command_with_lost_last_chunk_is_marked_incomplete() {
        let mut app = test_app();
        app.pending.push_back(pending(1));
        app.pending.push_back(pending(2));
        handle_frame(&mut app, &resp_frame(1, 0, false, "half a li")); // `last` never comes
        handle_frame(&mut app, &resp_frame(2, 0, true, "next"));
        assert_eq!(
            scrollback(&app),
            vec!["half a li", "[tower] incomplete response to `/c1`", "next"]
        );
        assert!(!app.resp_partial);
    }

    #[test]
    fn idle_commands_time_out_with_a_marker() {
        let mut app = test_app();
        app.pending.push_back(pending(1));
        let sent = app.pending[0].idle_since;
        expire_pending(&mut app, sent + PENDING_IDLE / 2);
        assert_eq!(app.pending.len(), 1, "still within its idle budget");
        expire_pending(&mut app, sent + PENDING_IDLE);
        assert!(app.pending.is_empty());
        assert_eq!(scrollback(&app), vec!["[tower] no response to `/c1` (timed out)"]);
    }

    #[test]
    fn activity_restarts_every_waiters_idle_clock() {
        // cmd 2 waits behind a long-running cmd 1; while cmd 1 keeps streaming, cmd 2 must not
        // be written off: the device can't answer it before cmd 1 finishes.
        let mut app = test_app();
        app.pending.push_back(pending(1));
        app.pending.push_back(pending(2));
        let sent = app.pending[1].idle_since;
        std::thread::sleep(Duration::from_millis(5));
        handle_frame(&mut app, &resp_frame(1, 0, false, "still working\n"));
        expire_pending(&mut app, sent + PENDING_IDLE);
        assert_eq!(app.pending.len(), 2, "streaming activity keeps the queue alive");
    }

    #[test]
    fn hello_closes_commands_from_the_previous_session() {
        let mut app = test_app();
        let mut old = pending(1);
        old.sent -= Duration::from_secs(1); // well before the device rebooted
        app.pending.push_back(old);
        app.pending.push_back(pending(2)); // typed in the very instant of the Hello
        handle_frame(&mut app, &hello_inner());
        assert_eq!(
            scrollback(&app),
            vec!["[tower] no response to `/c1` (device restarted)"]
        );
        assert_eq!(
            app.pending.len(),
            1,
            "a command inside the Hello race may be answered"
        );
        handle_frame(&mut app, &resp_frame(2, 0, true, "ok"));
        assert!(app.pending.is_empty());
    }

    #[test]
    fn lost_link_closes_everything_in_flight() {
        let mut app = test_app();
        app.pending.push_back(pending(1));
        app.pending_req = Some(4);
        drop_link(&mut app);
        assert!(app.pending.is_empty() && app.pending_req.is_none());
        assert_eq!(scrollback(&app), vec!["[tower] no response to `/c1` (link lost)"]);
    }

    #[test]
    fn endless_response_is_capped() {
        // A device streaming one response forever (no `last`, no newline) must not grow the
        // open partial line (or anything else) without bound.
        let mut app = test_app();
        let mut p = pending(1);
        p.received = MAX_RESPONSE_BYTES - 1;
        app.pending.push_back(p);
        handle_frame(&mut app, &resp_frame(1, 0, false, "xx"));
        assert!(app.pending.is_empty());
        assert!(!app.resp_partial);
        assert_eq!(
            scrollback(&app),
            vec!["[tower] response to `/c1` exceeded 256 KiB; truncated"]
        );
        handle_frame(&mut app, &resp_frame(1, 1, false, "more")); // the rest is ignored
        assert_eq!(app.responses.len(), 1);
    }

    // ---- AltGr (reported as Ctrl+Alt on Windows) is text (CLI-10) ----

    #[test]
    fn altgr_characters_are_typed() {
        let mut app = test_app();
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        for ch in ['@', '{', '[', '|', '\\', '~', '€'] {
            handle_key(&mut app, KeyCode::Char(ch), altgr);
        }
        handle_key(&mut app, KeyCode::Char('}'), altgr | KeyModifiers::SHIFT);
        assert_eq!(app.input, "@{[|\\~€}");
    }

    #[test]
    fn real_ctrl_and_ctrl_alt_chords_stay_commands() {
        let mut app = test_app();
        // A Ctrl+Alt+<letter> chord is not AltGr text…
        handle_key(
            &mut app,
            KeyCode::Char('u'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(app.input, "");
        // …and plain Ctrl shortcuts still edit the line.
        for ch in "/a b".chars() {
            handle_key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        handle_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(app.input, "/a ");
        handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit);
    }

    // ---- handle_frame: decode-failure surfacing + seq gaps + payload errors ----

    #[test]
    fn handle_frame_surfaces_a_device_from_another_checkout() {
        // Frames carrying another schema tag are counted (not silently dropped) and never
        // interpreted; the first one shouts a red line naming the tags and the remedy, the
        // device's Hello names its firmware (a second red line, once), the header shows the
        // tags, and a Hello with our tag (the device reflashed from this checkout) ends it.
        let mut app = test_app();
        handle_frame(&mut app, &tagged_inner(FOREIGN, MsgType::Dropped, 0, &[1]));
        assert_eq!(app.decode_failures, 1);
        assert!(app.warned_mismatch);
        let red = |app: &App| {
            app.logs
                .iter()
                .filter(|(s, _, c)| *c == Color::Red && s.contains("SCHEMA MISMATCH"))
                .map(|(s, _, _)| s.clone())
                .collect::<Vec<_>>()
        };
        let first = red(&app);
        assert_eq!(first.len(), 1, "the first foreign frame pushes a red line");
        assert!(first[0].contains(&format!("{FOREIGN:#06x}")), "{}", first[0]);
        assert!(
            first[0].contains("rebuild both from the same checkout"),
            "{}",
            first[0]
        );
        handle_frame(&mut app, &foreign_hello_inner());
        let named = red(&app);
        assert_eq!(named.len(), 2, "the Hello names the firmware, once");
        assert!(named[1].contains("app v0.0.9"), "{}", named[1]);
        handle_frame(&mut app, &foreign_hello_inner());
        handle_frame(&mut app, &tagged_inner(FOREIGN, MsgType::Log, 1, &[]));
        assert_eq!(red(&app).len(), 2, "no spam after that");
        assert_eq!(app.decode_failures, 4);
        let text = render_to_text_sized(&mut app, 160, 24);
        assert!(text.contains(&format!("schema {FOREIGN:#06x}")), "{text}");

        let logs_before = app.logs.len();
        handle_frame(&mut app, &hello_inner());
        assert!(app.mismatch.is_none() && !app.warned_mismatch);
        handle_frame(&mut app, &log_inner(1));
        assert_eq!(
            app.logs.len(),
            logs_before + 1,
            "a matching device is rendered again"
        );
    }

    #[test]
    fn handle_frame_reports_a_framing_difference_once() {
        // Every frame failing its CRC: one red line at the threshold, not one per frame.
        let mut app = test_app();
        let mut corrupt = log_inner(0);
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x01;
        for _ in 0..(FramingWatch::THRESHOLD * 2) {
            handle_frame(&mut app, &corrupt);
        }
        let lines = app
            .logs
            .iter()
            .filter(|(s, _, c)| *c == Color::Red && s.contains("every frame fails its CRC"))
            .count();
        assert_eq!(lines, 1);
        assert!(
            !app.warned_mismatch,
            "a framing difference is not a schema mismatch"
        );
        let text = render_to_text_sized(&mut app, 160, 24);
        assert!(text.contains("(framing?)"), "{text}");
    }

    #[test]
    fn handle_frame_counts_seq_gaps() {
        let mut app = test_app();
        handle_frame(&mut app, &log_inner(10));
        handle_frame(&mut app, &log_inner(11)); // contiguous: no gap
        handle_frame(&mut app, &log_inner(20)); // jump: one gap
        assert_eq!(app.seq_gaps, 1);
        assert_eq!(app.last_seq, Some(20));
    }

    #[test]
    fn handle_frame_rebaselines_seq_on_hello() {
        // A Hello re-baselines seq tracking, so a fresh session at seq 0 after an earlier
        // stream isn't reported as a spurious gap.
        let mut app = test_app();
        handle_frame(&mut app, &log_inner(100));
        handle_frame(&mut app, &valid_inner(MsgType::Hello, 0, &[])); // empty Hello payload
        // The empty-payload Hello counts as a payload error, but the seq re-baseline still ran.
        assert_eq!(app.seq_gaps, 0);
        assert_eq!(app.last_seq, Some(0));
    }

    #[test]
    fn handle_frame_counts_payload_errors() {
        // A frame that decodes at the frame layer (CRC + tag ok) but whose payload isn't a
        // valid Log is counted, not silently dropped.
        let mut app = test_app();
        handle_frame(&mut app, &valid_inner(MsgType::Log, 0, &[]));
        assert_eq!(app.payload_errors, 1);
        assert_eq!(app.decode_failures, 0);
    }

    // ---- ui(): decode diagnostics + reconnect error in the header (C-tui-1, C-tui-5) ----

    #[test]
    fn ui_header_shows_decode_diagnostics() {
        let mut app = test_app();
        app.decode_failures = 37;
        app.mismatch = Some(Mismatch {
            tag: 0x1234,
            device: None,
        });
        app.seq_gaps = 4;
        let text = render_to_text_sized(&mut app, 140, 24);
        assert!(text.contains("37 bad frames"));
        assert!(text.contains("schema 0x1234"));
        assert!(text.contains("4 seq gaps"));
    }

    #[test]
    fn ui_header_shows_reconnect_error() {
        let mut app = test_app(); // disconnected (sp = None)
        app.last_open_error = Some("Device or resource busy".to_string());
        let text = render_to_text_sized(&mut app, 140, 24);
        assert!(text.contains("reconnecting"));
        assert!(text.contains("Device or resource busy"));
    }

    // ---- send_command: a failed send must not tear down / desync the UI (C-tui-4) ----

    #[test]
    fn send_command_keeps_input_when_send_fails() {
        // With no connection the send fails; the typed line must survive (for retry), no
        // reassembly is armed, nothing is echoed, cmd_id doesn't advance, and a hint explains.
        let mut app = test_app(); // sp = None
        app.input = "/system radio".to_string();
        app.cursor = app.input.len();
        send_command(&mut app);
        assert_eq!(app.input, "/system radio");
        assert!(app.pending.is_empty());
        assert!(app.responses.is_empty());
        assert_eq!(app.cmd_id, 1);
        assert!(!app.hint.is_empty());
    }
}
