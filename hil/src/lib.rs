//! Hardware-in-the-loop harness library for the TOWER firmware SDK. Bench inventory,
//! cabling, and setup: `hil/docs/bench.md`; the test catalogue is
//! `firmware/docs/test-plan.md`.
//!
//! The bench (see `hil.toml`):
//!
//! - a **TOWER Core Module** on a fixed FTDI port, wired to a **SEGGER J-Link** (SWD flash via
//!   `probe-rs`) and a **Nordic PPK2** (scriptable current supply/measure via the
//!   `ppk2d.py` sidecar), for the power tests; and
//! - a **TOWER Radio Dongle** on a second FTDI port, USB-powered, flashed over the UART
//!   bootloader with `tower flash`, for the smoke + radio-peer tests.
//!
//! Each slot is identity-checked against the live roster (the board-type tag in the FTDI's USB
//! serial number, or a pinned `usb_serial`) before anything is flashed (see [`Bench::resolve`]).
//!
//! The images under test are built from the firmware workspace located by
//! [`firmware_dir`] (default `../firmware`, the sibling directory in the monorepo; override
//! with `TOWER_FIRMWARE_DIR`).
//!
//! The value over the firmware's old `tools/hwtest` shell scripts (`strings | grep`) is that this decodes
//! the framed console **natively** with `tower-protocol`, so a test asserts on typed
//! [`Frame::Log`] / [`Frame::Event`] payloads and on **sequence gaps** (dropped/duplicated
//! frames the byte-grep can't see), against the exact same wire version the firmware ships.
//!
//! Nothing here runs against hardware unless a test explicitly opens a port; the integration
//! tests that need the bench are `#[ignore]`d so `cargo test` compiles them on any machine.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tower_protocol::mgmt::MgmtOp;
use tower_protocol::msg::{Event, Hello, Level, Log, MgmtRequest, MgmtResponse, Print, RadioStat, Uplink};
use tower_protocol::{FrameDecoder, MsgType, decode_frame, encode_frame};

/// The framed-console baud rate (USART1, 115200 8N1): the rate `tower logs` / the SDK use.
pub const CONSOLE_BAUD: u32 = 115_200;

// ---------------------------------------------------------------------------
// Firmware checkout (the harness builds the images it flashes)
// ---------------------------------------------------------------------------

/// Locate the firmware workspace the bench tests build images from (`just build
/// example …`). Default: `../firmware` relative to this crate, its sibling in the monorepo
/// (`hil/` and `firmware/` share one root). For any other layout, point `TOWER_FIRMWARE_DIR`
/// at a firmware checkout. Validated by the presence of its
/// `justfile` (the harness drives builds through `just`), so a wrong path fails HERE with
/// instructions instead of as a cryptic spawn error inside a test.
pub fn firmware_dir() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("TOWER_FIRMWARE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../firmware"),
    };
    if dir.join("justfile").is_file() {
        Ok(dir)
    } else {
        Err(format!(
            "HIL: no firmware workspace at {} (its justfile is missing). The default is the \
             `firmware` directory next to `hil/` (the monorepo layout); set \
             TOWER_FIRMWARE_DIR=/path/to/firmware for any other layout.",
            dir.display()
        ))
    }
}

// ---------------------------------------------------------------------------
// Bench fixture (hil.toml)
// ---------------------------------------------------------------------------

/// The bench roster, loaded from `hil.toml`. Serial names re-enumerate between sessions, so the
/// fixture is only the *starting* guess; [`Bench::resolve`] re-checks the live roster, verifies
/// each slot holds the right KIND of board, and fails fast with a human-readable message if a
/// board is missing, swapped, or not a TOWER board.
#[derive(Debug, Clone, Deserialize)]
pub struct Bench {
    pub core: Board,
    pub dongle: Board,
    pub ppk2: Ppk2Config,
}

/// One board's fixture entry.
#[derive(Debug, Clone, Deserialize)]
pub struct Board {
    /// FTDI serial device path, e.g. `/dev/cu.usbserial-2120`. After [`Bench::resolve`] this is the
    /// board's LIVE path (re-derived from `usb_serial` when that is pinned).
    pub serial: String,
    /// Optional: the board's USB serial number (the FTDI's iSerial, e.g. `hio-core-module-2416969388`
    /// as shown per port in the roster message). Stable across re-enumeration, so when set it is the
    /// key and `serial` is only a fallback/hint. Also the only way to vouch for a board on a
    /// third-party adapter whose serial number carries no TOWER board-type tag.
    #[serde(default)]
    pub usb_serial: Option<String>,
}

/// PPK2 supply/measure configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Ppk2Config {
    /// Source-measure voltage (mV). The bench standardises on **1800 mV** (the rail the documented
    /// STOP floor (5.1 µA, examples/lowpower.rs) was measured at; 3.0 V reads ~1 µA higher from
    /// ordinary CMOS leakage, honest at both rails). Pinning one rail keeps runs comparable.
    pub supply_mv: u32,
}

/// Which bench slot a board fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The TOWER Core Module slot (`[core]`).
    Core,
    /// The TOWER Radio Dongle slot (`[dongle]`).
    Dongle,
}

impl Role {
    /// The fixture section name (`core` / `dongle`).
    pub fn name(self) -> &'static str {
        match self {
            Role::Core => "core",
            Role::Dongle => "dongle",
        }
    }

    /// The board-type tag HARDWARIO programs into each board's on-board FT231X USB serial number
    /// (`hio-core-module-<id>`, `bc-usb-dongle-r1.0-<id>`). The OS reports it from the cached USB
    /// descriptor (no port is opened), so it identifies the board TYPE before anything is flashed.
    fn usb_tag(self) -> &'static str {
        match self {
            Role::Core => "core-module",
            Role::Dongle => "usb-dongle",
        }
    }

    fn board_name(self) -> &'static str {
        match self {
            Role::Core => "TOWER Core Module",
            Role::Dongle => "TOWER Radio Dongle",
        }
    }

    fn other(self) -> Role {
        match self {
            Role::Core => Role::Dongle,
            Role::Dongle => Role::Core,
        }
    }
}

/// One live serial port, as the OS enumerates it (the same list `tower devices` prints).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePort {
    /// Device path, e.g. `/dev/cu.usbserial-2110`.
    pub path: String,
    /// Enumerated as a USB serial port.
    pub usb: bool,
    /// The USB bridge's serial number (iSerial), when the OS reports one.
    pub usb_serial: Option<String>,
}

/// The live serial-port roster with USB identity, via the `serialport` enumeration (the very list
/// `tower devices` prints, plus each port's USB serial number). Enumeration reads cached USB
/// descriptors; it never opens a port. An enumeration failure yields an empty roster, which
/// [`Bench::resolve`] then reports as "none present": the right fail-fast.
pub fn live_ports() -> Vec<LivePort> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| match p.port_type {
            serialport::SerialPortType::UsbPort(info) => LivePort {
                path: p.port_name,
                usb: true,
                usb_serial: info.serial_number,
            },
            _ => LivePort {
                path: p.port_name,
                usb: false,
                usb_serial: None,
            },
        })
        .collect()
}

/// macOS lists every USB-UART twice: `/dev/cu.X` (callout) and `/dev/tty.X` (dial-in). They are
/// one device, so compare paths with the `tty.` spelling folded onto `cu.`.
fn same_device(a: &str, b: &str) -> bool {
    let fold = |p: &str| p.strip_prefix("/dev/tty.").map(|rest| format!("/dev/cu.{rest}"));
    a == b || fold(a).as_deref().unwrap_or(a) == fold(b).as_deref().unwrap_or(b)
}

