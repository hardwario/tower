//! The framed console **session** over a byte transport: the boot-`Hello` handshake, the
//! `ShellResponse` reassembler, and the `ShellCompletions` request/response. Everything here
//! is generic over [`Transport`] (`Read + Write`), so
//! it runs unchanged over a real `Box<dyn serialport::SerialPort>` in production and over an
//! in-memory mock in tests, which is what makes the session logic testable without hardware.
//!
//! What is deliberately *not* here: opening/enumerating ports and the reset pulse (that's
//! `port`), rendering the stream (that's `render`), and the rustyline/clap glue for the
//! interactive shell (that stays in the command layer in `main`).

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use tower_protocol::msg::{CandidateKind, Hello, ShellComplete, ShellCompletions, ShellResponse};
use tower_protocol::{
    Error, FrameDecoder, MAX_WIRE, MsgType, PROTOCOL_VERSION, WIRE_FINGERPRINT, decode_frame, encode_frame,
};

/// A lockstep mismatch: the device was built against a different `tower-protocol` than this
/// `tower`. Either kind means postcard would silently mis-decode, so every consumer treats both
/// the same way (the mismatch banner, exit code 125, no further interpretation of the stream).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mismatch {
    /// The device speaks another `PROTOCOL_VERSION`: seen in a CRC-valid frame header
    /// (`Error::BadVersion`) or, as a secondary guard, in a `Hello` payload.
    Version(u8),
    /// Same `PROTOCOL_VERSION`, but the device announced (in `Hello` or the `Describe` reply's
    /// `DeviceInfo`) a different `WIRE_FINGERPRINT`: a different wire schema, i.e. the two ends
    /// were built from different tower-protocol source trees.
    Fingerprint(u32),
}

impl Mismatch {
    /// The lockstep verdict on a device's announced `(protocol_version, wire_fingerprint)`.
    pub(crate) fn check(protocol_version: u8, wire_fingerprint: u32) -> Option<Mismatch> {
        if protocol_version != PROTOCOL_VERSION {
            Some(Mismatch::Version(protocol_version))
        } else if wire_fingerprint != WIRE_FINGERPRINT {
            Some(Mismatch::Fingerprint(wire_fingerprint))
        } else {
            None
        }
    }

    /// [`Mismatch::check`] on a decoded `Hello`.
    pub(crate) fn of_hello(h: &Hello) -> Option<Mismatch> {
        Self::check(h.protocol_version, h.wire_fingerprint)
    }

    /// What differs, naming both sides: `protocol version mismatch (device v3, tower v4)`, or
    /// for the fingerprint the hex values plus the explanation.
    pub(crate) fn what(&self) -> String {
        match self {
            Mismatch::Version(v) => {
                format!("protocol version mismatch (device v{v}, tower v{PROTOCOL_VERSION})")
            }
            Mismatch::Fingerprint(fp) => format!(
                "wire fingerprint mismatch (device {fp:#010x}, tower {WIRE_FINGERPRINT:#010x}): same \
                 PROTOCOL_VERSION but a different wire schema: the tower-protocol sources differ"
            ),
        }
    }

    /// The remedy clause that ends a one-line refusal.
    pub(crate) fn remedy(&self) -> &'static str {
        match self {
            Mismatch::Version(_) => "rebuild `tower` and the firmware from the same checkout",
            Mismatch::Fingerprint(_) => "rebuild both from the same checkout",
        }
    }
}

/// The byte transport the session runs over: just `Read + Write`. A real
/// `Box<dyn serialport::SerialPort>` already satisfies this (so production callers are
/// unchanged), and a test can supply an in-memory duplex mock. A drained mock returns
/// `ErrorKind::TimedOut`, exactly as a serial port does on its read timeout, so the loops
/// exercise their real timeout paths.
pub(crate) trait Transport: Read + Write {}
impl<T: Read + Write + ?Sized> Transport for T {}

/// One `read` off the transport with the benign "nothing yet" errors folded into `Ok(0)`: the
/// port's read timeout (`TimedOut`: the normal idle poll) and a signal-interrupted wait
/// (`Interrupted`: serialport's `poll()` returns EINTR when e.g. a terminal resize delivers
/// SIGWINCH mid-read, which must not read as a dead link). Anything else is a real link failure
/// (unplugged, EIO, …) that callers surface instead of mistaking it for a mute device.
pub(crate) fn read_some(sp: &mut (impl Read + ?Sized), buf: &mut [u8]) -> std::io::Result<usize> {
    match sp.read(buf) {
        Ok(n) => Ok(n),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
            ) =>
        {
            Ok(0)
        }
        Err(e) => Err(e),
    }
}

