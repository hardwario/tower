//! `tower`, the HARDWARIO TOWER host CLI: devices, logs/events, shell/exec, the console TUI,
//! and flash/erase/reset/chip/protect/unprotect (via the tower-flasher engine, crates/flasher).
//!
//! The firmware's UART is always framed (`tower-protocol`: COBS + CRC + postcard),
//! so a plain terminal shows binary; this tool decodes it. The same `FrameDecoder`
//! / `decode_frame` run on both ends, so the wire format can't drift.

use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Editor, Helper};
use tower_protocol::msg::{CandidateKind, ShellCommand};
use tower_protocol::{FrameDecoder, MsgType, decode_frame, encode_frame};

mod client;
mod gateway;
mod highlight;
mod mgmt;
mod port;
mod render;
mod session;
mod term;
mod tui;

use port::{devices, open_console, pick_port};
use render::{ColorMode, OutputMode, Renderer, View, device_text, hexline, read_loop, warn_mismatch};
#[cfg(test)]
use session::await_ready;
use session::{
    CompletionOutcome, LockstepGate, ReadOutcome, Readiness, await_ready_with, completion_start,
    read_response, read_response_gated, read_some, request_completions, request_completions_gated,
    result_tag,
};
use term::sanitize_line;

// ---- exit-code contract ---------------------------------------------------
//
// A documented, stable exit-code contract so scripts/CI can branch on *why* a
// command failed, not just that it did. `main` returns an `ExitCode` built from
// these; commands surface failures as `Err` (→ EXIT_ERROR) or return one of the
// specific codes below. `exec` additionally forwards a device-reported non-zero
// `result` verbatim (1..=123), which is why the reserved codes start at 124.
//
//   0    ok
//   1    tool error: I/O (incl. a port that fails mid-command, or a stream link lost under
//        `--no-reconnect`), bad file, encode/decode, truncated/incomplete device response;
//        also `flash`/`erase` refusing a readout-protected board (its message names the way
//        out), and `protect`/`unprotect` not confirmed (declined, or no --yes off a terminal)
//   2    usage error (bad args, emitted by clap itself)
//   124  device command timed out (no response at all: `exec`, `complete`, an MQTT RPC; a
//        truncated response is 1)
//   125  protocol mismatch (the device was built from another checkout: its frames carry
//        another schema tag; rebuild both from the same checkout)
//   126  wrong firmware role (the role probe found the wrong kind of device, or was refused)
//
// So a device `result` can't be confused with the reserved codes, `exec` clamps a
// device-reported non-zero result into 1..=123 (see `exec_cmd`); 124..=126 stay reserved.
const EXIT_OK: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_DEVICE_TIMEOUT: u8 = 124;
/// The attached device answered the management role probe with the wrong role (or refused
/// it): e.g. `tower gateway` pointed at a node, or `nodes add --port` at a gateway. Distinct
/// from 124/125 so scripts can say "flash the right firmware" instead of "check the cable".
const EXIT_WRONG_FIRMWARE: u8 = 126;
/// The device's frames carry another console schema tag: it was built from another checkout
/// than this `tower`, so every frame would silently mis-decode, and `exec` refuses rather than
/// emitting junk. Distinct from a plain timeout so CI can branch on it.
const EXIT_PROTOCOL_MISMATCH: u8 = 125;

/// Default per-command response timeout (`--timeout`), in milliseconds.
///
/// Sized above the firmware's worst-case EEPROM compaction stall (~5.2 s with the whole
/// chip frozen; see firmware/docs/storage.md): any command that writes EEPROM
/// (`settings set`, …) can land on the append that fills the KV half and pay for the whole
/// compaction before it can respond. With the old 1.5 s default such a command *executed*
/// but the CLI reported "no response (timeout)": a phantom failure (2026-07-05). It is an
/// *idle* timeout, so healthy commands still return in milliseconds; only a genuinely mute
/// link waits the full 7 s.
const DEFAULT_TIMEOUT_MS: u64 = 7000;

/// Animated stderr feedback for the post-reset boot wait (up to `session::HELLO_WAIT` = 8 s;
/// a fallback EEPROM compaction can legitimately hold the boot ~5 s). Silent for fast boots:
/// nothing renders before 600 ms. On a TTY it redraws in place (`\r`); piped, it prints one
/// static line so scripts/CI logs aren't spammed with animation frames.
struct BootTicker {
    shown: bool,
    is_tty: bool,
    last: Duration,
}

impl BootTicker {
    fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            shown: false,
            is_tty: std::io::stderr().is_terminal(),
            last: Duration::ZERO,
        }
    }

    fn tick(&mut self, el: Duration) {
        if el < Duration::from_millis(600) || (el - self.last) < Duration::from_millis(200) {
            return;
        }
        self.last = el;
        if self.is_tty {
            eprint!(
                "\r[tower] waiting for the device to boot… {:.1}s ",
                el.as_secs_f32()
            );
            self.shown = true;
        } else if !self.shown {
            eprintln!("[tower] waiting for the device to boot (a compaction can take ~5 s)…");
            self.shown = true;
        }
    }

    /// Close out the ticker line: report the boot time on success, or clear the line so a
    /// following warning/diagnostic starts clean. No-op if nothing was ever drawn.
    fn finish(&mut self, readiness: &Readiness) {
        if !(self.shown && self.is_tty) {
            return;
        }
        match readiness {
            Readiness::Hello => {
                eprintln!(
                    "\r[tower] device booted in {:.1}s                    ",
                    self.last.as_secs_f32()
                )
            }
            // Clear the in-place line; the caller's own warning/mismatch output follows.
            _ => eprint!("\r\u{1b}[2K"),
        }
    }
}

/// `--version`: the package version and the wire schema tags this binary speaks. Firmware built
/// from other sources carries other tags, and its frames are refused (exit 125).
fn version_text() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        format!(
            "{} (wire: console 0x{:04x}, radio 0x{:04x})",
            env!("CARGO_PKG_VERSION"),
            tower_protocol::CONSOLE_TAG,
            tower_protocol::radio::RADIO_TAG
        )
    })
}

