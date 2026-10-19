//! Serial-port selection and the console line-control pulse (NRST/BOOT0 over RTS/DTR).
//!
//! This is the host side of attaching to a TOWER device's framed console: pick the USB
//! serial port, open it at the console baud, and drive the modem lines to a known state so
//! merely opening the port can't leave the MCU held in reset. The reset pulse mirrors tower-flasher.

use std::time::Duration;

use anyhow::{Context, Result, bail};

// ---- port selection -------------------------------------------------------

/// A TOWER board, recognised by its USB bridge's serial number (the FTDI iSerial the factory
/// programs: `hio-core-module-<id>` on a Core Module, `bc-usb-dongle-<rev>-<id>` on a Radio
/// Dongle; older boards carry a `bc-` prefix instead of `hio-`). A Core Module r1.x has no
/// bridge: its firmware's own USB CDC console carries `hio-core-module-r1-<uid>` (see
/// [`NATIVE_USB_CONSOLE`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Board {
    CoreModule,
    RadioDongle,
}

impl Board {
    fn from_usb_serial(serial: &str) -> Option<Self> {
        if serial.contains("core-module") {
            Some(Self::CoreModule)
        } else if serial.contains("usb-dongle") {
            Some(Self::RadioDongle)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::CoreModule => "Core Module",
            Self::RadioDongle => "Radio Dongle",
        }
    }
}

/// USB vendor / product id of the USB CDC console of TOWER firmware built for the Core Module
/// r1.x (the SDK's `core-module-r1` feature; the ids the legacy r1 SDK used). That console
/// mirrors the r2 auto-reset circuit on its control lines, so the reset pulses work on it, and
/// the reset into the bootloader brings up the ROM's USB DFU bootloader (see `target`).
pub(crate) const NATIVE_USB_CONSOLE: (u16, u16) = (0x0483, 0x5740);

/// One serial port as the OS enumerates it (cached USB descriptors; nothing is opened).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Port {
    name: String,
    usb: bool,
    usb_serial: Option<String>,
    /// USB vendor / product id, for a USB port.
    usb_id: Option<(u16, u16)>,
}

impl Port {
    fn board(&self) -> Option<Board> {
        self.usb_serial.as_deref().and_then(Board::from_usb_serial)
    }

    /// The USB CDC console of a Core Module r1.x (no USB-UART bridge).
    fn native_usb(&self) -> bool {
        self.usb_id == Some(NATIVE_USB_CONSOLE) && self.board() == Some(Board::CoreModule)
    }

    /// The kinds of port a TOWER board presents across platforms.
    fn usb_like(&self) -> bool {
        self.usb
            || ["usbserial", "ttyUSB", "ttyACM"]
                .iter()
                .any(|k| self.name.contains(k))
    }

    /// `/dev/cu.usbserial-2110 (Core Module)`, for a choice the user has to make.
    fn label(&self) -> String {
        match self.board() {
            Some(_) if self.native_usb() => format!("{} (Core Module r1)", self.name),
            Some(b) => format!("{} ({})", self.name, b.name()),
            None => self.name.clone(),
        }
    }
}

/// Every serial port, macOS's dial-in twins folded away (see [`prefer_callout`]).
fn enumerate(what: &str) -> Result<Vec<Port>> {
    let ports: Vec<Port> = serialport::available_ports()
        .context(what.to_string())?
        .into_iter()
        .map(|p| match p.port_type {
            serialport::SerialPortType::UsbPort(info) => Port {
                name: p.port_name,
                usb: true,
                usb_serial: info.serial_number,
                usb_id: Some((info.vid, info.pid)),
            },
            _ => Port {
                name: p.port_name,
                usb: false,
                usb_serial: None,
                usb_id: None,
            },
        })
        .collect();
    let keep = prefer_callout(ports.iter().map(|p| p.name.clone()).collect());
    Ok(ports.into_iter().filter(|p| keep.contains(&p.name)).collect())
}