/// The `[result N]` tag shown after a non-zero shell result, named when the code is one the
/// TOWER firmware SDK defines (`tower::shell::R_*`; app commands may return their own codes,
/// which print bare). The number stays first so scripts that grep for it keep working; `exec`'s
/// exit status is still the raw code.
pub(crate) fn result_tag(code: u8) -> String {
    let name = match code {
        1 => "not found",
        2 => "bad argument",
        3 => "storage error",
        4 => "truncated",
        5 => "denied (console-only command)",
        _ => return format!("[result {code}]"),
    };
    format!("[result {code}: {name}]")
}

/// Cap on one reassembled shell response (exec/shell text, and each TUI in-flight command). A
/// 20 KB-RAM target never legitimately streams anything near this; it only bounds a device that
/// streams matching chunks forever without a `last` (each chunk re-arms the idle timeout, so
/// without a cap the host would buffer without limit). Over the cap the response is cut off and
/// flagged incomplete.
pub(crate) const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// How long to wait for the boot `Hello` before falling back to `--delay`.
///
/// Sized to the measured worst case, not the typical one: a warm boot announces in
/// ~150 ms, but every Nth boot pays the EEPROM KV **compaction CPU stall** in the
/// session bump: ~5.2 s with the whole chip (ISRs included) frozen before the boot
/// proceeds (bench-proven 2026-07-05: flip counter 1:1 with slow boots; the LSE
/// crystal was ruled out by CSR forensics + a drive-strength sweep; see
/// firmware/docs/storage.md). The wait is event-driven (returns the moment
/// the Hello decodes), so the ceiling costs nothing on fast boots; a short ceiling
/// silently degraded `exec --reset` into firing the command at a device that
/// hadn't even booted yet.
const HELLO_WAIT: Duration = Duration::from_millis(8000);
/// Fallback settle when `--reset` is used on a send path but no `Hello` arrives
/// and no explicit `--delay` was given.
const DEFAULT_SETTLE: Duration = Duration::from_millis(250);
/// Guard settle after a post-reset `Hello` before the first host→device send.
///
/// Right after the boot `Hello`, a chatty firmware drains its whole boot log backlog
/// in one burst; while that flood saturates the UART ISR, host→device bytes can be
/// lost to receiver overrun, so a command sent immediately after the Hello vanishes
/// without a trace (measured window ≤ ~60 ms on console_full; zero on quiet
/// firmwares like blinky; 2026-07-05 wire probe). 150 ms is 2.5× the measured
/// worst case and imperceptible next to the reset itself. An explicit `--delay`
/// extends (never shortens) this guard.
const POST_HELLO_GUARD: Duration = Duration::from_millis(150);

/// The outcome of waiting for a freshly-reset device to announce itself. Tri-state on purpose:
/// a *real* protocol-version mismatch is rejected by `decode_frame` at the frame **header**
/// (top 3 bits of `ver_type`): it returns [`Error::BadVersion`] and the `Hello` payload never
/// parses, so a Hello-payload comparison alone would miss it and the caller would just time
/// out. Surfacing [`Readiness::BadVersion`] lets `exec` fail fast with a clear diagnostic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Readiness {
    /// A `Hello` decoded cleanly; carries the `protocol_version` and `wire_fingerprint` it
    /// announced, which the caller checks against its own ([`Readiness::mismatch`]): the
    /// version as a secondary guard, the fingerprint as the only way to see a same-version,
    /// different-schema peer.
    Hello { version: u8, fingerprint: u32 },
    /// `decode_frame` rejected a frame's header version: the peer speaks a different
    /// `tower-protocol` source. Carries the version byte actually seen on the wire.
    BadVersion(u8),
    /// No `Hello` (and no version-tagged frame) arrived before the timeout.
    Timeout,
}

impl Readiness {
    /// The lockstep mismatch this readiness reveals, if any: a header-level `BadVersion`, or a
    /// `Hello` announcing another version or wire fingerprint.
    pub(crate) fn mismatch(self) -> Option<Mismatch> {
        match self {
            Readiness::BadVersion(got) => Some(Mismatch::Version(got)),
            Readiness::Hello { version, fingerprint } => Mismatch::check(version, fingerprint),
            Readiness::Timeout => None,
        }
    }
}