#[derive(Parser)]
#[command(
    name = "tower",
    version = version_text(),
    about = "HARDWARIO TOWER host tool: console, flashing, and the radio to MQTT gateway \
             (no subcommand opens the console TUI)"
)]
struct Cli {
    /// Serial device (auto-detected when exactly one USB serial device is present).
    // The field stays `port` since it holds a serial-port path; the user-facing flag is `--device`.
    #[arg(short = 'd', long = "device", value_name = "DEVICE", global = true)]
    port: Option<String>,
    /// Don't auto-reconnect on the streaming commands (`logs`/`events`):
    /// exit when the link drops instead of retrying. (The first open is always fatal.)
    #[arg(long, global = true)]
    no_reconnect: bool,
    /// No subcommand opens the console TUI (the documented bare-`tower` UX).
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List connected serial devices.
    Devices,
    /// Stream device logs (and `print!` output) to stdout.
    Logs {
        /// When to colorize output.
        #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
        color: ColorMode,
        /// Emit one JSON object per line (NDJSON) instead of formatted text.
        #[arg(long)]
        json: bool,
        /// Send this text to the device once on connect (RX probe / quick poke).
        #[arg(long)]
        send: Option<String>,
        /// Reboot the application on connect (NRST pulse) so you see it come up from the start.
        #[arg(long)]
        reset: bool,
        /// With --reset and --send: extra ms to settle after the boot Hello (or fallback wait if none).
        #[arg(long, value_name = "MS", requires = "reset")]
        delay: Option<u64>,
    },
    /// Stream device events (structured key=value) to stdout.
    Events {
        /// When to colorize output.
        #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
        color: ColorMode,
        /// Emit one JSON object per line (NDJSON) instead of formatted text.
        #[arg(long)]
        json: bool,
        /// Reboot the application on connect (NRST pulse) so you see it come up from the start.
        #[arg(long)]
        reset: bool,
    },
    /// Open an interactive shell (commands start with `/`).
    Shell {
        /// Reboot the application before the shell opens, waiting for it to come up.
        #[arg(long)]
        reset: bool,
        /// With --reset: extra ms to settle after the boot Hello (or fallback wait if none).
        #[arg(long, value_name = "MS", requires = "reset")]
        delay: Option<u64>,
        /// Per-command idle response timeout in ms (reset each time a matching chunk arrives).
        #[arg(long, value_name = "MS", default_value_t = DEFAULT_TIMEOUT_MS)]
        timeout: u64,
    },
    /// Run one shell command and print its response, then exit (for scripts / CI).
    Exec {
        /// The command line, e.g. "/system/resource print".
        line: String,
        /// Reboot the application first, waiting for it to come up before sending (clean CI state).
        #[arg(long)]
        reset: bool,
        /// With --reset: extra ms to settle after the boot Hello (or fallback wait if none).
        #[arg(long, value_name = "MS", requires = "reset")]
        delay: Option<u64>,
        /// Idle response timeout in ms (reset each time a matching chunk arrives).
        #[arg(long, value_name = "MS", default_value_t = DEFAULT_TIMEOUT_MS)]
        timeout: u64,
    },
    /// Open the full-screen TUI console (logs + events + shell).
    Console {
        /// Reboot the application on connect (NRST pulse) so you see it come up from the start.
        #[arg(long)]
        reset: bool,
    },
    /// Ask the target to complete a partial command line (target-authoritative).
    Complete {
        /// The partial line (cursor is taken at its end).
        line: String,
    },
    /// Transport debugging: dump frames (or, with --hex, every raw byte).
    Monitor {
        /// Dump raw received bytes as hex instead of decoded frames.
        #[arg(long)]
        hex: bool,
        /// Reboot the application on connect (NRST pulse) so you capture its startup bytes.
        #[arg(long)]
        reset: bool,
    },
    /// Flash a raw firmware `.bin` over the STM32 UART bootloader.
    Flash {
        /// Path to the raw firmware `.bin`.
        file: PathBuf,
        /// Skip erasing before writing.
        #[arg(long)]
        no_erase: bool,
        /// Skip read-back verification.
        #[arg(long)]
        no_verify: bool,
        /// Do not reset/jump into the application after flashing.
        #[arg(long)]
        no_run: bool,
        /// Use the bootloader Go command instead of a hardware reset to start the app.
        #[arg(long)]
        go: bool,
        /// Print bootloader connect diagnostics.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Erase the entire device flash over the STM32 UART bootloader.
    Erase {
        /// Print bootloader connect diagnostics.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Read the chip id, bootloader version, supported commands and readout-protection state
    /// (read-only; the app is restored afterwards).
    Chip {
        /// Print bootloader connect diagnostics.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Production lock: enable readout protection (RDP level 1), so the firmware and the data
    /// EEPROM (radio keys, pairing) can no longer be read out. Undoing it erases the board.
    Protect {
        /// Don't ask for confirmation (required when stdin is not a terminal).
        #[arg(long)]
        yes: bool,
        /// Print bootloader connect diagnostics.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Remove readout protection. ERASES the board: flash AND data EEPROM (radio keys,
    /// pairing, settings, TX counters); the node must then be re-flashed and re-paired.
    Unprotect {
        /// Don't ask for confirmation (required when stdin is not a terminal).
        #[arg(long)]
        yes: bool,
        /// Print bootloader connect diagnostics.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Reset the device into the application (default) or the system bootloader.
    Reset {
        /// Reset into the system bootloader instead of the application.
        #[arg(long)]
        bootloader: bool,
    },
    /// Bridge the radio network to MQTT (TUI by default; --service for headless).
    Gateway(gateway::GatewayOpts),
    /// Manage the gateway's nodes over MQTT (list/add/remove/rename/shell/…).
    Nodes {
        #[command(subcommand)]
        cmd: client::nodes::NodesCmd,
        #[command(flatten)]
        mqtt: client::MqttOpts,
    },
    /// Network/gateway health over MQTT.
    Net {
        #[command(subcommand)]
        cmd: client::net::NetCmd,
        #[command(flatten)]
        mqtt: client::MqttOpts,
    },
}

fn main() -> ExitCode {
    // Rust's runtime installs SIG_IGN for SIGPIPE, so writing to a closed downstream pipe
    // (`tower logs | head`) surfaces as an EPIPE that print!/println! unwrap into a panic
    // (exit 101, "failed printing to stdout: Broken pipe"). Restore the default disposition
    // so we die quietly on the signal like every other Unix filter. No-op on non-unix.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("[tower] error: {e:#}");
            if cfg!(target_os = "linux") && permission_denied(&e) {
                eprintln!(
                    "[tower] hint: the serial port belongs to the `dialout` group (`uucp` on Arch): \
                     `sudo usermod -aG dialout $USER`, then log out and back in"
                );
            }
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Whether any cause in `e` is an OS "permission denied" (a serial port the user may not open).
fn permission_denied(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::PermissionDenied;
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == PermissionDenied)
            || c.downcast_ref::<serialport::Error>()
                .is_some_and(|e| e.kind() == serialport::ErrorKind::Io(PermissionDenied))
            || matches!(
                c.downcast_ref::<tower_flasher::error::Error>(),
                Some(tower_flasher::error::Error::Serial(e)) if e.kind() == serialport::ErrorKind::Io(PermissionDenied)
            )
            || matches!(
                c.downcast_ref::<tower_flasher::error::Error>(),
                Some(tower_flasher::error::Error::Io(e)) if e.kind() == PermissionDenied
            )
    })
}

/// Dispatch a parsed command. Returns the process exit code (see the exit-code contract
/// above); an `Err` here becomes `EXIT_ERROR`. A bare `tower` (no subcommand) opens the TUI.
fn run(cli: Cli) -> Result<u8> {
    let reconnect = !cli.no_reconnect;
    match cli.cmd.unwrap_or(Cmd::Console { reset: false }) {
        Cmd::Devices => devices().map(|()| EXIT_OK),
        Cmd::Logs {
            color,
            json,
            send,
            reset,
            delay,
        } => stream(
            cli.port,
            output_mode(color, json),
            View::Logs,
            send,
            reset,
            delay,
            reconnect,
        )
        .map(|()| EXIT_OK),
        Cmd::Events { color, json, reset } => stream(
            cli.port,
            output_mode(color, json),
            View::Events,
            None,
            reset,
            None,
            reconnect,
        )
        .map(|()| EXIT_OK),
        Cmd::Shell {
            reset,
            delay,
            timeout,
        } => shell(cli.port, reset, delay, Duration::from_millis(timeout)),
        Cmd::Exec {
            line,
            reset,
            delay,
            timeout,
        } => exec_cmd(cli.port, line, reset, delay, Duration::from_millis(timeout)),
        Cmd::Console { reset } => tui::run(pick_port(cli.port)?, reset).map(|()| EXIT_OK),
        Cmd::Complete { line } => complete_cmd(cli.port, line),
        Cmd::Monitor { hex, reset } => monitor(cli.port, hex, reset).map(|()| EXIT_OK),
        Cmd::Flash {
            file,
            no_erase,
            no_verify,
            no_run,
            go,
            verbose,
        } => flash_cmd(cli.port, file, !no_erase, !no_verify, !no_run, go, verbose).map(|()| EXIT_OK),
        Cmd::Erase { verbose } => erase_cmd(cli.port, verbose).map(|()| EXIT_OK),
        Cmd::Chip { verbose } => chip_cmd(cli.port, verbose).map(|()| EXIT_OK),
        Cmd::Protect { yes, verbose } => protect_cmd(cli.port, yes, verbose).map(|()| EXIT_OK),
        Cmd::Unprotect { yes, verbose } => unprotect_cmd(cli.port, yes, verbose).map(|()| EXIT_OK),
        Cmd::Reset { bootloader } => reset_cmd(cli.port, bootloader).map(|()| EXIT_OK),
        Cmd::Gateway(opts) => gateway::run(cli.port, opts),
        Cmd::Nodes { cmd, mqtt } => client::nodes::run(cmd, mqtt),
        Cmd::Net { cmd, mqtt } => client::net::run(cmd, mqtt),
    }
}

/// Build the streaming output mode: `--json` wins (structured NDJSON); otherwise text with
/// color resolved from `--color`.
fn output_mode(color: ColorMode, json: bool) -> OutputMode {
    if json {
        OutputMode::Json
    } else {
        OutputMode::Text {
            colors: color.enabled(),
        }
    }
}

fn stream(
    port: Option<String>,
    mode: OutputMode,
    view: View,
    send: Option<String>,
    reset: bool,
    delay: Option<u64>,
    reconnect: bool,
) -> Result<()> {
    // An explicit `--device` is honoured for the whole session; an auto-detected port is
    // re-detected on every reconnect, since a replugged board can come back under a new name.
    let explicit = port.is_some();
    let mut port = pick_port(port)?;
    // The FIRST open is fatal: a nonexistent --device must exit 1, not retry forever
    // (that used to spin silently). Enter the reconnect loop only after one success.
    // --reset fires once, on that initial attach, not on every auto-reconnect, or a
    // flaky link would turn into a reboot loop.
    let mut link = Some(open_console(&port, reset)?);
    let mut first = true;
    // The last reconnect failure, so a long unplug prints each distinct cause once rather than
    // the same line every 800 ms.
    let mut last_err = String::new();
    loop {
        if let Some(mut sp) = link.take() {
            eprintln!("[tower] connected {port}");
            last_err.clear();
            let mut out = Renderer::new(mode, view);
            // `--send` fires ONCE, on the initial attach (as documented), not on every
            // auto-reconnect. Re-sending on each reconnect would double-poke the device and,
            // worse, skip the readiness wait (which only runs on `first`), racing the boot.
            if first && let Some(s) = &send {
                // On a reset attach, wait for the device to come up before poking it, and
                // surface a schema mismatch immediately (the stream continues; render's
                // decode-failure path keeps counting, but the banner names the cause up front).
                if reset {
                    let mut boot = Vec::new();
                    let mut ticker = BootTicker::new();
                    let readiness = await_ready_with(
                        &mut *sp,
                        &mut FrameDecoder::new(),
                        delay,
                        |el| ticker.tick(el),
                        Some(&mut boot),
                    );
                    ticker.finish(&readiness);
                    if let Some(m) = readiness.mismatch() {
                        warn_mismatch(&m);
                    }
                    // Everything the wait consumed (the Hello and the early boot lines) is
                    // rendered now, in wire order, before the poke; the live stream then picks
                    // up at the very next byte, so nothing is lost and no frame is cut in half.
                    out.feed(&boot);
                }
                let _ = sp.write_all(s.as_bytes());
                let _ = sp.flush();
                eprintln!("[tower] sent {} byte(s)", s.len());
            }
            first = false;
            let e = read_loop(&mut *sp, &mut out);
            // Release the dead handle NOW, before sleeping/reopening. Holding it kept the fd
            // open across every retry: on Linux the stale ttyUSB minor stays claimed so a
            // replug renumbers (ttyUSB0 → ttyUSB1), and our own exclusive lock blocked
            // reopening the very same path.
            drop(sp);
            if !reconnect {
                // The link was lost and we were told not to retry: that's a failure (exit 1),
                // not a clean end of stream; scripts must be able to tell the two apart.
                return Err(anyhow::Error::new(e).context(format!("{port} lost")));
            }
            eprintln!("[tower] {port} lost: {e}");
            eprintln!("[tower] reconnecting…");
        }
        std::thread::sleep(Duration::from_millis(800));
        // Reopen without re-resetting; tolerate a transient failure and keep retrying.
        // "connected" is announced only once an open actually succeeded (top of the loop).
        let target = if explicit {
            Ok(port.clone())
        } else {
            pick_port(None)
        };
        match target.and_then(|p| Ok((open_console(&p, false)?, p))) {
            Ok((sp, p)) => {
                port = p;
                link = Some(sp);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg != last_err {
                    eprintln!("[tower] {msg}");
                    last_err = msg;
                }
            }
        }
    }
}

// ---- interactive shell ----------------------------------------------------

/// Shared serial connection: the TAB completer and the command loop both use it.
struct Conn {
    sp: Box<dyn serialport::SerialPort>,
    dec: FrameDecoder,
    /// Shared by every command and TAB request of the session, so the evidence of a device built
    /// from another checkout (frames refused for their schema tag, the firmware its `Hello`
    /// named) carries over from one command to the next.
    gate: LockstepGate,
    req_id: u16,
}

/// rustyline helper: TAB completion delegates entirely to the target.
struct ShellHelper {
    conn: Rc<RefCell<Conn>>,
}

impl Completer for ShellHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let mut conn = self.conn.borrow_mut();
        conn.req_id = conn.req_id.wrapping_add(1);
        let req_id = conn.req_id;
        let Conn { sp, dec, gate, .. } = &mut *conn;
        match request_completions_gated(
            &mut **sp,
            dec,
            gate,
            line,
            pos as u16,
            req_id,
            Duration::from_millis(800),
        ) {
            CompletionOutcome::Completions(r) => {
                // rustyline splices the pick into `line[start..pos]`, which panics on a start
                // past the cursor or inside a UTF-8 scalar, and `token_start` is
                // device-supplied. Same validation as the TUI; an unusable one offers nothing.
                let Some(start) = completion_start(line, pos, r.token_start) else {
                    return Ok((pos, Vec::new()));
                };
                let pairs = r
                    .candidates
                    .into_iter()
                    .map(|(text, kind)| {
                        let sep = match kind {
                            CandidateKind::Menu => "/",
                            CandidateKind::Command => " ",
                            CandidateKind::Arg => "=",
                            CandidateKind::Value => "",
                        };
                        // rustyline prints the candidate list straight to the terminal, so
                        // the device's text is escaped before it gets there.
                        let text = sanitize_line(&text);
                        Pair {
                            display: text.to_string(),
                            replacement: format!("{text}{sep}"),
                        }
                    })
                    .collect();
                Ok((start, pairs))
            }
            // Timeout (mismatch or not) or a failed link: offer no candidates. The
            // per-command path reports a lockstep mismatch / dead port loudly; TAB
            // shouldn't spam mid-edit.
            CompletionOutcome::Timeout { .. } | CompletionOutcome::Failed(_) => Ok((pos, Vec::new())),
        }
    }
}

impl Hinter for ShellHelper {
    type Hint = String;
}
impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

fn shell(port: Option<String>, reset: bool, delay: Option<u64>, timeout: Duration) -> Result<u8> {
    let port = pick_port(port)?;
    let mut sp = open_console(&port, reset)?;
    eprintln!("[tower] shell on {port}: TAB completes; commands start with '/'; 'exit' to quit");

    let mut dec = FrameDecoder::new();
    if reset {
        // Don't drop into the prompt until the freshly reset device can answer, and refuse
        // to open it at all on a schema mismatch (same rule as `exec`): every command
        // would otherwise just time out mute, which is exactly how the stale-binary
        // lockstep incident presented.
        let mut ticker = BootTicker::new();
        let readiness = await_ready_with(&mut *sp, &mut dec, delay, |el| ticker.tick(el), None);
        ticker.finish(&readiness);
        if let Some(m) = readiness.mismatch() {
            warn_mismatch(&m);
            eprintln!("[tower] {}; refusing to open the shell; {}", m.what(), m.remedy());
            return Ok(EXIT_PROTOCOL_MISMATCH);
        }
    }
    let conn = Rc::new(RefCell::new(Conn {
        sp,
        dec,
        gate: LockstepGate::default(),
        req_id: 0,
    }));
    let mut rl: Editor<ShellHelper, rustyline::history::DefaultHistory> = Editor::new()?;
    rl.set_helper(Some(ShellHelper { conn: conn.clone() }));

    let mut cmd_id: u16 = 1;
    let mut seq: u16 = 0;
    // The full remediation banner once per session; later mismatches get the short line.
    let mut warned_mismatch = false;
    loop {
        match rl.readline("> ") {
            Ok(input) => {
                let line = input.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "exit" || line == "quit" {
                    break;
                }
                let _ = rl.add_history_entry(line);

                let mut c = conn.borrow_mut();
                let Conn { sp, dec, gate, .. } = &mut *c;
                let mut buf = [0u8; tower_protocol::MAX_WIRE];
                // A line too long to fit one frame fails to encode. That's a *per-line* error,
                // not a session error: print a hint and return to the prompt instead of
                // propagating `?` (which used to tear the whole interactive shell down).
                let n = match encode_frame(
                    MsgType::ShellCommand,
                    seq,
                    &ShellCommand { cmd_id, line },
                    &mut buf,
                ) {
                    Ok(n) => n,
                    Err(_) => {
                        // The payload budget minus the `ShellCommand` overhead (a `cmd_id` varint of up
                        // to 3 bytes, the line's length varint of 2).
                        eprintln!(
                            "[tower] line too long (max ~{} bytes); not sent",
                            tower_protocol::MAX_PAYLOAD - 5
                        );
                        continue;
                    }
                };
                seq = seq.wrapping_add(1);
                sp.write_all(&buf[..n])?;
                sp.flush()?;
                match read_response_gated(&mut **sp, dec, gate, cmd_id, timeout) {
                    ReadOutcome::Response(r) => {
                        let text = device_text(&r.text);
                        print!("{text}");
                        if !text.is_empty() && !text.ends_with('\n') {
                            println!();
                        }
                        if r.result != 0 {
                            eprintln!("{}", result_tag(r.result));
                        }
                    }
                    // The port itself died (unplugged, EIO): every later command would fail
                    // the same way, so end the session as a tool error instead of answering
                    // each one with a misleading "no response (timeout)".
                    ReadOutcome::Failed(e) => {
                        return Err(anyhow::Error::new(e).context(format!("{port} lost")));
                    }
                    // The device answered, with frames carrying another schema tag. Without
                    // this, the interactive shell was the one consumer that stayed mute on the
                    // mismatch the ecosystem got burned by.
                    ReadOutcome::Timeout { mismatch: Some(m) } => {
                        if !warned_mismatch {
                            warn_mismatch(&m);
                            warned_mismatch = true;
                        }
                        eprintln!("[tower] no response: {}; {}", m.what(), m.remedy());
                    }
                    ReadOutcome::Timeout { mismatch: None } => {
                        eprintln!("[tower] no response (timeout)")
                    }
                }
                cmd_id = cmd_id.wrapping_add(1);
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            // A real readline failure (e.g. a broken terminal) is an error, not a clean
            // exit: propagate it so the shell exits non-zero rather than swallowing it.
            Err(e) => return Err(anyhow::Error::new(e).context("readline")),
        }
    }
    Ok(EXIT_OK)
}

/// A per-invocation `cmd_id` for the one-shot `exec`: the low 15 bits of the PID, never 0.
/// A previous `tower exec` (a *different* process) used a different PID, so its late/queued
/// `ShellResponse` (which would otherwise carry the same hardcoded `cmd_id` and satisfy this
/// run's wait) no longer matches. 15 bits keeps clear of the `0` sentinel other paths use.
fn exec_cmd_id() -> u16 {
    let id = (std::process::id() & 0x7FFF) as u16;
    if id == 0 { 1 } else { id }
}

/// Run a single shell command non-interactively: send it, print the (reassembled)
/// response, and return an exit code (see the exit-code contract): a device-reported
/// non-zero `result` (clamped into 1..=123 so it can't collide with our reserved 124..=126),
/// `EXIT_PROTOCOL_MISMATCH` on a schema mismatch, `EXIT_ERROR` on a truncated (chunk-dropped
/// or over-long) response or a port that fails mid-command, `EXIT_DEVICE_TIMEOUT` on no reply
/// at all, else `EXIT_OK`.
fn exec_cmd(
    port: Option<String>,
    line: String,
    reset: bool,
    delay: Option<u64>,
    timeout: Duration,
) -> Result<u8> {
    let port = pick_port(port)?;
    let mut sp = open_console(&port, reset)?;
    let mut dec = FrameDecoder::new();
    if reset {
        // Wait for the reset device to boot before issuing the command, so the
        // response we capture is from a known-clean state (the CI use case).
        // Fail fast on a schema mismatch: `exec` feeds CI, and a mismatch means every
        // subsequent frame would mis-decode; better a clear error than junk. It is caught at
        // the frame header (`Readiness::Mismatch`): the device's Hello itself carries the
        // foreign tag.
        let mut ticker = BootTicker::new();
        let readiness = await_ready_with(&mut *sp, &mut dec, delay, |el| ticker.tick(el), None);
        ticker.finish(&readiness);
        if let Some(m) = readiness.mismatch() {
            warn_mismatch(&m);
            eprintln!("[tower] {}; refusing to exec; {}", m.what(), m.remedy());
            return Ok(EXIT_PROTOCOL_MISMATCH);
        }
    }
    exec_line(&mut *sp, &mut dec, &line, timeout)
}

/// Send one shell command line over an established session, print its reassembled response,
/// and map the read outcome to the process exit code (see the exit-code contract). Split out
/// of [`exec_cmd`] (which owns port selection + the optional `--reset` boot wait) so this
/// send → read → map core runs unchanged over the real port and over an in-memory transport
/// in tests (the reserved-code mapping, incl. the `result.clamp(1, 123)` guard, is the part
/// worth pinning and it has no hardware in it).
fn exec_line(
    sp: &mut (impl crate::session::Transport + ?Sized),
    dec: &mut FrameDecoder,
    line: &str,
    timeout: Duration,
) -> Result<u8> {
    let cmd_id = exec_cmd_id();
    let mut buf = [0u8; tower_protocol::MAX_WIRE];
    let n = encode_frame(MsgType::ShellCommand, 0, &ShellCommand { cmd_id, line }, &mut buf)
        .map_err(|e| anyhow::anyhow!("encode: {e:?}"))?;
    sp.write_all(&buf[..n])?;
    sp.flush()?;
    match read_response(&mut *sp, dec, cmd_id, timeout) {
        ReadOutcome::Response(r) => {
            let text = device_text(&r.text);
            print!("{text}");
            if !text.is_empty() && !text.ends_with('\n') {
                println!();
            }
            if r.incomplete {
                // Output was silently truncated by a dropped chunk: fail even if the
                // device's `last` chunk said result 0 (the reported result is unreliable).
                eprintln!("[tower] response incomplete");
                return Ok(EXIT_ERROR);
            }
            if r.result != 0 {
                eprintln!("{}", result_tag(r.result));
                // Device results share the byte with our exit code; keep them in 1..=123 so
                // they never masquerade as the reserved timeout (124) code.
                return Ok(r.result.clamp(1, 123));
            }
            Ok(EXIT_OK)
        }
        // Frames arrived but every one was refused for its schema tag: that's the lockstep
        // failure (125), not a mute device (124), even without --reset, where the boot-time
        // check above never ran.
        ReadOutcome::Timeout { mismatch: Some(m) } => {
            warn_mismatch(&m);
            eprintln!("[tower] no response: {}; {}", m.what(), m.remedy());
            Ok(EXIT_PROTOCOL_MISMATCH)
        }
        ReadOutcome::Timeout { mismatch: None } => {
            eprintln!("[tower] no response (timeout)");
            Ok(EXIT_DEVICE_TIMEOUT)
        }
        // A hard transport error is a tool error (1), not a mute device (124).
        ReadOutcome::Failed(e) => Err(anyhow::Error::new(e).context("reading the response")),
    }
}

fn complete_cmd(port: Option<String>, line: String) -> Result<u8> {
    let port = pick_port(port)?;
    // No --reset here (completion is a momentary query), but still establish the
    // run baseline so we don't query a device the bridge left held in reset.
    let mut sp = open_console(&port, false)?;
    complete_line(
        &mut *sp,
        &mut FrameDecoder::new(),
        &line,
        Duration::from_millis(1500),
    )
}

/// Ask for completions of `line` (cursor at its end) over an established session, print them,
/// and map the outcome to the exit code: the hardware-free core of [`complete_cmd`], split out
/// for the same reason as [`exec_line`].
fn complete_line(
    sp: &mut (impl crate::session::Transport + ?Sized),
    dec: &mut FrameDecoder,
    line: &str,
    timeout: Duration,
) -> Result<u8> {
    let cursor = line.len() as u16;
    match request_completions(sp, dec, line, cursor, 1, timeout) {
        CompletionOutcome::Completions(r) => {
            // `{:?}` already escapes every control (C0/C1/DEL/bidi) in `common_prefix`; the
            // candidates are printed bare, so they go through the terminal-safety escape.
            println!(
                "token_start={} common_prefix={:?}{}",
                r.token_start,
                r.common_prefix,
                if r.more { " (more…)" } else { "" }
            );
            for (text, kind) in &r.candidates {
                println!("  {kind:?}  {}", sanitize_line(text));
            }
            Ok(EXIT_OK)
        }
        // Same 125 rule as exec: frames refused for their schema tag mean lockstep, not a dead
        // link.
        CompletionOutcome::Timeout { mismatch: Some(m) } => {
            warn_mismatch(&m);
            eprintln!("[tower] no completions: {}; {}", m.what(), m.remedy());
            Ok(EXIT_PROTOCOL_MISMATCH)
        }
        // No answer at all is the documented device timeout (124); exiting 0 told scripts a
        // mute or absent device had answered with nothing to complete.
        CompletionOutcome::Timeout { mismatch: None } => {
            eprintln!("[tower] no completions (timeout)");
            Ok(EXIT_DEVICE_TIMEOUT)
        }
        CompletionOutcome::Failed(e) => Err(anyhow::Error::new(e).context("requesting completions")),
    }
}

// ---- monitor (transport debugging) ----------------------------------------

fn monitor(port: Option<String>, hex: bool, reset: bool) -> Result<()> {
    let port = pick_port(port)?;
    let mut sp = open_console(&port, reset)?;
    eprintln!(
        "[tower] monitoring {port} ({})",
        if hex { "raw hex" } else { "frames" }
    );
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 512];
    loop {
        let n = read_some(&mut *sp, &mut buf)?;
        if n == 0 {
            continue;
        }
        if hex {
            for &b in &buf[..n] {
                print!("{b:02x} ");
            }
            let _ = std::io::stdout().flush();
            continue;
        }
        for &b in &buf[..n] {
            if let Some(inner) = dec.push(b) {
                match decode_frame(inner) {
                    Ok((mt, seq, payload)) => println!(
                        "frame seq={seq:<5} type={mt:?} payload={}B  {}",
                        payload.len(),
                        hexline(payload)
                    ),
                    Err(e) => println!("bad frame ({e:?}): {}", hexline(inner)),
                }
            }
        }
    }
}

// ---- firmware: flash / erase / reset / chip (STM32 UART bootloader, via tower-flasher) ----
//
// The console protocol above runs over the firmware's framed UART link; these
// commands instead drive the STM32 system bootloader (toggling NRST/BOOT0 over
// the bridge's RTS/DTR). The whole bootloader engine is the `tower-flasher` crate; we
// only pick the port (reusing the same auto-detect as the other commands) and
// hand off to it.

fn flash_cmd(
    port: Option<String>,
    file: PathBuf,
    erase: bool,
    verify: bool,
    run: bool,
    go: bool,
    verbose: bool,
) -> Result<()> {
    let port = pick_port(port)?;
    let fw = tower_flasher::firmware::load(&file)?;
    if fw.len() as u32 > tower_flasher::target::MAX_FLASH_SIZE {
        bail!(
            "firmware is {} bytes, exceeding the {} KiB maximum for any STM32L0 device",
            fw.len(),
            tower_flasher::target::MAX_FLASH_SIZE / 1024
        );
    }
    eprintln!(
        "[tower] flashing {} ({} bytes) to {port}",
        file.display(),
        fw.len()
    );
    let mut sp = tower_flasher::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    // tower-flasher's FlashOptions is #[non_exhaustive] (a new option won't break us): start from
    // Default and override. `verbose` moved out of the options into the progress sink below.
    let mut opts = tower_flasher::flash::FlashOptions::default();
    opts.erase = erase;
    opts.verify = verify;
    opts.run = run;
    opts.go = go;
    let mut report = flash_progress(verbose);
    let start = Instant::now();
    tower_flasher::flash::flash(&mut sp, &fw, &opts, &mut report)
        .map_err(|e| boot_err(e, "flashing firmware"))?;
    // A ~45 s flash prints only live progress above; without a terminal line the caller
    // can't tell success from a hang, so always confirm completion.
    eprintln!(
        "[tower] done: {} bytes in {:.1}s",
        fw.len(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn erase_cmd(port: Option<String>, verbose: bool) -> Result<()> {
    let port = pick_port(port)?;
    eprintln!("[tower] erasing {port}");
    let mut sp = tower_flasher::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    let mut report = flash_progress(verbose);
    let pages =
        tower_flasher::flash::erase(&mut sp, &mut report).map_err(|e| boot_err(e, "erasing flash"))?;
    eprintln!("[tower] erased {pages} page(s), reset into application");
    Ok(())
}

/// What `tower flash` / `tower erase` say about a readout-protected board, instead of the
/// library's generic description: the board can be opened again, but only by erasing it.
const READOUT_PROTECTED_HINT: &str = "this board is readout-protected; `tower unprotect` erases it \
     (flash + EEPROM) so it can be re-flashed; it must then be re-paired";

/// Map a tower-flasher error of `flash` / `erase` to the CLI's: a readout-protected board gets the
/// actionable [`READOUT_PROTECTED_HINT`] (still exit 1, the tool-error code), anything else keeps
/// the library message under `context`.
fn boot_err(e: tower_flasher::error::Error, context: &'static str) -> anyhow::Error {
    match e {
        tower_flasher::error::Error::ReadoutProtected { .. } => anyhow!(READOUT_PROTECTED_HINT),
        e => anyhow::Error::new(e).context(context),
    }
}

/// `tower chip`: what the STM32 ROM bootloader reports about the attached part, including the
/// readout-protection state. Read-only:
/// [`tower_flasher::chip::info`] enters the bootloader, reads `Get ID` + `Get`, probes the
/// protection with one Read Memory, and resets back into the application on every path. The
/// data goes to stdout (one `Label   : value` line each, script-friendly); status lines go to
/// stderr.
fn chip_cmd(port: Option<String>, verbose: bool) -> Result<()> {
    let port = pick_port(port)?;
    eprintln!("[tower] reading chip info on {port} (enters the bootloader, then restores the app)");
    let mut sp = tower_flasher::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    // Connect diagnostics only with --verbose; the id is printed from the result below.
    let mut report = |p: tower_flasher::flash::Progress| {
        if verbose {
            eprintln!("[tower] {p:?}");
        }
    };
    let info = tower_flasher::chip::info(&mut sp, &mut report).context("reading chip info")?;
    print!("{}", chip_report(&port, &info));
    if !info.is_known() {
        eprintln!(
            "[tower] warning: 0x{:03X} is not a recognized STM32L0 family id",
            info.id
        );
    }
    eprintln!("[tower] {port} reset into application");
    if info.readout_protected {
        // RM0367 3.4.1: at level 1 the flash is locked while the system memory is connected,
        // and after level 1 was set through the bootloader user code needs a power-down. Say
        // so rather than let a silent board look broken.
        eprintln!(
            "[tower] readout-protected: `tower flash` / `tower erase` refuse this board; `tower unprotect` \
             erases it (flash + EEPROM) to open it again. If the application does not restart, power-cycle \
             the board (unplug USB)."
        );
    }
    Ok(())
}

/// The `Readout : ...` line of `tower chip` (and of `protect` / `unprotect`, after the
/// read-back).
fn readout_line(readout_protected: bool) -> &'static str {
    if readout_protected {
        "Readout : protected (RDP level 1)\n"
    } else {
        "Readout : not protected\n"
    }
}

// ---- readout protection: protect / unprotect --------------------------------
//
// Opt-in production lock (tower-flasher's `rdp` module has the protocol and the ST citations).
// Dev boards stay open; `protect` is a deliberate provisioning step after flash, verify and
// pairing, and `unprotect` is the only way back, at the cost of a mass erase of the flash AND
// the data EEPROM. Both ask first: `protect` for "yes", `unprotect` for the word "erase", so a
// reflexive Enter or "y" can't wipe a node's keys. A script must say `--yes`; without it a
// non-terminal stdin is refused before any I/O, instead of reading an answer from a pipe.

/// What `tower protect` shows before asking. Ends with the prompt (no newline).
fn protect_notice(port: &str) -> String {
    format!(
        "tower protect: enable readout protection (RDP level 1) on {port}\n\
         \n\
         What it does:\n  \
           The chip's option bytes are set to RDP level 1. From then on neither the UART\n  \
           bootloader nor a debugger (SWD) can read, write or erase the flash (the firmware)\n  \
           or the data EEPROM (radio keys, pairing, settings, TX counters). The application\n  \
           keeps running from flash. `tower flash` and `tower erase` refuse the board.\n  \
           Level 2 (permanent, no way back) is never set by this tool.\n\
         \n\
         What undoing it costs:\n  \
           `tower unprotect` is the only way back, and it mass-erases the whole flash AND\n  \
           the data EEPROM: the node loses its firmware, radio keys, pairing, settings and\n  \
           TX counters, and must be re-flashed and re-paired.\n\
         \n\
         Afterwards, power-cycle the board once (unplug USB) so the application starts.\n\
         \n\
         Type \"yes\" to protect this board: "
    )
}

/// What `tower unprotect` shows before asking. Ends with the prompt (no newline).
fn unprotect_notice(port: &str) -> String {
    format!(
        "tower unprotect: remove readout protection (RDP level 1 to 0) on {port}\n\
         \n\
         WARNING: this ERASES the board. Leaving RDP level 1 makes the chip mass-erase the\n\
         whole flash (the firmware) AND the data EEPROM: radio keys, pairing, settings and\n\
         TX counters are lost for good. Nothing can be read out first.\n\
         \n\
         Afterwards the node must be re-flashed (`tower flash <bin>`) and re-paired with its\n\
         gateway (`tower nodes add`).\n\
         \n\
         Type \"erase\" to erase and unprotect this board: "
    )
}

/// Refuse a protection change that cannot be confirmed: without `--yes`, stdin must be a
/// terminal (the answer is read from it). Runs before any port I/O.
fn confirmation_gate(yes: bool, stdin_is_tty: bool, action: &str) -> Result<()> {
    if yes || stdin_is_tty {
        return Ok(());
    }
    bail!(
        "refusing to {action} without confirmation: stdin is not a terminal; pass --yes to confirm \
         non-interactively"
    )
}

/// Read one line of `input`; whether it is exactly `word` (surrounding whitespace ignored).
/// End of input is a "no".
fn read_confirmation(input: &mut impl std::io::BufRead, word: &str) -> Result<bool> {
    let mut line = String::new();
    input.read_line(&mut line).context("reading the confirmation")?;
    Ok(line.trim() == word)
}

/// Show `notice` (which ends with the prompt) on stderr and require `word` on stdin.
fn confirm(notice: &str, word: &str) -> Result<()> {
    eprint!("{notice}");
    let _ = std::io::stderr().flush();
    if !read_confirmation(&mut std::io::stdin().lock(), word)? {
        bail!("aborted (the answer was not \"{word}\"): nothing was changed");
    }
    Ok(())
}

/// A progress sink for `protect` / `unprotect`: one line per step (the chip id, each probe, the
/// accepted command); `--verbose` dumps the raw events instead.
fn rdp_progress(verbose: bool) -> impl FnMut(tower_flasher::flash::Progress) {
    use tower_flasher::flash::Progress;
    move |p| {
        if verbose {
            eprintln!("[tower] {p:?}");
            return;
        }
        match p {
            Progress::ChipIdentified { id } => eprintln!("[tower] chip 0x{id:03x}"),
            Progress::ReadoutProbed { readout_protected } => eprintln!(
                "[tower] readout protection: {}",
                if readout_protected {
                    "active (RDP level 1)"
                } else {
                    "not active"
                }
            ),
            Progress::ReadoutChanging { protect: true } => {
                eprintln!("[tower] Readout Protect accepted: writing the option bytes, then the chip resets")
            }
            Progress::ReadoutChanging { protect: false } => eprintln!(
                "[tower] Readout Unprotect accepted: mass-erasing flash and data EEPROM, then the chip resets"
            ),
            _ => {}
        }
    }
}

/// `tower protect`: RDP level 1 through [`tower_flasher::rdp::protect`], then the state read back in
/// a new bootloader session is printed (`Readout : ...` on stdout).
fn protect_cmd(port: Option<String>, yes: bool, verbose: bool) -> Result<()> {
    use std::io::IsTerminal;
    confirmation_gate(yes, std::io::stdin().is_terminal(), "enable readout protection")?;
    let port = pick_port(port)?;
    if !yes {
        confirm(&protect_notice(&port), "yes")?;
    }
    eprintln!("[tower] enabling readout protection (RDP level 1) on {port}");
    let mut sp = tower_flasher::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    let change = tower_flasher::rdp::protect(&mut sp, &mut rdp_progress(verbose)).context(
        "enabling readout protection (`tower chip` reads the current state; power-cycle the board first \
         if the bootloader does not answer)",
    )?;
    print!("{}", readout_line(change.readout_protected));
    if change.changed() {
        eprintln!(
            "[tower] {port} is readout-protected (read back in a new bootloader session). Power-cycle the \
             board once (unplug USB) so the application starts: after RDP level 1 is set through the \
             bootloader, the STM32L0 runs user code only after a power-down (RM0367 3.4.1)."
        );
    } else {
        eprintln!("[tower] {port} was already readout-protected; nothing was changed");
    }
    Ok(())
}

/// `tower unprotect`: RDP level 1 back to 0 through [`tower_flasher::rdp::unprotect`] (a mass erase
/// of flash + data EEPROM), then the state read back is printed. A board that is not protected
/// is left alone (nothing is erased).
fn unprotect_cmd(port: Option<String>, yes: bool, verbose: bool) -> Result<()> {
    use std::io::IsTerminal;
    confirmation_gate(
        yes,
        std::io::stdin().is_terminal(),
        "remove readout protection (which erases the board)",
    )?;
    let port = pick_port(port)?;
    if !yes {
        confirm(&unprotect_notice(&port), "erase")?;
    }
    eprintln!("[tower] removing readout protection on {port} (erases flash + data EEPROM)");
    let mut sp = tower_flasher::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    let change = tower_flasher::rdp::unprotect(&mut sp, &mut rdp_progress(verbose)).context(
        "removing readout protection (`tower chip` reads the current state; power-cycle the board first \
         if the bootloader does not answer)",
    )?;
    print!("{}", readout_line(change.readout_protected));
    if change.changed() {
        eprintln!(
            "[tower] {port}: flash and data EEPROM erased, readout protection removed. Re-flash it \
             (`tower flash <bin>`) and re-pair it (`tower nodes add`)."
        );
    } else {
        eprintln!("[tower] {port} is not readout-protected; nothing was erased or changed");
    }
    Ok(())
}

/// The `tower chip` stdout block. The bootloader lines are omitted when `Get` did not answer
/// (the id is the essential part).
fn chip_report(port: &str, info: &tower_flasher::chip::ChipInfo) -> String {
    let mut out = format!(
        "Device  : {port} @ {} baud, 8E1\nChip id : 0x{:03X} ({})\n{}",
        tower_flasher::target::BAUD,
        info.id,
        info.family,
        readout_line(info.readout_protected)
    );
    if let Some((major, minor)) = info.bootloader_version_parts() {
        let cmds: Vec<String> = info.commands.iter().map(|c| format!("0x{c:02X}")).collect();
        out.push_str(&format!(
            "BL ver  : {major}.{minor}\nCommands: {}\n",
            cmds.join(" ")
        ));
    }
    out
}

/// A tower-flasher flash/erase progress sink. tower-flasher is UI-free (it emits `Progress` events);
/// we render them with `indicatif` progress bars, so a ~45 s flash shows live
/// erase/write/verify progress instead of looking hung. `indicatif` auto-detects the terminal,
/// so a redirected stderr (a log, a pipe) stays clean. `--verbose` dumps the raw events instead.
fn flash_progress(verbose: bool) -> impl FnMut(tower_flasher::flash::Progress) {
    use indicatif::{ProgressBar, ProgressStyle};
    use tower_flasher::flash::Progress;

    fn pages_bar(total: usize, msg: &'static str) -> ProgressBar {
        let bar = ProgressBar::new(total as u64);
        bar.set_style(
            ProgressStyle::with_template("[tower] {msg:>9} [{bar:28.cyan/blue}] {pos:>4}/{len} pages")
                .unwrap()
                .progress_chars("=>-"),
        );
        bar.set_message(msg);
        bar
    }
    fn bytes_bar(total: u64, msg: &'static str) -> ProgressBar {
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template(
                "[tower] {msg:>9} [{bar:28.cyan/blue}] {bytes:>8}/{total_bytes:<8} {percent:>3}%",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        bar.set_message(msg);
        bar
    }

    // The single bar for the active phase; swapped as erase → write → verify advance.
    let mut bar: Option<ProgressBar> = None;
    move |p| {
        if verbose {
            eprintln!("[tower] {p:?}");
            return;
        }
        match p {
            Progress::ChipIdentified { id } => eprintln!("[tower] chip 0x{id:03x}"),
            Progress::Erase {
                pages_done,
                pages_total,
            } => {
                let b = bar.get_or_insert_with(|| pages_bar(pages_total, "erasing"));
                b.set_length(pages_total as u64);
                b.set_position(pages_done as u64);
                if pages_done == pages_total
                    && let Some(b) = bar.take()
                {
                    b.finish_with_message("erased");
                }
            }
            Progress::Write {
                bytes_done,
                bytes_total,
            } => {
                let b = bar.get_or_insert_with(|| bytes_bar(bytes_total as u64, "writing"));
                b.set_position(bytes_done as u64);
                if bytes_done == bytes_total
                    && let Some(b) = bar.take()
                {
                    b.finish_with_message("written");
                }
            }
            Progress::Verify {
                bytes_done,
                bytes_total,
            } => {
                let b = bar.get_or_insert_with(|| bytes_bar(bytes_total as u64, "verifying"));
                b.set_position(bytes_done as u64);
                if bytes_done == bytes_total
                    && let Some(b) = bar.take()
                {
                    b.finish_with_message("verified");
                }
            }
            // Finish any lingering bar (e.g. verify disabled) so it doesn't outlive the flash.
            Progress::Starting => {
                if let Some(b) = bar.take() {
                    b.finish_and_clear();
                }
            }
            // Connecting / ConnectError (and any future variant): quiet unless --verbose.
            _ => {}
        }
    }
}

fn reset_cmd(port: Option<String>, bootloader: bool) -> Result<()> {
    let port = pick_port(port)?;
    let mut sp = tower_flasher::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    if bootloader {
        sp.reset_into_bootloader().context("resetting into bootloader")?;
        eprintln!("[tower] {port} reset into bootloader");
    } else {
        sp.reset_into_app().context("resetting into application")?;
        eprintln!("[tower] {port} reset into application");
    }
    Ok(())
}

// ===========================================================================
// Tests
//
// The frame-session functions are generic over `Transport` (Read + Write), so we drive
// them against an in-memory duplex mock instead of hardware: the "device" side is a queue
// of bytes the code-under-test reads, and everything the code writes lands in a second
// queue we can inspect. The mock returns `ErrorKind::TimedOut` when its read queue is
// drained, exactly as a serial port does on its read timeout, so the read loops exercise
// their real timeout paths.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    // Session functions live in `crate::session`; pull in the ones exercised here that aren't
    // already re-exported into `main` (which imports only what the command layer calls).
    use crate::session::{Mismatch, wait_for_hello};
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::time::Duration;
    // (CandidateKind, ShellCommand, MsgType, decode_frame, encode_frame, FrameDecoder come
    // via `super::*` from the command layer's imports; only add what's test-only here.)
    use tower_protocol::msg::{
        Candidate, Dropped, Event, Hello, Level, Log, ShellCompletions, ShellResponse,
    };
    use tower_protocol::{CONSOLE_TAG, MAX_WIRE, encode_frame_with_tag};

    /// In-memory duplex transport. `to_read` feeds the code-under-test (as if from the
    /// device); `written` captures everything the code writes (host→device).
    struct MockPort {
        to_read: VecDeque<u8>,
        written: Vec<u8>,
        /// Cap each `read` to this many bytes, to exercise chunk reassembly across reads
        /// (a real UART delivers bytes in arbitrary-sized reads). 0 = no cap.
        read_chunk: usize,
    }

    impl MockPort {
        fn new(to_read: Vec<u8>) -> Self {
            MockPort {
                to_read: to_read.into(),
                written: Vec::new(),
                read_chunk: 0,
            }
        }
        /// A mock that hands out at most `n` bytes per `read` call.
        fn with_read_chunk(to_read: Vec<u8>, n: usize) -> Self {
            let mut m = MockPort::new(to_read);
            m.read_chunk = n;
            m
        }
    }

    impl Read for MockPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.to_read.is_empty() {
                // Drained: behave like a serial port past its read timeout.
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            let mut cap = buf.len().min(self.to_read.len());
            if self.read_chunk != 0 {
                cap = cap.min(self.read_chunk);
            }
            for slot in buf.iter_mut().take(cap) {
                *slot = self.to_read.pop_front().unwrap();
            }
            Ok(cap)
        }
    }

    impl Write for MockPort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // ---- frame construction helpers ----

    fn frame<T: serde::Serialize>(mt: MsgType, seq: u16, payload: &T) -> Vec<u8> {
        let mut buf = [0u8; MAX_WIRE];
        let n = encode_frame(mt, seq, payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn shell_resp(cmd_id: u16, result: u8, chunk: u16, last: bool, text: &str) -> Vec<u8> {
        frame(
            MsgType::ShellResponse,
            0,
            &ShellResponse {
                cmd_id,
                result,
                chunk,
                last,
                text,
            },
        )
    }

    fn hello() -> Vec<u8> {
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

    /// The console schema tag of a device built from another checkout.
    const FOREIGN: u16 = CONSOLE_TAG ^ 0x0F0F;

    /// A frame from a device built from another checkout: CRC intact, another schema tag, so
    /// `decode_frame` refuses it (`SchemaMismatch`) before its payload is read.
    fn foreign_frame<T: serde::Serialize>(mt: MsgType, seq: u16, payload: &T) -> Vec<u8> {
        let mut buf = [0u8; MAX_WIRE];
        let n = encode_frame_with_tag(FOREIGN, mt, seq, payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    /// The boot `Hello` of a device built from another checkout (it still names the firmware).
    fn foreign_hello() -> Vec<u8> {
        foreign_frame(
            MsgType::Hello,
            0,
            &Hello {
                firmware_name: "app",
                firmware_version: "v0.0.9",
                session_id: 1,
            },
        )
    }

    /// The mismatch [`foreign_hello`] reveals.
    fn foreign_mismatch() -> Mismatch {
        Mismatch {
            tag: FOREIGN,
            device: Some("app v0.0.9".into()),
        }
    }

    /// Canonical COBS encode of `inner`, plus the trailing `0x00` delimiter: the same wire
    /// shape `encode_frame` produces. Used to forge a frame the host's own encoder can't (a
    /// header from before the schema tags).
    fn cobs_frame(inner: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8]; // placeholder for the first code byte
        let mut code_pos = 0usize;
        let mut code: u8 = 1;
        for &b in inner {
            if b == 0 {
                out[code_pos] = code;
                code_pos = out.len();
                out.push(0);
                code = 1;
            } else {
                out.push(b);
                code += 1;
                if code == 0xFF {
                    out[code_pos] = code;
                    code_pos = out.len();
                    out.push(0);
                    code = 1;
                }
            }
        }
        out[code_pos] = code;
        out.push(0); // frame delimiter
        out
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

    const SHORT: Duration = Duration::from_millis(200);

    /// The Linux dialout hint fires for a denied port open however the error is wrapped (the
    /// console's serialport error, the flasher's, a plain io error), and for nothing else.
    #[test]
    fn permission_denied_is_found_in_the_chain() {
        use std::io::ErrorKind;
        let sp = |k| serialport::Error::new(serialport::ErrorKind::Io(k), "open");
        let denied = [
            anyhow::Error::new(sp(ErrorKind::PermissionDenied)).context("opening /dev/ttyUSB0"),
            anyhow::Error::new(tower_flasher::error::Error::Serial(sp(
                ErrorKind::PermissionDenied,
            )))
            .context("opening /dev/ttyUSB0"),
            anyhow::Error::new(std::io::Error::from(ErrorKind::PermissionDenied)),
        ];
        for e in &denied {
            assert!(permission_denied(e), "{e:#}");
        }
        let other = [
            anyhow::Error::new(sp(ErrorKind::NotFound)).context("opening /dev/ttyUSB0"),
            anyhow::anyhow!("permission denied (only words)"),
        ];
        for e in &other {
            assert!(!permission_denied(e), "{e:#}");
        }
    }

    #[test]
    fn result_tag_names_sdk_codes_and_keeps_unknown_bare() {
        assert_eq!(result_tag(5), "[result 5: denied (console-only command)]");
        assert_eq!(result_tag(2), "[result 2: bad argument]");
        assert_eq!(result_tag(42), "[result 42]");
    }

    // ---- read_response: reassembly, cmd_id filtering, chunk gaps ----

    #[test]
    fn read_response_reassembles_chunks() {
        let mut bytes = shell_resp(7, 0, 0, false, "hello ");
        bytes.extend(shell_resp(7, 0, 1, false, "wor"));
        bytes.extend(shell_resp(7, 0, 2, true, "ld\n"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 7, SHORT).unwrap();
        assert_eq!(r.text, "hello world\n");
        assert_eq!(r.result, 0);
        assert!(!r.incomplete);
    }

    #[test]
    fn read_response_survives_byte_at_a_time_reads() {
        // The same three chunks, but delivered one byte per read(): reassembly must not
        // depend on frame boundaries aligning with read boundaries.
        let mut bytes = shell_resp(1, 0, 0, false, "ab");
        bytes.extend(shell_resp(1, 0, 1, true, "cd"));
        let mut sp = MockPort::with_read_chunk(bytes, 1);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 1, SHORT).unwrap();
        assert_eq!(r.text, "abcd");
    }

    #[test]
    fn read_response_ignores_interleaved_logs() {
        // A Log frame lands between response chunks; it must not corrupt reassembly.
        let mut bytes = shell_resp(3, 0, 0, false, "x");
        bytes.extend(log_frame(9, "noise"));
        bytes.extend(shell_resp(3, 0, 1, true, "y"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 3, SHORT).unwrap();
        assert_eq!(r.text, "xy");
    }

    #[test]
    fn read_response_ignores_wrong_cmd_id() {
        // A complete response for cmd_id 99 must not satisfy a wait for cmd_id 1 (C18).
        let mut bytes = shell_resp(99, 0, 0, true, "stale");
        bytes.extend(shell_resp(1, 0, 0, true, "mine"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 1, SHORT).unwrap();
        assert_eq!(r.text, "mine");
    }

    #[test]
    fn read_response_flags_dropped_chunk() {
        // Chunk 1 is missing (as if CRC-dropped): 0 then 2 → incomplete (C19).
        let mut bytes = shell_resp(5, 0, 0, false, "a");
        bytes.extend(shell_resp(5, 0, 2, true, "c"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 5, SHORT).unwrap();
        assert!(
            r.incomplete,
            "a chunk-index gap must mark the response incomplete"
        );
        assert_eq!(r.text, "ac"); // what did arrive is still returned
    }

    #[test]
    fn read_response_times_out_when_silent() {
        let mut sp = MockPort::new(Vec::new());
        let mut dec = FrameDecoder::new();
        assert!(read_response(&mut sp, &mut dec, 1, SHORT).is_none());
    }

    #[test]
    fn read_response_drops_corrupt_crc_frame() {
        // Flip a byte inside the COBS-encoded frame so the CRC fails; the decoder drops it
        // and the waiter times out rather than returning garbage.
        let mut bytes = shell_resp(1, 0, 0, true, "hello");
        bytes[2] ^= 0xFF; // corrupt a payload byte (not the trailing 0x00 delimiter)
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        assert!(read_response(&mut sp, &mut dec, 1, SHORT).is_none());
    }

    #[test]
    fn read_response_timeout_carries_the_mismatch() {
        // The device answers, but every frame carries another schema tag. The timeout must
        // carry the mismatch so exec/shell report lockstep (125), not a mute device (124): the
        // exact stale-binary incident this ecosystem had.
        let mut bytes = foreign_hello();
        bytes.extend(foreign_frame(
            MsgType::ShellResponse,
            1,
            &ShellResponse {
                cmd_id: 1,
                result: 0,
                chunk: 0,
                last: true,
                text: "would mis-decode",
            },
        ));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        match read_response(&mut sp, &mut dec, 1, SHORT) {
            ReadOutcome::Timeout { mismatch } => assert_eq!(mismatch, Some(foreign_mismatch())),
            ReadOutcome::Response(_) => panic!("a foreign-tag frame must not decode"),
            ReadOutcome::Failed(e) => panic!("the mock never fails: {e}"),
        }
    }

    #[test]
    fn request_completions_timeout_carries_the_mismatch() {
        let mut sp = MockPort::new(foreign_hello());
        let mut dec = FrameDecoder::new();
        match request_completions(&mut sp, &mut dec, "/s", 2, 1, SHORT) {
            CompletionOutcome::Timeout { mismatch } => {
                assert_eq!(mismatch, Some(foreign_mismatch()))
            }
            CompletionOutcome::Completions(_) => {
                panic!("a foreign-tag frame must not decode")
            }
            CompletionOutcome::Failed(e) => panic!("the mock never fails: {e}"),
        }
    }

    // ---- Hello handshake ----

    #[test]
    fn wait_for_hello_accepts_a_matching_hello() {
        // The matched-peer case: `hello()` uses the host's own encoder, so the header carries
        // our tag and the Hello decodes cleanly.
        let mut sp = MockPort::new(hello());
        let mut dec = FrameDecoder::new();
        let readiness = wait_for_hello(&mut sp, &mut dec, SHORT);
        assert_eq!(readiness, Readiness::Hello);
        assert_eq!(readiness.mismatch(), None);
    }

    #[test]
    fn wait_for_hello_reports_a_device_from_another_checkout() {
        // The device's Hello carries another schema tag: refused at the header, but it still
        // names the firmware for the message.
        let mut sp = MockPort::new(foreign_hello());
        let mut dec = FrameDecoder::new();
        let readiness = wait_for_hello(&mut sp, &mut dec, SHORT);
        assert_eq!(readiness, Readiness::Mismatch(foreign_mismatch()));
        // await_ready propagates it (exec/shell then refuse with 125) and skips the
        // post-Hello settle: a mismatch is not worth waiting for.
        let mut sp = MockPort::new(foreign_hello());
        let t = std::time::Instant::now();
        let readiness = await_ready(&mut sp, &mut FrameDecoder::new(), Some(5_000));
        assert_eq!(readiness.mismatch(), Some(foreign_mismatch()));
        assert!(t.elapsed() < Duration::from_secs(4), "no settle on a mismatch");
        // Any frame of such a device gives it away, a Hello or not (then without a name).
        let mut sp = MockPort::new(foreign_frame(MsgType::Dropped, 3, &Dropped { count: 1 }));
        assert_eq!(
            wait_for_hello(&mut sp, &mut FrameDecoder::new(), SHORT),
            Readiness::Mismatch(Mismatch {
                tag: FOREIGN,
                device: None
            })
        );
    }

    #[test]
    fn mismatch_message_names_the_device_both_tags_and_the_remedy() {
        let m = foreign_mismatch();
        let what = m.what();
        assert!(what.contains("app v0.0.9"), "{what}");
        assert!(what.contains(&format!("{FOREIGN:#06x}")), "{what}");
        assert!(what.contains(&format!("{CONSOLE_TAG:#06x}")), "{what}");
        assert_eq!(m.remedy(), "rebuild both from the same checkout");
        let anonymous = Mismatch {
            tag: FOREIGN,
            device: None,
        };
        assert!(anonymous.what().starts_with("schema mismatch: the device speaks"));
        assert_eq!(EXIT_PROTOCOL_MISMATCH, 125);
    }

    #[test]
    fn a_frame_from_the_version_byte_era_is_a_mismatch() {
        // A board still running firmware from before the schema tags (its header was
        // `ver_type ‖ seq`): its frames pass the CRC and are refused for their "tag" (the old
        // seq bytes), so a stale board is a 125 like any other, not a mute device.
        let mut inner = vec![0x80 | MsgType::Hello as u8, 0x01, 0x00, 0x03, b'a', b'p', b'p'];
        let crc = tower_protocol::crc::crc32_ieee(&inner);
        inner.extend_from_slice(&crc.to_le_bytes());
        let mut sp = MockPort::new(cobs_frame(&inner));
        assert!(matches!(
            wait_for_hello(&mut sp, &mut FrameDecoder::new(), SHORT),
            Readiness::Mismatch(Mismatch { tag: 0x0001, .. })
        ));
    }

    #[test]
    fn wait_for_hello_times_out_without_hello() {
        let mut sp = MockPort::new(log_frame(0, "not a hello"));
        let mut dec = FrameDecoder::new();
        assert_eq!(wait_for_hello(&mut sp, &mut dec, SHORT), Readiness::Timeout);
    }

    #[test]
    fn exec_cmd_id_is_nonzero_and_15_bit() {
        let id = exec_cmd_id();
        assert_ne!(id, 0, "cmd_id 0 is a reserved sentinel");
        assert!(id <= 0x7FFF, "cmd_id must fit the low 15 bits");
        // Deterministic within a process (same PID) so the send and the wait agree on it.
        assert_eq!(id, exec_cmd_id());
    }

    #[test]
    fn overlong_shell_line_fails_encode_but_short_line_fits() {
        // The boundary the interactive shell + TUI now tolerate per-line (print a hint, keep the
        // prompt) instead of propagating `?` and killing the session: a line that overflows one
        // frame fails to encode; a normal line encodes fine.
        let mut buf = [0u8; MAX_WIRE];
        let long = "x".repeat(300);
        assert!(
            encode_frame(
                MsgType::ShellCommand,
                0,
                &ShellCommand {
                    cmd_id: 1,
                    line: &long,
                },
                &mut buf,
            )
            .is_err()
        );
        assert!(
            encode_frame(
                MsgType::ShellCommand,
                0,
                &ShellCommand {
                    cmd_id: 1,
                    line: "ok",
                },
                &mut buf,
            )
            .is_ok()
        );
    }

    // ---- exec_line: the reserved-exit-code mapping (over a MockPort) ----
    //
    // `exec_cmd` opens/selects the port; `exec_line` is its hardware-free core (send →
    // read_response → map to an exit code). These pin the mapping the outcome layer alone
    // can't, especially the `result.clamp(1, 123)` guard that keeps a device result off the
    // reserved 124/125 codes.

    #[test]
    fn exec_line_success_result_zero_is_exit_ok() {
        let cmd_id = exec_cmd_id();
        let mut sp = MockPort::new(shell_resp(cmd_id, 0, 0, true, "ok\n"));
        let mut dec = FrameDecoder::new();
        assert_eq!(exec_line(&mut sp, &mut dec, "/x", SHORT).unwrap(), EXIT_OK);
        // The command line was actually put on the wire before we read.
        assert!(!sp.written.is_empty());
    }

    #[test]
    fn exec_line_incomplete_response_is_exit_error() {
        // A dropped middle chunk (0 then 2) marks the response incomplete → EXIT_ERROR (1),
        // even though the `last` chunk reported result 0 (that result is unreliable).
        let cmd_id = exec_cmd_id();
        let mut bytes = shell_resp(cmd_id, 0, 0, false, "a");
        bytes.extend(shell_resp(cmd_id, 0, 2, true, "c"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        assert_eq!(exec_line(&mut sp, &mut dec, "/x", SHORT).unwrap(), EXIT_ERROR);
    }

    #[test]
    fn exec_line_clamps_device_result_into_1_123() {
        // A device result shares the exit-code byte with the reserved 124/125; a non-zero
        // result is clamped into 1..=123 so a device result of 124 surfaces as 123, never as
        // the reserved device-timeout code.
        for (result, expect) in [(5u8, 5u8), (123, 123), (124, 123), (125, 123), (255, 123)] {
            let cmd_id = exec_cmd_id();
            let mut sp = MockPort::new(shell_resp(cmd_id, result, 0, true, "boom\n"));
            let mut dec = FrameDecoder::new();
            assert_eq!(
                exec_line(&mut sp, &mut dec, "/x", SHORT).unwrap(),
                expect,
                "device result {result} must surface as {expect}"
            );
        }
    }

    #[test]
    fn exec_line_mute_device_is_device_timeout() {
        let mut sp = MockPort::new(Vec::new());
        let mut dec = FrameDecoder::new();
        assert_eq!(
            exec_line(&mut sp, &mut dec, "/x", SHORT).unwrap(),
            EXIT_DEVICE_TIMEOUT
        );
    }

    #[test]
    fn exec_line_foreign_device_is_protocol_mismatch() {
        // A device built from another checkout: its response (which would decode against the
        // wrong schema) is never interpreted, and the outcome is the lockstep exit code (125).
        let mut bytes = foreign_hello();
        bytes.extend(foreign_frame(
            MsgType::ShellResponse,
            1,
            &ShellResponse {
                cmd_id: exec_cmd_id(),
                result: 0,
                chunk: 0,
                last: true,
                text: "would mis-decode",
            },
        ));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        assert_eq!(
            exec_line(&mut sp, &mut dec, "/x", SHORT).unwrap(),
            EXIT_PROTOCOL_MISMATCH
        );
        // A matching Hello in the same position is harmless.
        let mut bytes = hello();
        bytes.extend(shell_resp(exec_cmd_id(), 0, 0, true, "ok"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        assert_eq!(exec_line(&mut sp, &mut dec, "/x", SHORT).unwrap(), EXIT_OK);
        // And `complete` maps it the same way.
        let mut dec = FrameDecoder::new();
        assert_eq!(
            complete_line(&mut MockPort::new(foreign_hello()), &mut dec, "/s", SHORT).unwrap(),
            EXIT_PROTOCOL_MISMATCH
        );
    }

    #[test]
    fn a_shared_gate_keeps_the_device_identity_across_commands() {
        // The interactive shell runs many commands over one link with one gate: the firmware the
        // foreign Hello named is still known when a later command's frames give the device away
        // (none of its frames is a Hello then). A matching Hello clears the verdict.
        let mut gate = LockstepGate::default();
        let mut sp = MockPort::new(foreign_hello());
        let mut dec = FrameDecoder::new();
        assert!(matches!(
            read_response_gated(&mut sp, &mut dec, &mut gate, 1, SHORT),
            ReadOutcome::Timeout { mismatch: Some(ref m) } if *m == foreign_mismatch()
        ));
        let mut sp = MockPort::new(foreign_frame(MsgType::Dropped, 2, &Dropped { count: 1 }));
        assert!(matches!(
            read_response_gated(&mut sp, &mut dec, &mut gate, 2, SHORT),
            ReadOutcome::Timeout { mismatch: Some(ref m) } if *m == foreign_mismatch()
        ));
        let mut bytes = hello();
        bytes.extend(shell_resp(3, 0, 0, true, "third"));
        let mut sp = MockPort::new(bytes);
        match read_response_gated(&mut sp, &mut dec, &mut gate, 3, SHORT) {
            ReadOutcome::Response(r) => assert_eq!(r.text, "third"),
            _ => panic!("a device reflashed from this checkout answers again"),
        }
        assert_eq!(gate.mismatch(), None);
    }

    // ---- hard I/O errors are tool errors (1), not device timeouts (124), CLI-8 ----

    /// A port whose reads fail outright (unplugged / EIO), unlike the mock's benign timeout.
    struct DeadPort;

    impl Read for DeadPort {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    impl Write for DeadPort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn exec_line_hard_io_error_is_a_tool_error() {
        // `main` maps an `Err` to EXIT_ERROR (1). It used to come back as "no response
        // (timeout)" → 124, sending CI after a mute firmware when the cable was the problem.
        let mut dec = FrameDecoder::new();
        assert!(exec_line(&mut DeadPort, &mut dec, "/x", SHORT).is_err());
    }

    #[test]
    fn read_some_treats_a_signal_interrupted_read_as_idle() {
        // EINTR from the port's poll (a terminal resize mid-read) is "nothing yet", not a dead
        // link; otherwise the stricter error mapping would end sessions on SIGWINCH.
        struct Interrupted;
        impl Read for Interrupted {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            }
        }
        let mut buf = [0u8; 8];
        assert_eq!(read_some(&mut Interrupted, &mut buf).unwrap(), 0);
        assert!(read_some(&mut DeadPort, &mut buf).is_err());
    }

    // ---- complete_line: the `tower complete` exit-code mapping (CLI-8) ----

    #[test]
    fn complete_line_no_answer_is_device_timeout() {
        // No answer at all is the documented 124; it used to exit 0, as if the device had
        // answered with nothing to complete.
        let mut sp = MockPort::new(Vec::new());
        let mut dec = FrameDecoder::new();
        assert_eq!(
            complete_line(&mut sp, &mut dec, "/s", SHORT).unwrap(),
            EXIT_DEVICE_TIMEOUT
        );
    }

    #[test]
    fn complete_line_maps_answer_mismatch_and_dead_port() {
        let cands: heapless::Vec<Candidate, 16> = heapless::Vec::new();
        let answer = frame(
            MsgType::ShellCompletions,
            0,
            &ShellCompletions {
                req_id: 1, // complete_line always asks with req_id 1
                token_start: 1,
                common_prefix: "",
                candidates: cands,
                more: false,
            },
        );
        let mut dec = FrameDecoder::new();
        assert_eq!(
            complete_line(&mut MockPort::new(answer), &mut dec, "/s", SHORT).unwrap(),
            EXIT_OK
        );
        let mut dec = FrameDecoder::new();
        let mismatched = MockPort::new(foreign_hello());
        assert_eq!(
            complete_line(&mut { mismatched }, &mut dec, "/s", SHORT).unwrap(),
            EXIT_PROTOCOL_MISMATCH
        );
        let mut dec = FrameDecoder::new();
        assert!(complete_line(&mut DeadPort, &mut dec, "/s", SHORT).is_err());
        // A line too long to frame is a tool error too, not a "timeout".
        let mut dec = FrameDecoder::new();
        let long = "x".repeat(300);
        assert!(complete_line(&mut MockPort::new(Vec::new()), &mut dec, &long, SHORT).is_err());
    }

    // ---- completion_start: device token_start can't panic the line editors (CLI-5) ----

    #[test]
    fn completion_start_rejects_unusable_token_starts() {
        assert_eq!(completion_start("/sys", 4, 1), Some(1));
        assert_eq!(completion_start("/sys", 4, 4), Some(4)); // empty token at the cursor
        assert_eq!(completion_start("/sys", 2, 3), None); // past the cursor
        assert_eq!(completion_start("/π", 3, 2), None); // inside a UTF-8 scalar
        assert_eq!(completion_start("ab", 2, 9), None); // past the end of the line
    }

    // ---- the Hello wait hands on every byte after the Hello (CLI-7) ----

    #[test]
    fn wait_for_hello_leaves_the_decoder_in_step_with_the_wire() {
        // The read that completes the Hello also carries a boot line and the first bytes of
        // the next frame; the rest of that frame comes in the next read. The wait used to
        // return mid-buffer and drop the remainder, so the next decode began mid-frame and that
        // frame was lost. Now the remainder still goes through `dec`, which resumes the frame.
        let mut bytes = hello();
        bytes.extend(log_frame(1, "boot line"));
        let split = bytes.len() + 5;
        bytes.extend(shell_resp(9, 0, 0, true, "split across reads"));
        let mut sp = MockPort::with_read_chunk(bytes, split);
        let mut dec = FrameDecoder::new();
        assert_eq!(wait_for_hello(&mut sp, &mut dec, SHORT), Readiness::Hello);
        let r = read_response(&mut sp, &mut dec, 9, SHORT).unwrap();
        assert_eq!(r.text, "split across reads");
    }

    #[test]
    fn hello_wait_transcript_holds_the_whole_boot() {
        // `logs --reset --send` renders the stream itself: the transcript is every byte read
        // during the wait (the Hello and the boot lines behind it) for a seamless replay.
        let mut bytes = hello();
        bytes.extend(log_frame(1, "early boot line"));
        let mut sp = MockPort::new(bytes.clone());
        let mut transcript = Vec::new();
        let readiness = crate::session::wait_for_hello_with(
            &mut sp,
            &mut FrameDecoder::new(),
            SHORT,
            |_| {},
            Some(&mut transcript),
        );
        assert_eq!(readiness, Readiness::Hello);
        assert_eq!(transcript, bytes);
    }

    // ---- response cap: a device that never sends `last` (hardening) ----

    #[test]
    fn endless_response_is_capped_and_flagged() {
        // Every matching chunk re-arms the idle timeout, so a device streaming without `last`
        // used to grow the buffer forever. Now it stops at the cap, flagged incomplete → exec 1.
        let cmd_id = exec_cmd_id();
        let piece = "x".repeat(200);
        let mut bytes = Vec::new();
        for i in 0..(crate::session::MAX_RESPONSE_BYTES / piece.len() + 2) {
            bytes.extend(shell_resp(cmd_id, 0, i as u16, false, &piece));
        }
        let r = read_response(
            &mut MockPort::new(bytes.clone()),
            &mut FrameDecoder::new(),
            cmd_id,
            SHORT,
        )
        .unwrap();
        assert!(r.incomplete);
        assert!(r.text.len() <= crate::session::MAX_RESPONSE_BYTES);
        assert_eq!(
            exec_line(&mut MockPort::new(bytes), &mut FrameDecoder::new(), "/x", SHORT).unwrap(),
            EXIT_ERROR
        );
    }

    // ---- render: seq-gap detection + Hello re-baseline ----

    // (render()/RenderState + ColorMode behavior are unit-tested in `render.rs`, next to
    // their now-private helpers. The byte-at-a-time decode below exercises the shared codec.)

    // ---- byte-at-a-time decode of a mixed stream ----

    #[test]
    fn frames_roundtrip_byte_at_a_time() {
        // Synthesize one of each rendered type, concatenate, and feed byte-by-byte through a
        // single decoder, asserting each decodes to the expected (type, seq) with a valid
        // payload. This pins the encode/decode contract shared with the firmware.
        let mut wire = Vec::new();
        wire.extend(hello()); // seq 0
        wire.extend(log_frame(1, "hi"));
        wire.extend(frame(
            MsgType::Event,
            2,
            &Event {
                name: "boot",
                fields: Default::default(),
            },
        ));
        wire.extend(frame(MsgType::Dropped, 3, &Dropped { count: 4 }));
        wire.extend(shell_resp(1, 0, 0, true, "ok"));

        let expected = [
            (MsgType::Hello, 0u16),
            (MsgType::Log, 1),
            (MsgType::Event, 2),
            (MsgType::Dropped, 3),
            (MsgType::ShellResponse, 0),
        ];
        let mut dec = FrameDecoder::new();
        let mut seen = Vec::new();
        for &b in &wire {
            if let Some(inner) = dec.push(b) {
                let (mt, seq, _payload) = decode_frame(inner).expect("valid frame");
                seen.push((mt, seq));
            }
        }
        assert_eq!(seen, expected);
    }

    // ---- request_completions ----

    #[test]
    fn request_completions_matches_req_id() {
        let mut cands: heapless::Vec<Candidate, 16> = heapless::Vec::new();
        cands
            .push(Candidate {
                text: "system",
                kind: CandidateKind::Menu,
            })
            .unwrap();
        let bytes = frame(
            MsgType::ShellCompletions,
            0,
            &ShellCompletions {
                req_id: 42,
                token_start: 1,
                common_prefix: "sys",
                candidates: cands,
                more: false,
            },
        );
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = request_completions(&mut sp, &mut dec, "/s", 2, 42, SHORT).unwrap();
        assert_eq!(r.token_start, 1);
        assert_eq!(r.common_prefix, "sys");
        assert_eq!(r.candidates.len(), 1);
        // And a ShellComplete was actually written to the wire.
        assert!(!sp.written.is_empty());
    }

    #[test]
    fn request_completions_ignores_wrong_req_id() {
        let cands: heapless::Vec<Candidate, 16> = heapless::Vec::new();
        let bytes = frame(
            MsgType::ShellCompletions,
            0,
            &ShellCompletions {
                req_id: 1,
                token_start: 0,
                common_prefix: "",
                candidates: cands,
                more: false,
            },
        );
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        // We asked for req_id 2, device answered 1 → no match, times out.
        assert!(request_completions(&mut sp, &mut dec, "", 0, 2, SHORT).is_none());
    }

    /// `tower chip` prints the device line, the chip id with its family, `Readout`, and (when `Get` answered)
    /// the bootloader version and commands.
    #[test]
    fn chip_report_lines() {
        let info = tower_flasher::chip::ChipInfo {
            id: 0x447,
            family: tower_flasher::target::chip_name(0x447),
            bootloader_version: Some(0x31),
            commands: vec![0x00, 0x02, 0x31],
            readout_protected: false,
        };
        assert_eq!(
            chip_report("/dev/ttyUSB0", &info),
            "Device  : /dev/ttyUSB0 @ 115200 baud, 8E1\n\
             Chip id : 0x447 (STM32L07x/L08x (e.g. STM32L083CZ))\n\
             Readout : not protected\n\
             BL ver  : 3.1\n\
             Commands: 0x00 0x02 0x31\n"
        );
        // `Get` unanswered: the bootloader lines are omitted, the id and protection still shown.
        let bare = tower_flasher::chip::ChipInfo {
            bootloader_version: None,
            commands: Vec::new(),
            readout_protected: true,
            ..info
        };
        assert_eq!(
            chip_report("/dev/ttyUSB0", &bare),
            "Device  : /dev/ttyUSB0 @ 115200 baud, 8E1\n\
             Chip id : 0x447 (STM32L07x/L08x (e.g. STM32L083CZ))\n\
             Readout : protected (RDP level 1)\n"
        );
    }

    // ---- readout protection: protect / unprotect (no hardware) ----

    /// `flash` / `erase` on a protected board: the actionable message replaces the library's,
    /// with no extra context; any other error keeps its context.
    #[test]
    fn boot_err_names_the_way_out_of_readout_protection() {
        use tower_flasher::error::Error;
        let msg = format!(
            "{:#}",
            boot_err(
                Error::ReadoutProtected {
                    operation: "flashing"
                },
                "flashing firmware"
            )
        );
        assert_eq!(msg, READOUT_PROTECTED_HINT);
        for needle in [
            "readout-protected",
            "`tower unprotect`",
            "flash + EEPROM",
            "re-flashed",
            "re-paired",
        ] {
            assert!(msg.contains(needle), "{needle}: {msg}");
        }
        let other = format!(
            "{:#}",
            boot_err(Error::BootloaderInit { attempts: 5 }, "erasing flash")
        );
        assert!(
            other.starts_with("erasing flash: could not enter the bootloader"),
            "{other}"
        );
    }

    /// Without `--yes`, a non-terminal stdin is refused (before any I/O); `--yes` or a terminal
    /// passes.
    #[test]
    fn confirmation_gate_needs_a_terminal_or_yes() {
        let err = confirmation_gate(false, false, "enable readout protection").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not a terminal") && msg.contains("--yes"), "{msg}");
        assert!(confirmation_gate(true, false, "x").is_ok());
        assert!(confirmation_gate(false, true, "x").is_ok());
        assert!(confirmation_gate(true, true, "x").is_ok());
    }

    /// Only the exact word confirms (whitespace around it ignored); "y", another word, an empty
    /// line and end of input are all a "no".
    #[test]
    fn read_confirmation_wants_the_exact_word() {
        let answer = |input: &str, word: &str| read_confirmation(&mut input.as_bytes(), word).unwrap();
        assert!(answer("yes\n", "yes"));
        assert!(answer("  erase \r\n", "erase"));
        assert!(!answer("y\n", "yes"));
        assert!(!answer("YES\n", "yes"));
        assert!(!answer("yes\n", "erase"), "unprotect wants `erase`, not `yes`");
        assert!(!answer("\n", "erase"));
        assert!(!answer("", "erase"), "end of input");
    }

    /// The notices say what the step does and what it costs, and end with the prompt that names
    /// the word to type.
    #[test]
    fn protection_notices_explain_the_cost() {
        let p = protect_notice("/dev/ttyUSB0");
        for needle in [
            "/dev/ttyUSB0",
            "RDP level 1",
            "data EEPROM",
            "`tower unprotect`",
            "mass-erases",
            "re-flashed and re-paired",
            "Level 2",
            "power-cycle",
        ] {
            assert!(p.contains(needle), "protect notice lacks {needle:?}");
        }
        assert!(p.ends_with("Type \"yes\" to protect this board: "), "{p}");

        let u = unprotect_notice("/dev/ttyUSB0");
        for needle in [
            "/dev/ttyUSB0",
            "ERASES",
            "data EEPROM",
            "radio keys, pairing, settings",
            "TX counters",
            "re-flashed",
            "re-paired",
        ] {
            assert!(u.contains(needle), "unprotect notice lacks {needle:?}");
        }
        assert!(
            u.ends_with("Type \"erase\" to erase and unprotect this board: "),
            "{u}"
        );
    }

    #[test]
    fn readout_line_wording() {
        assert_eq!(readout_line(true), "Readout : protected (RDP level 1)\n");
        assert_eq!(readout_line(false), "Readout : not protected\n");
    }
}