/// USB serial ports, filtered to the kinds a TOWER board presents across platforms.
/// Propagates the enumeration error (mirroring [`devices`]) rather than swallowing it into an
/// empty list, so `pick_port` can tell "no ports attached" apart from "couldn't enumerate"
/// (e.g. no udev in a container) and report the real cause.
pub(crate) fn usb_ports() -> Result<Vec<String>> {
    Ok(usb_port_list()?.into_iter().map(|p| p.name).collect())
}

fn usb_port_list() -> Result<Vec<Port>> {
    Ok(enumerate("enumerating serial ports")?
        .into_iter()
        .filter(Port::usb_like)
        .collect())
}

/// Collapse macOS's two device nodes per serial port to the one we want. macOS publishes every
/// port twice, `/dev/tty.X` (dial-in: an open waits for carrier detect) and `/dev/cu.X`
/// (call-out: what a host talking *to* a device opens), and serialport lists both, so one
/// attached board looked like two and auto-detect failed with "multiple USB serial devices".
/// Drop each `/dev/tty.X` whose `/dev/cu.X` twin is present (a lone `tty.` node is kept). No
/// other platform has `/dev/cu.*`, so this is a no-op there.
fn prefer_callout(names: Vec<String>) -> Vec<String> {
    let has_callout = |suffix: &str| names.iter().any(|n| n.strip_prefix("/dev/cu.") == Some(suffix));
    names
        .iter()
        .filter(|n| !n.strip_prefix("/dev/tty.").is_some_and(has_callout))
        .cloned()
        .collect()
}

/// Resolve the serial-port path to use: the explicit `--device`, else the sole USB serial
/// device. Ambiguity (zero or several) is an error telling the user to pass `--device`; a
/// failed enumeration surfaces its own cause (not a misleading "no device found").
pub(crate) fn pick_port(explicit: Option<String>) -> Result<String> {
    pick_port_for(explicit, None)
}

/// [`pick_port`], except that among several USB serial devices the one `board` is chosen when
/// it is the only one of its kind (`tower gateway` takes the Radio Dongle next to a Core Module
/// that is being flashed or watched).
pub(crate) fn pick_port_for(explicit: Option<String>, board: Option<Board>) -> Result<String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let (name, preferred) = choose(usb_port_list()?, board)?;
    if preferred && let Some(b) = board {
        eprintln!(
            "[tower] using the {} on {name} (pass -d/--device to pick another)",
            b.name()
        );
    }
    Ok(name)
}

/// The decision of [`pick_port_for`] (pure): the port, and whether `board` broke a tie.
fn choose(ports: Vec<Port>, board: Option<Board>) -> Result<(String, bool)> {
    if ports.len() == 1 {
        return Ok((ports.into_iter().next().unwrap().name, false));
    }
    if ports.is_empty() {
        bail!(
            "no USB serial device found: plug in the board (with a data cable) or pass -d/--device \
             (`tower devices` lists every port)"
        );
    }
    if let Some(b) = board {
        let mut of_kind = ports.iter().filter(|p| p.board() == Some(b));
        if let (Some(only), None) = (of_kind.next(), of_kind.next()) {
            return Ok((only.name.clone(), true));
        }
    }
    let labels: Vec<String> = ports.iter().map(Port::label).collect();
    bail!(
        "multiple USB serial devices; pass -d/--device (one of: {})",
        labels.join(", ")
    )
}

/// Whether `name` is the USB CDC console of a Core Module r1.x (looked up among the ports the OS
/// lists now; a port that is not there is not one).
pub(crate) fn is_native_usb_console(name: &str) -> Result<bool> {
    Ok(enumerate("enumerating serial ports")?
        .iter()
        .any(|p| p.name == name && p.native_usb()))
}