/// Block until the device announces itself with a `Hello` frame (so its shell is up before we
/// send), or a header-level version mismatch is seen, or `timeout`. Bytes seen meanwhile feed
/// `dec`. Only meaningful right after a reset.
/// Tick-less convenience wrapper (production callers all animate; the unit tests don't).
#[cfg(test)]
pub(crate) fn wait_for_hello(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    timeout: Duration,
) -> Readiness {
    wait_for_hello_with(sp, dec, timeout, |_| {}, None)
}

/// `wait_for_hello` with a progress callback, invoked with the elapsed wait roughly at the
/// port's read-timeout cadence: the boot can legitimately take seconds (worst case a fallback
/// EEPROM compaction ≈ 5 s), so interactive callers animate the wait instead of sitting mute.
///
/// The wait never desyncs the stream: the read that completes the `Hello` usually carries more
/// (the boot-log burst right behind it), and the rest of that buffer is still pushed through
/// `dec` (frames that complete inside it are boot output, consumed here; exec/shell never show
/// it), so a caller that keeps `dec` resumes exactly at the next byte instead of mid-frame. A caller
/// that renders the stream itself (`logs --reset --send`) passes `transcript` to get every raw
/// byte read during the wait (the `Hello` and the early boot lines included) and replays it
/// through its own decoder, so the boot shows up as if it had been streaming all along.
pub(crate) fn wait_for_hello_with(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    timeout: Duration,
    mut on_tick: impl FnMut(Duration),
    mut transcript: Option<&mut Vec<u8>>,
) -> Readiness {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        on_tick(start.elapsed());
        let n = match read_some(sp, &mut buf) {
            Ok(n) => n,
            Err(_) => return Readiness::Timeout,
        };
        if let Some(t) = transcript.as_deref_mut() {
            t.extend_from_slice(&buf[..n]);
        }
        for (i, &b) in buf[..n].iter().enumerate() {
            let Some(inner) = dec.push(b) else { continue };
            let found = match decode_frame(inner) {
                Ok((MsgType::Hello, _, payload)) => {
                    postcard::from_bytes::<Hello>(payload)
                        .ok()
                        .map(|h| Readiness::Hello {
                            version: h.protocol_version,
                            fingerprint: h.wire_fingerprint,
                        })
                }
                // The smoking gun for a tag mismatch: the header version didn't match, so
                // this (and every) frame is rejected before its payload is even read.
                Err(Error::BadVersion { got }) => Some(Readiness::BadVersion(got)),
                _ => None,
            };
            if let Some(readiness) = found {
                // Keep `dec` in step with the wire: the bytes after the Hello in this read are
                // the start of the stream the caller continues with (their frames aren't ours
                // to interpret: they're boot logs, and the transcript already holds them).
                for &rest in &buf[i + 1..n] {
                    let _ = dec.push(rest);
                }
                return readiness;
            }
        }
    }
    Readiness::Timeout
}

/// Get a freshly reset device ready to accept a command: wait for the boot
/// `Hello` (self-calibrating to real boot time), then honor an explicit `--delay`
/// as extra settle. If no `Hello` arrives, fall back to `--delay` (or a default)
/// so we don't send into a link that isn't up yet. Returns the [`Readiness`] so callers can
/// enforce the lockstep rule (and, for `BadVersion`, bail immediately without settling).
/// Tick-less convenience wrapper (production callers all animate; the unit tests don't).
#[cfg(test)]
pub(crate) fn await_ready(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    delay: Option<u64>,
) -> Readiness {
    await_ready_with(sp, dec, delay, |_| {}, None)
}

