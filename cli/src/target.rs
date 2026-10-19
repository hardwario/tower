//! Where the bootloader commands (`tower flash` / `erase` / `chip` / `reset`) reach the STM32 ROM
//! bootloader: through a serial port (the UART bootloader behind the board's USB-UART bridge,
//! entered by the bridge's RTS/DTR: a Core Module r2, a Radio Dongle) or through USB DFU (a Core
//! Module r1.x, which has no bridge; tower-flasher's `dfu`).
//!
//! A Core Module r1.x gets into its DFU bootloader in one of two ways: by hand (hold BOOT, press
//! and release RESET, release BOOT), or, when it runs TOWER firmware built for it, by the same
//! reset-into-bootloader line sequence the UART path uses: its USB CDC console mirrors the r2
//! auto-reset circuit and reboots into the ROM bootloader. That second way **erases the start of
//! the application** (the STM32L083's bootloader, started from the application, returns to it
//! while the flash holds one; firmware/src/sysboot.rs), so this module does it only for the
//! commands that rewrite the flash anyway ([`Entry::Erasing`]), then waits for the DFU device.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tower_flasher::dfu::{self, DeviceInfo};

use crate::port;

/// How long a Core Module r1 rebooted into its bootloader may take to enumerate as the DFU
/// device (the firmware waits ~30 ms after the line sequence, the ROM bootloader then starts USB).
const DFU_APPEAR: Duration = Duration::from_secs(5);
/// Poll period while waiting for it.
const DFU_POLL: Duration = Duration::from_millis(100);

/// How to put a Core Module r1.x into its USB DFU bootloader by hand (nothing is erased).
const BUTTONS: &str = "hold BOOT, press and release RESET, release BOOT";

/// The hint for a user with neither a serial port nor a DFU device, or a board that did not come
/// back as one.
pub(crate) const MANUAL_DFU_HINT: &str = "a Core Module r1.x without TOWER firmware enters its USB DFU \
     bootloader by hand: hold BOOT, press and release RESET, release BOOT";

/// The ROM bootloader a command talks to.
pub(crate) enum Target {
    /// The UART bootloader through this serial port.
    Serial(String),
    /// A USB DFU bootloader.
    Dfu { device: DeviceInfo },
}

impl Target {
    /// `dfu:<location>` or the serial port, for messages.
    pub(crate) fn label(&self) -> String {
        match self {
            Target::Serial(p) => p.clone(),
            Target::Dfu { device, .. } => format!("dfu:{}", device.location()),
        }
    }
}

/// What a command may do to get a Core Module r1 running TOWER firmware into its DFU bootloader.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    /// Reboot it into DFU over its console, erasing the start of its application: for the
    /// commands that rewrite the flash (`flash`, `erase`).
    Erasing,
    /// Never: a read-only command (`chip`) asks for the buttons instead.
    Refuse,
    /// Leave the console as the target (`reset`: it emulates the reset lines itself).
    Console,
}

/// Pick the bootloader for a command. `-d dfu` takes the sole DFU device, `-d dfu:<location>`
/// one by its location (as `tower devices` lists it), `-d <port>` that serial port. Without `-d`
/// a sole DFU device wins (a board someone put in DFU is the one to flash), else the serial
/// auto-detect decides. A serial port that is a Core Module r1 USB console is handled as `entry`
/// says.
pub(crate) fn pick(explicit: Option<String>, entry: Entry) -> Result<Target> {
    if let Some(spec) = explicit.as_deref().and_then(dfu_spec) {
        return Ok(Target::Dfu {
            device: select_dfu(spec)?,
        });
    }
    let path = match explicit {
        Some(p) => p,
        None => {
            let found = list_or_warn();
            match found.as_slice() {
                [only] => {
                    eprintln!(
                        "[tower] using the STM32 DFU bootloader at dfu:{} (pass -d/--device to pick a serial port)",
                        only.location()
                    );
                    return Ok(Target::Dfu { device: only.clone() });
                }
                [] => port::pick_port(None).map_err(|e| e.context(MANUAL_DFU_HINT))?,
                several => bail!(
                    "several STM32 DFU bootloaders; pass -d/--device (one of: {})",
                    locations(several)
                ),
            }
        }
    };
    if entry != Entry::Console && port::is_native_usb_console(&path)? {
        if entry == Entry::Refuse {
            bail!(
                "{path} is a Core Module r1 running TOWER firmware: from the application its DFU bootloader is \
                 reached only by erasing the start of the application, which a read-only command won't do; \
                 put it into DFU by hand ({BUTTONS}; nothing is erased), then run it again"
            );
        }
        return Ok(Target::Dfu {
            device: enter_dfu(&path)?,
        });
    }
    Ok(Target::Serial(path))
}

