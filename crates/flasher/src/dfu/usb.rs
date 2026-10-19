//! The USB side of [`super`]: finding the STM32 ROM DFU bootloaders attached (`0483:df11`) and
//! talking to one through [`nusb`] (pure Rust: usbfs on Linux, IOKit on macOS, WinUSB on
//! Windows). Opening one reads the DfuSe memory layout of each alternate setting of its DFU
//! interface, selects the one of the main flash and hands it to [`Dfu::new`].
//!
//! Access: on Linux the user needs write access to the device node (a udev rule for
//! `0483:df11`); on Windows the device must be bound to the WinUSB driver (e.g. with Zadig);
//! macOS needs nothing.

use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient, TransferError};

use super::{Dfu, Layout, PID, Transport, VID, request_name};
use crate::error::{Error, Result};
use crate::target;

/// Timeout of one control transfer. The long operations (an erase) run during the poll timeouts
/// between requests, not inside a request, so this only has to outlast a slow host.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The language of the string descriptors (US English, the one ST's bootloaders carry).
const LANG_EN_US: u16 = 0x0409;
/// Interface class / subclass / protocol of a device in DFU mode (DFU 1.1, 4.2.3).
const DFU_INTERFACE: (u8, u8, u8) = (0xFE, 0x01, 0x02);
/// Descriptor type of the DFU functional descriptor (DFU 1.1, 4.1.3).
const DFU_FUNCTIONAL: u8 = 0x21;

/// One attached STM32 ROM DFU bootloader, as enumerated (nothing is opened).
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    info: nusb::DeviceInfo,
}

impl DeviceInfo {
    /// Where it is attached: the bus and the hub-port chain (`1-2.3`), stable across a
    /// re-enumeration on the same port.
    pub fn location(&self) -> String {
        let ports: Vec<String> = self.info.port_chain().iter().map(u8::to_string).collect();
        format!("{}-{}", self.info.bus_id(), ports.join("."))
    }

    /// The device's USB serial number (ST derives it from the chip's unique id), if the OS
    /// cached it.
    pub fn serial(&self) -> Option<&str> {
        self.info.serial_number()
    }

    /// `bcdDevice`: the bootloader version.
    pub fn bootloader_version(&self) -> u16 {
        self.info.device_version()
    }
}

/// The STM32 ROM DFU bootloaders attached now.
pub fn list() -> Result<Vec<DeviceInfo>> {
    let devices = nusb::list_devices().wait().map_err(|e| Error::Usb {
        context: "listing USB devices",
        source: e.into(),
    })?;
    Ok(devices
        .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
        .map(|info| DeviceInfo { info })
        .collect())
}

/// The DFU interface of an open device, on the main-flash alternate setting.
pub struct UsbTransport {
    iface: nusb::Interface,
    number: u8,
}

fn transfer_error(request: u8, e: TransferError) -> Error {
    match e {
        TransferError::Stall => Error::UsbStall {
            request: request_name(request),
        },
        e => Error::Usb {
            context: request_name(request),
            source: e.into(),
        },
    }
}

impl Transport for UsbTransport {
    fn control_out(&mut self, request: u8, value: u16, data: &[u8]) -> Result<()> {
        self.iface
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request,
                    value,
                    index: u16::from(self.number),
                    data,
                },
                TIMEOUT,
            )
            .wait()
            .map_err(|e| transfer_error(request, e))
    }

    fn control_in(&mut self, request: u8, value: u16, length: u16) -> Result<Vec<u8>> {
        self.iface
            .control_in(
                ControlIn {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request,
                    value,
                    index: u16::from(self.number),
                    length,
                },
                TIMEOUT,
            )
            .wait()
            .map_err(|e| transfer_error(request, e))
    }
}

/// `wTransferSize` of the DFU functional descriptor among `descriptors`.
fn transfer_size<'a>(
    mut descriptors: impl Iterator<Item = nusb::descriptors::Descriptor<'a>>,
) -> Option<u16> {
    descriptors
        .find(|d| d.descriptor_type() == DFU_FUNCTIONAL && d.len() >= 7)
        .map(|d| u16::from_le_bytes([d[5], d[6]]))
}

impl Dfu<UsbTransport> {
    /// Open `device`, select its main-flash alternate setting and bring it to `dfuIDLE`.
    pub fn open(device: &DeviceInfo) -> Result<Self> {
        let usb_err = |context: &'static str| {
            move |e: nusb::Error| Error::Usb {
                context,
                source: e.into(),
            }
        };
        let dev = device
            .info
            .open()
            .wait()
            .map_err(usb_err("opening the DFU bootloader"))?;
        let config = dev.active_configuration().map_err(|e| Error::Usb {
            context: "reading the DFU bootloader's configuration",
            source: e.into(),
        })?;
        let mut size = None;
        let mut flash = None;
        let mut names = Vec::new();
        for alt in config.interface_alt_settings() {
            if (alt.class(), alt.subclass(), alt.protocol()) != DFU_INTERFACE {
                continue;
            }
            size = size.or_else(|| transfer_size(alt.descriptors()));
            let Some(index) = alt.string_index() else {
                continue;
            };
            let name = dev
                .get_string_descriptor(index, LANG_EN_US, TIMEOUT)
                .wait()
                .map_err(|e| Error::Usb {
                    context: "reading a DFU memory layout",
                    source: e.into(),
                })?;
            if flash.is_none()
                && let Some(layout) = Layout::parse(&name)
                && layout.start() == target::FLASH_BASE
            {
                flash = Some((alt.interface_number(), alt.alternate_setting(), layout));
            }
            names.push(name.trim().to_string());
        }
        let size = size
            .or_else(|| transfer_size(config.descriptors()))
            .ok_or(Error::DfuDevice {
                reason: "the DFU bootloader has no DFU functional descriptor (no transfer size)".into(),
            })?;
        let Some((number, alt, layout)) = flash else {
            return Err(Error::DfuDevice {
                reason: format!(
                    "the DFU bootloader offers no main-flash memory at 0x{:08X} (it offers: {})",
                    target::FLASH_BASE,
                    if names.is_empty() {
                        "nothing".into()
                    } else {
                        names.join(", ")
                    }
                ),
            });
        };
        let iface = dev
            .claim_interface(number)
            .wait()
            .map_err(usb_err("claiming the DFU interface"))?;
        iface
            .set_alt_setting(alt)
            .wait()
            .map_err(usb_err("selecting the main-flash alternate setting"))?;
        let version = dev.device_descriptor().device_version();
        Dfu::new(UsbTransport { iface, number }, layout, size, version)
    }
}
