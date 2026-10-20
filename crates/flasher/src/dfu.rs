//! USB DFU flashing through the STM32 ROM bootloader (ST's DfuSe extensions of DFU 1.1a, AN3156),
//! for a board whose STM32 USB pins go straight to its connector: the TOWER **Core Module r1.x**
//! (the 2016 board, without a USB-UART bridge). There the UART path of [`crate::flash`] has no
//! port to talk to; the ROM bootloader's USB DFU interface (`0483:df11`) is the way in.
//!
//! **Entering it.** BOOT0 high at reset selects the system memory: hold BOOT, press and release
//! RESET, release BOOT. With TOWER firmware built for that board (the SDK's `core-module-r1`
//! feature) running, the `tower` CLI gets there without buttons: the firmware's USB CDC console
//! mimics the r2 auto-reset circuit, so the reset-into-bootloader line sequence of
//! [`Port::reset_into_bootloader`](crate::port::Port::reset_into_bootloader) on its port reboots
//! it into the ROM bootloader.
//!
//! **The same guarantees as the UART path:**
//! - *Main flash only.* Only the alternate setting whose memory layout starts at
//!   [`target::FLASH_BASE`] is ever selected ([`Layout::check_main_flash`]), and every erase and
//!   write is checked against that layout before it is sent ([`Dfu::erase_page`],
//!   [`Dfu::write`]). The data EEPROM (radio keys, TX counters) and the option bytes sit behind
//!   other alternate settings and are never selected. Neither DfuSe's mass erase (whose reach on
//!   the STM32L0 AN3156 does not pin down) nor Read Unprotect (`0x92`, a mass erase of the flash
//!   AND the data EEPROM) is ever built: the only command bytes this module sends are
//!   [`CMD_SET_ADDRESS`] and [`CMD_ERASE`] with an address (a test scans the source for that).
//! - *Vector table last.* [`flash`] writes the first transfer block (it holds the vector table)
//!   after the rest is written and verified, so an interrupted flash never leaves a bootable
//!   truncated image (the rationale is on [`crate::flash::flash`]).
//! - *Never guess the protection state.* Before anything is erased, [`flash`] and [`erase`] read
//!   four bytes at the flash base. While readout protection (RDP level 1) is active the ROM
//!   bootloader does not perform a read; it stalls it and its status turns to `errVENDOR`
//!   (AN3156 4.1, and Figure 4's host side: a refused upload whose status is `errVENDOR` is
//!   "protection is active", any other status an error). That, and only that, is
//!   [`Error::ReadoutProtected`]. Protection is never changed over DFU; `tower protect` /
//!   `unprotect` need the UART bootloader.
//!
//! **Where the board is left.** Unlike the UART path, nothing here resets into the application
//! on a failure: without NRST the only way to start it is the bootloader's own jump
//! ([`Dfu::leave`]), and a board put into DFU with the buttons would then need the buttons again
//! for the corrected retry. So the board stays in the bootloader on every path except a
//! successful [`flash`] with [`FlashOptions::run`], which leaves DFU into the new application.
//! [`Dfu::leave`] starts the application on request (`tower reset`).
//!
//! **Protocol** (AN3156 sections 3 to 5; the sequences match dfu-util's DfuSe code). Every
//! request is a class request to the DFU interface. A DfuSe command (`DFU_DNLOAD` block 0: Set
//! Address Pointer `0x21` + address, 5.2; Erase `0x41` + page address, 5.3) and a Write memory
//! (block > 1, 5.1) are carried out only on the `DFU_GETSTATUS` that follows, which answers
//! `dfuDNBUSY`; the host waits the reported `bwPollTimeout` and asks again until the state leaves
//! `dfuDNBUSY` (`dfuDNLOAD-IDLE` with status OK is success), then returns to `dfuIDLE` with
//! `DFU_ABORT`, the state an upload must start from (Figure 3). Data moves as `DFU_DNLOAD` /
//! `DFU_UPLOAD` block 2 right after setting the address pointer, so the address
//! `((wBlockNum - 2) * wTransferSize) + pointer` (4.1, 5.1) is the pointer itself. Leave DFU (5.5)
//! is a zero-length `DFU_DNLOAD` after setting the pointer to the application; the
//! `DFU_GETSTATUS` that follows answers `dfuMANIFEST`, and the bootloader disconnects and jumps to
//! the reset vector at `pointer + 4` (a jump, not a reset: AN3156 5.5 requires the application to
//! reset the core before it enables interrupts, which the TOWER SDK does).

use std::time::Duration;

use crate::error::{Error, Result};
use crate::flash::{FlashOptions, Progress, check_firmware_len, pad_block};
use crate::target;

mod usb;

#[cfg(test)]
mod fake;

pub use usb::{DeviceInfo, UsbTransport, list};

/// USB vendor id of the STM32 ROM DFU bootloader (STMicroelectronics).
pub const VID: u16 = 0x0483;
/// USB product id of the STM32 ROM DFU bootloader ("STM32 BOOTLOADER", `0483:df11`).
pub const PID: u16 = 0xDF11;

// DFU 1.1 class requests.
const DFU_DNLOAD: u8 = 1;
const DFU_UPLOAD: u8 = 2;
const DFU_GETSTATUS: u8 = 3;
const DFU_CLRSTATUS: u8 = 4;
const DFU_ABORT: u8 = 6;

/// DfuSe Set Address Pointer (`DFU_DNLOAD` block 0, followed by the address, little-endian).
pub const CMD_SET_ADDRESS: u8 = 0x21;
/// DfuSe Erase, always with the 4-byte address of the page to erase. Sent alone it would be a
/// mass erase, which this module never sends.
pub const CMD_ERASE: u8 = 0x41;

