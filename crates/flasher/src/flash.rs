//! High-level flash orchestration: connect, erase, write, verify, run.
//!
//! This layer is UI-free: it emits [`Progress`] events through a caller-supplied
//! callback rather than printing, so consumers (a plain CLI, a TUI, a
//! machine-readable frontend) render progress however they like. The `tower`
//! CLI turns these events into progress bars (`tower flash` / `tower erase`).

use std::ops::Range;
use std::time::Duration;

use crate::bootloader::{Bootloader, Transport};
use crate::error::{Error, NackStage, Result};
use crate::port::{self, Port};
use crate::target;

const ERASE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_ATTEMPTS: u32 = 5;

/// Progress events emitted by the flash/erase engine. A consumer passes a
/// `&mut dyn FnMut(Progress)` to render these; [`no_progress`] is a no-op sink.
///
/// [`flash`] writes the image's first block (the vector table) last, after the rest is
/// written and verified, so with verify on the phases interleave: `Write` up to the image less
/// that block, `Verify` of the same, then `Write { bytes_total, bytes_total }` for the first
/// block and `Verify` again up to `bytes_total`. Within a phase the counts never go down.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Starting bootloader-entry attempt `attempt` of `of`.
    Connecting { attempt: u32, of: u32 },
    /// A bootloader-entry attempt failed, with its error (a front end may show it,
    /// e.g. under `--verbose`). The engine retries until `of` is reached.
    ConnectError { attempt: u32, of: u32, error: String },
    /// The chip reported product id `id` (via `Get ID`).
    ChipIdentified { id: u16 },
    /// The USB DFU bootloader's flash interface was found ([`crate::dfu`]): the name of its memory
    /// layout and the size of the main flash it describes. The DFU path's counterpart of
    /// [`ChipIdentified`](Self::ChipIdentified), as DfuSe has no `Get ID`.
    DfuLayout { name: String, flash_size: u32 },
    /// Erase progress: `pages_done` of `pages_total` pages erased.
    Erase { pages_done: usize, pages_total: usize },
    /// Write progress: `bytes_done` of `bytes_total` bytes written.
    Write { bytes_done: usize, bytes_total: usize },
    /// Verify progress: `bytes_done` of `bytes_total` bytes verified.
    Verify { bytes_done: usize, bytes_total: usize },
    /// Starting the application (reset into app or `Go`).
    Starting,
    /// The readout-protection probe ran ([`crate::rdp`]): `readout_protected` is whether RDP
    /// level 1 is active. Emitted right after `Get ID` by [`flash`], [`erase`],
    /// [`chip::info`](crate::chip::info), and before and after the change by
    /// [`rdp::protect`](crate::rdp::protect) / [`rdp::unprotect`](crate::rdp::unprotect).
    ReadoutProbed { readout_protected: bool },
    /// The bootloader accepted Readout Protect (`protect: true`) or Readout Unprotect
    /// (`protect: false`) and is updating the option bytes (Unprotect: after mass-erasing the
    /// flash and the data EEPROM); the device resets itself when done. Emitted only by
    /// [`rdp::protect`](crate::rdp::protect) / [`rdp::unprotect`](crate::rdp::unprotect).
    ReadoutChanging { protect: bool },
}

/// A `Progress` sink that discards every event. Handy default for callers that
/// don't want progress output: `flash::flash(&mut port, &fw, &opts, &mut flash::no_progress)`.
pub fn no_progress(_: Progress) {}

/// Options controlling [`flash`]. See per-field docs for the erase/run coupling.
///
/// Construct with [`FlashOptions::default`] and tweak the fields you care about;
/// the struct is `#[non_exhaustive]` so future options don't break callers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FlashOptions {
    /// Erase before writing. The erase covers only the **image footprint**
    /// (the pages the firmware spans), not the whole device (a full-chip wipe
    /// is [`erase`]). Writing STM32L0 flash that has *not* been erased corrupts
    /// the affected 8-byte words, so skip this only when you know the target
    /// pages are already erased.
    pub erase: bool,
    /// Read the image back after writing and compare byte-for-byte, failing with
    /// [`Error::VerifyMismatch`] on the first difference.
    pub verify: bool,
    /// Start the application after a successful write (see `go` for how).
    pub run: bool,
    /// When starting, use the bootloader `Go` command instead of a hardware
    /// reset. Only meaningful when `run` is set (`run: false` leaves the device
    /// parked in the bootloader regardless of `go`).
    pub go: bool,
}

impl Default for FlashOptions {
    fn default() -> Self {
        FlashOptions {
            erase: true,
            verify: true,
            run: true,
            go: false,
        }
    }
}

/// Number of flash pages spanning `length` bytes of firmware, clamped to
/// `max_pages` (the connected device's page count).
fn pages_for_length(length: usize, max_pages: usize) -> usize {
    length.div_ceil(target::PAGE_SIZE).min(max_pages)
}

/// Validate the image length before touching the device.
///
/// Rejects an **empty** image up front: erase/write/verify would each be a
/// no-op, so [`flash`] would reset the chip and return `Ok(())` having flashed
/// nothing, and a library consumer could "successfully flash" an empty file
/// without noticing. Also rejects an image larger than the family maximum
/// [`target::MAX_FLASH_SIZE`] (the real per-part flash may be smaller; that is
/// checked by [`check_fits_chip`] once `Get ID` has identified the part).
pub(crate) fn check_firmware_len(len: usize) -> Result<()> {
    if len == 0 {
        return Err(Error::InvalidArgument {
            context: "firmware image is empty (nothing to flash)",
        });
    }
    if len as u32 > target::MAX_FLASH_SIZE {
        return Err(Error::FirmwareTooLarge {
            size: len,
            max: target::MAX_FLASH_SIZE,
            max_kib: target::MAX_FLASH_SIZE / 1024,
        });
    }
    Ok(())
}

/// Validate the image length against the **identified** chip's flash
/// ([`target::max_flash_size`]), after `Get ID` but before any page is erased.
/// Without this an image that fits the family maximum but not, say, a 16 KiB
/// part would erase the app's pages and only then fail on an out-of-range
/// page NACK, leaving the board bricked until re-flashed. An unrecognized id
/// (no known size or layout) is refused ([`Error::UnknownChip`]).
fn check_fits_chip(len: usize, id: u16) -> Result<()> {
    let Some(max) = target::max_flash_size(id) else {
        return Err(Error::UnknownChip {
            id,
            operation: "flash",
        });
    };
    if len as u64 > u64::from(max) {
        return Err(Error::FirmwareTooLarge {
            size: len,
            max,
            max_kib: max / 1024,
        });
    }
    Ok(())
}

/// The Extended Erase page numbers of `pages`. A page number that does not fit a `u16` becomes
/// `u16::MAX` rather than wrapping onto a low page, so [`Bootloader::extended_erase_pages`]
/// refuses it with the rest of what lies past main flash.
fn page_list(pages: Range<usize>) -> Vec<u16> {
    pages.map(|p| u16::try_from(p).unwrap_or(u16::MAX)).collect()
}

