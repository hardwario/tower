//! High-level flash orchestration: connect, erase, write, verify, run.
//!
//! This layer is UI-free: it emits [`Progress`] events through a caller-supplied
//! callback rather than printing, so consumers (a plain CLI, a TUI, a
//! machine-readable frontend) render progress however they like. The `jolt`
//! binary turns these events into progress bars in `src/commands.rs`.

use std::time::Duration;

use crate::bootloader::{Bootloader, Transport};
use crate::error::{Error, NackStage, Result};
use crate::port::Port;
use crate::target;

const ERASE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_ATTEMPTS: u32 = 5;

/// Progress events emitted by the flash/erase engine. A consumer passes a
/// `&mut dyn FnMut(Progress)` to render these; [`no_progress`] is a no-op sink.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Starting bootloader-entry attempt `attempt` of `of`.
    Connecting { attempt: u32, of: u32 },
    /// A bootloader-entry attempt failed (surfaces the per-attempt error that
    /// `--verbose` used to print). The engine retries until `of` is reached.
    ConnectError {
        attempt: u32,
        of: u32,
        error: String,
    },
    /// The chip reported product id `id` (via `Get ID`).
    ChipIdentified { id: u16 },
    /// Erase progress: `pages_done` of `pages_total` pages erased.
    Erase {
        pages_done: usize,
        pages_total: usize,
    },
    /// Write progress: `bytes_done` of `bytes_total` bytes written.
    Write {
        bytes_done: usize,
        bytes_total: usize,
    },
    /// Verify progress: `bytes_done` of `bytes_total` bytes verified.
    Verify {
        bytes_done: usize,
        bytes_total: usize,
    },
    /// Starting the application (reset into app or `Go`).
    Starting,
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
    /// (the pages the firmware spans), not the whole device — a full-chip wipe
    /// is [`erase`]. Writing STM32L0 flash that has *not* been erased corrupts
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
/// nothing — a library consumer could "successfully flash" an empty file
/// without noticing. Also rejects an image larger than the family maximum
/// [`target::MAX_FLASH_SIZE`] (the real per-part flash is smaller and is
/// enforced later by an out-of-range NACK).
fn check_firmware_len(len: usize) -> Result<()> {
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

/// Erase pages `0..count` in chunks of ERASE_CHUNK, reporting progress. Every
/// page must be in range; a NACK is a real error (used for the firmware
/// footprint, which always fits).
pub fn erase_pages<T: Transport>(
    bl: &mut Bootloader<T>,
    count: usize,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let mut page = 0usize;
    progress(Progress::Erase {
        pages_done: 0,
        pages_total: count,
    });
    while page < count {
        let end = (page + target::ERASE_CHUNK).min(count);
        let list: Vec<u16> = (page..end).map(|p| p as u16).collect();
        bl.extended_erase_pages(&list, ERASE_TIMEOUT)?;
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
    let list: Vec<u16> = (start..end).map(|p| p as u16).collect();
    bl.extended_erase_pages(&list, ERASE_TIMEOUT)
}

/// A chunk `[start..end)` was NACKed (page list ran past this part's flash).
/// Bisect it to find the flash boundary — the first page the device rejects as
/// out of range — and erase every valid page below it. Returns the boundary
/// page number (== the number of pages in `[0..)` that are in range up to
/// `end`, i.e. the first out-of-range page).
///
/// AN3155 erases *nothing* for a page list containing any out-of-range page, so
/// a plain "stop at the first NACKed chunk" leaves up to `ERASE_CHUNK - 1` valid
/// pages at the density boundary un-erased on every STM32L0 density except the
/// 192 KiB part whose boundary happens to fall on a chunk edge. Bisecting the
/// failing chunk and re-erasing the valid prefix fixes that.
fn bisect_erase_boundary<T: Transport>(
    bl: &mut Bootloader<T>,
    start: usize,
    end: usize,
) -> Result<usize> {
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
/// succeeded, the page list has run past this part's flash — `bisect_erase_boundary`
/// then finds the exact density boundary and erases the valid pages below it, so
/// no in-range page is left un-erased. Returns the number of pages erased.
///
/// Only a NACK to the page-list *payload* ([`NackStage::Payload`]) is treated as
/// the flash boundary; a NACK to the Extended Erase *command* byte
/// ([`NackStage::Command`]) is line corruption and propagates as an error rather
/// than being mistaken for successful completion.
///
/// A payload NACK is ambiguous, though: AN3155 returns it both for an
/// out-of-range page (the end of flash — the intended stop) *and* for a
/// write-protected (WRP) page (a truncated erase). To tell them apart, the
/// discovered boundary is cross-checked — using the `id` from `Get ID` — against
/// the known STM32L0 densities ([`target::FLASH_DENSITIES_PAGES`]) and the
/// identified chip's flash size. If the stop point isn't a plausible full-flash
/// density, the erase left pages behind and [`Error::PartialErase`] is returned
/// rather than a silent `Ok` reporting fewer pages than were requested.
pub fn erase_chip<T: Transport>(
    bl: &mut Bootloader<T>,
    id: u16,
    progress: &mut dyn FnMut(Progress),
) -> Result<usize> {
    let max_pages = target::pages_in(target::MAX_FLASH_SIZE);
    let mut page = 0usize;
    let mut hit_boundary = false;
    progress(Progress::Erase {
        pages_done: 0,
        pages_total: max_pages,
    });
    while page < max_pages {
        let end = (page + target::ERASE_CHUNK).min(max_pages);
        let list: Vec<u16> = (page..end).map(|p| p as u16).collect();
        match bl.extended_erase_pages(&list, ERASE_TIMEOUT) {
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
    // still un-erased — surface it instead of reporting a silent, partial Ok.
    if hit_boundary {
        let fits_family =
            target::max_flash_size(id).is_none_or(|max| page <= target::pages_in(max));
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
/// Note: unlike [`flash`], the device is left running the application. On an
/// error *after* erase begins the device is left parked in the bootloader (its
/// flash partly wiped) — power-cycle or re-run to recover.
pub fn erase(port: &mut Port, progress: &mut dyn FnMut(Progress)) -> Result<usize> {
    connect(port, progress)?;
    let id = Bootloader::new(port).get_id().inspect_err(|_| {
        // Get ID failed before any erase — the app is intact; put it back.
        let _ = port.reset_into_app();
    })?;
    progress(Progress::ChipIdentified { id });
    let pages = erase_chip(&mut Bootloader::new(port), id, progress)?;
    port.reset_into_app()?;
    Ok(pages)
}

/// Pad `chunk` up to a multiple of `align` with 0xFF (erased-flash value).
fn pad_block(chunk: &[u8], align: usize) -> Vec<u8> {
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
pub fn connect(port: &mut Port, progress: &mut dyn FnMut(Progress)) -> Result<()> {
    connect_with(port, |p| p.reset_into_bootloader(), progress)
}

/// Retry core of [`connect`], generic over the transport and its reset action
/// so the retry count can be tested without a real port.
fn connect_with<T, R>(
    transport: &mut T,
    mut reset: R,
    progress: &mut dyn FnMut(Progress),
) -> Result<()>
where
    T: Transport,
    R: FnMut(&mut T) -> Result<()>,
{
    for attempt in 1..=CONNECT_ATTEMPTS {
        progress(Progress::Connecting {
            attempt,
            of: CONNECT_ATTEMPTS,
        });
        reset(transport)?;
        match Bootloader::new(transport).init() {
            Ok(()) => return Ok(()),
            Err(e) => progress(Progress::ConnectError {
                attempt,
                of: CONNECT_ATTEMPTS,
                error: e.to_string(),
            }),
        }
    }
    Err(Error::BootloaderInit {
        attempts: CONNECT_ATTEMPTS,
    })
}

/// Flash `firmware` to the connected device.
///
/// Full sequence: enter the bootloader ([`connect`]) → `Get ID`
/// ([`Progress::ChipIdentified`]) → optional footprint erase (only the pages the
/// image spans, not the whole device — see [`FlashOptions::erase`]) → write →
/// optional read-back verify → optional start (reset into app, or `Go` when
/// [`FlashOptions::go`] is set).
///
/// The image length is validated up front (`check_firmware_len`): an **empty**
/// image is rejected ([`Error::InvalidArgument`]) — erase/write/verify would all
/// be vacuous, so without this check `flash` would reset the chip and return
/// `Ok(())` having programmed nothing — and an image larger than
/// [`target::MAX_FLASH_SIZE`] is rejected ([`Error::FirmwareTooLarge`]). The
/// latter is the family *maximum*; an image that fits the maximum but exceeds
/// the connected part's real flash still passes this check and instead fails
/// mid-erase/write with an out-of-range page NACK.
///
/// Failure behaviour: on an error *before* the first erase/write (Get ID, or the
/// footprint bounds check) the device is reset back into the application so the
/// intact app keeps running. On any error *at or after* erase begins the device
/// is left parked in the bootloader (flash partly modified) — recover by
/// re-flashing or power-cycling.
pub fn flash(
    port: &mut Port,
    firmware: &[u8],
    opts: &FlashOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    check_firmware_len(firmware.len())?;

    connect(port, progress)?;

    // Get ID happens before any flash modification: on error, restore the app.
    let id = match Bootloader::new(port).get_id() {
        Ok(id) => id,
        Err(e) => {
            let _ = port.reset_into_app();
            return Err(e);
        }
    };
    progress(Progress::ChipIdentified { id });

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
/// reset lines or start the app — [`flash`] wraps that around it.
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

    // Write
    let total = firmware.len();
    progress(Progress::Write {
        bytes_done: 0,
        bytes_total: total,
    });
    let mut addr = target::FLASH_BASE;
    let mut written = 0usize;
    for chunk in firmware.chunks(target::MAX_BLOCK) {
        let buf = pad_block(chunk, target::WRITE_ALIGN);
        bl.write_block(addr, &buf)?;
        addr = addr.wrapping_add(buf.len() as u32);
        written += chunk.len();
        progress(Progress::Write {
            bytes_done: written,
            bytes_total: total,
        });
    }

    // Verify
    if opts.verify {
        progress(Progress::Verify {
            bytes_done: 0,
            bytes_total: total,
        });
        let mut addr = target::FLASH_BASE;
        let mut offset = 0usize;
        while offset < total {
            let len = std::cmp::min(target::MAX_BLOCK, total - offset);
            let data = bl.read_block(addr, len)?;
            for i in 0..len {
                if data[i] != firmware[offset + i] {
                    return Err(Error::VerifyMismatch {
                        address: addr.wrapping_add(i as u32),
                        got: data[i],
                        expected: firmware[offset + i],
                    });
                }
            }
            addr = addr.wrapping_add(len as u32);
            offset += len;
            progress(Progress::Verify {
                bytes_done: offset,
                bytes_total: total,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::tests::ScriptedTransport;

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
            assert_eq!(
                addr % target::WRITE_ALIGN as u32,
                0,
                "address stays aligned"
            );
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
    /// because it runs past page 128 — the end of this test's 16 KiB part, so
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

        // id 0x457 is the Cat-1 (16 KiB) part, whose full density is 128 pages —
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
    /// boundary" — it propagates as an error.
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
        // Boundary at page 200 (25 KiB) — not any STM32L0 density. Models a WRP
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
    /// truncated erase — the family cross-check must catch it.
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

    /// A full-density part (192 KiB, 1536 pages) never NACKs a page-list — the
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
        let err = connect_with(
            &mut t,
            |_| {
                resets += 1;
                Ok(())
            },
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
    }

    /// connect succeeds on the second attempt (first init NACKs, second ACKs).
    #[test]
    fn connect_succeeds_after_one_retry() {
        let mut t = ScriptedTransport::new();
        t.push_nack(); // attempt 1 init -> NACK
        t.push_ack(); // attempt 2 init -> ACK
        let mut resets = 0u32;
        connect_with(
            &mut t,
            |_| {
                resets += 1;
                Ok(())
            },
            &mut no_progress,
        )
        .unwrap();
        assert_eq!(resets, 2);
    }
}