/// `await_ready` with a boot-wait progress callback and an optional raw-byte `transcript` of
/// the wait (see `wait_for_hello_with`). Bytes that arrive during the post-Hello settle stay in
/// the port's buffer for the caller's next read; nothing is consumed behind its back.
pub(crate) fn await_ready_with(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    delay: Option<u64>,
    on_tick: impl FnMut(Duration),
    transcript: Option<&mut Vec<u8>>,
) -> Readiness {
    let readiness = wait_for_hello_with(sp, dec, HELLO_WAIT, on_tick, transcript);
    match readiness {
        // Mismatch (a foreign header version, or a Hello announcing another version or wire
        // fingerprint): don't settle; the caller is about to bail.
        _ if readiness.mismatch().is_some() => {}
        // A Hello arrived: guard against the boot-burst deaf window (see POST_HELLO_GUARD),
        // extended by an explicit `--delay` if the caller asked for more.
        Readiness::Hello { .. } => {
            std::thread::sleep(POST_HELLO_GUARD.max(Duration::from_millis(delay.unwrap_or(0))));
        }
        Readiness::BadVersion(_) => {}
        // No Hello even at the worst-case ceiling: the device may be wedged, held in reset,
        // or running firmware with no console. Say so: the send that follows is a best-effort
        // shot in the dark, and the old silent fallback made this indistinguishable from a
        // healthy send that got no reply.
        Readiness::Timeout => {
            eprintln!(
                "[tower] no boot Hello within {}s of the reset; device wedged or console-less? sending anyway",
                HELLO_WAIT.as_secs()
            );
            std::thread::sleep(delay.map_or(DEFAULT_SETTLE, Duration::from_millis));
        }
    }
    readiness
}

// ---- lockstep gate for the request/response waits ----------------------------

/// Lockstep bookkeeping for a wait loop (`read_response`, `request_completions`, the mgmt
/// round trip): remembers a header-level version rejection, and a `Hello` that announces a
/// foreign version or wire fingerprint. After such a `Hello` the stream is on a foreign schema,
/// so further frames are not interpreted (postcard would silently mis-decode them) until a
/// matching `Hello` (the device reflashed back) re-opens it.
#[derive(Default)]
pub(crate) struct LockstepGate {
    mismatch: Option<Mismatch>,
    foreign: bool,
}

impl LockstepGate {
    /// Validate one deframed frame and return `(MsgType, payload)` to interpret, or `None` when
    /// it is corrupt, version-rejected, a `Hello` (consumed here: it only feeds the check) or
    /// from a foreign schema.
    pub(crate) fn admit<'a>(&mut self, inner: &'a [u8]) -> Option<(MsgType, &'a [u8])> {
        match decode_frame(inner) {
            Ok((MsgType::Hello, _, payload)) => {
                if let Ok(h) = postcard::from_bytes::<Hello>(payload) {
                    // A matching Hello (the device reflashed back) clears earlier evidence.
                    let verdict = Mismatch::of_hello(&h);
                    self.foreign = verdict.is_some();
                    self.mismatch = verdict;
                }
                None
            }
            Ok(_) if self.foreign => None,
            Ok((mt, _, payload)) => Some((mt, payload)),
            // The device IS talking; its frames just carry a different protocol version.
            Err(Error::BadVersion { got }) => {
                self.mismatch = Some(Mismatch::Version(got));
                None
            }
            Err(_) => None,
        }
    }

    /// The lockstep mismatch seen so far, if any (for the timeout diagnosis).
    pub(crate) fn mismatch(&self) -> Option<Mismatch> {
        self.mismatch
    }
}

// ---- shell command response ------------------------------------------------

/// The reassembled outcome of a shell command.
pub(crate) struct Response {
    /// Device-reported result byte (authoritative only on the `last` chunk).
    pub(crate) result: u8,
    /// Concatenated chunk text.
    pub(crate) text: String,
    /// A `chunk` index gap was seen: a middle chunk was CRC-dropped, so `text` is
    /// silently truncated. Callers should warn and (for `exec`) exit non-zero.
    pub(crate) incomplete: bool,
}

/// The result of waiting for a shell response. The timeout variant carries the lockstep
/// smoking gun: if frames DID arrive but every one was rejected at the header for its
/// protocol version (or a `Hello` announced a foreign wire fingerprint, after which the
/// stream is not interpreted), the link isn't dead (the two ends speak different
/// `tower-protocol` sources) and callers must say so (exit 125) instead of reporting a mute
/// device (124).
pub(crate) enum ReadOutcome {
    Response(Response),
    Timeout {
        mismatch: Option<Mismatch>,
    },
    /// The transport itself failed (unplugged, EIO, …): a tool error (exit 1), not a device
    /// that stayed silent (124): conflating the two sent CI chasing a mute firmware.
    Failed(std::io::Error),
}