/// `tower devices`: one port per line, macOS's dial-in twins folded away (see
/// [`prefer_callout`]) so the list shows the names `--device` should be given. The path comes
/// first (`awk '{print $1}'` keeps working); a USB port adds what it is (a TOWER board, by its
/// USB serial number) and that serial number. It deliberately lists *all* ports, not just the
/// USB ones auto-detect picks from. STM32 ROM DFU bootloaders follow as `dfu:<location>` (what
/// `-d` takes to pick one), with their serial number.
pub(crate) fn devices() -> Result<()> {
    let ports = enumerate("listing serial ports")?;
    let dfu: Vec<(String, Option<String>)> = crate::target::list_or_warn()
        .iter()
        .map(|d| (format!("dfu:{}", d.location()), d.serial().map(String::from)))
        .collect();
    let width = ports
        .iter()
        .map(|p| p.name.len())
        .chain(dfu.iter().map(|(n, _)| n.len()))
        .max()
        .unwrap_or(0);
    for p in &ports {
        println!("{}", device_line(p, width));
    }
    for (name, serial) in &dfu {
        println!("{}", dfu_line(name, serial.as_deref(), width));
    }
    Ok(())
}

fn dfu_line(name: &str, serial: Option<&str>, width: usize) -> String {
    let line = format!("{name:width$}  {:<12}", "STM32 DFU");
    match serial {
        Some(sn) => format!("{line}  {sn}"),
        None => line.trim_end().to_string(),
    }
}

fn device_line(p: &Port, width: usize) -> String {
    if !p.usb {
        return p.name.clone();
    }
    let what = if p.native_usb() {
        "Core Module r1"
    } else {
        p.board().map_or("USB serial", Board::name)
    };
    let line = format!("{:width$}  {what:<12}", p.name);
    match &p.usb_serial {
        Some(sn) => format!("{line}  {sn}"),
        None => line.trim_end().to_string(),
    }
}

/// Open the console port at 115200 baud with a short read timeout (so read loops can poll).
pub(crate) fn open(port: &str) -> Result<Box<dyn serialport::SerialPort>> {
    serialport::new(port, 115_200)
        .timeout(Duration::from_millis(200))
        .open()
        .with_context(|| format!("opening {port}"))
}

// ---- console line control (NRST/BOOT0 over RTS/DTR) -----------------------
//
// The AUTHORITATIVE copy of this sequence lives in tower-flasher (crates/flasher/src/port.rs:
// `open_with` / `reset_into_app`). We keep a minimal local pulse so a console
// command can reset on the *same* handle it streams from and thus capture boot
// output from the very first byte; reopening the port would drop the `Hello` +
// early logs and re-undefine the line state. RTS->NRST, DTR->BOOT0; (true,true)
// is the safe "run" baseline.
//
// The tuned *delays* are not copied: they come straight from tower-flasher's public
// constants below, so the two cannot drift apart (a rename there becomes a compile
// error here). The line polarity/ordering is basic,
// stable bridge wiring.
//
// Convergence opportunity:
// `tower_flasher::port::Port::from_handle(sp)` + `reset_into_app_no_settle()` implement the same run-baseline + pulse over a
// borrowed handle. We don't call it here for one reason: our console pulse clears
// the input buffer *while the chip is held in reset* (dropping pre-reset garbage so
// the boot `Hello` isn't preceded by junk), and `reset_into_app_no_settle` has no
// mid-reset clear hook; clearing after release would race the boot bytes we want.
// If tower-flasher grows a mid-reset-clear variant, drop this fork for `from_handle`.

/// Drive RTS/DTR to the run baseline so merely opening the port can't leave the
/// MCU held in reset by whatever level the USB bridge asserts on open. Mirrors
/// tower-flasher's `open_with` (and reuses its tuned `RUN_SETTLE`).
pub(crate) fn set_run_baseline(sp: &mut dyn serialport::SerialPort) -> Result<()> {
    sp.write_request_to_send(true)?;
    sp.write_data_terminal_ready(true)?;
    std::thread::sleep(tower_flasher::port::RUN_SETTLE);
    Ok(())
}