/// The DFU bootloaders attached, or none when USB cannot be enumerated (a container that was
/// handed only a serial port): the serial path must keep working there, so that is a warning.
pub(crate) fn list_or_warn() -> Vec<DeviceInfo> {
    dfu::list().unwrap_or_else(|e| {
        eprintln!("[tower] warning: cannot list USB DFU devices ({e}); looking at serial ports only");
        Vec::new()
    })
}

/// `Some(location)` for `dfu:<location>`, `Some("")` for a bare `dfu`.
fn dfu_spec(device: &str) -> Option<&str> {
    if device == "dfu" {
        Some("")
    } else {
        device.strip_prefix("dfu:")
    }
}

fn locations(devices: &[DeviceInfo]) -> String {
    devices
        .iter()
        .map(|d| format!("dfu:{}", d.location()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The DFU device at `location`, or the sole one for an empty `location`.
fn select_dfu(location: &str) -> Result<DeviceInfo> {
    let found = dfu::list().context("listing USB DFU devices")?;
    if location.is_empty() {
        return match found.as_slice() {
            [only] => Ok(only.clone()),
            [] => bail!("no STM32 DFU bootloader found; {MANUAL_DFU_HINT}"),
            several => bail!(
                "several STM32 DFU bootloaders; pass -d dfu:<location> (one of: {})",
                locations(several)
            ),
        };
    }
    found
        .iter()
        .find(|d| d.location() == location)
        .cloned()
        .with_context(|| {
            format!(
                "no STM32 DFU bootloader at dfu:{location} (`tower devices` lists them{})",
                if found.is_empty() {
                    "; none is attached"
                } else {
                    ""
                }
            )
        })
}

/// Reboot the Core Module r1 on the USB console `path` into its ROM DFU bootloader and return
/// the DFU device that appears. The line sequence is the UART path's own
/// ([`Port::reset_into_bootloader`](tower_flasher::port::Port::reset_into_bootloader)); the
/// firmware reads it off the CDC control lines. The new device is the one that was not there
/// before.
fn enter_dfu(path: &str) -> Result<DeviceInfo> {
    let before: Vec<String> = list_or_warn().iter().map(DeviceInfo::location).collect();
    eprintln!(
        "[tower] {path} is a Core Module r1 USB console: rebooting it into its USB DFU bootloader \
         (this erases the start of its application, which is about to be rewritten)"
    );
    // The console reboots ~30 ms after RESET is released, while the sequence still holds BOOT0,
    // so the line writes after that fail on a port that is gone; whether it worked is whether the
    // DFU bootloader appears.
    let _ = request_bootloader(path)?;
    wait_for_new_dfu(&before, path)
}

/// Play the reset-into-bootloader line sequence on a Core Module r1 USB console. The firmware
/// reboots shortly after RESET is released, so the port may vanish under the rest of the
/// sequence: an error past opening the port is returned as `Some` (the caller decides whether
/// the board came back as wanted), only a failure to open it is an `Err`.
pub(crate) fn request_bootloader(path: &str) -> Result<Option<tower_flasher::error::Error>> {
    let mut sp = tower_flasher::port::Port::open(path).with_context(|| format!("opening {path}"))?;
    Ok(sp.reset_into_bootloader().err())
}

/// Wait for the STM32 DFU bootloader at a location not in `before` to appear.
pub(crate) fn wait_for_new_dfu(before: &[String], path: &str) -> Result<DeviceInfo> {
    let deadline = Instant::now() + DFU_APPEAR;
    loop {
        let appeared = list_or_warn()
            .into_iter()
            .find(|d| !before.contains(&d.location()));
        if let Some(d) = appeared {
            return Ok(d);
        }
        if Instant::now() >= deadline {
            bail!(
                "{path} did not come back as a USB DFU bootloader within {} s (is it running TOWER firmware \
                 built with the `core-module-r1` feature?); {MANUAL_DFU_HINT}",
                DFU_APPEAR.as_secs()
            );
        }
        std::thread::sleep(DFU_POLL);
    }
}

/// Open a DFU bootloader on its main-flash interface.
pub(crate) fn open_dfu(device: &DeviceInfo) -> Result<dfu::Dfu<dfu::UsbTransport>> {
    dfu::Dfu::open(device).with_context(|| {
        let mut what = format!("opening the STM32 DFU bootloader at dfu:{}", device.location());
        if cfg!(target_os = "windows") {
            what.push_str(" (Windows needs the WinUSB driver bound to it, e.g. with Zadig)");
        }
        what
    })
}

#[cfg(test)]
mod tests {
    use super::dfu_spec;

    #[test]
    fn dfu_device_specs() {
        assert_eq!(dfu_spec("dfu"), Some(""));
        assert_eq!(dfu_spec("dfu:1-2.3"), Some("1-2.3"));
        assert_eq!(dfu_spec("/dev/ttyACM0"), None);
        assert_eq!(dfu_spec("dfux"), None);
    }
}