impl ReadOutcome {
    /// Test convenience mirroring the previous `Option` API.
    #[cfg(test)]
    pub(crate) fn unwrap(self) -> Response {
        match self {
            ReadOutcome::Response(r) => r,
            ReadOutcome::Timeout { .. } => panic!("read_response timed out"),
            ReadOutcome::Failed(e) => panic!("read_response failed: {e}"),
        }
    }
    /// Test convenience mirroring the previous `Option` API ("timed out?").
    #[cfg(test)]
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, ReadOutcome::Timeout { .. })
    }
}

/// Read frames until the `ShellResponse` for `cmd_id` completes (`last`), or the idle
/// `timeout` elapses. Non-matching frames (logs/events, other `cmd_id`s) are ignored.
///
/// `timeout` is an *idle* deadline: it is reset every time a matching chunk arrives, so a
/// long command that keeps producing output isn't cut off; only a genuinely silent link
/// times out. The `chunk` index is tracked per response: a gap means a middle chunk was
/// CRC-dropped (the frame decoder silently discards corrupt frames), which would otherwise
/// yield silently-truncated output with result 0, so we flag it `incomplete` instead.
///
/// Times out only if no matching chunk arrives before the idle deadline; a lockstep mismatch
/// seen while waiting (version-rejected frames, or a `Hello` with a foreign wire fingerprint;
/// see [`LockstepGate`]) is reported in the [`ReadOutcome::Timeout`] so the caller can
/// diagnose it. A hard transport error is [`ReadOutcome::Failed`]. A response
/// that outgrows [`MAX_RESPONSE_BYTES`] is returned early, cut off and flagged `incomplete`.
pub(crate) fn read_response(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    cmd_id: u16,
    timeout: Duration,
) -> ReadOutcome {
    read_response_gated(sp, dec, &mut LockstepGate::default(), cmd_id, timeout)
}

/// [`read_response`] with a caller-owned [`LockstepGate`], so a session that runs several
/// commands over one link (the interactive `tower shell`) keeps a foreign-schema verdict
/// across commands instead of re-trusting the stream on the next one.
pub(crate) fn read_response_gated(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    gate: &mut LockstepGate,
    cmd_id: u16,
    timeout: Duration,
) -> ReadOutcome {
    let mut deadline = Instant::now() + timeout;
    let mut text = String::new();
    let mut next_chunk: u16 = 0; // expected `chunk` index of the next matching frame
    let mut incomplete = false;
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        let nread = match read_some(sp, &mut buf) {
            Ok(n) => n,
            Err(e) => return ReadOutcome::Failed(e),
        };
        for &b in &buf[..nread] {
            let Some(inner) = dec.push(b) else { continue };
            // Corrupt, version-rejected and foreign-schema frames are skipped; the gate keeps
            // the lockstep evidence so a "timeout" can be diagnosed as a mismatch.
            let Some((mt, payload)) = gate.admit(inner) else {
                continue;
            };
            if mt != MsgType::ShellResponse {
                continue;
            }
            let Ok(r) = postcard::from_bytes::<ShellResponse>(payload) else {
                continue;
            };
            if r.cmd_id != cmd_id {
                continue;
            }
            // A matching chunk: extend the idle window.
            deadline = Instant::now() + timeout;
            if r.chunk != next_chunk {
                // A middle chunk was dropped (or reordered): the output is truncated.
                eprintln!(
                    "[tower] response chunk dropped (expected #{next_chunk}, got #{}); output truncated",
                    r.chunk
                );
                incomplete = true;
            }
            next_chunk = r.chunk.wrapping_add(1);
            if text.len() + r.text.len() > MAX_RESPONSE_BYTES {
                // A device streaming without end: stop buffering and hand back what we have,
                // flagged (exec exits 1). The rest of its chunks are ignored by cmd_id later.
                eprintln!(
                    "[tower] response exceeded {} KiB; output truncated",
                    MAX_RESPONSE_BYTES / 1024
                );
                return ReadOutcome::Response(Response {
                    result: r.result,
                    text,
                    incomplete: true,
                });
            }
            text.push_str(r.text);
            if r.last {
                return ReadOutcome::Response(Response {
                    result: r.result,
                    text,
                    incomplete,
                });
            }
        }
    }
    ReadOutcome::Timeout {
        mismatch: gate.mismatch(),
    }
}

// ---- completion (target-authoritative) ------------------------------------

/// An owned copy of a completion result (the wire form borrows the frame buffer).
pub(crate) struct CompletionResult {
    pub(crate) token_start: u16,
    pub(crate) common_prefix: String,
    pub(crate) candidates: Vec<(String, CandidateKind)>,
    pub(crate) more: bool,
}