/// Pulse NRST to reboot into the application (BOOT0 low) so the caller can capture
/// boot output from byte 0. Mirrors tower-flasher's `reset_into_app` (reusing its tuned
/// `RESET_PULSE`) minus the post-boot settle (we want the boot logs), plus a
/// mid-reset input-buffer clear tower-flasher's no-settle variant doesn't offer.
pub(crate) fn pulse_reset_into_app(sp: &mut dyn serialport::SerialPort) -> Result<()> {
    sp.write_request_to_send(true)?; // RTS asserted
    sp.write_data_terminal_ready(false)?; // BOOT0 low -> RESET asserted
    std::thread::sleep(tower_flasher::port::RESET_PULSE);
    let _ = sp.clear(serialport::ClearBuffer::Input); // drop pre-reset bytes while held in reset
    sp.write_request_to_send(false)?; // RESET released -> boot the app
    Ok(())
}

/// Open a console port with the lines in a known state. With `reset`, reboot the
/// application first so the caller observes it coming up from the start.
///
/// On a non-reset open we clear the input buffer: a late frame buffered from a *previous*
/// run (e.g. a trailing `ShellResponse` from an earlier `tower exec`, still using the same
/// `cmd_id`) must not satisfy this run's wait. The reset path deliberately keeps the buffer:
/// `pulse_reset_into_app` already drops pre-reset bytes while the chip is held in reset,
/// and we want the boot `Hello` + early logs that arrive after release.
pub(crate) fn open_console(port: &str, reset: bool) -> Result<Box<dyn serialport::SerialPort>> {
    let mut sp = open(port)?;
    set_run_baseline(&mut *sp)?;
    if reset {
        pulse_reset_into_app(&mut *sp)?;
        eprintln!("[tower] reset into application");
    } else {
        let _ = sp.clear(serialport::ClearBuffer::Input);
    }
    Ok(sp)
}

/// Like [`open_console`] but with a short read timeout so the TUI's drain loop stays snappy
/// (it polls the keyboard between reads and can't afford to block on a quiet link). Same
/// line-state / reset / stale-input handling as [`open_console`]. Kept fallible so the
/// console's *first* open is fatal: a bad `--device` exits 1 instead of spinning in the
/// reconnect loop (the contract the streaming commands already honour).
pub(crate) fn open_console_responsive(port: &str, reset: bool) -> Result<Box<dyn serialport::SerialPort>> {
    let mut sp = serialport::new(port, 115_200)
        .timeout(Duration::from_millis(10))
        .open()
        .with_context(|| format!("opening {port}"))?;
    set_run_baseline(&mut *sp)?;
    if reset {
        pulse_reset_into_app(&mut *sp)?;
        eprintln!("[tower] reset into application");
    } else {
        let _ = sp.clear(serialport::ClearBuffer::Input);
    }
    Ok(sp)
}

#[cfg(test)]
mod tests {
    use super::{Board, NATIVE_USB_CONSOLE, Port, choose, device_line, dfu_line, prefer_callout};

    fn port(name: &str, usb_serial: Option<&str>) -> Port {
        Port {
            name: name.into(),
            usb: true,
            usb_serial: usb_serial.map(String::from),
            usb_id: Some((0x0403, 0x6015)),
        }
    }

    /// One port is taken as is; among several, a wanted board kind breaks the tie only when it
    /// is the one port of that kind; otherwise the error names every port with its kind.
    #[test]
    fn choosing_a_port() {
        let core = port("/dev/ttyUSB0", Some("hio-core-module-2416969388"));
        let dongle = port("/dev/ttyUSB1", Some("bc-usb-dongle-r1.0-6435252593b5"));
        let other = port("/dev/ttyUSB2", Some("A50285BI"));
        assert_eq!(
            choose(vec![core.clone()], Some(Board::RadioDongle)).unwrap(),
            ("/dev/ttyUSB0".into(), false)
        );
        let pair = vec![core.clone(), dongle.clone()];
        assert_eq!(
            choose(pair.clone(), Some(Board::RadioDongle)).unwrap(),
            ("/dev/ttyUSB1".into(), true)
        );
        assert_eq!(
            choose(pair.clone(), Some(Board::CoreModule)).unwrap(),
            ("/dev/ttyUSB0".into(), true)
        );
        let e = format!("{:#}", choose(pair, None).unwrap_err());
        assert!(
            e.contains("/dev/ttyUSB0 (Core Module), /dev/ttyUSB1 (Radio Dongle)"),
            "{e}"
        );
        let two_dongles = vec![
            dongle.clone(),
            port("/dev/ttyUSB3", Some("bc-usb-dongle-r1.0-1")),
            other,
        ];
        assert!(choose(two_dongles, Some(Board::RadioDongle)).is_err());
        assert!(choose(Vec::new(), None).is_err());
    }