// DFU 1.1 `bState` values.
const STATE_DFU_IDLE: u8 = 2;
const STATE_DNBUSY: u8 = 4;
const STATE_DNLOAD_IDLE: u8 = 5;
const STATE_MANIFEST: u8 = 7;
const STATE_ERROR: u8 = 10;

// DFU 1.1 `bStatus` values the engine acts on.
const STATUS_OK: u8 = 0x00;
/// `errVENDOR`: what the STM32 ROM bootloader answers a memory read with while readout
/// protection is active.
pub const STATUS_ERR_VENDOR: u8 = 0x0B;

/// Bound on the `DFU_GETSTATUS` polls of one operation while the device reports `dfuDNBUSY`
/// (each poll first waits the device's `bwPollTimeout`), so a device stuck busy fails instead of
/// hanging the flash.
const MAX_BUSY_POLLS: u32 = 2000;
/// Upper bound on the transfer size taken from the device's DFU functional descriptor: a read or
/// write of the internal flash is 2 to 2048 bytes (AN3156 4.1, 5.1).
const MAX_TRANSFER: usize = 2048;
/// Ceiling on one `bwPollTimeout` wait: a device reporting minutes still gets polled.
const MAX_POLL_WAIT: Duration = Duration::from_secs(5);
/// `DFU_GETSTATUS` polls of [`Dfu::leave`] before it gives up on the bootloader dropping off the
/// bus, [`LEAVE_POLL_WAIT`] apart (2 s in all).
const LEAVE_POLLS: u32 = 40;
/// Poll after which [`Dfu::leave`] sends the refused request that makes the STM32L0's bootloader
/// jump (see there).
const LEAVE_NUDGE_AFTER: u32 = 3;
/// Wait between the polls of [`Dfu::leave`].
const LEAVE_POLL_WAIT: Duration = Duration::from_millis(50);

/// Name of a DFU 1.1 `bStatus` code, for error messages.
pub fn status_name(status: u8) -> &'static str {
    match status {
        0x00 => "OK",
        0x01 => "errTARGET (the file is not for this device)",
        0x02 => "errFILE",
        0x03 => "errWRITE (cannot write memory)",
        0x04 => "errERASE (memory erase failed)",
        0x05 => "errCHECK_ERASED (memory erase check failed)",
        0x06 => "errPROG (program memory failed)",
        0x07 => "errVERIFY (programmed memory failed verification)",
        0x08 => "errADDRESS (address out of range)",
        0x09 => "errNOTDONE (download ended too early)",
        0x0A => "errFIRMWARE (the device's firmware is corrupt)",
        0x0B => "errVENDOR (on an STM32: readout protection is active)",
        0x0C => "errUSBR (unexpected USB reset)",
        0x0D => "errPOR (unexpected power-on reset)",
        0x0E => "errUNKNOWN",
        0x0F => "errSTALLEDPKT (unexpected request)",
        _ => "unknown status",
    }
}

/// Name of a DFU 1.1 `bState` code, for error messages.
pub fn state_name(state: u8) -> &'static str {
    match state {
        0 => "appIDLE",
        1 => "appDETACH",
        2 => "dfuIDLE",
        3 => "dfuDNLOAD-SYNC",
        4 => "dfuDNBUSY",
        5 => "dfuDNLOAD-IDLE",
        6 => "dfuMANIFEST-SYNC",
        7 => "dfuMANIFEST",
        8 => "dfuMANIFEST-WAIT-RESET",
        9 => "dfuUPLOAD-IDLE",
        10 => "dfuERROR",
        _ => "unknown",
    }
}

/// Name of a DFU class request, for transport errors.
pub(crate) fn request_name(request: u8) -> &'static str {
    match request {
        DFU_DNLOAD => "DFU_DNLOAD",
        DFU_UPLOAD => "DFU_UPLOAD",
        DFU_GETSTATUS => "DFU_GETSTATUS",
        DFU_CLRSTATUS => "DFU_CLRSTATUS",
        DFU_ABORT => "DFU_ABORT",
        _ => "a DFU request",
    }
}

/// The class requests the engine sends, to the DFU interface of the device. The USB backend is
/// [`UsbTransport`]; the tests drive a simulated STM32 bootloader.
pub trait Transport {
    /// A host-to-device class request with `data` as its data stage. A request the device
    /// refuses fails with [`Error::UsbStall`].
    fn control_out(&mut self, request: u8, value: u16, data: &[u8]) -> Result<()>;
    /// A device-to-host class request reading at most `length` bytes. A refusal is
    /// [`Error::UsbStall`].
    fn control_in(&mut self, request: u8, value: u16, length: u16) -> Result<Vec<u8>>;
    /// Wait out a device's `bwPollTimeout`.
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// One `DFU_GETSTATUS` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status {
    status: u8,
    poll_timeout: Duration,
    state: u8,
}

impl Status {
    fn parse(b: &[u8]) -> Option<Self> {
        let &[status, t0, t1, t2, state, ..] = b else {
            return None;
        };
        let ms = u32::from_le_bytes([t0, t1, t2, 0]);
        Some(Self {
            status,
            poll_timeout: Duration::from_millis(u64::from(ms)).min(MAX_POLL_WAIT),
            state,
        })
    }
}

/// One run of equal pages in a DfuSe memory layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Address of the first page.
    pub start: u32,
    /// Number of pages.
    pub pages: u32,
    /// Size of each page in bytes: the erase granularity.
    pub page_size: u32,
    /// The type letter's bits (`a`..`g` = 1..7): 1 readable, 2 erasable, 4 writable.
    pub access: u8,
}

impl Segment {
    /// One past the last byte.
    pub fn end(&self) -> u64 {
        u64::from(self.start) + u64::from(self.pages) * u64::from(self.page_size)
    }