/// Where a completion's replacement starts in `line`, or `None` if the device-supplied
/// `token_start` is unusable. Both frontends splice candidates into `line[start..cursor]`
/// (`String::replace_range` in the TUI, rustyline's `LineBuffer::replace` in `tower shell`), and
/// both panic on a start past the cursor or inside a multi-byte UTF-8 scalar, so a confused or
/// hostile device must never be able to hand either one a raw index. `cursor` is the caller's own
/// (always on a char boundary, `<= line.len()`).
pub(crate) fn completion_start(line: &str, cursor: usize, token_start: u16) -> Option<usize> {
    let start = token_start as usize;
    (start <= cursor && cursor <= line.len() && line.is_char_boundary(start)).then_some(start)
}

/// The result of a completion request: same tri-state rationale as [`ReadOutcome`].
pub(crate) enum CompletionOutcome {
    Completions(CompletionResult),
    Timeout {
        mismatch: Option<Mismatch>,
    },
    /// The request couldn't be sent (line too long to frame, write failed) or the transport
    /// failed while waiting: a tool error, not a silent device.
    Failed(std::io::Error),
}

impl CompletionOutcome {
    /// Test convenience mirroring the previous `Option` API.
    #[cfg(test)]
    pub(crate) fn unwrap(self) -> CompletionResult {
        match self {
            CompletionOutcome::Completions(c) => c,
            CompletionOutcome::Timeout { .. } => panic!("request_completions timed out"),
            CompletionOutcome::Failed(e) => panic!("request_completions failed: {e}"),
        }
    }
    /// Test convenience mirroring the previous `Option` API ("timed out?").
    #[cfg(test)]
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, CompletionOutcome::Timeout { .. })
    }
}

/// Send a `ShellComplete` and wait for the matching `ShellCompletions`. Shared by the
/// `complete` command and the interactive TAB handler. A lockstep mismatch seen while waiting
/// is reported in the timeout variant (like [`read_response`]);
/// an unsendable request or a transport failure is [`CompletionOutcome::Failed`].
pub(crate) fn request_completions(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    line: &str,
    cursor: u16,
    req_id: u16,
    timeout: Duration,
) -> CompletionOutcome {
    let mut gate = LockstepGate::default();
    request_completions_gated(sp, dec, &mut gate, line, cursor, req_id, timeout)
}

/// [`request_completions`] with a caller-owned [`LockstepGate`] (see [`read_response_gated`]).
pub(crate) fn request_completions_gated(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    gate: &mut LockstepGate,
    line: &str,
    cursor: u16,
    req_id: u16,
    timeout: Duration,
) -> CompletionOutcome {
    let mut buf = [0u8; MAX_WIRE];
    let n = match encode_frame(
        MsgType::ShellComplete,
        0,
        &ShellComplete { req_id, line, cursor },
        &mut buf,
    ) {
        Ok(n) => n,
        Err(e) => {
            return CompletionOutcome::Failed(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("line too long to frame ({e:?})"),
            ));
        }
    };
    if let Err(e) = sp.write_all(&buf[..n]).and_then(|()| sp.flush()) {
        return CompletionOutcome::Failed(e);
    }

    let deadline = Instant::now() + timeout;
    let mut rbuf = [0u8; 256];
    while Instant::now() < deadline {
        let nread = match read_some(sp, &mut rbuf) {
            Ok(n) => n,
            Err(e) => return CompletionOutcome::Failed(e),
        };
        for &b in &rbuf[..nread] {
            let Some(inner) = dec.push(b) else { continue };
            let Some((mt, payload)) = gate.admit(inner) else {
                continue;
            };
            if mt != MsgType::ShellCompletions {
                continue;
            }
            let Ok(c) = postcard::from_bytes::<ShellCompletions>(payload) else {
                continue;
            };
            if c.req_id != req_id {
                continue;
            }
            return CompletionOutcome::Completions(CompletionResult {
                token_start: c.token_start,
                common_prefix: c.common_prefix.to_string(),
                candidates: c
                    .candidates
                    .iter()
                    .map(|cd| (cd.text.to_string(), cd.kind))
                    .collect(),
                more: c.more,
            });
        }
    }
    CompletionOutcome::Timeout {
        mismatch: gate.mismatch(),
    }
}