/// Erase pages `0..count` in chunks of ERASE_CHUNK, reporting progress. Every
/// page must be in range; a NACK is a real error (used for the firmware
/// footprint, which always fits). A `count` past main flash
/// (`pages_in(MAX_FLASH_SIZE)`) is refused before anything is erased.
pub fn erase_pages<T: Transport>(
    bl: &mut Bootloader<T>,
    count: usize,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    if count > target::pages_in(target::MAX_FLASH_SIZE) {
        return Err(Error::InvalidArgument {
            context: "erase_pages erases main flash only (never the data EEPROM)",
        });
    }
    let mut page = 0usize;
    progress(Progress::Erase {
        pages_done: 0,
        pages_total: count,
    });
    while page < count {
        let end = (page + target::ERASE_CHUNK).min(count);
        bl.extended_erase_pages(&page_list(page..end), ERASE_TIMEOUT)?;
        page = end;
        progress(Progress::Erase {
            pages_done: page,
            pages_total: count,
        });
    }
    Ok(())
}

/// Erase pages `[start..end)` in a single Extended Erase command.
fn erase_range<T: Transport>(bl: &mut Bootloader<T>, start: usize, end: usize) -> Result<()> {
    debug_assert!(start < end && end - start <= target::ERASE_CHUNK);
    bl.extended_erase_pages(&page_list(start..end), ERASE_TIMEOUT)
}

/// A chunk `[start..end)` was NACKed (page list ran past this part's flash).
/// Bisect it to find the flash boundary (the first page the device rejects as
/// out of range) and erase every valid page below it. Returns the boundary
/// page number (== the number of pages in `[0..)` that are in range up to
/// `end`, i.e. the first out-of-range page).
///
/// AN3155 erases *nothing* for a page list containing any out-of-range page, so
/// a plain "stop at the first NACKed chunk" leaves up to `ERASE_CHUNK - 1` valid
/// pages at the density boundary un-erased on every STM32L0 density except the
/// 192 KiB part whose boundary happens to fall on a chunk edge. Bisecting the
/// failing chunk and re-erasing the valid prefix fixes that.
fn bisect_erase_boundary<T: Transport>(bl: &mut Bootloader<T>, start: usize, end: usize) -> Result<usize> {
    // Invariant: erasing [start..start+good) is known to succeed (good may be
    // 0); erasing [start..start+bad) is known to fail. Narrow until adjacent.
    let mut good = 0usize;
    let mut bad = end - start;
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        match erase_range(bl, start, start + mid) {
            Ok(()) => good = mid,
            Err(Error::Nack {
                stage: NackStage::Payload,
                ..
            }) => bad = mid,
            Err(e) => return Err(e),
        }
    }
    // The last trial above may have ended on a failing range, so re-erase the
    // known-good prefix to guarantee those pages are actually wiped.
    if good > 0 {
        erase_range(bl, start, start + good)?;
    }
    Ok(start + good)
}

/// Erase the entire device without knowing its density. The STM32L0 bootloader
/// can't report its flash size (Read Memory of the factory region is rejected)
/// and refuses the 0xFFFF mass-erase code, so erase pages from 0 up to the
/// family maximum in chunks. When a chunk is NACKed *after* at least one chunk
/// succeeded, the page list has run past this part's flash; `bisect_erase_boundary`
/// then finds the exact density boundary and erases the valid pages below it, so
/// no in-range page is left un-erased. Returns the number of pages erased.
///
/// Only a NACK to the page-list *payload* ([`NackStage::Payload`]) is treated as
/// the flash boundary; a NACK to the Extended Erase *command* byte
/// ([`NackStage::Command`]) is line corruption and propagates as an error rather
/// than being mistaken for successful completion.
///
/// A payload NACK is ambiguous, though: AN3155 returns it both for an
/// out-of-range page (the end of flash, the intended stop) *and* for a
/// write-protected (WRP) page (a truncated erase). To tell them apart, the
/// discovered boundary is cross-checked (using the `id` from `Get ID`) against
/// the known STM32L0 densities ([`target::FLASH_DENSITIES_PAGES`]) and the
/// identified chip's flash size. If the stop point isn't a plausible full-flash
/// density, the erase left pages behind and [`Error::PartialErase`] is returned
/// rather than a silent `Ok` reporting fewer pages than were requested.
///
/// That cross-check needs the identified chip's flash size, so an unrecognized `id` is refused
/// ([`Error::UnknownChip`]) before anything is erased.
pub fn erase_chip<T: Transport>(
    bl: &mut Bootloader<T>,
    id: u16,
    progress: &mut dyn FnMut(Progress),
) -> Result<usize> {
    let Some(chip_max) = target::max_flash_size(id) else {
        return Err(Error::UnknownChip {
            id,
            operation: "erase",
        });
    };
    let max_pages = target::pages_in(target::MAX_FLASH_SIZE);
    let mut page = 0usize;
    let mut hit_boundary = false;
    progress(Progress::Erase {
        pages_done: 0,
        pages_total: max_pages,
    });
    while page < max_pages {
        let end = (page + target::ERASE_CHUNK).min(max_pages);
        match bl.extended_erase_pages(&page_list(page..end), ERASE_TIMEOUT) {
            Ok(()) => {
                page = end;
                progress(Progress::Erase {
                    pages_done: page,
                    pages_total: max_pages,
                });
            }
            // A page-list NACK after we've erased at least one chunk means the
            // list ran past the end of this part's flash. Bisect this chunk to
            // erase every valid page up to the density boundary, then stop.
            Err(Error::Nack {
                stage: NackStage::Payload,
                ..
            }) if page > 0 => {
                page = bisect_erase_boundary(bl, page, end)?;
                hit_boundary = true;
                progress(Progress::Erase {
                    pages_done: page,
                    pages_total: page,
                });
                break;
            }
            Err(e) => return Err(e),
        }
    }

    // If a payload NACK ended the erase before the family maximum, that NACK
    // could equally be a write-protected (WRP) page rather than the end of
    // flash. Cross-check the stop point: it must be a real STM32L0 flash density
    // and fit the identified chip. Anything else is a truncated erase with pages
    // still un-erased; surface it instead of reporting a silent, partial Ok.
    if hit_boundary {
        let fits_family = page <= target::pages_in(chip_max);
        if !target::is_valid_density_pages(page) || !fits_family {
            return Err(Error::PartialErase {
                erased: page,
                kib: page * target::PAGE_SIZE / 1024,
                id,
            });
        }
    }

    Ok(page)
}

/// Enter the bootloader, identify the chip, erase the whole device, and reset
/// back into the application. The high-level counterpart to [`flash`] for a
/// standalone erase. Returns the number of pages erased.
///
/// Note: unlike [`flash`], the device is left running the application. A
/// failure to enter the bootloader or to read the id resets it back into the
/// (intact) application; on an error *after* erase begins the device is left
/// parked in the bootloader (its flash partly wiped); power-cycle or re-run to
/// recover.
///
/// Only **main flash** is erased: the STM32L0's data EEPROM (e.g. 6 KiB on the
/// L083, at 0x0808_0000) is a separate memory and keeps its contents.
///
/// A readout-protected chip (RDP level 1) is detected right after `Get ID` and fails with
/// [`Error::ReadoutProtected`] before any erase is attempted (the bootloader would NACK it);
/// the application is reset back into, as for any failure before the erase. So is an
/// unrecognized product id ([`Error::UnknownChip`]): its flash size is unknown.
pub fn erase(port: &mut Port, progress: &mut dyn FnMut(Progress)) -> Result<usize> {
    connect(port, progress)?;
    let id = Bootloader::new(port).get_id().inspect_err(|_| {
        // Get ID failed before any erase: the app is intact, so put it back.
        let _ = port.reset_into_app();
    })?;
    progress(Progress::ChipIdentified { id });
    if target::max_flash_size(id).is_none() {
        let _ = port.reset_into_app();
        return Err(Error::UnknownChip {
            id,
            operation: "erase",
        });
    }
    refuse_if_protected(port, "erase", progress)?;
    let pages = erase_chip(&mut Bootloader::new(port), id, progress)?;
    port.reset_into_app()?;
    Ok(pages)
}