    fn contains(&self, addr: u32) -> bool {
        addr >= self.start && u64::from(addr) < self.end()
    }
}

/// The memory layout a DfuSe alternate setting describes in its interface string (UM0424 4.3.2),
/// e.g. `@Internal Flash  /0x08000000/1536*128 g`: a name, then one or more
/// `/0xADDRESS/count*size<unit><type>[,count*size<unit><type>...]` groups, the unit being a byte
/// count (` ` or `B`), `K` or `M`, the type a letter `a`..`g`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The name before the first `/`, trimmed (`Internal Flash`).
    pub name: String,
    /// The segments, in address order as listed.
    pub segments: Vec<Segment>,
}

impl Layout {
    /// Parse an interface string; `None` when it is not a DfuSe layout.
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.trim().strip_prefix('@')?;
        let mut groups = rest.split('/');
        let name = groups.next()?.trim().to_string();
        let mut segments = Vec::new();
        let mut parts: Vec<&str> = groups.collect();
        // A trailing '/' leaves an empty last part.
        if parts.last() == Some(&"") {
            parts.pop();
        }
        if parts.is_empty() || !parts.len().is_multiple_of(2) {
            return None;
        }
        for pair in parts.chunks(2) {
            let hex = pair[0].trim();
            let hex = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X"))?;
            let mut addr = u32::from_str_radix(hex, 16).ok()?;
            for desc in pair[1].split(',') {
                let seg = parse_segment(desc.trim_start(), addr)?;
                addr = u32::try_from(seg.end()).ok()?;
                segments.push(seg);
            }
        }
        Some(Self { name, segments })
    }

    /// Address of the first byte.
    pub fn start(&self) -> u32 {
        self.segments.first().map_or(0, |s| s.start)
    }

    /// Bytes from [`start`](Self::start) to the end of the last segment.
    pub fn size(&self) -> u64 {
        self.segments.last().map_or(0, |s| s.end()) - u64::from(self.start())
    }

    /// The page holding `addr`: its start and size.
    fn page_at(&self, addr: u32) -> Option<(u32, u32)> {
        let seg = self.segments.iter().find(|s| s.contains(addr))?;
        let index = (addr - seg.start) / seg.page_size;
        Some((seg.start + index * seg.page_size, seg.page_size))
    }

    /// Whether this is an STM32L0 main flash the engine may write: it starts at
    /// [`target::FLASH_BASE`], its segments follow each other without gaps, every page can be
    /// read, erased and written, and the whole fits [`target::MAX_FLASH_SIZE`]. The data EEPROM
    /// (`0x0808_0000`) and the option bytes fail the first test, which is what keeps them out of
    /// reach.
    pub fn check_main_flash(&self) -> std::result::Result<(), String> {
        if self.start() != target::FLASH_BASE {
            return Err(format!(
                "starts at 0x{:08X}, not at the main flash (0x{:08X})",
                self.start(),
                target::FLASH_BASE
            ));
        }
        let mut next = u64::from(target::FLASH_BASE);
        for s in &self.segments {
            if u64::from(s.start) != next || s.pages == 0 || s.page_size == 0 {
                return Err("its segments are not one contiguous run of pages".into());
            }
            if s.access & 7 != 7 {
                return Err(format!(
                    "the segment at 0x{:08X} is not readable, erasable and writable",
                    s.start
                ));
            }
            next = s.end();
        }
        if self.size() > u64::from(target::MAX_FLASH_SIZE) {
            return Err(format!(
                "describes {} KiB, more than any STM32L0 ({} KiB)",
                self.size() / 1024,
                target::MAX_FLASH_SIZE / 1024
            ));
        }
        Ok(())
    }
}

/// `count*size<unit><type>` starting at `start`.
fn parse_segment(desc: &str, start: u32) -> Option<Segment> {
    let (count, rest) = desc.split_once('*')?;
    let pages: u32 = count.trim().parse().ok()?;
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let size: u32 = rest[..digits].parse().ok()?;
    let mut tail = rest[digits..].chars().filter(|c| !c.is_whitespace());
    let (multiplier, kind) = match (tail.next()?, tail.next()) {
        ('B', Some(t)) => (1, t),
        ('K', Some(t)) => (1024, t),
        ('M', Some(t)) => (1024 * 1024, t),
        // No unit: a byte count followed directly by the type (`1536*128 g`).
        (t, None) => (1, t),
        _ => return None,
    };
    if !('a'..='g').contains(&kind) || tail.next().is_some() {
        return None;
    }
    Some(Segment {
        start,
        pages,
        page_size: size.checked_mul(multiplier)?,
        access: kind as u8 - b'a' + 1,
    })
}

/// An open STM32 DFU bootloader, on the alternate setting of its main flash.
pub struct Dfu<T: Transport> {
    t: T,
    layout: Layout,
    transfer_size: usize,
    bootloader_version: u16,
}

impl<T: Transport> Dfu<T> {
    /// Wrap a transport already on the main-flash alternate setting whose interface string
    /// parsed to `layout`. Refuses a layout that is not an STM32L0 main flash
    /// ([`Layout::check_main_flash`]), then brings the bootloader to `dfuIDLE` (clearing an
    /// error a previous session left).
    pub fn new(t: T, layout: Layout, transfer_size: u16, bootloader_version: u16) -> Result<Self> {
        layout.check_main_flash().map_err(|why| Error::DfuDevice {
            reason: format!("the DFU memory layout \"{}\" {why}", layout.name),
        })?;
        // A multiple of the flash write alignment, within what one control transfer can carry.
        let transfer_size =
            (usize::from(transfer_size).min(MAX_TRANSFER) / target::WRITE_ALIGN) * target::WRITE_ALIGN;
        if transfer_size < 64 {
            return Err(Error::DfuDevice {
                reason: format!("the DFU bootloader reports a {transfer_size}-byte transfer size"),
            });
        }
        let mut dfu = Self {
            t,
            layout,
            transfer_size,
            bootloader_version,
        };
        dfu.idle()?;
        Ok(dfu)
    }

