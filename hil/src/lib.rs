//! Hardware-in-the-loop harness library for the TOWER firmware SDK. Bench inventory,
//! cabling, and setup: `docs/bench.md` (in this repo); the test catalogue is
//! tower-firmware's docs/test-plan.md.
//!
//! The bench (see `hil.toml`):
//!
//! - a **TOWER Core Module** on a fixed FTDI port, wired to a **SEGGER J-Link** (SWD flash via
//!   `probe-rs`) and a **Nordic PPK2** (scriptable current supply/measure via the
//!   `ppk2d.py` sidecar), for the power tests; and
//! - a **TOWER Radio Dongle** on a second FTDI port, USB-powered, flashed over the UART
//!   bootloader with `tower flash`, for the smoke + radio-peer tests.
//!
//! The images under test are built from a `tower-firmware` checkout located by
//! [`firmware_dir`] (default `../firmware` — the control-plane layout; override with
//! `TOWER_FIRMWARE_DIR`).
//!
//! The value over tower-firmware's old `tools/hwtest` shell scripts (`strings | grep`) is that this decodes
//! the framed console **natively** with `tower-protocol` — so a test asserts on typed
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

/// The framed-console baud rate (USART1, 115200 8N1) — the rate `tower logs` / the SDK use.
pub const CONSOLE_BAUD: u32 = 115_200;

// ---------------------------------------------------------------------------
// Firmware checkout (the harness builds the images it flashes)
// ---------------------------------------------------------------------------

/// Locate the `tower-firmware` checkout the bench tests build images from (`just build
/// example …`). Default: a `firmware` checkout NEXT TO this repo — the TOWER control-plane
/// layout, where `hil/` and `firmware/` are sibling submodules of one root. For any other
/// layout, point `TOWER_FIRMWARE_DIR` at the checkout. Validated by the presence of its
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
            "HIL: no tower-firmware checkout at {} (its justfile is missing). The default is a \
             `firmware` checkout next to this repo (the TOWER control-plane layout); set \
             TOWER_FIRMWARE_DIR=/path/to/tower-firmware for any other layout.",
            dir.display()
        ))
    }
}

// ---------------------------------------------------------------------------
// Bench fixture (hil.toml)
// ---------------------------------------------------------------------------

/// The bench roster, loaded from `hil.toml`. Serial names re-enumerate between sessions, so the
/// fixture is only the *starting* guess — [`Bench::resolve`] re-checks the live roster and
/// fails fast with a human-readable message if a board is missing.
#[derive(Debug, Clone, Deserialize)]
pub struct Bench {
    pub core: Board,
    pub dongle: Board,
    pub ppk2: Ppk2Config,
}

/// One board's fixture entry.
#[derive(Debug, Clone, Deserialize)]
pub struct Board {
    /// FTDI serial device path, e.g. `/dev/cu.usbserial-2120`.
    pub serial: String,
}