/// The readout-protection check of [`flash`] and [`erase`], after `Get ID` and before the
/// first erase or write: on a protected chip every one of those would be NACKed on its command
/// byte, which would surface as a generic NACK (or, for a full-chip erase, could be misread).
/// Probe instead ([`crate::rdp`]) and fail with [`Error::ReadoutProtected`]. On that error, and
/// on a failed probe, nothing has been modified, so the application is reset back into.
fn refuse_if_protected(
    port: &mut Port,
    operation: &'static str,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let result = crate::rdp::probe(&mut Bootloader::new(port)).and_then(|readout_protected| {
        progress(Progress::ReadoutProbed { readout_protected });
        if readout_protected {
            Err(Error::ReadoutProtected { operation })
        } else {
            Ok(())
        }
    });
    if result.is_err() {
        let _ = port.reset_into_app();
    }
    result
}

/// Pad `chunk` up to a multiple of `align` with 0xFF. The pad bytes lie past the image and
/// are never read back.
pub(crate) fn pad_block(chunk: &[u8], align: usize) -> Vec<u8> {
    let mut buf = chunk.to_vec();
    while !buf.len().is_multiple_of(align) {
        buf.push(0xFF);
    }
    buf
}

/// Enter the bootloader and complete the 0x7F auto-baud handshake, retrying the
/// full reset sequence between attempts. Per-attempt failures are reported via
/// [`Progress::ConnectError`]; only exhausting all attempts is an error
/// ([`Error::BootloaderInit`]).
///
/// Each retry holds BOOT0 high longer after releasing NRST: [`port::BOOT_RELEASE_HOLD`] (50 ms)
/// doubled per retry, capped at [`port::BOOT_RELEASE_HOLD_MAX`] (50, 100, 200, 400, 400 ms), so
/// a board whose NRST ramps slower than the tuned 50 ms is not failed five times by the same
/// timing.
///
/// On that error the device is reset back into the application (best-effort;
/// the original error is returned either way): every attempt pulsed it into
/// the system bootloader, and if the last one *did* get there but its ACK was
/// lost on the line, the app would otherwise not run again until a power cycle,
/// even after a read-only [`chip::info`](crate::chip::info).
pub fn connect(port: &mut Port, progress: &mut dyn FnMut(Progress)) -> Result<()> {
    connect_with(
        port,
        |p, hold| p.reset_into_bootloader_with_hold(hold),
        |p| {
            let _ = p.reset_into_app();
        },
        progress,
    )
}

/// The BOOT0 hold after NRST release on bootloader-entry attempt `attempt` (1-based):
/// [`port::BOOT_RELEASE_HOLD`] (the tuned 50 ms) on the first, doubled on each retry, capped at
/// [`port::BOOT_RELEASE_HOLD_MAX`]. The 50 ms is an estimate against the RC ramp of NRST; a
/// retry with the same timing fails the same way on a board whose ramp is slower, and holding
/// BOOT0 longer costs nothing once the ROM bootloader runs (BOOT0 is sampled only at reset).
fn boot_release_hold(attempt: u32) -> Duration {
    port::BOOT_RELEASE_HOLD
        .saturating_mul(1 << attempt.saturating_sub(1).min(16))
        .min(port::BOOT_RELEASE_HOLD_MAX)
}

/// Retry core of [`connect`], generic over the transport and its reset /
/// restore actions so the retry count, the per-attempt BOOT0 hold and the give-up restore can
/// be tested without a real port.
fn connect_with<T, R, A>(
    transport: &mut T,
    mut reset: R,
    mut restore: A,
    progress: &mut dyn FnMut(Progress),
) -> Result<()>
where
    T: Transport,
    R: FnMut(&mut T, Duration) -> Result<()>,
    A: FnMut(&mut T),
{
    for attempt in 1..=CONNECT_ATTEMPTS {
        progress(Progress::Connecting {
            attempt,
            of: CONNECT_ATTEMPTS,
        });
        reset(transport, boot_release_hold(attempt))?;
        match Bootloader::new(transport).init() {
            Ok(()) => return Ok(()),
            Err(e) => progress(Progress::ConnectError {
                attempt,
                of: CONNECT_ATTEMPTS,
                error: e.to_string(),
            }),
        }
    }
    // Possibly parked in the bootloader by the last attempt; put the app back.
    restore(transport);
    Err(Error::BootloaderInit {
        attempts: CONNECT_ATTEMPTS,
    })
}

/// Flash `firmware` to the connected device.
///
/// Full sequence: enter the bootloader ([`connect`]) → `Get ID`
/// ([`Progress::ChipIdentified`]) → optional footprint erase (only the pages the
/// image spans, not the whole device; see [`FlashOptions::erase`]) → write every
/// 256-byte block but the first → optional read-back verify of those → write the
/// first block (the vector table) → optional read-back of it → optional start
/// (reset into app, or `Go` when [`FlashOptions::go`] is set).
///
/// The first block goes last so that an interrupted flash never leaves a bootable
/// truncated image: until the very end the vector table is blank (erased), with no
/// valid stack pointer or reset vector, so a board unplugged at 60% does not boot
/// into half an application on its next power-on. See `write_image`.
///
/// The image length is validated up front (`check_firmware_len`): an **empty**
/// image is rejected ([`Error::InvalidArgument`]), since erase/write/verify would all
/// be vacuous (without this check `flash` would reset the chip and return
/// `Ok(())` having programmed nothing), and an image larger than
/// [`target::MAX_FLASH_SIZE`] is rejected ([`Error::FirmwareTooLarge`]). Once
/// `Get ID` has identified the part, the image is checked again against *that*
/// chip's flash ([`target::max_flash_size`]), still before any page is erased; an
/// unrecognized product id is refused there ([`Error::UnknownChip`]).
/// (The image *content* is not inspected here; [`crate::firmware::load`] /
/// [`crate::firmware::validate`] do that.)
///
/// A readout-protected chip (RDP level 1) is detected right after the size check, by the
/// probe in [`crate::rdp`], and fails with [`Error::ReadoutProtected`] before any erase or
/// write is sent (the bootloader would NACK them).
///
/// Failure behaviour: on an error *before* the first erase/write (bootloader
/// entry, Get ID, the per-chip check, or the protection probe) the device is reset back
/// into the application so the intact app keeps running. On any error *at or after*
/// erase begins the device is left parked in the bootloader (flash partly
/// modified); recover by re-flashing or power-cycling.
pub fn flash(
    port: &mut Port,
    firmware: &[u8],
    opts: &FlashOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    check_firmware_len(firmware.len())?;

    connect(port, progress)?;

    // Get ID and the per-chip size check happen before any flash modification:
    // on error, restore the app.
    let id = match Bootloader::new(port).get_id() {
        Ok(id) => id,
        Err(e) => {
            let _ = port.reset_into_app();
            return Err(e);
        }
    };
    progress(Progress::ChipIdentified { id });
    if let Err(e) = check_fits_chip(firmware.len(), id) {
        let _ = port.reset_into_app();
        return Err(e);
    }
    refuse_if_protected(port, "flashing", progress)?;

    {
        let mut bl = Bootloader::new(port);
        write_image(&mut bl, firmware, opts, progress)?;

        if opts.run && opts.go {
            progress(Progress::Starting);
            bl.go(target::FLASH_BASE)?;
        }
    }

    if opts.run && !opts.go {
        progress(Progress::Starting);
        port.reset_into_app()?;
    }

    Ok(())
}