    /// The main-flash layout.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Bytes per `DFU_DNLOAD` / `DFU_UPLOAD` (the functional descriptor's `wTransferSize`).
    pub fn transfer_size(&self) -> usize {
        self.transfer_size
    }

    /// The device descriptor's `bcdDevice`, whose high byte is the bootloader version on ST's DFU
    /// bootloaders (`0x2200` is v2.2, AN3156 1).
    pub fn bootloader_version(&self) -> u16 {
        self.bootloader_version
    }

    /// Size of the main flash the layout describes.
    pub fn flash_size(&self) -> u32 {
        // check_main_flash bounded it by MAX_FLASH_SIZE.
        self.layout.size() as u32
    }

    fn flash_end(&self) -> u64 {
        u64::from(target::FLASH_BASE) + u64::from(self.flash_size())
    }

    fn get_status(&mut self) -> Result<Status> {
        let b = self.t.control_in(DFU_GETSTATUS, 0, 6)?;
        Status::parse(&b).ok_or(Error::DfuDevice {
            reason: format!("short DFU_GETSTATUS answer ({} bytes)", b.len()),
        })
    }

    /// The bootloader refused `context` with `st`: clear the error state (best effort, so the
    /// next request is served) and report it.
    fn refused(&mut self, context: &'static str, st: Status) -> Error {
        if st.state == STATE_ERROR {
            let _ = self.t.control_out(DFU_CLRSTATUS, 0, &[]);
        }
        Error::Dfu {
            context,
            status: st.status,
            state: st.state,
        }
    }

    /// A transfer the device stalled: read why, clear it, report it as [`Error::Dfu`].
    fn stalled(&mut self, context: &'static str, e: Error) -> Error {
        if !matches!(e, Error::UsbStall { .. }) {
            return e;
        }
        match self.get_status() {
            Ok(st) => self.refused(context, st),
            Err(_) => e,
        }
    }

    /// Bring the bootloader to `dfuIDLE`: clear an error, abort a download or upload in progress.
    fn idle(&mut self) -> Result<()> {
        let st = self.get_status()?;
        match st.state {
            STATE_DFU_IDLE => return Ok(()),
            STATE_ERROR => self.t.control_out(DFU_CLRSTATUS, 0, &[])?,
            _ => self.t.control_out(DFU_ABORT, 0, &[])?,
        }
        let st = self.get_status()?;
        if st.state == STATE_DFU_IDLE {
            Ok(())
        } else {
            Err(Error::Dfu {
                context: "returning to dfuIDLE",
                status: st.status,
                state: st.state,
            })
        }
    }

    /// Poll `DFU_GETSTATUS` after a `DFU_DNLOAD` until the operation is done: the first answer
    /// starts it (`dfuDNBUSY`), each further one waits the reported poll timeout first. Done means
    /// `dfuDNLOAD-IDLE` with status OK; anything else is the refusal.
    fn finish_download(&mut self, context: &'static str) -> Result<()> {
        let mut st = self.get_status()?;
        let mut polls = 0;
        while st.state == STATE_DNBUSY && st.status == STATUS_OK {
            polls += 1;
            if polls > MAX_BUSY_POLLS {
                return Err(Error::Dfu {
                    context,
                    status: st.status,
                    state: st.state,
                });
            }
            self.t.sleep(st.poll_timeout);
            st = self.get_status()?;
        }
        if st.state == STATE_DNLOAD_IDLE && st.status == STATUS_OK {
            Ok(())
        } else {
            Err(self.refused(context, st))
        }
    }

    /// One DfuSe command (`DFU_DNLOAD` block 0), run to completion, then back to `dfuIDLE`.
    fn command(&mut self, payload: &[u8], context: &'static str) -> Result<()> {
        debug_assert!(
            matches!(payload, [CMD_SET_ADDRESS | CMD_ERASE, _, _, _, _]),
            "only address-carrying Set Address Pointer / Erase commands are sent"
        );
        self.t
            .control_out(DFU_DNLOAD, 0, payload)
            .map_err(|e| self.stalled(context, e))?;
        self.finish_download(context)?;
        self.abort()
    }

    fn abort(&mut self) -> Result<()> {
        self.t.control_out(DFU_ABORT, 0, &[])
    }

    fn set_address(&mut self, addr: u32) -> Result<()> {
        let a = addr.to_le_bytes();
        self.command(&[CMD_SET_ADDRESS, a[0], a[1], a[2], a[3]], "Set Address Pointer")
    }

    /// Erase the page that starts at `addr`, which must be a page of the main flash.
    pub fn erase_page(&mut self, addr: u32) -> Result<()> {
        match self.layout.page_at(addr) {
            Some((start, _)) if start == addr && u64::from(addr) < self.flash_end() => {}
            _ => {
                return Err(Error::InvalidArgument {
                    context: "erase_page erases whole pages of main flash only (never the data EEPROM)",
                });
            }
        }
        let a = addr.to_le_bytes();
        self.command(&[CMD_ERASE, a[0], a[1], a[2], a[3]], "Erase")
    }

    /// Write `data` (at most one transfer, a multiple of the write alignment) at `addr`, which
    /// must lie in the main flash with all of `data`. The pages must have been erased.
    pub fn write(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        let end = u64::from(addr) + data.len() as u64;
        if addr < target::FLASH_BASE
            || end > self.flash_end()
            || data.is_empty()
            || data.len() > self.transfer_size
            || !data.len().is_multiple_of(target::WRITE_ALIGN)
        {
            return Err(Error::InvalidArgument {
                context: "DFU writes go to main flash only, one aligned transfer at a time",
            });
        }
        self.set_address(addr)?;
        self.t
            .control_out(DFU_DNLOAD, 2, data)
            .map_err(|e| self.stalled("writing memory", e))?;
        self.finish_download("writing memory")?;
        self.abort()
    }

