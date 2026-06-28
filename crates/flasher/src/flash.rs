//! High-level flash orchestration: connect, erase, write, verify, run.

use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

use crate::bootloader::Bootloader;
use crate::error::{Error, Result};
use crate::port::Port;
use crate::target;

const ERASE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_ATTEMPTS: u32 = 5;

pub struct FlashOptions {
    pub erase: bool,
    pub verify: bool,
    pub run: bool,
    pub go: bool,
    pub verbose: bool,
}

/// Number of flash pages spanning `length` bytes of firmware, clamped to
/// `max_pages` (the connected device's page count).
fn pages_for_length(length: usize, max_pages: usize) -> usize {
    length.div_ceil(target::PAGE_SIZE).min(max_pages)
}

fn pages_bar(count: usize) -> ProgressBar {
    let bar = ProgressBar::new(count as u64);
    bar.set_style(
        ProgressStyle::with_template("{msg:>9} [{bar:30.cyan/blue}] {pos:>4}/{len} pages")
            .unwrap()
            .progress_chars("=>-"),
    );
    bar.set_message("Erasing");
    bar
}

/// Erase pages `0..count` in chunks of ERASE_CHUNK, reporting progress. Every
/// page must be in range; a NACK is a real error (used for the firmware
/// footprint, which always fits).
pub fn erase_pages(bl: &mut Bootloader, count: usize) -> Result<()> {
    let bar = pages_bar(count);
    let mut page = 0usize;
    while page < count {
        let end = (page + target::ERASE_CHUNK).min(count);
        let list: Vec<u16> = (page..end).map(|p| p as u16).collect();
        bl.extended_erase_pages(&list, ERASE_TIMEOUT)?;
        page = end;
        bar.set_position(page as u64);
    }
    bar.finish_with_message("Erased");
    Ok(())
}

/// Erase the entire device without knowing its density. The STM32L0 bootloader
/// can't report its flash size (Read Memory of the factory region is rejected)
/// and refuses the 0xFFFF mass-erase code, so erase pages from 0 up to the
/// family maximum in chunks and stop cleanly the moment the bootloader NACKs a
/// page as out of range — i.e. once this part's flash has been fully erased.
/// Returns the number of pages erased.
pub fn erase_chip(bl: &mut Bootloader) -> Result<usize> {
    let max_pages = target::pages_in(target::MAX_FLASH_SIZE);
    let bar = pages_bar(max_pages);
    let mut page = 0usize;
    while page < max_pages {
        let end = (page + target::ERASE_CHUNK).min(max_pages);
        let list: Vec<u16> = (page..end).map(|p| p as u16).collect();
        match bl.extended_erase_pages(&list, ERASE_TIMEOUT) {
            Ok(()) => {
                page = end;
                bar.set_position(page as u64);
            }
            // A NACK after we've erased at least one chunk means the page list
            // ran past the end of this part's flash — we're done.
            Err(Error::Nack { .. }) if page > 0 => break,
            Err(e) => {
                bar.abandon_with_message("Erase FAILED");
                return Err(e);
            }
        }
    }
    bar.set_length(page as u64);
    bar.finish_with_message(format!("Erased {} KiB", page * target::PAGE_SIZE / 1024));
    Ok(page)
}

/// Enter the bootloader, identify the chip, erase the whole device, and reset
/// back into the application. The high-level counterpart to [`flash`] for a
/// standalone erase. Returns the number of pages erased.
pub fn erase(port: &mut Port, verbose: bool) -> Result<usize> {
    println!("Entering bootloader...");
    connect(port, verbose)?;
    let id = Bootloader::new(port).get_id()?;
    println!("Chip id : 0x{id:03X} ({})", target::chip_name(id));
    if !target::is_known(id) {
        eprintln!("warning: 0x{id:03X} is not a recognized STM32L0 family id — continuing anyway");
    }
    let pages = erase_chip(&mut Bootloader::new(port))?;
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
/// full reset sequence between attempts.
pub fn connect(port: &mut Port, verbose: bool) -> Result<()> {
    for attempt in 1..=CONNECT_ATTEMPTS {
        port.reset_into_bootloader()?;
        let mut bl = Bootloader::new(port);
        match bl.init() {
            Ok(()) => return Ok(()),
            Err(e) if verbose => eprintln!("  init attempt {attempt}/{CONNECT_ATTEMPTS}: {e}"),
            Err(_) => {}
        }
    }
    Err(Error::BootloaderInit {
        attempts: CONNECT_ATTEMPTS,
    })
}

fn bytes_bar(len: u64, msg: &'static str) -> ProgressBar {
    let bar = ProgressBar::new(len);
    bar.set_style(
        ProgressStyle::with_template(
            "{msg:>9} [{bar:30.cyan/blue}] {bytes:>8}/{total_bytes:<8} {percent:>3}%",
        )
        .unwrap()
        .progress_chars("=>-"),
    );
    bar.set_message(msg);
    bar
}

pub fn flash(port: &mut Port, firmware: &[u8], opts: &FlashOptions) -> Result<()> {
    let start = Instant::now();

    println!("Entering bootloader...");
    connect(port, opts.verbose)?;

    {
        let mut bl = Bootloader::new(port);

        let id = bl.get_id()?;
        println!("Chip id : 0x{id:03X} ({})", target::chip_name(id));
        if !target::is_known(id) {
            eprintln!(
                "warning: 0x{id:03X} is not a recognized STM32L0 family id — continuing anyway"
            );
        }

        if opts.erase {
            let pages = pages_for_length(firmware.len(), target::pages_in(target::MAX_FLASH_SIZE));
            erase_pages(&mut bl, pages)?;
        }

        // Write
        let bar = bytes_bar(firmware.len() as u64, "Writing");
        let mut addr = target::FLASH_BASE;
        for chunk in firmware.chunks(target::MAX_BLOCK) {
            let buf = pad_block(chunk, target::WRITE_ALIGN);
            bl.write_block(addr, &buf)?;
            addr = addr.wrapping_add(buf.len() as u32);
            bar.inc(chunk.len() as u64);
        }
        bar.finish_with_message("Written");

        // Verify
        if opts.verify {
            let bar = bytes_bar(firmware.len() as u64, "Verifying");
            let mut addr = target::FLASH_BASE;
            let mut offset = 0usize;
            while offset < firmware.len() {
                let len = std::cmp::min(target::MAX_BLOCK, firmware.len() - offset);
                let data = bl.read_block(addr, len)?;
                for i in 0..len {
                    if data[i] != firmware[offset + i] {
                        bar.abandon_with_message("Verify FAILED");
                        return Err(Error::VerifyMismatch {
                            address: addr.wrapping_add(i as u32),
                            got: data[i],
                            expected: firmware[offset + i],
                        });
                    }
                }
                addr = addr.wrapping_add(len as u32);
                offset += len;
                bar.inc(len as u64);
            }
            bar.finish_with_message("Verified");
        }

        if opts.run && opts.go {
            println!(
                "Jumping to application (Go 0x{:08X})...",
                target::FLASH_BASE
            );
            bl.go(target::FLASH_BASE)?;
        }
    }

    if opts.run && !opts.go {
        println!("Resetting into application...");
        port.reset_into_app()?;
    }

    println!(
        "Done: {} bytes in {:.1}s.",
        firmware.len(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