/// The bootloader-protocol core of [`flash`]: optional footprint erase, write,
/// and optional read-back verify. Generic over the transport so it can be
/// exercised against a scripted in-memory device in tests. Does **not** touch
/// reset lines or start the app; [`flash`] wraps that around it.
///
/// Write order: every 256-byte block except the first, then (with verify) the read-back of
/// those, and only then the first block, read back last. The first block holds the vector
/// table (the initial stack pointer and the reset vector at `FLASH_BASE`). Written in address
/// order, it would go first, and an interruption (a cable pulled at 60%, a lost link) would
/// leave a valid vector table in front of a blank tail: the next power-on would boot into half
/// an application. Written last, it is blank (erased by the footprint erase; STM32L0 flash
/// erases to zeros) until everything else is in place and verified, and a blank vector table
/// has no valid reset vector, so a truncated image never runs; the bootloader entry for the
/// next attempt does not depend on it. (With `erase: false` the caller vouches that the pages
/// are blank, see [`FlashOptions::erase`].)
fn write_image<T: Transport>(
    bl: &mut Bootloader<T>,
    firmware: &[u8],
    opts: &FlashOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    if opts.erase {
        let pages = pages_for_length(firmware.len(), target::pages_in(target::MAX_FLASH_SIZE));
        erase_pages(bl, pages, progress)?;
    }

    let total = firmware.len();
    // The vector-table block, written last (above).
    let first = total.min(target::MAX_BLOCK);

    // Write every block but the first.
    progress(Progress::Write {
        bytes_done: 0,
        bytes_total: total,
    });
    let mut written = 0usize;
    for (i, chunk) in firmware.chunks(target::MAX_BLOCK).enumerate().skip(1) {
        let addr = target::FLASH_BASE + (i * target::MAX_BLOCK) as u32;
        bl.write_block(addr, &pad_block(chunk, target::WRITE_ALIGN))?;
        written += chunk.len();
        progress(Progress::Write {
            bytes_done: written,
            bytes_total: total,
        });
    }

    // Verify them before the vector table makes the image bootable.
    let mut verified = 0usize;
    if opts.verify && first < total {
        verify_range(bl, firmware, first..total, &mut verified, progress)?;
    }

    // Commit: the vector table, then its read-back.
    bl.write_block(
        target::FLASH_BASE,
        &pad_block(&firmware[..first], target::WRITE_ALIGN),
    )?;
    progress(Progress::Write {
        bytes_done: total,
        bytes_total: total,
    });
    if opts.verify {
        verify_range(bl, firmware, 0..first, &mut verified, progress)?;
    }
    Ok(())
}