/// PPK2 supply/measure configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Ppk2Config {
    /// Source-measure voltage (mV). The bench measures at **1800 mV**: the Core's brown-out /
    /// regulator "knee" sits near ~2 V, so a reading taken at 3 V can read low while 1.8 V is the
    /// honest deep-sleep floor (one of the three confounders — see `ppk2d.py`).
    pub supply_mv: u32,
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

    /// Re-resolve both boards against the LIVE port roster (`tower devices`), since FTDI names
    /// re-enumerate. On a miss this returns a fail-fast message naming the fixture serial and the
    /// ports actually present, so the operator fixes `hil.toml` (or the cabling) immediately
    /// instead of watching a test hang on a dead port.
    pub fn resolve(&self) -> Result<(), String> {
        let present = list_ports();
        let mut missing = Vec::new();
        for (role, board) in [("core", &self.core), ("dongle", &self.dongle)] {
            if !present.iter().any(|p| p == &board.serial) {
                missing.push(format!("  {role}: fixture serial {} not present", board.serial));
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(format!(
            "HIL bench roster mismatch — re-resolve ports (they re-enumerate each session):\n{}\n\
             ports present now:\n{}\n\
             Fix the serials in hil.toml (or `tower devices`), then re-run.",
            missing.join("\n"),
            if present.is_empty() {
                "  (none — is any board plugged in?)".to_string()
            } else {
                present.iter().map(|p| format!("  {p}")).collect::<Vec<_>>().join("\n")
            }
        ))
    }
}

/// Startup roster resolution: load the default fixture and confirm both boards are present. Every
/// integration test calls this first so a missing board fails with the roster message rather than
/// a serial timeout ten seconds later.
pub fn bench_or_fail() -> Bench {
    let bench = Bench::load_default().unwrap_or_else(|e| panic!("{e}"));
    if let Err(e) = bench.resolve() {
        panic!("{e}");
    }
    bench
}

/// The live serial-port roster, via `tower devices` (the CLI knows which ports are TOWER boards).
/// Falls back to an empty list if `tower` isn't on PATH — [`Bench::resolve`] then reports "none
/// present", which is the right fail-fast for a bench with no CLI.
pub fn list_ports() -> Vec<String> {
    let out = match Command::new("tower").arg("devices").output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    // `tower devices` lists one device path per line (possibly with trailing description). We take
    // the first whitespace-delimited token of any line that looks like a device path.
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|tok| tok.contains("usbserial") || tok.starts_with("/dev/") || tok.starts_with("COM"))
        .map(|s| s.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Native framed-console decode
// ---------------------------------------------------------------------------

/// A decoded console frame (the typed subset the tests assert on). Owned copies of the borrowed
/// `tower-protocol` message fields, so a frame outlives the decoder's buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Boot banner: protocol version, firmware name + version, and per-boot session id.
    Hello { protocol_version: u8, firmware_name: String, firmware_version: String, session_id: u32 },
    /// A `log::` record.
    Log { level: Level, uptime_us: u64, module: String, message: String },
    /// A `println!`-style print.
    Print { text: String },
    /// A structured `console::event(...)`.
    Event { name: String, fields: Vec<(String, String)> },
    /// The writer's dropped-frame marker (queue overflow / unplugged drain).
    Dropped { count: u32 },
    /// A forwarded radio uplink from the gateway firmware (wire v3): the decrypted,
    /// authenticated payload verbatim, plus reception metadata.
    Uplink { src: u32, counter: u32, rssi: i16, lqi: u8, data: Vec<u8> },
    /// One chunk of a management reply (wire v3) — reassemble by `req_id` (see
    /// [`Console::mgmt_roundtrip`]).
    Mgmt { req_id: u16, result: u8, chunk: u16, last: bool, data: Vec<u8> },
    /// A radio-diagnostics sample (wire v3): ambient channel RSSI or a TX report.
    Stat(RadioStat),
    /// A frame whose `MsgType` we don't model here (e.g. shell traffic) — kept so seq accounting
    /// stays exact.
    Other(MsgType),
}

/// The framed console over one serial port, with native `tower-protocol` decode + seq tracking.
///
/// Read [`next`](Self::next) (blocking up to a timeout) or [`collect_for`](Self::collect_for)
/// (drain a fixed window). Every decoded frame's 16-bit `seq` is checked for **gaps** (the number
/// of frames the device skipped, i.e. dropped in transit or by the writer's queue), surfaced via
/// [`seq_gaps`](Self::seq_gaps) — something `strings | grep` fundamentally cannot see.
pub struct Console {
    // Option: reset_into_app() temporarily loans the handle to jolt (Port::from_handle) for the
    // NRST pulse — serialport 4.9 flocks the tty on every open, so a second open while the
    // console is attached fails with EWOULDBLOCK; sharing the one handle is the only way.
    port: Option<Box<dyn serialport::SerialPort>>,
    decoder: FrameDecoder,
    last_seq: Option<u16>,
    seq_gaps: u32,
    /// Host→device frame counter for the writer helpers (wire v3 gateway tests) —
    /// per-link, restarting at 0 like every sender's.
    tx_seq: u16,
}

impl Console {
    /// Open `path` at the console baud with a short read timeout (so reads poll rather than block
    /// forever). The FTDI bridge does not reset the MCU on open (NRST/BOOT0 are on the aux lines),
    /// so attaching here does not perturb a running app.
    pub fn open(path: &str) -> Result<Self, String> {
        #[allow(unused_mut)]
        let mut port = serialport::new(path, CONSOLE_BAUD)
            .timeout(Duration::from_millis(50))
            .open_native()
            .map_err(|e| format!("HIL: open console {path}: {e}"))?;
        // serialport-rs opens TTYs with TIOCEXCL by default, which makes jolt's re-open of the
        // same tty (reset_into_app's NRST pulse, while the console stays attached to catch the
        // boot burst) fail with EBUSY. Clear it: jolt only toggles modem lines, it never reads,
        // so sharing the tty with the console reader is safe.
        #[cfg(unix)]
        port.set_exclusive(false)
            .map_err(|e| format!("HIL: clear TIOCEXCL on {path}: {e}"))?;
        Ok(Self {
            port: Some(Box::new(port)),
            decoder: FrameDecoder::new(),
            last_seq: None,
            seq_gaps: 0,
            tx_seq: 0,
        })
    }

    /// Write one host→device frame (the harness's first TX path — until wire v3 it only
    /// ever read). Used to drive the management channel and the shell from tests.
    pub fn send_frame<T: serde::Serialize>(&mut self, msg_type: MsgType, payload: &T) -> Result<(), String> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(msg_type, self.tx_seq, payload, &mut buf)
            .map_err(|e| format!("HIL: encode {msg_type:?}: {e}"))?;
        self.tx_seq = self.tx_seq.wrapping_add(1);
        let port = self.port.as_mut().ok_or("HIL: console handle loaned out")?;
        port.write_all(&buf[..n]).map_err(|e| format!("HIL: write: {e}"))?;
        port.flush().map_err(|e| format!("HIL: flush: {e}"))
    }

    /// Send a `ShellCommand` (the console shell, not the radio remote shell).
    pub fn send_shell(&mut self, cmd_id: u16, line: &str) -> Result<(), String> {
        self.send_frame(MsgType::ShellCommand, &tower_protocol::msg::ShellCommand { cmd_id, line })
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
        self.send_frame(MsgType::MgmtRequest, &MgmtRequest { req_id, op: op.clone() })?;
        let mut data = Vec::new();
        let mut deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.next(remaining)? {
                Some(Frame::Mgmt { req_id: r, result, data: d, last, .. }) if r == req_id => {
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
        Err(format!("HIL: mgmt req {req_id} got no complete reply"))
    }

    /// Pulse NRST so the board reboots into its application, WITHOUT re-opening the tty: the
    /// console's own handle is loaned to jolt (`Port::from_handle`) for the tuned reset sequence
    /// and taken back afterwards. (A second open — the old free-function approach — fails on
    /// serialport 4.9, which flocks the tty on every open.) The reset only toggles modem lines,
    /// so the console's 115200 8N1 frame format is untouched.
    pub fn reset_into_app(&mut self) -> Result<(), String> {
        let handle = self.port.take().ok_or("HIL: console handle already loaned")?;
        let mut port = jolt::port::Port::from_handle(handle);
        let res = port.reset_into_app().map_err(|e| format!("HIL: reset-into-app: {e}"));
        self.port = Some(port.into_inner());
        res
    }

    /// Total sequence gaps seen so far (frames the device advanced its `seq` past but we never
    /// received — dropped in transit or by the console writer under backpressure/unplug).
    pub fn seq_gaps(&self) -> u32 {
        self.seq_gaps
    }

    /// Discard any partial frame + reset seq accounting (e.g. right after a device reset, where
    /// the `seq` counter restarts from a fresh boot).
    pub fn resync(&mut self) {
        self.decoder.reset();
        self.last_seq = None;
    }

    /// Read the next complete frame, waiting up to `timeout`. `Ok(None)` means the window elapsed
    /// with no full frame (idle line). A decode error (bad CRC/version) is skipped, not returned —
    /// the next `0x00` resynchronizes, exactly like the CLI.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Frame>, String> {
        let deadline = Instant::now() + timeout;
        let mut byte = [0u8; 1];
        let port = self.port.as_mut().ok_or("HIL: console handle loaned out")?;
        while Instant::now() < deadline {
            match port.read(&mut byte) {
                Ok(0) => continue,
                Ok(_) => {
                    // Feed one byte; on a frame boundary decode + account for seq. Build the OWNED
                    // Frame + copy out `seq` inside the decoder borrow, so the borrow ends before
                    // `account_seq` takes its own `&mut self`.
                    let decoded = self.decoder.push(byte[0]).and_then(|inner| {
                        decode_frame(inner)
                            .ok()
                            .map(|(msg_type, seq, payload)| (seq, to_frame(msg_type, payload)))
                    });
                    if let Some((seq, frame)) = decoded {
                        self.account_seq(seq);
                        return Ok(Some(frame));
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
    pub fn wait_for(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&Frame) -> bool,
    ) -> Result<Option<Frame>, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if let Some(f) = self.next(remaining.min(Duration::from_millis(500)))?
                && pred(&f)
            {
                return Ok(Some(f));
            }
        }
        Ok(None)
    }

    /// Convenience: await a Log/Print frame whose text contains `needle` (the framed equivalent
    /// of the old `strings | grep`, but on decoded ASCII with version + CRC already validated).
    pub fn wait_for_text(&mut self, timeout: Duration, needle: &str) -> Result<bool, String> {
        Ok(self.wait_for(timeout, |f| frame_text(f).is_some_and(|t| t.contains(needle)))?.is_some())
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
    /// (`account_seq` / [`seq_gaps`](Self::seq_gaps)) can be exercised on the SHIPPED code — not
    /// an inline copy — without opening a tty. Never compiled into a non-test build.
    #[cfg(test)]
    fn for_test() -> Self {
        Self { port: None, decoder: FrameDecoder::new(), last_seq: None, seq_gaps: 0, tx_seq: 0 }
    }
}

/// Decode a `(MsgType, payload)` into an owned [`Frame`]. Unknown/unmodelled types become
/// [`Frame::Other`] so seq accounting is still exact.
fn to_frame(msg_type: MsgType, payload: &[u8]) -> Frame {
    match msg_type {
        MsgType::Hello => match postcard::from_bytes::<Hello>(payload) {
            Ok(h) => Frame::Hello {
                protocol_version: h.protocol_version,
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
            Ok(p) => Frame::Print { text: p.text.to_string() },
            Err(_) => Frame::Other(msg_type),
        },
        MsgType::Event => match postcard::from_bytes::<Event>(payload) {
            Ok(e) => Frame::Event {
                name: e.name.to_string(),
                fields: e.fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
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
// Reset over the FTDI aux lines (jolt)
// ---------------------------------------------------------------------------

/// Pulse NRST so the board reboots into its application, using jolt's tuned reset sequence (the
/// 1 µF NRST cap makes the line ordering load-bearing — we reuse jolt's, not a hand-rolled one).
/// Used to re-run a one-shot KAT from a clean boot and re-observe its verdict.
pub fn reset_into_app(path: &str) -> Result<(), String> {
    let mut port = jolt::port::Port::open(path).map_err(|e| format!("HIL: jolt open {path}: {e}"))?;
    port.reset_into_app().map_err(|e| format!("HIL: reset-into-app {path}: {e}"))
}

// ---------------------------------------------------------------------------
// PPK2 sidecar client (line-JSON over stdio, ppk2d.py)
// ---------------------------------------------------------------------------

/// A client for the `ppk2d.py` current sidecar. Protocol is **line-JSON over stdio**: we write one
/// command per line and read one JSON reply per line. Commands:
///
/// - `{"cmd":"on","mv":<mV>}`   — enable source-measure at `<mV>`
/// - `{"cmd":"off"}`            — disable the supply
/// - `{"cmd":"cycle","mv":<mV>}`— power-cycle (off→settle→on) at `<mV>` (clears the probe's
///   debug-domain residual current — confounder #1)
/// - `{"cmd":"avg","ms":<ms>}`  — average current over `<ms>` → `{"ua": <µA>}`
///
/// The sidecar encodes the three known confounders (see `ppk2d.py`); this client just speaks the
/// protocol. It is only spawned by the `power`-gated tests.
pub struct Ppk2 {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

/// Reject any current reading above this — mid-SWD-flash the PPK2's CDC can desync and report
/// garbage tens of mA (confounder #3). A real deep-sleep floor is tens of µA, so >50 mA is never
/// a valid STOP reading; treat it as a desync and fail loudly rather than pass/fail on noise.
pub const PPK2_SANE_MAX_UA: f64 = 50_000.0;

impl Ppk2 {
    /// Spawn the sidecar (`python3 ppk2d.py`). `python` is the launcher; the path is
    /// resolved relative to this crate so it works from any CWD.
    pub fn spawn() -> Result<Self, String> {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/ppk2d.py");
        let mut child = Command::new("python3")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("HIL: spawn ppk2d.py: {e}"))?;
        let stdin = child.stdin.take().ok_or("HIL: ppk2d stdin unavailable")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("HIL: ppk2d stdout unavailable")?);
        Ok(Self { child, stdin, stdout })
    }

    fn send(&mut self, line: &str) -> Result<String, String> {
        writeln!(self.stdin, "{line}").map_err(|e| format!("HIL: ppk2d write: {e}"))?;
        self.stdin.flush().map_err(|e| format!("HIL: ppk2d flush: {e}"))?;
        let mut reply = String::new();
        self.stdout
            .read_line(&mut reply)
            .map_err(|e| format!("HIL: ppk2d read: {e}"))?;
        if reply.is_empty() {
            return Err("HIL: ppk2d closed the pipe".to_string());
        }
        Ok(reply.trim().to_string())
    }

    /// Power-cycle the supply at `mv` — the standard preamble to any measurement, because a
    /// debug probe leaves the STM32 debug domain powered (~+200 µA) until a clean power cycle
    /// (confounder #1). Always cycle before measuring.
    pub fn cycle(&mut self, mv: u32) -> Result<(), String> {
        let _ = self.send(&format!("{{\"cmd\":\"cycle\",\"mv\":{mv}}}"))?;
        Ok(())
    }

    /// Enable the supply at `mv`.
    pub fn on(&mut self, mv: u32) -> Result<(), String> {
        let _ = self.send(&format!("{{\"cmd\":\"on\",\"mv\":{mv}}}"))?;
        Ok(())
    }

    /// Disable the supply.
    pub fn off(&mut self) -> Result<(), String> {
        let _ = self.send("{\"cmd\":\"off\"}")?;
        Ok(())
    }

    /// Average current over `ms` milliseconds, in microamps. Rejects an insane reading
    /// (> [`PPK2_SANE_MAX_UA`]) as a CDC desync rather than returning noise.
    pub fn avg_ua(&mut self, ms: u32) -> Result<f64, String> {
        let reply = self.send(&format!("{{\"cmd\":\"avg\",\"ms\":{ms}}}"))?;
        // Minimal parse: find "ua": <number>. Avoids a JSON dep for one field.
        let ua = parse_ua(&reply).ok_or_else(|| format!("HIL: ppk2d bad avg reply: {reply}"))?;
        if ua > PPK2_SANE_MAX_UA {
            return Err(format!(
                "HIL: PPK2 reading {ua:.0} µA > {PPK2_SANE_MAX_UA:.0} µA — CDC desync (do not \
                 sample mid-flash); retry after the flash settles"
            ));
        }
        Ok(ua)
    }
}

impl Drop for Ppk2 {
    fn drop(&mut self) {
        // Best-effort: cut the supply and reap the sidecar so a panicking test doesn't leave the
        // board powered or the process lingering.
        let _ = self.off();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extract the `"ua": <number>` field from a sidecar reply without a JSON dependency.
fn parse_ua(reply: &str) -> Option<f64> {
    let idx = reply.find("\"ua\"")?;
    let after = &reply[idx + 4..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure host unit tests for the harness plumbing (no hardware) — these RUN in `cargo test`.

    #[test]
    fn parse_ua_handles_various_shapes() {
        assert_eq!(parse_ua("{\"ua\": 42.5}"), Some(42.5));
        assert_eq!(parse_ua("{\"ua\":12}"), Some(12.0));
        assert_eq!(parse_ua("{\"other\":1,\"ua\": -3.0e1 }"), Some(-30.0));
        assert_eq!(parse_ua("{\"nope\": 1}"), None);
    }

    #[test]
    fn seq_gap_accounting_uses_real_console() {
        // Exercises the SHIPPED Console::account_seq + seq_gaps() (not an inline copy). The first
        // frame only establishes the baseline — no gap.
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
    /// receive path `Console::next` uses — `FrameDecoder` → `decode_frame` → `to_frame` — returning
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
                protocol_version: 3,
                firmware_name: "blinky",
                firmware_version: "v0.1.0",
                session_id: 0xDEAD_BEEF,
            },
        );
        assert_eq!(seq, 1, "seq is carried through decode");
        assert_eq!(
            frame,
            Frame::Hello {
                protocol_version: 3,
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
            &Log { level: Level::Warn, uptime_us: 1_234_567, module: "radio", message: "carrier lost" },
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
        assert_eq!(frame, Frame::Print { text: "hello world".to_string() });
    }

    #[test]
    fn to_frame_maps_event() {
        // `Event.fields` is a heapless Vec; build it via Default + push so the test needs no
        // direct heapless dep (the type is inferred from the field).
        let mut ev = Event { name: "boot", fields: Default::default() };
        ev.fields.push(("phase", "init")).unwrap();
        ev.fields.push(("rc", "0")).unwrap();
        let (_, frame) = decode_via_wire(MsgType::Event, 4, &ev);
        assert_eq!(
            frame,
            Frame::Event {
                name: "boot".to_string(),
                fields: vec![("phase".to_string(), "init".to_string()), ("rc".to_string(), "0".to_string())],
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
        // wire-v3 gateway frame.
        let (_, frame) = decode_via_wire(
            MsgType::Uplink,
            6,
            &Uplink { src: 0x1122_3344, counter: 7, rssi: -80, lqi: 12, data: &[1, 2, 3, 4] },
        );
        assert_eq!(
            frame,
            Frame::Uplink { src: 0x1122_3344, counter: 7, rssi: -80, lqi: 12, data: vec![1, 2, 3, 4] }
        );
    }

    #[test]
    fn to_frame_maps_mgmt_response() {
        // wire-v3 gateway frame.
        let (_, frame) = decode_via_wire(
            MsgType::MgmtResponse,
            7,
            &MgmtResponse { req_id: 5, result: 0, chunk: 1, last: true, data: &[9, 8, 7] },
        );
        assert_eq!(frame, Frame::Mgmt { req_id: 5, result: 0, chunk: 1, last: true, data: vec![9, 8, 7] });
    }

    #[test]
    fn to_frame_maps_radio_stat() {
        // wire-v3 gateway frame — both RadioStat variants map verbatim into Frame::Stat.
        let (_, ch) = decode_via_wire(MsgType::RadioStat, 8, &RadioStat::Channel { channel: 4, rssi: -95 });
        assert_eq!(ch, Frame::Stat(RadioStat::Channel { channel: 4, rssi: -95 }));

        let tx = RadioStat::Tx { dest: 0x00AA_BB00, item: 3, outcome: 1, ack_rssi: Some(-70) };
        let (_, txf) = decode_via_wire(MsgType::RadioStat, 9, &tx);
        assert_eq!(txf, Frame::Stat(tx));
    }

    #[test]
    fn to_frame_unknown_type_and_truncated_fall_to_other() {
        // An unmodelled MsgType (shell traffic) is kept as Frame::Other so seq accounting stays
        // exact — `to_frame`'s catch-all arm. Empty payload is fine: the Other arm never decodes.
        let (seq_unknown, unknown) = decode_via_wire(MsgType::ShellResponse, 7, &());
        assert_eq!(unknown, Frame::Other(MsgType::ShellResponse));
        assert_eq!(seq_unknown, 7);

        // A CRC-valid but truncated/undeserializable payload for a MODELLED type also falls to
        // Frame::Other (postcard::from_bytes fails) rather than the frame being dropped.
        let (seq_trunc, truncated) = decode_via_wire(MsgType::Hello, 8, &());
        assert_eq!(truncated, Frame::Other(MsgType::Hello));
        assert_eq!(seq_trunc, 8);

        // `Console::next` accounts seq for EVERY decoded frame, Other included, before returning it
        // — so an unknown/truncated frame still advances the counter. Verify on the real accountant.
        let mut console = Console::for_test();
        console.account_seq(seq_unknown);
        console.account_seq(seq_trunc);
        assert_eq!(console.seq_gaps(), 0, "consecutive Other frames advance seq with no gap");
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

        // Default (no override) is the sibling `../firmware` checkout (the control-plane layout),
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
        assert!(err.contains(&without.display().to_string()), "err names the bad path: {err}");
        assert!(err.contains("justfile"), "err mentions the missing justfile: {err}");
    }

    #[test]
    fn fixture_schema_parses() {
        // Parse a FIXED inline fixture: this smoke-tests the TOML schema without coupling to
        // whichever ports the current bench's committed hil.toml happens to name (they
        // re-enumerate per session and the operator edits them — a CP210x adapter, a Linux
        // /dev/ttyUSB0, or a Windows COM7 must not fail a plain `cargo test`). The live serials
        // are validated at resolve time against the `tower devices` roster, not here.
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
    fn committed_fixture_loads() {
        // The committed hil.toml must still be structurally valid + name both boards, but we do
        // NOT assert specific port strings or supply voltage (bench-local, operator-edited).
        let bench = Bench::load_default().expect("hil.toml must parse");
        assert!(!bench.core.serial.is_empty(), "core serial must be set");
        assert!(!bench.dongle.serial.is_empty(), "dongle serial must be set");
    }
}