/// Resolve one fixture slot against the live roster: returns the board's live path, or a one-line
/// problem. Identity, strongest first:
///
/// 1. `usb_serial` pinned → the port with exactly that USB serial number (path is only a hint);
/// 2. otherwise the fixture path must be present, and the port's USB serial number must carry THIS
///    role's board-type tag ([`Role::usb_tag`]): the other role's tag means the cables/slots are
///    swapped, no TOWER tag means some other FTDI sits at the path; either way `tower flash` would
///    erase the wrong device, so refuse. A USB port that reports no serial number can't be checked
///    and is accepted with a warning; a non-USB port (Bluetooth, PCI) is never a TOWER board.
fn resolve_board(role: Role, board: &Board, live: &[LivePort]) -> Result<String, String> {
    let r = role.name();
    if let Some(sn) = &board.usb_serial {
        if sn.contains(role.other().usb_tag()) {
            return Err(format!(
                "{r}: pinned usb_serial {sn} is a {}; it belongs in [{}]",
                role.other().board_name(),
                role.other().name()
            ));
        }
        let mut hits: Vec<&LivePort> = live
            .iter()
            .filter(|p| p.usb_serial.as_deref() == Some(sn))
            .collect();
        // One device = one hit after folding the macOS tty./cu. twins (prefer the cu. node).
        hits.sort_by_key(|p| p.path.starts_with("/dev/tty."));
        hits.dedup_by(|a, b| same_device(&a.path, &b.path));
        return match hits.as_slice() {
            [one] => Ok(one.path.clone()),
            [] => Err(format!(
                "{r}: no port with USB serial {sn} (fixture path hint {})",
                board.serial
            )),
            many => Err(format!(
                "{r}: USB serial {sn} matches several devices ({}): ambiguous, refusing to guess",
                many.iter()
                    .map(|p| p.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    }
    let Some(port) = live.iter().find(|p| same_device(&p.path, &board.serial)) else {
        return Err(format!("{r}: fixture serial {} not present", board.serial));
    };
    if !port.usb {
        return Err(format!(
            "{r}: {} is not a USB serial port, so not a TOWER board",
            port.path
        ));
    }
    match &port.usb_serial {
        Some(sn) if sn.contains(role.usb_tag()) => Ok(board.serial.clone()),
        Some(sn) if sn.contains(role.other().usb_tag()) => Err(format!(
            "{r}: {} is a {} (USB serial {sn}), not a {}: cables or hil.toml slots swapped?",
            board.serial,
            role.other().board_name(),
            role.board_name()
        )),
        Some(sn) => Err(format!(
            "{r}: {} (USB serial {sn}) is not a {}: refusing to flash an unknown device. If it IS \
             the board (e.g. on a third-party adapter), pin it: usb_serial = \"{sn}\" under [{r}]",
            board.serial,
            role.board_name()
        )),
        None => {
            eprintln!(
                "HIL: warning: {r} port {} reports no USB serial number; its board type can't be \
                 verified; trusting the fixture path",
                board.serial
            );
            Ok(board.serial.clone())
        }
    }
}

impl Bench {
    /// Load the fixture from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("HIL: cannot read fixture {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("HIL: bad fixture {}: {e}", path.display()))
    }

    /// The default fixture location at the repo root (`hil.toml`, next to `Cargo.toml`).
    pub fn load_default() -> Result<Self, String> {
        Self::load(concat!(env!("CARGO_MANIFEST_DIR"), "/hil.toml"))
    }

    /// The fixture entry for `role`.
    pub fn board(&self, role: Role) -> &Board {
        match role {
            Role::Core => &self.core,
            Role::Dongle => &self.dongle,
        }
    }

    /// Re-resolve both boards against the LIVE port roster, since FTDI names re-enumerate, and
    /// verify each slot's identity (`resolve_board`: pinned USB serial, else the board-type tag
    /// in the port's USB serial number) and that core ≠ dongle. On success the fixture paths are
    /// rewritten to the live ones. On any problem this returns a fail-fast message naming it plus
    /// every port present (with its USB serial), so the operator fixes `hil.toml` (or the cabling)
    /// immediately instead of watching a test hang on a dead port (or erase the wrong board).
    pub fn resolve(&mut self) -> Result<(), String> {
        self.resolve_against(&live_ports())
    }

    fn resolve_against(&mut self, live: &[LivePort]) -> Result<(), String> {
        let core = resolve_board(Role::Core, &self.core, live);
        let dongle = resolve_board(Role::Dongle, &self.dongle, live);
        let mut problems: Vec<String> = [&core, &dongle]
            .into_iter()
            .filter_map(|r| r.as_ref().err().cloned())
            .collect();
        if let (Ok(c), Ok(d)) = (&core, &dongle)
            && same_device(c, d)
        {
            problems.push(format!(
                "core and dongle both resolve to {c}; one board can't fill both slots"
            ));
        }
        if problems.is_empty() {
            self.core.serial = core.unwrap_or_default();
            self.dongle.serial = dongle.unwrap_or_default();
            return Ok(());
        }
        Err(format!(
            "HIL bench roster mismatch: re-resolve ports (they re-enumerate each session):\n{}\n\
             ports present now:\n{}\n\
             Fix hil.toml (serial paths, or pin usb_serial) or the cabling, then re-run.",
            problems
                .iter()
                .map(|p| format!("  {p}"))
                .collect::<Vec<_>>()
                .join("\n"),
            roster_listing(live)
        ))
    }

    /// Flash `bin` to the board in `role` over the UART bootloader (`tower flash`), re-verifying the
    /// slot's identity against the live roster IMMEDIATELY before the erase, since a build can take long
    /// enough for a cable to be moved. Sequential by design (concurrent FTDI flashing is unreliable
    /// (Write-Memory timeouts / re-enumeration, see firmware/tools/hwtest/README.md).
    pub fn flash(&self, role: Role, bin: &Path) -> Result<(), String> {
        let path = resolve_board(role, self.board(role), &live_ports())
            .map_err(|e| format!("HIL: refusing to flash: {e}"))?;
        let out = Command::new("tower")
            .args(["-d", &path, "flash"])
            .arg(bin)
            .output()
            .map_err(|e| format!("HIL: spawn `tower flash`: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "HIL: `tower -d {path} flash {}` failed:\n{}\n{}",
                bin.display(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    }
}

/// The roster as the operator needs to see it: each live port with its USB serial number.
fn roster_listing(live: &[LivePort]) -> String {
    if live.is_empty() {
        return "  (none; is any board plugged in?)".to_string();
    }
    live.iter()
        .map(|p| match (&p.usb_serial, p.usb) {
            (Some(sn), _) => format!("  {}  (USB serial {sn})", p.path),
            (None, true) => format!("  {}  (USB, no serial number)", p.path),
            (None, false) => format!("  {}  (not USB)", p.path),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Startup roster resolution: load the default fixture, then resolve + identity-check both boards.
/// Every integration test calls this first so a missing or mis-cabled board fails with the roster
/// message rather than a serial timeout ten seconds later (or a flash of the wrong device).
pub fn bench_or_fail() -> Bench {
    let mut bench = Bench::load_default().unwrap_or_else(|e| panic!("{e}"));
    if let Err(e) = bench.resolve() {
        panic!("{e}");
    }
    bench
}

/// Roster load for the **power** group: deliberately does NOT resolve or require any FTDI console
/// port. The power test flashes the Core over SWD (J-Link, found by probe-rs `--chip`) and
/// measures via the PPK2 (found by USB VID), so it needs no FTDI. The supported power topology is a
/// PPK2-powered Core whose FTDI adapter is **data-only** (no VBUS) and may stay plugged; a
/// VBUS-carrying USB back-feeds VDD through the LDO and is caught by the test's plausibility floor
/// (the PPK2 then reads ~0 µA), not by looking at the port roster.
pub fn bench_or_fail_power() -> Bench {
    Bench::load_default().unwrap_or_else(|e| panic!("{e}"))
}

/// Build a firmware target into the checkout's `target/firmware.bin` with `just build <kind>
/// <name>` (`kind` = `example` | `app`) and return the bin path. (Kept as a `just` call rather than
/// re-implementing objcopy: the firmware justfile owns the build.)
///
/// `TOWER_FEATURES` is ALWAYS set explicitly (to `features`, empty meaning "no features"): the
/// justfile reads it with `env_var_or_default`, so an operator's exported `TOWER_FEATURES` (say
/// `role-node` left over from a manual flash) would otherwise silently change the gateway build.
pub fn build_image(kind: &str, name: &str, features: &str) -> Result<PathBuf, String> {
    let repo = firmware_dir()?;
    let out = Command::new("just")
        .current_dir(&repo)
        .args(["build", kind, name])
        .env("TOWER_FEATURES", features)
        .output()
        .map_err(|e| format!("HIL: spawn `just build {kind} {name}`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "HIL: `just build {kind} {name}` (features {features:?}) failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // `just build` writes the merged image to target/firmware.bin (see the justfile `bin`).
    Ok(repo.join("target/firmware.bin"))
}

// ---------------------------------------------------------------------------
// Native framed-console decode
// ---------------------------------------------------------------------------

/// A decoded console frame (the typed subset the tests assert on). Owned copies of the borrowed
/// `tower-protocol` message fields, so a frame outlives the decoder's buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Boot banner: firmware name + version and the per-boot session id. (Its frame carried
    /// this harness's schema tag, or [`Console::next`] would have failed on it.)
    Hello {
        firmware_name: String,
        firmware_version: String,
        session_id: u32,
    },
    /// A `log::` record.
    Log {
        level: Level,
        uptime_us: u64,
        module: String,
        message: String,
    },
    /// A `println!`-style print.
    Print { text: String },
    /// A structured `console::event(...)`.
    Event {
        name: String,
        fields: Vec<(String, String)>,
    },
    /// The writer's dropped-frame marker (queue overflow / unplugged drain).
    Dropped { count: u32 },
    /// A forwarded radio uplink from the gateway firmware: the decrypted,
    /// authenticated payload verbatim, plus reception metadata.
    Uplink {
        src: u32,
        counter: u32,
        rssi: i16,
        lqi: u8,
        data: Vec<u8>,
    },
    /// One chunk of a management reply; reassemble by `req_id` (see
    /// [`Console::mgmt_roundtrip`]).
    Mgmt {
        req_id: u16,
        result: u8,
        chunk: u16,
        last: bool,
        data: Vec<u8>,
    },
    /// One chunk of a shell-command reply; reassemble by `cmd_id` (see
    /// [`Console::shell_roundtrip`]). `text` chunks concatenate; `result` is authoritative on `last`.
    Shell {
        cmd_id: u16,
        result: u8,
        chunk: u16,
        last: bool,
        text: String,
    },
    /// A radio-diagnostics sample: ambient channel RSSI or a TX report.
    Stat(RadioStat),
    /// A frame whose `MsgType` we don't model here (e.g. shell traffic), kept so seq accounting
    /// stays exact.
    Other(MsgType),
}

/// The framed console over one serial port, with native `tower-protocol` decode + seq tracking.
///
/// Read [`next`](Self::next) (blocking up to a timeout) or [`collect_for`](Self::collect_for)
/// (drain a fixed window). Every decoded frame's 16-bit `seq` is checked for **gaps** (the number
/// of frames the device skipped, i.e. dropped in transit or by the writer's queue), surfaced via
/// [`seq_gaps`](Self::seq_gaps): something `strings | grep` fundamentally cannot see. Frames that
/// fail to decode are counted ([`decode_errors`](Self::decode_errors)) and named in timeout
/// diagnostics; a CRC-valid frame carrying another schema tag (a firmware built from another
/// checkout) fails [`next`](Self::next) fast.
pub struct Console {
    // Option only so the host unit tests can build a port-less Console (`for_test`). The reset
    // pulse drives the modem lines on THIS handle (Console::reset_into_app); serialport 4.9 flocks
    // the tty on every open, so a second open while the console is attached fails with EWOULDBLOCK.
    port: Option<Box<dyn serialport::SerialPort>>,
    decoder: FrameDecoder,
    last_seq: Option<u16>,
    seq_gaps: u32,
    /// `session_id` of the most recent `Hello` (a per-boot counter; the same id re-sent means a
    /// USB re-plug within one boot, a new id a reboot).
    session: Option<u32>,
    /// Complete frames that failed to decode (bad CRC / COBS / type …): counted, never silent.
    decode_errors: u32,
    last_decode_error: Option<String>,
    /// Host→device frame counter for the writer helpers (the gateway tests),
    /// per-link, restarting at 0 like every sender's.
    tx_seq: u16,
}

impl Console {
    /// Open `path` at the console baud with a short read timeout (so reads poll rather than block
    /// forever), then drive the modem lines to the RUN baseline (RTS + DTR asserted). The bridge's
    /// RTS/DTR drive NRST/BOOT0 through the board's transistor pair (the tower-flasher / tower-cli wiring:
    /// only RTS-asserted + DTR-released holds RESET); the OS asserts both on open anyway, and
    /// re-driving them explicitly (like tower-cli's `open_console`) guarantees merely attaching
    /// can't leave the MCU held in reset, and does not perturb a running app.
    pub fn open(path: &str) -> Result<Self, String> {
        #[allow(unused_mut)]
        let mut port = serialport::new(path, CONSOLE_BAUD)
            .timeout(Duration::from_millis(50))
            .open_native()
            .map_err(|e| format!("HIL: open console {path}: {e}"))?;
        // serialport-rs opens TTYs with TIOCEXCL by default, which makes tower-flasher's re-open of
        // the same tty (reset_into_app's NRST pulse, while the console stays attached to catch the
        // boot burst) fail with EBUSY. Clear it: tower-flasher only toggles modem lines, it never reads,
        // so sharing the tty with the console reader is safe.
        #[cfg(unix)]
        port.set_exclusive(false)
            .map_err(|e| format!("HIL: clear TIOCEXCL on {path}: {e}"))?;
        // Run baseline (RTS=T, DTR=T), with tower-flasher's tuned settle.
        serialport::SerialPort::write_request_to_send(&mut port, true)
            .and_then(|()| serialport::SerialPort::write_data_terminal_ready(&mut port, true))
            .map_err(|e| format!("HIL: drive run-state lines on {path}: {e}"))?;
        std::thread::sleep(tower_flasher::port::RUN_SETTLE);
        Ok(Self {
            port: Some(Box::new(port)),
            decoder: FrameDecoder::new(),
            last_seq: None,
            seq_gaps: 0,
            session: None,
            decode_errors: 0,
            last_decode_error: None,
            tx_seq: 0,
        })
    }

    /// Write one host→device frame (the harness's only TX path). Used to drive the management
    /// channel and the shell from tests.
    pub fn send_frame<T: serde::Serialize>(&mut self, msg_type: MsgType, payload: &T) -> Result<(), String> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(msg_type, self.tx_seq, payload, &mut buf)
            .map_err(|e| format!("HIL: encode {msg_type:?}: {e}"))?;
        self.tx_seq = self.tx_seq.wrapping_add(1);
        let port = self.port.as_mut().ok_or("HIL: console handle loaned out")?;
        port.write_all(&buf[..n])
            .map_err(|e| format!("HIL: write: {e}"))?;
        port.flush().map_err(|e| format!("HIL: flush: {e}"))
    }

    /// Send a `ShellCommand` (the console shell, not the radio remote shell).
    pub fn send_shell(&mut self, cmd_id: u16, line: &str) -> Result<(), String> {
        self.send_frame(
            MsgType::ShellCommand,
            &tower_protocol::msg::ShellCommand { cmd_id, line },
        )
    }

    /// One shell round-trip: send `line` as a `ShellCommand`, reassemble the chunked
    /// `ShellResponse` for `cmd_id`. Returns `(result_code, concatenated text)`. `timeout` is an
    /// idle deadline (reset per matching chunk). Non-matching frames pass through seq accounting.
    pub fn shell_roundtrip(
        &mut self,
        cmd_id: u16,
        line: &str,
        timeout: Duration,
    ) -> Result<(u8, String), String> {
        self.send_shell(cmd_id, line)?;
        let mut text = String::new();
        let mut deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.next(remaining)? {
                Some(Frame::Shell {
                    cmd_id: c,
                    result,
                    text: t,
                    last,
                    ..
                }) if c == cmd_id => {
                    deadline = Instant::now() + timeout;
                    text.push_str(&t);
                    if last {
                        return Ok((result, text));
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        Err(format!(
            "HIL: shell cmd {cmd_id} got no complete reply{}",
            self.decode_note()
        ))
    }

    /// One management round-trip: send `op`, reassemble the chunked reply for `req_id`.
    /// Returns `(result_code, concatenated record bytes)`. `timeout` is an idle deadline
    /// (reset per matching chunk). Frames for other req_ids / other types pass through
    /// the normal seq accounting and are otherwise ignored.
    pub fn mgmt_roundtrip(
        &mut self,
        req_id: u16,
        op: &MgmtOp<'_>,
        timeout: Duration,
    ) -> Result<(u8, Vec<u8>), String> {
        self.send_frame(
            MsgType::MgmtRequest,
            &MgmtRequest {
                req_id,
                op: op.clone(),
            },
        )?;
        let mut data = Vec::new();
        let mut deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.next(remaining)? {
                Some(Frame::Mgmt {
                    req_id: r,
                    result,
                    data: d,
                    last,
                    ..
                }) if r == req_id => {
                    deadline = Instant::now() + timeout;
                    data.extend_from_slice(&d);
                    if last {
                        return Ok((result, data));
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        Err(format!(
            "HIL: mgmt req {req_id} got no complete reply{}",
            self.decode_note()
        ))
    }

    /// Pulse NRST so the board reboots into its application, WITHOUT re-opening the tty (a second
    /// open fails on serialport 4.9, which flocks the tty on every open). The reset only toggles
    /// modem lines, so the console's 115200 8N1 frame format is untouched.
    ///
    /// Line sequence + timings are tower-flasher's `Port::reset_into_app` exactly (RTS=T/DTR=F asserts
    /// RESET with BOOT0 low for `RESET_PULSE`, RTS=F releases, `APP_BOOT` settle; the 1 µF NRST
    /// cap makes the ordering load-bearing), plus the one thing tower-flasher has no hook for: the
    /// input buffer is cleared WHILE the chip is held in reset (as tower-cli's
    /// `pulse_reset_into_app` does). Clearing after release would race the boot bytes
    /// we want; not clearing lets pre-reset frames (the gateway's 1 Hz RadioStat, a post-flash
    /// boot's banner) satisfy the next `wait_for` and make the new boot's `seq` 0 look like ~65k
    /// gaps.
    /// Decode + seq state is re-baselined too: everything after this belongs to the new boot.
    pub fn reset_into_app(&mut self) -> Result<(), String> {
        let port = self.port.as_mut().ok_or("HIL: console has no port")?;
        let res = pulse_reset_clearing_input(port.as_mut());
        self.resync();
        res
    }

    /// Total sequence gaps seen so far (frames the device advanced its `seq` past but we never
    /// received: dropped in transit or by the console writer under backpressure/unplug).
    pub fn seq_gaps(&self) -> u32 {
        self.seq_gaps
    }

    /// Complete frames that failed to decode so far (bad CRC / COBS / unknown type …).
    pub fn decode_errors(&self) -> u32 {
        self.decode_errors
    }

    /// `session_id` of the most recent `Hello` seen, if any.
    pub fn session_id(&self) -> Option<u32> {
        self.session
    }

    /// `""`, or a parenthetical naming the undecodable frames, appended to timeout / failure
    /// messages so a wall of corrupt frames reads as a diagnosable cause, not "did it boot?".
    fn decode_note(&self) -> String {
        match &self.last_decode_error {
            Some(last) if self.decode_errors > 0 => format!(
                " ({} undecodable console frame(s) dropped meanwhile, last: {last})",
                self.decode_errors
            ),
            _ => String::new(),
        }
    }

    /// Discard any partial frame + reset seq accounting (e.g. right after a device reset, where
    /// the `seq` counter restarts from a fresh boot).
    pub fn resync(&mut self) {
        self.decoder.reset();
        self.last_seq = None;
    }

    /// Read the next complete frame, waiting up to `timeout`. `Ok(None)` means the window elapsed
    /// with no full frame (idle line). A corrupt frame (bad CRC/COBS/type) is skipped and counted
    /// ([`decode_errors`](Self::decode_errors)); the next `0x00` resynchronizes, exactly like the
    /// CLI. A **CRC-valid** frame carrying another schema tag (the firmware was built from another
    /// checkout than this harness) is the lockstep break, so it is an `Err` at once, naming the
    /// firmware when the frame is its `Hello` (otherwise every test would just report "no Hello").
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Frame>, String> {
        let deadline = Instant::now() + timeout;
        let mut byte = [0u8; 1];
        let port = self.port.as_mut().ok_or("HIL: console has no port")?;
        while Instant::now() < deadline {
            match port.read(&mut byte) {
                Ok(0) => continue,
                Ok(_) => {
                    // Feed one byte; on a frame boundary decode + account for seq. Build the OWNED
                    // Frame + copy out `seq` inside the decoder borrow, so the borrow ends before
                    // `on_frame` takes its own `&mut self`.
                    let Some(decoded) = self.decoder.push(byte[0]).map(|inner| {
                        decode_frame(inner)
                            .map(|(msg_type, seq, payload)| (seq, to_frame(msg_type, payload)))
                            .map_err(|e| match e {
                                // Only ever reported for a CRC-valid frame (tower-protocol checks
                                // the CRC first), so it is never line noise.
                                tower_protocol::Error::SchemaMismatch { got } => {
                                    Err(lockstep_break(got, inner))
                                }
                                other => Ok(other),
                            })
                    }) else {
                        continue;
                    };
                    match decoded {
                        Ok((seq, frame)) => {
                            self.on_frame(seq, &frame);
                            return Ok(Some(frame));
                        }
                        Err(Err(lockstep)) => return Err(lockstep),
                        Err(Ok(e)) => {
                            self.decode_errors = self.decode_errors.saturating_add(1);
                            self.last_decode_error = Some(format!("{e:?}"));
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(format!("HIL: console read error: {e}")),
            }
        }
        Ok(None)
    }

    /// Drain frames for `window`, returning everything decoded. Useful to snapshot a boot burst.
    pub fn collect_for(&mut self, window: Duration) -> Result<Vec<Frame>, String> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.next(remaining.min(Duration::from_millis(200)))? {
                Some(f) => out.push(f),
                None => continue,
            }
        }
        Ok(out)
    }

    /// Wait until a frame satisfies `pred` (returning it), or `timeout` elapses (`Ok(None)`).
    /// The canonical smoke assertion: "await the `ALL PASS` verdict line."
    ///
    /// On a timeout with corrupt frames dropped during the wait, says so on stderr (shown with the
    /// failing test's output); the caller's "no verdict" message alone would hide the cause.
    pub fn wait_for(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&Frame) -> bool,
    ) -> Result<Option<Frame>, String> {
        let deadline = Instant::now() + timeout;
        let errors_before = self.decode_errors;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if let Some(f) = self.next(remaining.min(Duration::from_millis(500)))?
                && pred(&f)
            {
                return Ok(Some(f));
            }
        }
        if self.decode_errors > errors_before {
            eprintln!("HIL: wait_for timed out after {timeout:?}{}", self.decode_note());
        }
        Ok(None)
    }

    /// Convenience: await a Log/Print frame whose text contains `needle` (the framed equivalent
    /// of the old `strings | grep`, but on decoded ASCII with version + CRC already validated).
    pub fn wait_for_text(&mut self, timeout: Duration, needle: &str) -> Result<bool, String> {
        Ok(self
            .wait_for(timeout, |f| frame_text(f).is_some_and(|t| t.contains(needle)))?
            .is_some())
    }

    /// Per-frame bookkeeping: a `Hello` opens a new device session (the firmware restarts its
    /// `seq` on boot and re-emits `Hello` on a USB re-plug), so re-baseline the gap tracking on
    /// it, exactly like tower-cli (`render.rs`), instead of counting the jump as ~65k lost frames.
    fn on_frame(&mut self, seq: u16, frame: &Frame) {
        if let Frame::Hello { session_id, .. } = frame {
            self.last_seq = None;
            self.session = Some(*session_id);
        }
        self.account_seq(seq);
    }

    fn account_seq(&mut self, seq: u16) {
        if let Some(prev) = self.last_seq {
            // Frames the device emitted between `prev` and `seq` that we didn't get. `seq` wraps
            // at u16; `wrapping_sub` gives the forward distance. Distance 1 = contiguous.
            let gap = seq.wrapping_sub(prev).wrapping_sub(1);
            self.seq_gaps = self.seq_gaps.saturating_add(gap as u32);
        }
        self.last_seq = Some(seq);
    }

    /// Test-only: a `Console` with no serial port, so the pure seq-accounting path
    /// (`account_seq` / [`seq_gaps`](Self::seq_gaps)) can be exercised on the SHIPPED code (not
    /// an inline copy) without opening a tty. Never compiled into a non-test build.
    #[cfg(test)]
    fn for_test() -> Self {
        Self {
            port: None,
            decoder: FrameDecoder::new(),
            last_seq: None,
            seq_gaps: 0,
            session: None,
            decode_errors: 0,
            last_decode_error: None,
            tx_seq: 0,
        }
    }
}

/// The NRST pulse of [`Console::reset_into_app`] on a borrowed handle: tower-flasher's line order and
/// timings, with the input buffer cleared while RESET is held. On a line-write failure the run
/// state (RTS=T, DTR=T) is restored best-effort, as tower-flasher does, so the board isn't left in
/// reset.
fn pulse_reset_clearing_input(sp: &mut dyn serialport::SerialPort) -> Result<(), String> {
    let mut pulse = || -> serialport::Result<()> {
        sp.write_request_to_send(true)?;
        sp.write_data_terminal_ready(false)?; // RESET asserted, BOOT0 low
        std::thread::sleep(tower_flasher::port::RESET_PULSE);
        sp.clear(serialport::ClearBuffer::Input)?; // drop pre-reset bytes while held in reset
        sp.write_request_to_send(false) // RESET released -> boot main flash
    };
    if let Err(e) = pulse() {
        let _ = sp.write_request_to_send(true);
        let _ = sp.write_data_terminal_ready(true);
        return Err(format!("HIL: reset-into-app: {e}"));
    }
    std::thread::sleep(tower_flasher::port::APP_BOOT);
    Ok(())
}

/// The fail-fast message for a CRC-valid frame carrying another schema tag `got`. The tag is a
/// hash: it can't say which side is older, so the message names the firmware (from its `Hello`,
/// when `inner` is one) and asks for both to come from this checkout.
fn lockstep_break(got: u16, inner: &[u8]) -> String {
    let device = tower_protocol::decode_foreign_hello(inner)
        .map(|h| format!(" ({} {})", h.firmware_name, h.firmware_version))
        .unwrap_or_default();
    format!(
        "HIL: the device's console frames carry schema tag {got:#06x}, but this harness decodes \
         {:#06x}: the firmware{device} and the harness were built from different checkouts, a \
         lockstep break (postcard would silently mis-decode). Rebuild and reflash the firmware \
         from this checkout, so it and the harness share the one crates/protocol.",
        tower_protocol::CONSOLE_TAG
    )
}

/// Decode a `(MsgType, payload)` into an owned [`Frame`]. Unknown/unmodelled types become
/// [`Frame::Other`] so seq accounting is still exact.
fn to_frame(msg_type: MsgType, payload: &[u8]) -> Frame {
    match msg_type {
        MsgType::Hello => match postcard::from_bytes::<Hello>(payload) {
            Ok(h) => Frame::Hello {
                firmware_name: h.firmware_name.to_string(),
                firmware_version: h.firmware_version.to_string(),
                session_id: h.session_id,
            },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::Log => match postcard::from_bytes::<Log>(payload) {
            Ok(l) => Frame::Log {
                level: l.level,
                uptime_us: l.uptime_us,
                module: l.module.to_string(),
                message: l.message.to_string(),
            },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::Print => match postcard::from_bytes::<Print>(payload) {
            Ok(p) => Frame::Print {
                text: p.text.to_string(),
            },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::Event => match postcard::from_bytes::<Event>(payload) {
            Ok(e) => Frame::Event {
                name: e.name.to_string(),
                fields: e
                    .fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::Dropped => match postcard::from_bytes::<tower_protocol::msg::Dropped>(payload) {
            Ok(d) => Frame::Dropped { count: d.count },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::Uplink => match postcard::from_bytes::<Uplink>(payload) {
            Ok(u) => Frame::Uplink {
                src: u.src,
                counter: u.counter,
                rssi: u.rssi,
                lqi: u.lqi,
                data: u.data.to_vec(),
            },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::ShellResponse => match postcard::from_bytes::<tower_protocol::msg::ShellResponse>(payload) {
            Ok(s) => Frame::Shell {
                cmd_id: s.cmd_id,
                result: s.result,
                chunk: s.chunk,
                last: s.last,
                text: s.text.to_string(),
            },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::MgmtResponse => match postcard::from_bytes::<MgmtResponse>(payload) {
            Ok(m) => Frame::Mgmt {
                req_id: m.req_id,
                result: m.result,
                chunk: m.chunk,
                last: m.last,
                data: m.data.to_vec(),
            },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::RadioStat => match postcard::from_bytes::<RadioStat>(payload) {
            Ok(s) => Frame::Stat(s),
            Err(_) => Frame::Other(msg_type),
        },
        other => Frame::Other(other),
    }
}

/// The human-readable text of a Log/Print frame, if any (for `grep`-style assertions).
pub fn frame_text(f: &Frame) -> Option<&str> {
    match f {
        Frame::Log { message, .. } => Some(message),
        Frame::Print { text } => Some(text),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Reset over the FTDI modem lines (tower-flasher): RTS drives NRST, DTR drives BOOT0
// ---------------------------------------------------------------------------

/// Pulse NRST so the board reboots into its application, using tower-flasher's tuned reset sequence
/// (the 1 µF NRST cap makes the line ordering load-bearing; we reuse tower-flasher's, not a
/// hand-rolled one).
/// For a port with NO console attached (it opens the tty itself); with a [`Console`] open on the
/// port use [`Console::reset_into_app`] instead.
pub fn reset_into_app(path: &str) -> Result<(), String> {
    let mut port =
        tower_flasher::port::Port::open(path).map_err(|e| format!("HIL: tower-flasher open {path}: {e}"))?;
    port.reset_into_app()
        .map_err(|e| format!("HIL: reset-into-app {path}: {e}"))
}

// ---------------------------------------------------------------------------
// PPK2 sidecar client (line-JSON over stdio, ppk2d.py)
// ---------------------------------------------------------------------------

/// A client for the `ppk2d.py` current sidecar. Protocol is **line-JSON over stdio**: the sidecar
/// first writes one startup line naming its backend, then we write one command per line and read
/// one JSON reply per line. Every reply carries `"ok"`; anything but `"ok": true` is an error
/// (its `"error"` text is surfaced). Commands:
///
/// - startup                    → `{"ok":true,"backend":"real"|"stub","port":…}` (or an error +
///   exit when no PPK2 initialises; the stub is opt-in only, `TOWER_PPK2_STUB=1`)
/// - `{"cmd":"on","mv":<mV>}`:    enable source-measure at `<mV>`
/// - `{"cmd":"off"}`:             disable the supply
/// - `{"cmd":"cycle","mv":<mV>}`: power-cycle (off→settle→on) at `<mV>` (clears the probe's
///   debug-domain residual current: confounder #1)
/// - `{"cmd":"avg","ms":<ms>}`:   sample for `<ms>` → `{"median_ua","mean_ua","samples"}`
///
/// The sidecar encodes the three known confounders (see `ppk2d.py`); this client speaks the
/// protocol with a deadline on every reply (a wedged sidecar must fail the test, not hang it). It
/// is only spawned by the `power`-gated tests.
pub struct Ppk2 {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    /// Reply lines, read by a helper thread so every wait can time out.
    replies: std::sync::mpsc::Receiver<String>,
    backend: String,
}

/// Reject any current reading above this: mid-SWD-flash the PPK2's CDC can desync and report
/// garbage tens of mA (confounder #3). A real deep-sleep floor is single-digit µA, so >50 mA is
/// never a valid STOP reading; treat it as a desync and fail loudly rather than pass/fail on noise.
pub const PPK2_SANE_MAX_UA: f64 = 50_000.0;

/// Sidecar startup budget: a venv re-exec, the `ppk2-api` import, and up to two CDC candidates'
/// stale-stream drain + calibration-read retries (~3 s each): generous, but finite.
const PPK2_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Reply budget for a non-sampling command (`on` / `off` / `cycle`, ~0.3 s settle inside).
const PPK2_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// One `avg` window from the PPK2: both statistics, because they answer different questions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ppk2Reading {
    /// Median of the samples: the quiescent STOP **floor** (robust to the brief periodic wake
    /// spikes).
    pub median_ua: f64,
    /// Arithmetic mean: the true average draw (what a battery sees), wake spikes included.
    pub mean_ua: f64,
    /// Plausible samples the statistics were computed over.
    pub samples: u64,
}

impl Ppk2 {
    /// Spawn the sidecar (`python3 ppk2d.py`) and read its startup line. `python3` is the
    /// launcher; the path is resolved relative to this crate so it works from any CWD. Fails
    /// unless the sidecar drives a REAL PPK2; the modelled stub (a plausible fake ~12 µA) is
    /// accepted only when `TOWER_PPK2_STUB=1` explicitly opts in to a plumbing run.
    pub fn spawn() -> Result<Self, String> {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/ppk2d.py");
        let mut child = Command::new("python3")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("HIL: spawn ppk2d.py: {e}"))?;
        let stdin = child.stdin.take().ok_or("HIL: ppk2d stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("HIL: ppk2d stdout unavailable")?;
        let (tx, replies) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut ppk2 = Self {
            child,
            stdin,
            replies,
            backend: String::new(),
        };
        let hello = ppk2.recv(PPK2_STARTUP_TIMEOUT, "startup")?;
        ppk2.backend = json_str(&hello, "backend").unwrap_or_default();
        let stub_ok = std::env::var("TOWER_PPK2_STUB").is_ok_and(|v| v == "1");
        match ppk2.backend.as_str() {
            "real" => {}
            "stub" if stub_ok => eprintln!(
                "HIL: ppk2d is the MODELLED STUB (TOWER_PPK2_STUB=1), a plumbing run; its \
                 readings are fake and prove nothing about the board"
            ),
            other => {
                return Err(format!(
                    "HIL: ppk2d backend {other:?} is not a real PPK2 (startup: {hello}); refusing \
                     to measure; set TOWER_PPK2_STUB=1 only for a deliberate plumbing run"
                ));
            }
        }
        Ok(ppk2)
    }

    /// The sidecar's backend: `"real"` (a calibrated PPK2) or `"stub"` (opt-in plumbing run).
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Wait up to `timeout` for the next reply line and require `"ok": true`. On a timeout or a
    /// closed pipe the sidecar is killed, since a late reply would desync the line protocol anyway.
    fn recv(&mut self, timeout: Duration, what: &str) -> Result<String, String> {
        let line = match self.replies.recv_timeout(timeout) {
            Ok(line) => line,
            Err(e) => {
                let _ = self.child.kill();
                let why = match e {
                    std::sync::mpsc::RecvTimeoutError::Timeout => format!("no reply within {timeout:?}"),
                    std::sync::mpsc::RecvTimeoutError::Disconnected => "closed the pipe".to_string(),
                };
                return Err(format!(
                    "HIL: ppk2d {why} ({what}); killed it (its stderr is above)"
                ));
            }
        };
        if json_raw(&line, "ok") == Some("true") {
            Ok(line)
        } else {
            let err = json_str(&line, "error").unwrap_or_else(|| line.clone());
            Err(format!("HIL: ppk2d {what} failed: {err}"))
        }
    }

    fn send(&mut self, line: &str, timeout: Duration) -> Result<String, String> {
        writeln!(self.stdin, "{line}").map_err(|e| format!("HIL: ppk2d write: {e}"))?;
        self.stdin.flush().map_err(|e| format!("HIL: ppk2d flush: {e}"))?;
        self.recv(timeout, line)
    }

    /// Power-cycle the supply at `mv`: the standard preamble to any measurement, because a
    /// debug probe leaves the STM32 debug domain powered (~+200 µA) until a clean power cycle
    /// (confounder #1). Always cycle before measuring.
    pub fn cycle(&mut self, mv: u32) -> Result<(), String> {
        self.send(&format!("{{\"cmd\":\"cycle\",\"mv\":{mv}}}"), PPK2_CMD_TIMEOUT)
            .map(drop)
    }

    /// Enable the supply at `mv`.
    pub fn on(&mut self, mv: u32) -> Result<(), String> {
        self.send(&format!("{{\"cmd\":\"on\",\"mv\":{mv}}}"), PPK2_CMD_TIMEOUT)
            .map(drop)
    }

    /// Disable the supply.
    pub fn off(&mut self) -> Result<(), String> {
        self.send("{\"cmd\":\"off\"}", PPK2_CMD_TIMEOUT).map(drop)
    }

    /// Sample the current for `ms` milliseconds: median (the STOP floor) and mean (the average
    /// draw), in microamps. Rejects an insane reading (> [`PPK2_SANE_MAX_UA`]) as a CDC desync
    /// rather than returning noise.
    pub fn measure(&mut self, ms: u32) -> Result<Ppk2Reading, String> {
        let timeout = PPK2_CMD_TIMEOUT + Duration::from_millis(u64::from(ms));
        let reply = self.send(&format!("{{\"cmd\":\"avg\",\"ms\":{ms}}}"), timeout)?;
        let num =
            |key| json_num(&reply, key).ok_or_else(|| format!("HIL: ppk2d avg reply lacks {key}: {reply}"));
        let reading = Ppk2Reading {
            median_ua: num("median_ua")?,
            mean_ua: num("mean_ua")?,
            samples: num("samples")? as u64,
        };
        let worst = reading.median_ua.max(reading.mean_ua);
        if worst > PPK2_SANE_MAX_UA {
            return Err(format!(
                "HIL: PPK2 reading {worst:.0} µA > {PPK2_SANE_MAX_UA:.0} µA: CDC desync (do not \
                 sample mid-flash); retry after the flash settles"
            ));
        }
        Ok(reading)
    }
}

impl Drop for Ppk2 {
    fn drop(&mut self) {
        // Best-effort: cut the supply and reap the sidecar so a panicking test doesn't leave the
        // board powered or the process lingering. Bounded (a wedged sidecar gets the command
        // timeout, then the kill), so Drop can never hang the test binary.
        let _ = self.off();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The raw value token of `"key": <value>` in a flat JSON object line (a number / `true` /
/// `false` / `null`, or a string INCLUDING its quotes), without a JSON dependency. The sidecar
/// writes only flat objects with the `ok` key first, so the first occurrence is the real key.
fn json_raw<'a>(reply: &'a str, key: &str) -> Option<&'a str> {
    let idx = reply.find(&format!("\"{key}\""))?;
    let after = &reply[idx + key.len() + 2..];
    let rest = after.trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with('"') {
        // Scan to the closing quote, skipping escaped characters.
        let mut escaped = false;
        for (i, c) in rest.char_indices().skip(1) {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => return Some(&rest[..=i]),
                _ => {}
            }
        }
        return None;
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim_end())
}

/// A numeric field (`"median_ua": 5.1`).
fn json_num(reply: &str, key: &str) -> Option<f64> {
    json_raw(reply, key)?.parse::<f64>().ok()
}

/// A string field, unescaped (`json.dumps` escapes non-ASCII as `\uXXXX`, e.g. `µ`).
fn json_str(reply: &str, key: &str) -> Option<String> {
    let raw = json_raw(reply, key)?;
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                out.push(
                    u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .unwrap_or('\u{FFFD}'),
                );
            }
            other => out.push(other), // \" \\ \/
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure host unit tests for the harness plumbing (no hardware); these RUN in `cargo test`.

    #[test]
    fn json_fields_handle_various_shapes() {
        assert_eq!(json_num("{\"ua\": 42.5}", "ua"), Some(42.5));
        assert_eq!(json_num("{\"ua\":12}", "ua"), Some(12.0));
        assert_eq!(json_num("{\"other\":1,\"ua\": -3.0e1 }", "ua"), Some(-30.0));
        assert_eq!(json_num("{\"nope\": 1}", "ua"), None);

        // A real sidecar avg reply (json.dumps spacing).
        let avg = "{\"ok\": true, \"median_ua\": 5.1, \"mean_ua\": 9.75, \"samples\": 98000}";
        assert_eq!(json_raw(avg, "ok"), Some("true"));
        assert_eq!(json_num(avg, "median_ua"), Some(5.1));
        assert_eq!(json_num(avg, "mean_ua"), Some(9.75));
        assert_eq!(json_num(avg, "samples"), Some(98000.0));

        // Errors: ok is false, the message is unescaped (µ arrives as \u00b5), quotes survive.
        let err = "{\"ok\": false, \"error\": \"averaged 6e4 \\u00b5A > max \\\"x\\\", ok\"}";
        assert_eq!(json_raw(err, "ok"), Some("false"));
        assert_eq!(
            json_str(err, "error").as_deref(),
            Some("averaged 6e4 µA > max \"x\", ok")
        );

        // The startup line.
        let hello = "{\"ok\": true, \"backend\": \"real\", \"port\": \"/dev/cu.usbmodem1\"}";
        assert_eq!(json_str(hello, "backend").as_deref(), Some("real"));
        assert_eq!(json_str(hello, "port").as_deref(), Some("/dev/cu.usbmodem1"));
        assert_eq!(json_str(hello, "missing"), None);
    }

    #[test]
    fn seq_gap_accounting_uses_real_console() {
        // Exercises the SHIPPED Console::account_seq + seq_gaps() (not an inline copy). The first
        // frame only establishes the baseline: no gap.
        let mut c = Console::for_test();
        for s in [10u16, 11, 12] {
            c.account_seq(s);
        }
        assert_eq!(c.seq_gaps(), 0, "contiguous frames = no gaps");
        c.account_seq(15); // skipped 13, 14
        assert_eq!(c.seq_gaps(), 2);
        c.account_seq(0); // u16 wrap from 15: skipped 16..=65535 (65520)
        assert_eq!(c.seq_gaps(), 2 + 65_535 - 15);

        // A lone first frame is a baseline, never a gap.
        let mut first = Console::for_test();
        first.account_seq(5);
        assert_eq!(first.seq_gaps(), 0);
    }

    // --- Native decode: to_frame + the receive seam --------------------------------------

    /// Encode `payload` as a REAL wire frame (`encode_frame`) and run it back through the exact
    /// receive path `Console::next` uses (`FrameDecoder` → `decode_frame` → `to_frame`), returning
    /// the `(seq, Frame)` it produces. Exercises the shipped decode, not a re-implementation.
    fn decode_via_wire<T: serde::Serialize>(msg_type: MsgType, seq: u16, payload: &T) -> (u16, Frame) {
        let mut wire = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(msg_type, seq, payload, &mut wire).expect("encode_frame");
        let mut decoder = FrameDecoder::new();
        let mut decoded = None;
        for &b in &wire[..n] {
            if let Some(inner) = decoder.push(b) {
                let (mt, s, pl) = decode_frame(inner).expect("decode_frame");
                decoded = Some((s, to_frame(mt, pl)));
            }
        }
        decoded.expect("one full frame decoded from the wire")
    }

    #[test]
    fn to_frame_maps_hello() {
        let (seq, frame) = decode_via_wire(
            MsgType::Hello,
            1,
            &Hello {
                firmware_name: "blinky",
                firmware_version: "v0.1.0",
                session_id: 0xDEAD_BEEF,
            },
        );
        assert_eq!(seq, 1, "seq is carried through decode");
        assert_eq!(
            frame,
            Frame::Hello {
                firmware_name: "blinky".to_string(),
                firmware_version: "v0.1.0".to_string(),
                session_id: 0xDEAD_BEEF,
            }
        );
    }

    #[test]
    fn to_frame_maps_log() {
        let (_, frame) = decode_via_wire(
            MsgType::Log,
            2,
            &Log {
                level: Level::Warn,
                uptime_us: 1_234_567,
                module: "radio",
                message: "carrier lost",
            },
        );
        assert_eq!(
            frame,
            Frame::Log {
                level: Level::Warn,
                uptime_us: 1_234_567,
                module: "radio".to_string(),
                message: "carrier lost".to_string(),
            }
        );
    }

    #[test]
    fn to_frame_maps_print() {
        let (_, frame) = decode_via_wire(MsgType::Print, 3, &Print { text: "hello world" });
        assert_eq!(
            frame,
            Frame::Print {
                text: "hello world".to_string()
            }
        );
    }

    #[test]
    fn to_frame_maps_event() {
        // `Event.fields` is a heapless Vec; build it via Default + push so the test needs no
        // direct heapless dep (the type is inferred from the field).
        let mut ev = Event {
            name: "boot",
            fields: Default::default(),
        };
        ev.fields.push(("phase", "init")).unwrap();
        ev.fields.push(("rc", "0")).unwrap();
        let (_, frame) = decode_via_wire(MsgType::Event, 4, &ev);
        assert_eq!(
            frame,
            Frame::Event {
                name: "boot".to_string(),
                fields: vec![
                    ("phase".to_string(), "init".to_string()),
                    ("rc".to_string(), "0".to_string())
                ],
            }
        );
    }

    #[test]
    fn to_frame_maps_dropped() {
        let (_, frame) = decode_via_wire(MsgType::Dropped, 5, &tower_protocol::msg::Dropped { count: 42 });
        assert_eq!(frame, Frame::Dropped { count: 42 });
    }

    #[test]
    fn to_frame_maps_uplink() {
        // A gateway-link frame.
        let (_, frame) = decode_via_wire(
            MsgType::Uplink,
            6,
            &Uplink {
                src: 0x1122_3344,
                counter: 7,
                rssi: -80,
                lqi: 12,
                data: &[1, 2, 3, 4],
            },
        );
        assert_eq!(
            frame,
            Frame::Uplink {
                src: 0x1122_3344,
                counter: 7,
                rssi: -80,
                lqi: 12,
                data: vec![1, 2, 3, 4]
            }
        );
    }

    #[test]
    fn to_frame_maps_mgmt_response() {
        // A gateway-link frame.
        let (_, frame) = decode_via_wire(
            MsgType::MgmtResponse,
            7,
            &MgmtResponse {
                req_id: 5,
                result: 0,
                chunk: 1,
                last: true,
                data: &[9, 8, 7],
            },
        );
        assert_eq!(
            frame,
            Frame::Mgmt {
                req_id: 5,
                result: 0,
                chunk: 1,
                last: true,
                data: vec![9, 8, 7]
            }
        );
    }

    #[test]
    fn to_frame_maps_radio_stat() {
        // A gateway-link frame: both RadioStat variants map verbatim into Frame::Stat.
        let (_, ch) = decode_via_wire(
            MsgType::RadioStat,
            8,
            &RadioStat::Channel {
                channel: 4,
                rssi: -95,
            },
        );
        assert_eq!(
            ch,
            Frame::Stat(RadioStat::Channel {
                channel: 4,
                rssi: -95
            })
        );

        let tx = RadioStat::Tx {
            dest: 0x00AA_BB00,
            item: 3,
            outcome: 1,
            ack_rssi: Some(-70),
        };
        let (_, txf) = decode_via_wire(MsgType::RadioStat, 9, &tx);
        assert_eq!(txf, Frame::Stat(tx));
    }

    #[test]
    fn to_frame_unknown_type_and_truncated_fall_to_other() {
        // An unmodelled MsgType (shell traffic) is kept as Frame::Other so seq accounting stays
        // exact (`to_frame`'s catch-all arm). Empty payload is fine: the Other arm never decodes.
        let (seq_unknown, unknown) = decode_via_wire(MsgType::ShellResponse, 7, &());
        assert_eq!(unknown, Frame::Other(MsgType::ShellResponse));
        assert_eq!(seq_unknown, 7);

        // A CRC-valid but truncated/undeserializable payload for a MODELLED type also falls to
        // Frame::Other (postcard::from_bytes fails) rather than the frame being dropped.
        let (seq_trunc, truncated) = decode_via_wire(MsgType::Hello, 8, &());
        assert_eq!(truncated, Frame::Other(MsgType::Hello));
        assert_eq!(seq_trunc, 8);

        // `Console::next` accounts seq for EVERY decoded frame, Other included, before returning it,
        // so an unknown/truncated frame still advances the counter. Verify on the real accountant.
        let mut console = Console::for_test();
        console.account_seq(seq_unknown);
        console.account_seq(seq_trunc);
        assert_eq!(
            console.seq_gaps(),
            0,
            "consecutive Other frames advance seq with no gap"
        );
    }

    #[test]
    fn firmware_dir_discovery() {
        // firmware_dir() reads the process-global TOWER_FIRMWARE_DIR. Keep ALL of this env mutation
        // inside ONE test (the crate has no serial_test dep) so it can't race a sibling, and restore
        // the prior value BEFORE asserting so a failed assertion can't leak into other tests.
        const NAME: &str = "TOWER_FIRMWARE_DIR";
        let saved = std::env::var_os(NAME);

        let base = std::env::temp_dir().join(format!("tower-hil-fwdir-{}", std::process::id()));
        let with = base.join("with");
        let without = base.join("without");
        std::fs::create_dir_all(&with).expect("mk with dir");
        std::fs::create_dir_all(&without).expect("mk without dir");
        std::fs::write(with.join("justfile"), "# hil test fixture\n").expect("write justfile");

        // SAFETY: single-threaded within this one test; no other test reads/writes the var.
        unsafe { std::env::remove_var(NAME) };
        let default_res = firmware_dir();
        unsafe { std::env::set_var(NAME, &with) };
        let with_res = firmware_dir();
        unsafe { std::env::set_var(NAME, &without) };
        let without_res = firmware_dir();

        // Restore the environment before any assertion can unwind.
        // SAFETY: same single-threaded justification as above.
        match &saved {
            Some(v) => unsafe { std::env::set_var(NAME, v) },
            None => unsafe { std::env::remove_var(NAME) },
        }
        let _ = std::fs::remove_dir_all(&base);

        // Default (no override) is the sibling `../firmware` directory (the monorepo layout),
        // whether or not it exists on this machine: Ok carries that path, Err names it.
        let expected_default = Path::new(env!("CARGO_MANIFEST_DIR")).join("../firmware");
        match default_res {
            Ok(p) => assert_eq!(p, expected_default),
            Err(e) => assert!(
                e.contains(&expected_default.display().to_string()),
                "default-path error should name ../firmware, got: {e}"
            ),
        }

        // Override → a dir WITH a justfile resolves Ok to exactly that dir.
        assert_eq!(with_res.expect("dir with a justfile resolves"), with);

        // Override → a dir WITHOUT a justfile is an Err naming the path and the missing justfile.
        let err = without_res.expect_err("dir without a justfile must fail");
        assert!(
            err.contains(&without.display().to_string()),
            "err names the bad path: {err}"
        );
        assert!(
            err.contains("justfile"),
            "err mentions the missing justfile: {err}"
        );
    }

    #[test]
    fn fixture_schema_parses() {
        // Parse a FIXED inline fixture: this smoke-tests the TOML schema without coupling to
        // whichever ports the current bench's committed hil.toml happens to name (they
        // re-enumerate per session and the operator edits them: a CP210x adapter, a Linux
        // /dev/ttyUSB0, or a Windows COM7 must not fail a plain `cargo test`). The live serials
        // are validated at resolve time against the live port roster, not here.
        let text = concat!(
            "[core]\nserial = \"/dev/ttyUSB0\"\n",
            "[dongle]\nserial = \"COM7\"\n",
            "[ppk2]\nsupply_mv = 1800\n",
        );
        let bench: Bench = toml::from_str(text).expect("fixture schema must parse");
        assert_eq!(bench.core.serial, "/dev/ttyUSB0");
        assert_eq!(bench.dongle.serial, "COM7");
        assert_eq!(bench.ppk2.supply_mv, 1800);
    }

    #[test]
    fn fixture_usb_serial_is_optional() {
        let text = concat!(
            "[core]\nserial = \"/dev/cu.usbserial-140\"\nusb_serial = \"hio-core-module-1\"\n",
            "[dongle]\nserial = \"/dev/cu.usbserial-120\"\n",
            "[ppk2]\nsupply_mv = 1800\n",
        );
        let bench: Bench = toml::from_str(text).expect("fixture with usb_serial parses");
        assert_eq!(bench.core.usb_serial.as_deref(), Some("hio-core-module-1"));
        assert_eq!(
            bench.dongle.usb_serial, None,
            "a path-only entry is a supported mode (identity from the FTDI's board-type tag)"
        );
    }

    // --- Roster identity (H5): pure, over a synthetic live roster ------------------------

    const CORE_SN: &str = "hio-core-module-2416969388";
    const DONGLE_SN: &str = "bc-usb-dongle-r1.0-6435252593b5";

    fn port(path: &str, sn: Option<&str>) -> LivePort {
        LivePort {
            path: path.to_string(),
            usb: true,
            usb_serial: sn.map(str::to_string),
        }
    }

    /// The macOS shape: every USB-UART twice (cu./tty.), plus non-USB ports.
    fn mac_roster() -> Vec<LivePort> {
        vec![
            LivePort {
                path: "/dev/cu.Bluetooth-Incoming-Port".into(),
                usb: false,
                usb_serial: None,
            },
            port("/dev/cu.usbserial-2110", Some(CORE_SN)),
            port("/dev/tty.usbserial-2110", Some(CORE_SN)),
            port("/dev/cu.usbserial-120", Some(DONGLE_SN)),
            port("/dev/tty.usbserial-120", Some(DONGLE_SN)),
            port("/dev/cu.usbserial-999", Some("FT8X1234")), // some other FTDI
        ]
    }

    fn board(path: &str, sn: Option<&str>) -> Board {
        Board {
            serial: path.to_string(),
            usb_serial: sn.map(str::to_string),
        }
    }

    #[test]
    fn resolve_board_by_tagged_path() {
        let live = mac_roster();
        assert_eq!(
            resolve_board(Role::Core, &board("/dev/cu.usbserial-2110", None), &live).unwrap(),
            "/dev/cu.usbserial-2110"
        );
        // The tty. twin names the same device.
        assert!(resolve_board(Role::Core, &board("/dev/tty.usbserial-2110", None), &live).is_ok());
        assert!(resolve_board(Role::Dongle, &board("/dev/cu.usbserial-120", None), &live).is_ok());
    }

    #[test]
    fn resolve_board_rejects_swapped_unknown_missing_and_non_usb() {
        let live = mac_roster();
        let swapped = resolve_board(Role::Core, &board("/dev/cu.usbserial-120", None), &live).unwrap_err();
        assert!(swapped.contains("swapped"), "{swapped}");
        let unknown = resolve_board(Role::Dongle, &board("/dev/cu.usbserial-999", None), &live).unwrap_err();
        assert!(
            unknown.contains("usb_serial = \"FT8X1234\""),
            "tells how to vouch: {unknown}"
        );
        let missing = resolve_board(Role::Core, &board("/dev/cu.usbserial-140", None), &live).unwrap_err();
        assert!(missing.contains("not present"), "{missing}");
        let bt =
            resolve_board(Role::Core, &board("/dev/cu.Bluetooth-Incoming-Port", None), &live).unwrap_err();
        assert!(bt.contains("not a USB"), "{bt}");
    }

    #[test]
    fn resolve_board_by_pinned_usb_serial() {
        let live = mac_roster();
        // A stale path hint is overridden by the pinned serial; cu. preferred over tty.
        assert_eq!(
            resolve_board(Role::Core, &board("/dev/cu.usbserial-140", Some(CORE_SN)), &live).unwrap(),
            "/dev/cu.usbserial-2110"
        );
        // Pinning vouches for an untagged third-party adapter.
        assert_eq!(
            resolve_board(Role::Dongle, &board("", Some("FT8X1234")), &live).unwrap(),
            "/dev/cu.usbserial-999"
        );
        // A pinned serial of the OTHER board type is a misconfiguration.
        assert!(resolve_board(Role::Core, &board("", Some(DONGLE_SN)), &live).is_err());
        // Absent → error; two distinct devices with one serial → ambiguous.
        assert!(resolve_board(Role::Core, &board("", Some("hio-core-module-0")), &live).is_err());
        let mut dup = live.clone();
        dup.push(port("/dev/cu.usbserial-3000", Some(CORE_SN)));
        let amb = resolve_board(Role::Core, &board("", Some(CORE_SN)), &dup).unwrap_err();
        assert!(amb.contains("ambiguous"), "{amb}");
    }

    #[test]
    fn bench_resolve_rewrites_paths_and_rejects_one_board_in_both_slots() {
        let live = mac_roster();
        let mut bench = Bench {
            core: board("/dev/cu.usbserial-140", Some(CORE_SN)),
            dongle: board("/dev/tty.usbserial-120", None),
            ppk2: Ppk2Config { supply_mv: 1800 },
        };
        bench.resolve_against(&live).expect("resolves");
        assert_eq!(
            bench.core.serial, "/dev/cu.usbserial-2110",
            "re-enumerated path picked up"
        );

        let mut same = Bench {
            core: board("", Some("FT8X1234")),
            dongle: board("", Some("FT8X1234")),
            ppk2: Ppk2Config { supply_mv: 1800 },
        };
        let err = same.resolve_against(&live).unwrap_err();
        assert!(err.contains("both resolve"), "{err}");
        assert!(err.contains(CORE_SN), "roster lists USB serials: {err}");
    }

    // --- Console bookkeeping (H7/H8) ------------------------------------------------------

    #[test]
    fn hello_rebaselines_seq_tracking() {
        let mut c = Console::for_test();
        let hello = Frame::Hello {
            firmware_name: "x".into(),
            firmware_version: "v".into(),
            session_id: 7,
        };
        let stat = Frame::Other(MsgType::RadioStat);
        for s in [900u16, 901, 902] {
            c.on_frame(s, &stat);
        }
        // Reboot: the new boot's Hello is seq 0, not ~65k gaps.
        c.on_frame(0, &hello);
        c.on_frame(1, &stat);
        assert_eq!(c.seq_gaps(), 0);
        assert_eq!(c.session_id(), Some(7));
        // A real gap inside the new session still counts.
        c.on_frame(3, &stat);
        assert_eq!(c.seq_gaps(), 1);
    }

    /// A CRC-valid frame carrying another schema tag is the lockstep break: the message names
    /// both tags, the firmware when the frame is its `Hello`, and the remedy. A corrupted frame
    /// is never one (tower-protocol checks the CRC first).
    #[test]
    fn a_foreign_schema_tag_is_a_lockstep_break() {
        let foreign = tower_protocol::CONSOLE_TAG ^ 0x0001;
        fn deframe<T: serde::Serialize>(tag: u16, mt: MsgType, payload: &T) -> Vec<u8> {
            let mut wire = [0u8; tower_protocol::MAX_WIRE];
            let n = tower_protocol::encode_frame_with_tag(tag, mt, 0, payload, &mut wire).expect("encode");
            let mut decoder = FrameDecoder::new();
            wire[..n]
                .iter()
                .find_map(|&b| decoder.push(b).map(|f| f.to_vec()))
                .expect("one frame")
        }
        let hello = Hello {
            firmware_name: "blinky",
            firmware_version: "v0.1.0",
            session_id: 1,
        };
        let inner = deframe(foreign, MsgType::Hello, &hello);
        assert_eq!(
            decode_frame(&inner),
            Err(tower_protocol::Error::SchemaMismatch { got: foreign })
        );
        let msg = lockstep_break(foreign, &inner);
        assert!(msg.contains("lockstep"), "{msg}");
        assert!(
            msg.contains(&format!("{foreign:#06x}")),
            "names the device's tag: {msg}"
        );
        assert!(
            msg.contains(&format!("{:#06x}", tower_protocol::CONSOLE_TAG)),
            "names ours: {msg}"
        );
        assert!(msg.contains("(blinky v0.1.0)"), "names the firmware: {msg}");
        assert!(msg.contains("from this checkout"), "{msg}");

        // Any other frame of that firmware is a break too, just without the name.
        let print = deframe(foreign, MsgType::Print, &Print { text: "hi" });
        let msg = lockstep_break(foreign, &print);
        assert!(msg.contains("the firmware and the harness"), "{msg}");

        // The same tag flip WITHOUT fixing the CRC is line noise, not a mismatch.
        let mut noisy = deframe(tower_protocol::CONSOLE_TAG, MsgType::Print, &Print { text: "hi" });
        noisy[1] ^= 0x01;
        assert_eq!(decode_frame(&noisy), Err(tower_protocol::Error::BadCrc));
    }

    #[test]
    fn decode_note_names_dropped_frames() {
        let mut c = Console::for_test();
        assert_eq!(c.decode_note(), "");
        c.decode_errors = 2;
        c.last_decode_error = Some("BadCrc".into());
        assert!(c.decode_note().contains("2 undecodable") && c.decode_note().contains("BadCrc"));
    }

    #[test]
    fn committed_fixture_loads() {
        // The committed hil.toml must still be structurally valid + name both boards, but we do
        // NOT assert specific port strings or supply voltage (bench-local, operator-edited).
        let bench = Bench::load_default().expect("hil.toml must parse");
        assert!(!bench.core.serial.is_empty(), "core serial must be set");
        assert!(!bench.dongle.serial.is_empty(), "dongle serial must be set");
    }
}