/// Read `firmware[range]` back from flash in blocks and compare it byte for byte, failing with
/// [`Error::VerifyMismatch`] (at the absolute address) on the first difference. Adds each block
/// to `verified` and reports [`Progress::Verify`] of the whole image, starting with the count
/// so far. `range` starts on a block boundary.
fn verify_range<T: Transport>(
    bl: &mut Bootloader<T>,
    firmware: &[u8],
    range: Range<usize>,
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
        let len = target::MAX_BLOCK.min(range.end - offset);
        let addr = target::FLASH_BASE + offset as u32;
        let data = bl.read_block(addr, len)?;
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
    use super::*;
    use crate::bootloader::tests::ScriptedTransport;
    use crate::fake_stm32::{ERASED, FakeStm32, Fault, Op};
    use crate::port::tests::{BOOTLOADER_ENTRY_LINES, device_port, fake_port};
    use crate::rdp::tests::{GET_ID_447, INIT, PROBE_PROTECTED, frames, script};

    const ACK: u8 = 0x79;

    /// The modem-line tail of `Port::reset_into_app`: RESET asserted with BOOT0
    /// low, then released.
    const APP_RESET_TAIL: [(char, bool); 3] = [('R', true), ('D', false), ('R', false)];

    #[test]
    fn pages_for_length_rounds_up_and_clamps() {
        let max_pages = target::pages_in(target::MAX_FLASH_SIZE);
        assert_eq!(pages_for_length(1, max_pages), 1);
        assert_eq!(pages_for_length(target::PAGE_SIZE, max_pages), 1);
        assert_eq!(pages_for_length(target::PAGE_SIZE + 1, max_pages), 2);
        assert_eq!(pages_for_length(3 * target::PAGE_SIZE, max_pages), 3);
        // never exceeds the connected device's page count
        assert_eq!(pages_for_length(usize::MAX, max_pages), max_pages);
        assert_eq!(pages_for_length(usize::MAX, 256), 256);
    }

    #[test]
    fn pad_to_8_bytes() {
        assert_eq!(
            pad_block(&[1, 2, 3], 8),
            vec![1, 2, 3, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        // already aligned -> unchanged
        let aligned: Vec<u8> = (0..16).collect();
        assert_eq!(pad_block(&aligned, 8), aligned);
        // exactly one over -> pads to next multiple
        assert_eq!(pad_block(&[0; 9], 8).len(), 16);
    }

    #[test]
    fn chunking_covers_whole_image_with_aligned_addresses() {
        let fw: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let mut addr = target::FLASH_BASE;
        let mut total = 0usize;
        for chunk in fw.chunks(target::MAX_BLOCK) {
            assert_eq!(addr % target::WRITE_ALIGN as u32, 0, "address stays aligned");
            let buf = pad_block(chunk, target::WRITE_ALIGN);
            assert!(buf.len() <= target::MAX_BLOCK);
            assert_eq!(buf.len() % target::WRITE_ALIGN, 0);
            addr = addr.wrapping_add(buf.len() as u32);
            total += chunk.len();
        }
        assert_eq!(total, fw.len());
    }

    /// erase_chip on a part whose flash ends part-way through a chunk (not on a
    /// chunk boundary): chunk 0 (pages 0..80) ACKs, chunk 1 (80..160) is NACKed
    /// because it runs past page 128 (the end of this test's 16 KiB part), so
    /// pages 128..160 are out of range. The C8 fix must bisect and erase the
    /// valid pages of the boundary chunk instead of dropping them.
    #[test]
    fn erase_chip_bisects_boundary_chunk() {
        // Simulate a part with 128 pages (16 KiB). Pages 0..128 erase; >=128 NACK.
        let boundary = 128usize;
        let mut t = ScriptedTransport::new();
        // Reply to every Extended Erase: the transport decides ACK/NACK based on
        // the highest page in the list it was sent.
        t.set_erase_boundary(boundary);

        // id 0x457 is the Cat-1 (16 KiB) part, whose full density is 128 pages,
        // so this boundary is a valid full-flash stop, not a truncated erase.
        let mut bl = Bootloader::new(&mut t);
        let pages = erase_chip(&mut bl, 0x457, &mut no_progress).unwrap();
        assert_eq!(pages, boundary, "all in-range pages erased, none dropped");

        // The very first bytes on the wire are the Extended Erase command frame.
        assert_eq!(&t.written()[..2], &[0x44, 0xBB]);

        // Every page below the boundary must have been erased at least once.
        let erased = t.erased_pages();
        for p in 0..boundary as u16 {
            assert!(erased.contains(&p), "page {p} was never erased");
        }
        // No out-of-range page was erased.
        assert!(erased.iter().all(|&p| (p as usize) < boundary));
    }

    /// C25: a NACK to the Extended Erase *command* byte (line corruption),
    /// distinct from a page-list NACK, must NOT be read as "reached the flash
    /// boundary"; it propagates as an error.
    #[test]
    fn erase_chip_command_nack_is_an_error_not_a_boundary() {
        let mut t = ScriptedTransport::new();
        // First chunk's command byte ACKs, its page list ACKs (chunk 0 done).
        // Second chunk: NACK the *command* byte.
        t.push_erase_chunk_ack(); // chunk 0 ok
        t.push_command_nack(); // chunk 1 command frame corrupted
        let mut bl = Bootloader::new(&mut t);
        let err = erase_chip(&mut bl, 0x447, &mut no_progress).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Nack {
                    stage: NackStage::Command,
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// A payload NACK also means a write-protected page, not just the end of
    /// flash. If the erase stops on a page count that is not a real STM32L0
    /// density, that is a truncated (partial) erase and must surface as an error
    /// rather than a silent Ok reporting fewer pages than were requested.
    #[test]
    fn erase_chip_partial_erase_on_non_density_boundary() {
        // Boundary at page 200 (25 KiB): not any STM32L0 density. Models a WRP
        // page at 200 on a part whose real flash is larger.
        let mut t = ScriptedTransport::new();
        t.set_erase_boundary(200);
        let mut bl = Bootloader::new(&mut t);
        let err = erase_chip(&mut bl, 0x447, &mut no_progress).unwrap_err();
        match err {
            Error::PartialErase { erased, id, .. } => {
                assert_eq!(erased, 200, "erase stopped at the WRP boundary");
                assert_eq!(id, 0x447);
            }
            other => panic!("expected PartialErase, got {other:?}"),
        }
    }

    /// A boundary that is a valid density but is larger than the identified
    /// chip's flash (e.g. a 64 KiB stop reported for a 16 KiB part) is also a
    /// truncated erase; the family cross-check must catch it.
    #[test]
    fn erase_chip_partial_erase_when_boundary_exceeds_chip_family() {
        // 512 pages == 64 KiB (a valid density), but chip 0x457 is a 16 KiB part.
        let mut t = ScriptedTransport::new();
        t.set_erase_boundary(512);
        let mut bl = Bootloader::new(&mut t);
        let err = erase_chip(&mut bl, 0x457, &mut no_progress).unwrap_err();
        assert!(
            matches!(err, Error::PartialErase { erased: 512, .. }),
            "got {err:?}"
        );
    }

    /// A full-density part (192 KiB, 1536 pages) never NACKs a page-list: the
    /// erase walks to the family maximum and returns it, no PartialErase.
    #[test]
    fn erase_chip_full_density_reaches_maximum() {
        let max_pages = target::pages_in(target::MAX_FLASH_SIZE);
        let mut t = ScriptedTransport::new();
        t.set_erase_boundary(max_pages);
        let mut bl = Bootloader::new(&mut t);
        let pages = erase_chip(&mut bl, 0x447, &mut no_progress).unwrap();
        assert_eq!(pages, max_pages);
    }

    /// check_firmware_len rejects an empty image (nothing to flash) and one
    /// larger than the family maximum, and accepts a normal image.
    #[test]
    fn check_firmware_len_rejects_empty_and_oversize() {
        assert!(matches!(
            check_firmware_len(0),
            Err(Error::InvalidArgument { .. })
        ));
        assert!(matches!(
            check_firmware_len(target::MAX_FLASH_SIZE as usize + 1),
            Err(Error::FirmwareTooLarge { .. })
        ));
        assert!(check_firmware_len(1).is_ok());
        assert!(check_firmware_len(target::MAX_FLASH_SIZE as usize).is_ok());
    }

    /// R20(6): write_image verify reports VerifyMismatch at the correct absolute
    /// address when the read-back differs from the image.
    #[test]
    fn verify_mismatch_reports_absolute_address() {
        let fw = vec![0xAAu8; 8];
        let mut t = ScriptedTransport::new();
        // erase footprint (1 page): command ACK + page-list ACK.
        t.push_erase_chunk_ack();
        // write one block: command ACK, address ACK, data ACK.
        t.push_ack(); // Write Memory command
        t.push_ack(); // Write Memory address
        t.push_ack(); // Write Memory data
        // verify read of 8 bytes: command ACK, address ACK, length ACK, then
        // 8 data bytes that differ at index 3.
        t.push_ack(); // Read Memory command
        t.push_ack(); // Read Memory address
        t.push_ack(); // Read Memory length
        let mut readback = fw.clone();
        readback[3] = 0x55; // mismatch at offset 3
        t.push_bytes(&readback);

        let mut bl = Bootloader::new(&mut t);
        let opts = FlashOptions::default();
        let err = write_image(&mut bl, &fw, &opts, &mut no_progress).unwrap_err();
        match err {
            Error::VerifyMismatch {
                address,
                got,
                expected,
            } => {
                assert_eq!(address, target::FLASH_BASE + 3);
                assert_eq!(got, 0x55);
                assert_eq!(expected, 0xAA);
            }
            other => panic!("expected VerifyMismatch, got {other:?}"),
        }
    }

    /// R20(3): connect retries exactly CONNECT_ATTEMPTS times then fails with
    /// BootloaderInit. Every init NACKs, so no attempt succeeds.
    #[test]
    fn connect_retries_then_gives_up() {
        let mut t = ScriptedTransport::new();
        for _ in 0..CONNECT_ATTEMPTS {
            t.push_nack(); // init read_ack sees NACK each attempt
        }
        let mut resets = 0u32;
        let mut connects = 0u32;
        let mut report = |p: Progress| {
            if matches!(p, Progress::Connecting { .. }) {
                connects += 1;
            }
        };
        let mut restores = 0u32;
        let err = connect_with(
            &mut t,
            |_, _| {
                resets += 1;
                Ok(())
            },
            |_| restores += 1,
            &mut report,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            Error::BootloaderInit {
                attempts
            } if attempts == CONNECT_ATTEMPTS
        ));
        assert_eq!(resets, CONNECT_ATTEMPTS, "reset attempted once per try");
        assert_eq!(connects, CONNECT_ATTEMPTS, "one Connecting event per try");
        assert_eq!(restores, 1, "J1: app restored once after giving up");
    }

    /// connect succeeds on the second attempt (first init NACKs, second ACKs).
    #[test]
    fn connect_succeeds_after_one_retry() {
        let mut t = ScriptedTransport::new();
        t.push_nack(); // attempt 1 init -> NACK
        t.push_ack(); // attempt 2 init -> ACK
        let mut resets = 0u32;
        let mut restores = 0u32;
        connect_with(
            &mut t,
            |_, _| {
                resets += 1;
                Ok(())
            },
            |_| restores += 1,
            &mut no_progress,
        )
        .unwrap();
        assert_eq!(resets, 2);
        assert_eq!(restores, 0, "a successful connect stays in the bootloader");
    }

    /// J2: the per-chip bound: 16/32/64 KiB parts reject an image one byte over
    /// their flash; the 192 KiB part defers to the family bound; an unrecognized id is
    /// refused whatever the size.
    #[test]
    fn check_fits_chip_uses_the_identified_density() {
        for (id, kib) in [(0x457u16, 16usize), (0x425, 32), (0x417, 64), (0x447, 192)] {
            assert!(check_fits_chip(kib * 1024, id).is_ok(), "0x{id:03X} exact fit");
            assert!(
                matches!(
                    check_fits_chip(kib * 1024 + 1, id),
                    Err(Error::FirmwareTooLarge { max, .. }) if max as usize == kib * 1024
                ),
                "0x{id:03X} one byte over"
            );
        }
        for len in [1, 8 * 1024] {
            let err = check_fits_chip(len, 0x123).unwrap_err();
            assert!(
                matches!(
                    err,
                    Error::UnknownChip {
                        id: 0x123,
                        operation: "flash"
                    }
                ),
                "unknown id refused: {err:?}"
            );
            assert!(err.to_string().contains("0x123"), "{err}");
        }
        // The per-chip wording differs from the family-maximum wording.
        let msg = check_fits_chip(20 * 1024, 0x457).unwrap_err().to_string();
        assert!(msg.contains("connected chip"), "{msg}");
    }

    /// J2 end to end: an image that fits the family maximum but not the
    /// identified 16 KiB part (0x457) fails *before* any Extended Erase is sent,
    /// and the device is reset back into its intact application.
    #[test]
    fn flash_rejects_image_too_big_for_the_chip_before_erasing() {
        // init ACK, then Get ID: cmd ACK, len=1, 0x04 0x57, final ACK.
        let (mut port, state) = fake_port(&[ACK, ACK, 0x01, 0x04, 0x57, ACK]);
        let fw = vec![0u8; 20 * 1024];
        let err = flash(&mut port, &fw, &FlashOptions::default(), &mut no_progress).unwrap_err();
        assert!(
            matches!(err, Error::FirmwareTooLarge { max: 16384, .. }),
            "got {err:?}"
        );
        let st = state.lock().unwrap();
        assert!(
            !st.written.windows(2).any(|w| w == [0x44, 0xBB]),
            "no Extended Erase may be sent"
        );
        assert!(
            st.lines.ends_with(&APP_RESET_TAIL),
            "reset into app: {:?}",
            st.lines
        );
    }

    /// J1: when bootloader entry fails (every init reads garbage), `flash` and
    /// `erase` reset the device back into the application instead of leaving it
    /// possibly parked in the system bootloader.
    #[test]
    fn failed_connect_resets_into_the_app() {
        let (mut port, state) = fake_port(&[]);
        let err = flash(&mut port, &[0u8; 8], &FlashOptions::default(), &mut no_progress).unwrap_err();
        assert!(matches!(err, Error::BootloaderInit { .. }), "got {err:?}");
        assert!(state.lock().unwrap().lines.ends_with(&APP_RESET_TAIL));

        let (mut port, state) = fake_port(&[]);
        let err = erase(&mut port, &mut no_progress).unwrap_err();
        assert!(matches!(err, Error::BootloaderInit { .. }), "got {err:?}");
        assert!(state.lock().unwrap().lines.ends_with(&APP_RESET_TAIL));
    }

    /// `flash` on a readout-protected chip fails fast with `ReadoutProtected` (not the generic
    /// command NACK the first erase would get): nothing is erased or written, and the device is
    /// reset back into its (intact) application.
    #[test]
    fn flash_on_a_protected_chip_is_readout_protected_and_touches_nothing() {
        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_PROTECTED]));
        let mut seen = Vec::new();
        let err = flash(&mut port, &[0u8; 8], &FlashOptions::default(), &mut |p| {
            seen.push(p)
        })
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::ReadoutProtected {
                    operation: "flashing"
                }
            ),
            "got {err:?}"
        );
        assert!(err.to_string().contains("readout-protected"), "{err}");
        let st = state.lock().unwrap();
        assert_eq!(frames(&st.written, [0x44, 0xBB]), 0, "no Extended Erase");
        assert_eq!(frames(&st.written, [0x31, 0xCE]), 0, "no Write Memory");
        assert!(st.lines.ends_with(&APP_RESET_TAIL), "reset into app");
        assert!(
            seen.iter().any(|p| matches!(
                p,
                Progress::ReadoutProbed {
                    readout_protected: true
                }
            )),
            "{seen:?}"
        );
    }

    /// `erase` on a readout-protected chip: the same fail-fast, no erase sent.
    #[test]
    fn erase_on_a_protected_chip_is_readout_protected_and_touches_nothing() {
        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_PROTECTED]));
        let err = erase(&mut port, &mut no_progress).unwrap_err();
        assert!(
            matches!(err, Error::ReadoutProtected { operation: "erase" }),
            "got {err:?}"
        );
        let st = state.lock().unwrap();
        assert_eq!(frames(&st.written, [0x44, 0xBB]), 0, "no Extended Erase");
        assert!(st.lines.ends_with(&APP_RESET_TAIL), "reset into app");
    }

    /// A probe that gets no clean answer is an error before any erase (never assumed open),
    /// and the app is restored.
    #[test]
    fn flash_with_a_failed_probe_erases_nothing() {
        // Init, Get ID, then the Read Memory command reads 0x00 (past the script).
        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447]));
        let err = flash(&mut port, &[0u8; 8], &FlashOptions::default(), &mut no_progress).unwrap_err();
        assert!(matches!(err, Error::UnexpectedByte { .. }), "got {err:?}");
        let st = state.lock().unwrap();
        assert_eq!(frames(&st.written, [0x44, 0xBB]), 0, "no Extended Erase");
        assert!(st.lines.ends_with(&APP_RESET_TAIL));
    }

    // ---- end to end against the simulated chip (`FakeStm32`) ----------------------------

    /// A deterministic image whose bytes are mostly not the erased value.
    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 256 + 1) as u8).collect()
    }

    /// The Write / Verify / Starting events of a run, as `(kind, done, total)`.
    fn data_events(seen: &[Progress]) -> Vec<(char, usize, usize)> {
        seen.iter()
            .filter_map(|p| match *p {
                Progress::Write {
                    bytes_done,
                    bytes_total,
                } => Some(('W', bytes_done, bytes_total)),
                Progress::Verify {
                    bytes_done,
                    bytes_total,
                } => Some(('V', bytes_done, bytes_total)),
                Progress::Starting => Some(('S', 0, 0)),
                _ => None,
            })
            .collect()
    }

    /// What `flash` sends an open, blank chip for a `len`-byte image with the default options:
    /// bootloader entry, `Get ID`, the protection probe, the footprint erase in 80-page chunks,
    /// every block but the first written and then read back, and the first block (the vector
    /// table) written and read back last.
    fn expected_flash_ops(len: usize) -> Vec<Op> {
        let base = target::FLASH_BASE;
        let mut ops = vec![Op::Init, Op::GetId, Op::Read { addr: base, len: 4 }];
        let pages = len.div_ceil(target::PAGE_SIZE);
        for start in (0..pages).step_by(target::ERASE_CHUNK) {
            let end = pages.min(start + target::ERASE_CHUNK);
            ops.push(Op::Erase((start..end).map(|p| p as u16).collect()));
        }
        let block = |i: usize| {
            let offset = i * target::MAX_BLOCK;
            (base + offset as u32, target::MAX_BLOCK.min(len - offset))
        };
        let blocks = len.div_ceil(target::MAX_BLOCK);
        for i in 1..blocks {
            let (addr, n) = block(i);
            ops.push(Op::Write {
                addr,
                len: n.next_multiple_of(target::WRITE_ALIGN),
            });
        }
        for i in 1..blocks {
            let (addr, len) = block(i);
            ops.push(Op::Read { addr, len });
        }
        let (addr, n) = block(0);
        ops.push(Op::Write {
            addr,
            len: n.next_multiple_of(target::WRITE_ALIGN),
        });
        ops.push(Op::Read { addr, len: n });
        ops
    }

    /// A 601-byte image end to end, every Write Memory and Read Memory pinned by address and
    /// length (the last block padded to 8 bytes on write, read back unpadded): the blocks at
    /// 0x100 and 0x200 are written and verified first, the vector-table block at the flash base
    /// last. The progress stays per-phase monotonic, and the chip ends up holding the image and
    /// running it.
    #[test]
    fn flash_small_image_writes_the_vector_table_block_last() {
        let fw = image(601);
        let (mut port, state) = device_port(FakeStm32::new(0x447, 192));
        let mut seen = Vec::new();
        flash(&mut port, &fw, &FlashOptions::default(), &mut |p| seen.push(p)).unwrap();

        let st = state.lock().unwrap();
        let chip = st.device.as_ref().unwrap();
        assert_eq!(
            chip.ops,
            [
                Op::Init,
                Op::GetId,
                Op::Read {
                    addr: 0x0800_0000,
                    len: 4
                },
                Op::Erase(vec![0, 1, 2, 3, 4]),
                Op::Write {
                    addr: 0x0800_0100,
                    len: 256
                },
                Op::Write {
                    addr: 0x0800_0200,
                    len: 96
                },
                Op::Read {
                    addr: 0x0800_0100,
                    len: 256
                },
                Op::Read {
                    addr: 0x0800_0200,
                    len: 89
                },
                Op::Write {
                    addr: 0x0800_0000,
                    len: 256
                },
                Op::Read {
                    addr: 0x0800_0000,
                    len: 256
                },
            ]
        );
        assert_eq!(chip.ops, expected_flash_ops(fw.len()));
        assert_eq!(&chip.flash[..601], &fw[..]);
        assert_eq!(&chip.flash[601..608], &[0xFF; 7], "write padding");
        assert!(chip.flash[608..].iter().all(|&b| b == ERASED));
        assert!(chip.running_app());
        let mut lines = BOOTLOADER_ENTRY_LINES.to_vec();
        lines.extend(APP_RESET_TAIL);
        assert_eq!(st.lines, lines, "one bootloader entry, one reset into the app");
        assert_eq!(
            data_events(&seen),
            [
                ('W', 0, 601),
                ('W', 256, 601),
                ('W', 345, 601),
                ('V', 0, 601),
                ('V', 256, 601),
                ('V', 345, 601),
                ('W', 601, 601),
                ('V', 345, 601),
                ('V', 601, 601),
                ('S', 0, 0),
            ]
        );
    }

    /// A full 192 KiB image (768 blocks): the whole flash erased in 20 chunks, blocks 1..768
    /// written and verified in address order, then block 0; the chip holds the exact image.
    #[test]
    fn flash_full_192k_image_writes_the_vector_table_block_last() {
        let fw = image(target::MAX_FLASH_SIZE as usize);
        let (mut port, state) = device_port(FakeStm32::new(0x447, 192));
        flash(&mut port, &fw, &FlashOptions::default(), &mut no_progress).unwrap();

        let st = state.lock().unwrap();
        let chip = st.device.as_ref().unwrap();
        let expected = expected_flash_ops(fw.len());
        assert_eq!(
            expected.iter().filter(|op| matches!(op, Op::Erase(_))).count(),
            20
        );
        assert_eq!(
            expected
                .iter()
                .filter(|op| matches!(op, Op::Write { len: 256, .. }))
                .count(),
            768
        );
        assert_eq!(chip.ops, expected);
        let writes: Vec<u32> = chip
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Write { addr, .. } => Some(*addr),
                _ => None,
            })
            .collect();
        assert_eq!(writes.last(), Some(&target::FLASH_BASE), "block 0 written last");
        assert_eq!(
            writes.iter().filter(|&&a| a == target::FLASH_BASE).count(),
            1,
            "and only then"
        );
        assert!(chip.flash == fw, "the chip holds the image");
        assert!(chip.running_app());
    }

    /// A NACK or a timeout in the middle of the write, or of the verify, fails `flash` with that
    /// error and ends the session right there: no further byte reaches the chip, no `Go`, no
    /// reset into the app (it stays parked in the bootloader), no `Starting`. The vector-table
    /// block is still blank, so the half-written image can never boot.
    #[test]
    fn flash_failure_mid_write_or_mid_verify_stops_and_leaves_the_vector_table_blank() {
        let fw = image(8 * target::MAX_BLOCK);
        let block_3 = target::FLASH_BASE + 3 * target::MAX_BLOCK as u32;
        // Write Memory's fault point is its data frame; the body writes start at block 1, so
        // occurrence 2 is block 3. Read Memory's is its length; occurrence 0 is the protection
        // probe, so occurrence 3 is the read-back of block 3.
        for (opcode, occurrence, fault) in [
            (0x31, 2, Fault::Nack),
            (0x31, 2, Fault::Silent),
            (0x11, 3, Fault::Nack),
            (0x11, 3, Fault::Silent),
        ] {
            let what = format!("opcode 0x{opcode:02X} {fault:?}");
            let chip = FakeStm32::new(0x447, 192).with_fault(opcode, occurrence, fault);
            let (mut port, state) = device_port(chip);
            let mut seen = Vec::new();
            let err = flash(&mut port, &fw, &FlashOptions::default(), &mut |p| seen.push(p)).unwrap_err();
            match fault {
                Fault::Nack => assert!(
                    matches!(
                        err,
                        Error::Nack {
                            stage: NackStage::Payload,
                            ..
                        }
                    ),
                    "{what}: {err:?}"
                ),
                _ => assert!(matches!(err, Error::Timeout { .. }), "{what}: {err:?}"),
            }

            let st = state.lock().unwrap();
            let chip = st.device.as_ref().unwrap();
            assert_eq!(
                chip.bytes_after_fault, 0,
                "{what}: nothing sent after the failure"
            );
            let failed = if opcode == 0x31 {
                Op::Write {
                    addr: block_3,
                    len: 256,
                }
            } else {
                Op::Read {
                    addr: block_3,
                    len: 256,
                }
            };
            assert_eq!(chip.ops.last(), Some(&failed), "{what}");
            assert_eq!(frames(&st.written, [0x21, 0xDE]), 0, "{what}: no Go");
            assert_eq!(st.lines, BOOTLOADER_ENTRY_LINES, "{what}: no reset into the app");
            assert!(!chip.running_app(), "{what}: parked in the bootloader");
            assert!(
                chip.flash[..target::MAX_BLOCK].iter().all(|&b| b == ERASED),
                "{what}: the vector table is still blank"
            );
            assert!(
                !seen.iter().any(|p| matches!(p, Progress::Starting)),
                "{what}: {seen:?}"
            );
        }
    }

    /// `go`: the application is started with the bootloader's Go command (`21 DE` and the
    /// flash-base address frame), the last bytes on the wire, and without an NRST pulse: the
    /// lines never move after the bootloader entry.
    #[test]
    fn flash_with_go_sends_go_and_no_reset_pulse() {
        let fw = image(601);
        let (mut port, state) = device_port(FakeStm32::new(0x447, 192));
        let opts = FlashOptions {
            go: true,
            ..FlashOptions::default()
        };
        let mut seen = Vec::new();
        flash(&mut port, &fw, &opts, &mut |p| seen.push(p)).unwrap();

        let st = state.lock().unwrap();
        assert!(
            st.written.ends_with(&[0x21, 0xDE, 0x08, 0x00, 0x00, 0x00, 0x08]),
            "Go to 0x0800_0000 last on the wire"
        );
        assert_eq!(frames(&st.written, [0x21, 0xDE]), 1);
        assert_eq!(st.lines, BOOTLOADER_ENTRY_LINES, "no NRST pulse after the entry");
        let chip = st.device.as_ref().unwrap();
        assert_eq!(chip.ops.last(), Some(&Op::Go(target::FLASH_BASE)));
        assert!(chip.running_app());
        assert!(matches!(seen.last(), Some(Progress::Starting)), "{seen:?}");
    }

    /// An unrecognized product id: `flash` and `erase` refuse it right after `Get ID`, naming
    /// the id, before the probe or any erase or write, and put the application back.
    #[test]
    fn flash_and_erase_refuse_an_unknown_chip_before_touching_it() {
        let (mut port, state) = device_port(FakeStm32::new(0x499, 192));
        let err = flash(&mut port, &image(601), &FlashOptions::default(), &mut no_progress).unwrap_err();
        assert!(
            matches!(
                err,
                Error::UnknownChip {
                    id: 0x499,
                    operation: "flash"
                }
            ),
            "got {err:?}"
        );
        assert!(err.to_string().contains("0x499"), "{err}");
        let st = state.lock().unwrap();
        assert_eq!(st.device.as_ref().unwrap().ops, [Op::Init, Op::GetId]);
        assert!(st.lines.ends_with(&APP_RESET_TAIL), "reset into app");
        assert!(st.device.as_ref().unwrap().running_app());

        let (mut port, state) = device_port(FakeStm32::new(0x499, 192));
        let err = erase(&mut port, &mut no_progress).unwrap_err();
        assert!(
            matches!(
                err,
                Error::UnknownChip {
                    id: 0x499,
                    operation: "erase"
                }
            ),
            "got {err:?}"
        );
        let st = state.lock().unwrap();
        assert_eq!(st.device.as_ref().unwrap().ops, [Op::Init, Op::GetId]);
        assert!(st.lines.ends_with(&APP_RESET_TAIL), "reset into app");
    }

    /// The lower-level `erase_chip` refuses an unknown id too, with nothing sent.
    #[test]
    fn erase_chip_refuses_an_unknown_chip() {
        let mut chip = FakeStm32::new(0x499, 192).in_session();
        let err = erase_chip(&mut Bootloader::new(&mut chip), 0x499, &mut no_progress).unwrap_err();
        assert!(matches!(err, Error::UnknownChip { id: 0x499, .. }), "got {err:?}");
        assert!(chip.ops.is_empty(), "{:?}", chip.ops);
    }

    /// Product id 0x447 covers the 64, 128 and 192 KiB Cat-5 parts: the full-chip erase finds
    /// each density (bisecting the boundary chunk where it falls mid-chunk), wipes every page
    /// below it and reports the page count, with no PartialErase.
    #[test]
    fn erase_chip_stops_at_the_density_of_each_cat5_part() {
        for kib in [64, 128, 192] {
            let mut chip = FakeStm32::new(0x447, kib).in_session();
            chip.flash.fill(0xA5);
            let pages = erase_chip(&mut Bootloader::new(&mut chip), 0x447, &mut no_progress).unwrap();
            assert_eq!(pages, kib * 1024 / target::PAGE_SIZE, "{kib} KiB");
            assert!(
                chip.flash.iter().all(|&b| b == ERASED),
                "{kib} KiB: every page erased"
            );
        }
    }

    /// `erase_pages` refuses a page count past main flash up front (nothing sent), including one
    /// whose page numbers would wrap a `u16` back onto low pages; `page_list` never wraps.
    #[test]
    fn erase_pages_refuses_a_count_past_main_flash() {
        let max_pages = target::pages_in(target::MAX_FLASH_SIZE);
        for count in [max_pages + 1, 4096 + 1, 70_000] {
            let mut t = ScriptedTransport::new();
            let err = erase_pages(&mut Bootloader::new(&mut t), count, &mut no_progress).unwrap_err();
            assert!(matches!(err, Error::InvalidArgument { .. }), "{count}: {err:?}");
            assert!(t.written().is_empty(), "{count}: nothing on the wire");
        }
        assert_eq!(page_list(65_535..65_538), [u16::MAX, u16::MAX, u16::MAX]);
        // The whole of main flash is fine.
        let mut chip = FakeStm32::new(0x447, 192).in_session();
        erase_pages(&mut Bootloader::new(&mut chip), max_pages, &mut no_progress).unwrap();
    }

    /// Each bootloader-entry retry holds BOOT0 longer: 50, 100, 200, 400, 400 ms, starting from
    /// the tuned `BOOT_RELEASE_HOLD` and capped at `BOOT_RELEASE_HOLD_MAX`; `connect` hands that
    /// hold to its reset on each attempt.
    #[test]
    fn connect_lengthens_the_boot0_hold_on_each_retry() {
        let ms = Duration::from_millis;
        let holds: Vec<Duration> = (1..=CONNECT_ATTEMPTS).map(boot_release_hold).collect();
        assert_eq!(holds, [ms(50), ms(100), ms(200), ms(400), ms(400)]);
        assert_eq!(boot_release_hold(1), port::BOOT_RELEASE_HOLD);
        assert_eq!(boot_release_hold(u32::MAX), port::BOOT_RELEASE_HOLD_MAX);

        let mut t = ScriptedTransport::new();
        for _ in 0..CONNECT_ATTEMPTS {
            t.push_nack();
        }
        let mut used = Vec::new();
        connect_with(
            &mut t,
            |_, hold| {
                used.push(hold);
                Ok(())
            },
            |_| {},
            &mut no_progress,
        )
        .unwrap_err();
        assert_eq!(used, holds);
    }
}