    /// Read `len` bytes (at most one transfer) at `addr`, in the main flash.
    pub fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>> {
        if addr < target::FLASH_BASE
            || u64::from(addr) + len as u64 > self.flash_end()
            || len == 0
            || len > self.transfer_size
        {
            return Err(Error::InvalidArgument {
                context: "DFU reads come from main flash only, one transfer at a time",
            });
        }
        self.set_address(addr)?;
        let data = self
            .t
            .control_in(DFU_UPLOAD, 2, len as u16)
            .map_err(|e| self.stalled("reading memory", e))?;
        // dfuUPLOAD-IDLE back to dfuIDLE, for the next Set Address Pointer.
        self.abort()?;
        if data.len() != len {
            return Err(Error::DfuDevice {
                reason: format!(
                    "DFU_UPLOAD returned {} of {len} bytes at 0x{addr:08X}",
                    data.len()
                ),
            });
        }
        Ok(data)
    }

    /// Whether readout protection (RDP level 1) is active: a read of four bytes at the flash
    /// base, refused with `errVENDOR` on a protected chip (see the module docs). Any other
    /// failure is returned as the error it is.
    pub fn readout_protected(&mut self) -> Result<bool> {
        match self.read(target::FLASH_BASE, 4) {
            Ok(_) => Ok(false),
            Err(Error::Dfu {
                status: STATUS_ERR_VENDOR,
                ..
            }) => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Leave DFU into the application at the flash base (AN3156 5.5): Set Address Pointer, then
    /// a zero-length `DFU_DNLOAD`; the `DFU_GETSTATUS` that follows answers `dfuMANIFEST`, and
    /// the bootloader disconnects and jumps. Done means the device dropped off the bus (a transfer
    /// fails), so the status is polled until it does. The STM32L0's bootloader (v2.2) does not
    /// always jump on its own: seen on a rev 1.3 Core Module, it kept answering `dfuMANIFEST` and
    /// jumped only once it refused a request (`DFU_ABORT`, which `dfuMANIFEST` does not accept),
    /// so after a few `dfuMANIFEST` answers that is sent too. Any state but `dfuMANIFEST` is the
    /// refusal, and a bootloader still on the bus after 2 s of polling is an error.
    pub fn leave(&mut self) -> Result<()> {
        self.set_address(target::FLASH_BASE)?;
        self.t
            .control_out(DFU_DNLOAD, 2, &[])
            .map_err(|e| self.stalled("leaving DFU", e))?;
        let mut st = None;
        for poll in 0..LEAVE_POLLS {
            match self.get_status() {
                Err(_) => return Ok(()),
                Ok(s) if s.state != STATE_MANIFEST => return Err(self.refused("leaving DFU", s)),
                Ok(s) => st = Some(s),
            }
            if poll == LEAVE_NUDGE_AFTER {
                let _ = self.t.control_out(DFU_ABORT, 0, &[]);
            }
            self.t.sleep(LEAVE_POLL_WAIT);
        }
        Err(Error::Dfu {
            context: "leaving DFU (the bootloader is still on the bus)",
            status: st.map_or(STATUS_OK, |s| s.status),
            state: STATE_MANIFEST,
        })
    }

    /// The transport, e.g. to close it.
    pub fn into_transport(self) -> T {
        self.t
    }
}

/// What [`info`] reads from a DFU bootloader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DfuInfo {
    /// The main-flash alternate setting's memory layout.
    pub layout: Layout,
    /// Bytes per transfer.
    pub transfer_size: usize,
    /// `bcdDevice` of the bootloader; its high byte is the version (`0x2200` is v2.2, AN3156 1).
    pub bootloader_version: u16,
    /// Whether readout protection (RDP level 1) is active (see [`Dfu::readout_protected`]).
    pub readout_protected: bool,
}

/// Read-only: the layout, transfer size, bootloader version and readout-protection state. The
/// board stays in the bootloader (see the module docs).
pub fn info<T: Transport>(dfu: &mut Dfu<T>, progress: &mut dyn FnMut(Progress)) -> Result<DfuInfo> {
    report_layout(dfu, progress);
    let readout_protected = dfu.readout_protected()?;
    progress(Progress::ReadoutProbed { readout_protected });
    Ok(DfuInfo {
        layout: dfu.layout.clone(),
        transfer_size: dfu.transfer_size,
        bootloader_version: dfu.bootloader_version,
        readout_protected,
    })
}

fn report_layout<T: Transport>(dfu: &Dfu<T>, progress: &mut dyn FnMut(Progress)) {
    progress(Progress::DfuLayout {
        name: dfu.layout.name.clone(),
        flash_size: dfu.flash_size(),
    });
}

/// The protection check of [`flash`] and [`erase`], before anything is modified.
fn refuse_if_protected<T: Transport>(
    dfu: &mut Dfu<T>,
    operation: &'static str,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let readout_protected = dfu.readout_protected()?;
    progress(Progress::ReadoutProbed { readout_protected });
    if readout_protected {
        Err(Error::ReadoutProtected { operation })
    } else {
        Ok(())
    }
}

/// Erase the pages holding `[FLASH_BASE, FLASH_BASE + len)`, reporting progress; the number of
/// pages erased.
fn erase_span<T: Transport>(dfu: &mut Dfu<T>, len: u64, progress: &mut dyn FnMut(Progress)) -> Result<usize> {
    let end = u64::from(target::FLASH_BASE) + len;
    let mut pages = Vec::new();
    let mut addr = target::FLASH_BASE;
    while u64::from(addr) < end {
        let (start, size) = dfu.layout.page_at(addr).ok_or(Error::InvalidArgument {
            context: "the image runs past the DFU memory layout",
        })?;
        pages.push(start);
        addr = start + size;
    }
    let total = pages.len();
    progress(Progress::Erase {
        pages_done: 0,
        pages_total: total,
    });
    for (i, page) in pages.into_iter().enumerate() {
        dfu.erase_page(page)?;
        progress(Progress::Erase {
            pages_done: i + 1,
            pages_total: total,
        });
    }
    Ok(total)
}

/// Erase the whole main flash the layout describes (the data EEPROM is not part of it), after
/// the protection check. The number of pages erased. The board stays in the bootloader: there is
/// no application left to start, and a [`flash`] can follow at once.
pub fn erase<T: Transport>(dfu: &mut Dfu<T>, progress: &mut dyn FnMut(Progress)) -> Result<usize> {
    report_layout(dfu, progress);
    refuse_if_protected(dfu, "erase", progress)?;
    let size = u64::from(dfu.flash_size());
    erase_span(dfu, size, progress)
}

/// Flash `firmware` over DFU: the size check against the layout, the protection check, the
/// footprint erase ([`FlashOptions::erase`]), every transfer block but the first, their
/// read-back ([`FlashOptions::verify`]), then the first block (the vector table) and its
/// read-back, then leave DFU into the new application ([`FlashOptions::run`]; DFU has no reset
/// line, so [`FlashOptions::go`] changes nothing: the start is always the bootloader's jump).
///
/// Errors before the erase modify nothing; after it, the flash is partly written. Either way the
/// board stays in the bootloader, ready for a retry (see the module docs).
pub fn flash<T: Transport>(
    dfu: &mut Dfu<T>,
    firmware: &[u8],
    opts: &FlashOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    check_firmware_len(firmware.len())?;
    report_layout(dfu, progress);
    if firmware.len() as u64 > u64::from(dfu.flash_size()) {
        return Err(Error::FirmwareTooLarge {
            size: firmware.len(),
            max: dfu.flash_size(),
            max_kib: dfu.flash_size() / 1024,
        });
    }
    refuse_if_protected(dfu, "flashing", progress)?;
    if opts.erase {
        erase_span(dfu, firmware.len() as u64, progress)?;
    }

    let total = firmware.len();
    let block = dfu.transfer_size;
    let first = total.min(block);
    progress(Progress::Write {
        bytes_done: 0,
        bytes_total: total,
    });
    let mut written = 0;
    for (i, chunk) in firmware.chunks(block).enumerate().skip(1) {
        dfu.write(
            target::FLASH_BASE + (i * block) as u32,
            &pad_block(chunk, target::WRITE_ALIGN),
        )?;
        written += chunk.len();
        progress(Progress::Write {
            bytes_done: written,
            bytes_total: total,
        });
    }
    let mut verified = 0;
    if opts.verify && first < total {
        verify(dfu, firmware, first..total, &mut verified, progress)?;
    }
    dfu.write(
        target::FLASH_BASE,
        &pad_block(&firmware[..first], target::WRITE_ALIGN),
    )?;
    progress(Progress::Write {
        bytes_done: total,
        bytes_total: total,
    });
    if opts.verify {
        verify(dfu, firmware, 0..first, &mut verified, progress)?;
    }
    if opts.run {
        progress(Progress::Starting);
        dfu.leave()?;
    }
    Ok(())
}

/// Read `firmware[range]` back block by block and compare (see [`crate::flash`]'s verify).
fn verify<T: Transport>(
    dfu: &mut Dfu<T>,
    firmware: &[u8],
    range: std::ops::Range<usize>,
    verified: &mut usize,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let total = firmware.len();
    progress(Progress::Verify {
        bytes_done: *verified,
        bytes_total: total,
    });
    let mut offset = range.start;
    while offset < range.end {
        let len = dfu.transfer_size.min(range.end - offset);
        let addr = target::FLASH_BASE + offset as u32;
        let data = dfu.read(addr, len)?;
        let expected = &firmware[offset..offset + len];
        if let Some(i) = data.iter().zip(expected).position(|(got, want)| got != want) {
            return Err(Error::VerifyMismatch {
                address: addr + i as u32,
                got: data[i],
                expected: expected[i],
            });
        }
        offset += len;
        *verified += len;
        progress(Progress::Verify {
            bytes_done: *verified,
            bytes_total: total,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeDfu, Op};
    use super::*;
    use crate::flash::no_progress;

    const L0_LAYOUT: &str = "@Internal Flash  /0x08000000/1536*128 g";

    fn open(fake: FakeDfu) -> Dfu<FakeDfu> {
        Dfu::new(fake, Layout::parse(L0_LAYOUT).unwrap(), 2048, 0x2200).unwrap()
    }

    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8 | 1).collect()
    }

    #[test]
    fn parses_dfuse_layouts() {
        let l0 = Layout::parse(L0_LAYOUT).unwrap();
        assert_eq!(l0.name, "Internal Flash");
        assert_eq!(
            l0.segments,
            vec![Segment {
                start: 0x0800_0000,
                pages: 1536,
                page_size: 128,
                access: 7
            }]
        );
        assert_eq!(l0.size(), 192 * 1024);
        assert!(l0.check_main_flash().is_ok());

        let f4 = Layout::parse("@Internal Flash  /0x08000000/04*016Kg,01*064Kg,07*128Kg").unwrap();
        assert_eq!(f4.segments.len(), 3);
        assert_eq!(f4.segments[1].start, 0x0801_0000);
        assert_eq!(f4.segments[2].page_size, 128 * 1024);
        assert_eq!(f4.size(), 1024 * 1024);
        // Too big for an STM32L0.
        assert!(f4.check_main_flash().is_err());

        let f0 = Layout::parse("@Internal Flash  /0x08000000/064*0002Kg").unwrap();
        assert_eq!(f0.segments[0].page_size, 2048);

        // Other memories are parsed but are not the main flash.
        let ob = Layout::parse("@Option Bytes  /0x1FF80000/01*032 e").unwrap();
        assert_eq!(ob.segments[0].access, 5);
        assert!(ob.check_main_flash().is_err());
        let eeprom = Layout::parse("@Data EEPROM /0x08080000/06*1Kg").unwrap();
        assert!(eeprom.check_main_flash().is_err());

        for bad in [
            "Internal Flash",
            "@Flash /0x08000000/",
            "@F /zz/1*1Kg",
            "@F /0x0800/1*1Kz",
            "@F /0x0800/x",
        ] {
            assert_eq!(Layout::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_layout_that_is_not_main_flash_is_refused() {
        let wrong = Layout::parse("@Data EEPROM /0x08080000/06*1Kg").unwrap();
        assert!(matches!(
            Dfu::new(FakeDfu::new(), wrong, 2048, 0x2200),
            Err(Error::DfuDevice { .. })
        ));
        let read_only = Layout::parse("@Internal Flash  /0x08000000/1536*128 a").unwrap();
        assert!(Dfu::new(FakeDfu::new(), read_only, 2048, 0x2200).is_err());
    }

    #[test]
    fn flash_writes_verifies_and_leaves_with_the_vector_table_last() {
        let fw = image(5000);
        let mut dfu = open(FakeDfu::new());
        flash(&mut dfu, &fw, &FlashOptions::default(), &mut no_progress).unwrap();
        let fake = dfu.into_transport();
        assert_eq!(&fake.mem[..fw.len()], &fw[..]);
        // Exactly the footprint's pages were erased: 5000 bytes = 40 pages of 128.
        let erased: Vec<u32> = fake
            .ops
            .iter()
            .filter_map(|o| match o {
                Op::Erase(a) => Some(*a),
                _ => None,
            })
            .collect();
        assert_eq!(erased, (0..40).map(|p| 0x0800_0000 + p * 128).collect::<Vec<_>>());
        // The vector-table block is written after every other block and its read-back.
        let writes: Vec<u32> = fake
            .ops
            .iter()
            .filter_map(|o| match o {
                Op::Write(a, _) => Some(*a),
                _ => None,
            })
            .collect();
        assert_eq!(writes, vec![0x0800_0800, 0x0800_1000, 0x0800_0000]);
        let first_write = fake
            .ops
            .iter()
            .position(|o| *o == Op::Write(0x0800_0000, 2048))
            .unwrap();
        let last_other_read = fake
            .ops
            .iter()
            .rposition(|o| matches!(o, Op::Read(a, _) if *a != 0x0800_0000))
            .unwrap();
        assert!(first_write > last_other_read);
        // Then DFU is left, into the application at the flash base.
        assert_eq!(fake.left, Some(0x0800_0000));
    }

    #[test]
    fn an_image_smaller_than_one_transfer_still_flashes() {
        let fw = image(300);
        let mut dfu = open(FakeDfu::new());
        flash(&mut dfu, &fw, &FlashOptions::default(), &mut no_progress).unwrap();
        let fake = dfu.into_transport();
        assert_eq!(&fake.mem[..300], &fw[..]);
        // 300 bytes padded to 304 (8-byte alignment) in one write.
        assert!(fake.ops.contains(&Op::Write(0x0800_0000, 304)));
    }

    #[test]
    fn no_run_stays_in_the_bootloader() {
        let opts = FlashOptions {
            run: false,
            ..FlashOptions::default()
        };
        let mut dfu = open(FakeDfu::new());
        flash(&mut dfu, &image(1000), &opts, &mut no_progress).unwrap();
        assert_eq!(dfu.into_transport().left, None);
    }

    #[test]
    fn a_protected_chip_is_refused_before_anything_is_modified() {
        let mut fake = FakeDfu::new();
        fake.protected = true;
        let mut dfu = open(fake);
        let e = flash(&mut dfu, &image(1000), &FlashOptions::default(), &mut no_progress).unwrap_err();
        assert!(
            matches!(
                e,
                Error::ReadoutProtected {
                    operation: "flashing"
                }
            ),
            "{e}"
        );
        let e = erase(&mut dfu, &mut no_progress).unwrap_err();
        assert!(matches!(e, Error::ReadoutProtected { operation: "erase" }), "{e}");
        let fake = dfu.into_transport();
        assert!(
            fake.ops
                .iter()
                .all(|o| !matches!(o, Op::Erase(_) | Op::Write(..)))
        );
        assert_eq!(fake.left, None);
    }

    #[test]
    fn info_reports_the_layout_and_the_protection_state() {
        let mut dfu = open(FakeDfu::new());
        let i = info(&mut dfu, &mut no_progress).unwrap();
        assert!(!i.readout_protected);
        assert_eq!(i.layout.size(), 192 * 1024);
        assert_eq!(i.transfer_size, 2048);
        let mut fake = FakeDfu::new();
        fake.protected = true;
        let mut dfu = open(fake);
        assert!(info(&mut dfu, &mut no_progress).unwrap().readout_protected);
        // Read-only: nothing erased or written, and the board stays in the bootloader.
        let fake = dfu.into_transport();
        assert!(
            fake.ops
                .iter()
                .all(|o| !matches!(o, Op::Erase(_) | Op::Write(..)))
        );
        assert_eq!(fake.left, None);
    }

    #[test]
    fn a_read_failing_otherwise_is_an_error_not_a_verdict() {
        let mut fake = FakeDfu::new();
        fake.upload_status = Some(0x08); // errADDRESS
        let mut dfu = open(fake);
        let e = dfu.readout_protected().unwrap_err();
        assert!(matches!(e, Error::Dfu { status: 0x08, .. }), "{e}");
    }

    #[test]
    fn erase_wipes_the_whole_main_flash_and_stays_in_dfu() {
        let mut fake = FakeDfu::new();
        fake.mem.fill(0x5A);
        let mut dfu = open(fake);
        let mut last = None;
        let pages = erase(&mut dfu, &mut |p| {
            if let Progress::Erase {
                pages_done,
                pages_total,
            } = p
            {
                last = Some((pages_done, pages_total));
            }
        })
        .unwrap();
        assert_eq!(pages, 1536);
        assert_eq!(last, Some((1536, 1536)));
        let fake = dfu.into_transport();
        assert!(fake.mem.iter().all(|&b| b == 0));
        assert_eq!(fake.left, None);
    }

    #[test]
    fn an_image_larger_than_the_layout_is_refused_before_anything_is_sent() {
        let small = Layout::parse("@Internal Flash  /0x08000000/512*128 g").unwrap();
        let mut dfu = Dfu::new(FakeDfu::new(), small, 2048, 0x2200).unwrap();
        let e = flash(
            &mut dfu,
            &image(65 * 1024),
            &FlashOptions::default(),
            &mut no_progress,
        )
        .unwrap_err();
        assert!(matches!(e, Error::FirmwareTooLarge { max: 65536, .. }), "{e}");
        assert!(dfu.into_transport().ops.is_empty());
    }

    #[test]
    fn a_corrupted_write_fails_verification() {
        let mut fake = FakeDfu::new();
        fake.corrupt_write_at = Some(0x0800_0900);
        let mut dfu = open(fake);
        let e = flash(&mut dfu, &image(5000), &FlashOptions::default(), &mut no_progress).unwrap_err();
        assert!(
            matches!(
                e,
                Error::VerifyMismatch {
                    address: 0x0800_0900,
                    ..
                }
            ),
            "{e}"
        );
        // Verification of the other blocks failed before the vector table was written.
        let fake = dfu.into_transport();
        assert!(!fake.ops.iter().any(|o| matches!(o, Op::Write(0x0800_0000, _))));
        assert_eq!(fake.left, None);
    }

    #[test]
    fn writing_a_page_that_was_not_erased_is_reported() {
        let mut fake = FakeDfu::new();
        fake.mem.fill(0x5A);
        let opts = FlashOptions {
            erase: false,
            ..FlashOptions::default()
        };
        let mut dfu = open(fake);
        let e = flash(&mut dfu, &image(3000), &opts, &mut no_progress).unwrap_err();
        assert!(
            matches!(
                e,
                Error::Dfu {
                    context: "writing memory",
                    status: 0x06,
                    ..
                }
            ),
            "{e}"
        );
    }

    #[test]
    fn an_error_left_by_a_previous_session_is_cleared_on_open() {
        let mut fake = FakeDfu::new();
        fake.state = STATE_ERROR;
        fake.status = 0x0F;
        let dfu = open(fake);
        assert_eq!(dfu.into_transport().state, STATE_DFU_IDLE);
    }

    #[test]
    fn erases_and_writes_outside_main_flash_are_refused() {
        let mut dfu = open(FakeDfu::new());
        // The data EEPROM, a page past the end of flash, an unaligned page.
        for addr in [0x0808_0000, 0x0803_0000, 0x0800_0040] {
            assert!(
                matches!(dfu.erase_page(addr), Err(Error::InvalidArgument { .. })),
                "{addr:#x}"
            );
        }
        assert!(dfu.write(0x0808_0000, &[0; 8]).is_err());
        assert!(dfu.write(0x0802_FFF8, &[0; 16]).is_err());
        assert!(dfu.write(0x0800_0000, &[0; 7]).is_err());
        assert!(dfu.into_transport().ops.is_empty());
    }

    #[test]
    fn a_busy_device_is_polled_until_done() {
        let mut fake = FakeDfu::new();
        fake.busy_polls = 3;
        let mut dfu = open(fake);
        dfu.erase_page(0x0800_0000).unwrap();
        assert!(dfu.into_transport().sleeps >= 3);
    }

    /// The command bytes this module can put on the wire: Set Address Pointer and Erase, each
    /// with an address (the `command` debug assertion holds the runtime side). Pins that no mass
    /// erase (`0x41` alone) and no Read Unprotect (`0x92`) can be built here: the source holds
    /// no other DfuSe command constant and no `0x92` outside this test.
    #[test]
    fn only_set_address_and_page_erase_are_ever_sent() {
        let src = include_str!("dfu.rs");
        // `\nmod tests {`, not `#[cfg(test)]\nmod tests`: a Windows checkout has CRLF line ends.
        let code: Vec<&str> = src[..src.find("\nmod tests {").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        assert!(
            !code.iter().any(|l| l.contains("0x92")),
            "Read Unprotect must not be buildable over DFU"
        );
        let consts: Vec<&&str> = code.iter().filter(|l| l.starts_with("pub const CMD_")).collect();
        assert_eq!(consts.len(), 2, "{consts:?}");
        let mut fake = FakeDfu::new();
        fake.mem.fill(0x5A);
        let mut dfu = open(fake);
        erase(&mut dfu, &mut no_progress).unwrap();
        flash(&mut dfu, &image(5000), &FlashOptions::default(), &mut no_progress).unwrap();
        let fake = dfu.into_transport();
        assert!(
            fake.commands
                .iter()
                .all(|c| c.len() == 5 && (c[0] == CMD_SET_ADDRESS || c[0] == CMD_ERASE))
        );
    }
}