    #[test]
    fn device_lines_start_with_the_path() {
        let core = port("/dev/cu.usbserial-2110", Some("hio-core-module-1"));
        assert_eq!(
            device_line(&core, 22),
            "/dev/cu.usbserial-2110  Core Module   hio-core-module-1"
        );
        assert_eq!(device_line(&port("COM3", None), 4), "COM3  USB serial");
        let builtin = Port {
            name: "/dev/cu.Bluetooth-Incoming-Port".into(),
            usb: false,
            usb_serial: None,
            usb_id: None,
        };
        assert_eq!(device_line(&builtin, 40), "/dev/cu.Bluetooth-Incoming-Port");
        assert_eq!(
            dfu_line("dfu:1-2", Some("206E3A6B5048"), 7),
            "dfu:1-2  STM32 DFU     206E3A6B5048"
        );
        assert_eq!(dfu_line("dfu:1-2", None, 7), "dfu:1-2  STM32 DFU");
    }

    /// The r1 USB CDC console is told apart from an r2 FTDI port by its USB id, and named so.
    #[test]
    fn a_core_module_r1_console_is_recognised() {
        let r1 = Port {
            usb_id: Some(NATIVE_USB_CONSOLE),
            ..port(
                "/dev/ttyACM0",
                Some("hio-core-module-r1-0123456789ABCDEF01234567"),
            )
        };
        assert!(r1.native_usb());
        assert_eq!(r1.board(), Some(Board::CoreModule));
        assert_eq!(r1.label(), "/dev/ttyACM0 (Core Module r1)");
        assert!(device_line(&r1, 12).starts_with("/dev/ttyACM0  Core Module r1  hio-core-module-r1-"));
        // An r2 board (FTDI) and some other ST virtual COM port are not.
        assert!(!port("/dev/ttyUSB0", Some("hio-core-module-1")).native_usb());
        let st_vcp = Port {
            usb_id: Some(NATIVE_USB_CONSOLE),
            ..port("/dev/ttyACM1", Some("00000000001A"))
        };
        assert!(!st_vcp.native_usb());
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn macos_dial_in_twin_is_dropped() {
        // One board on macOS: serialport lists both nodes; auto-detect must see exactly one.
        let got = prefer_callout(names(&["/dev/cu.usbserial-2110", "/dev/tty.usbserial-2110"]));
        assert_eq!(got, names(&["/dev/cu.usbserial-2110"]));
        // Order doesn't matter, and two real boards stay two.
        let got = prefer_callout(names(&[
            "/dev/tty.usbserial-A",
            "/dev/cu.usbserial-A",
            "/dev/cu.usbserial-B",
            "/dev/tty.usbserial-B",
        ]));
        assert_eq!(got, names(&["/dev/cu.usbserial-A", "/dev/cu.usbserial-B"]));
    }

    #[test]
    fn lone_tty_and_other_platforms_are_untouched() {
        // A tty. node without its cu. twin is kept; Linux/Windows names pass through.
        let v = names(&["/dev/tty.usbmodem1", "/dev/ttyUSB0", "/dev/ttyACM0", "COM3"]);
        assert_eq!(prefer_callout(v.clone()), v);
    }
}
