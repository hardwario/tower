//! Loading and sanity-checking the raw firmware image from disk.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::error::{Error, Result};
use crate::target;

/// Why an in-memory image is not a flashable raw STM32L0 binary; returned by [`validate`].
///
/// [`load`] maps these onto [`Error`]: [`ImageError::Elf`] / [`ImageError::IntelHex`] become
/// [`Error::FirmwareFormat`] (with the conversion hint); the vector-table checks become an
/// [`Error::Io`] of kind [`io::ErrorKind::InvalidData`] whose message names the file. (Kept a
/// separate, `#[non_exhaustive]` type so new checks can be added without a breaking change to
/// [`Error`], which is exhaustive.)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImageError {
    /// Starts with the ELF magic (`\x7fELF`), e.g. a stripped or extensionless ELF.
    #[error("it is an ELF file (\\x7fELF magic), not a raw binary")]
    Elf,
    /// Starts with an Intel HEX record (`:LLAAAATT…`).
    #[error("it is an Intel HEX file (':' records), not a raw binary")]
    IntelHex,
    /// Shorter than the two vector-table words every Cortex-M image starts with.
    #[error(
        "it is {len} byte(s), too short for a Cortex-M vector table (initial SP + reset vector, 8 bytes)"
    )]
    TooShort { len: usize },
    /// Word 0 (the initial stack pointer) does not point into STM32L0 SRAM.
    #[error(
        "its initial stack pointer 0x{sp:08X} (word 0) is outside STM32L0 SRAM \
         (0x{:08X}, 0x{:08X}]; not a Cortex-M image for this part",
        target::SRAM_BASE,
        target::SRAM_BASE + target::MAX_SRAM_SIZE
    )]
    BadStackPointer { sp: u32 },
    /// Word 1 (the reset vector) is not a Thumb address inside the image itself.
    #[error(
        "its reset vector 0x{pc:08X} (word 1) is not a Thumb (odd) address inside the image \
         [0x{:08X}, 0x{end:08X}); not a Cortex-M image linked for 0x{:08X}",
        target::FLASH_BASE,
        target::FLASH_BASE
    )]
    BadResetVector { pc: u32, end: u32 },
}

/// Check that `data` looks like a raw Cortex-M image for an STM32L0, written at
/// [`target::FLASH_BASE`]: what the boot ROM will execute after reset.
///
/// Rejects an ELF or Intel HEX file by **content** (whatever its extension), then sanity-checks
/// the vector table: word 0 (initial SP) must point into SRAM, i.e. `(SRAM_BASE, SRAM_BASE +
/// MAX_SRAM_SIZE]` (the family's largest 20 KiB, since the chip isn't identified yet), and word 1
/// (reset vector) must be a Thumb (odd) address inside the image's own flash range. Without this
/// any blob would be erased-and-written and the read-back verify would happily pass, leaving a
/// board that hard-faults on boot.
///
/// [`load`] calls this; it is public so a caller holding image bytes from elsewhere can apply the
/// same check before [`flash`](crate::flash::flash).
pub fn validate(data: &[u8]) -> std::result::Result<(), ImageError> {
    if data.starts_with(b"\x7fELF") {
        return Err(ImageError::Elf);
    }
    // A raw image can never start with ':' (0x3A as the low byte of the initial SP is not
    // word-aligned), so a ':' + hex-digit record header is unambiguous.
    if data.len() >= 11 && data[0] == b':' && data[1..11].iter().all(u8::is_ascii_hexdigit) {
        return Err(ImageError::IntelHex);
    }
    if data.len() < 8 {
        return Err(ImageError::TooShort { len: data.len() });
    }
    let word = |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
    let (sp, pc) = (word(0), word(4));
    // A full-descending stack starts at its top, so SRAM_BASE itself (an empty stack) is out
    // and the very top of SRAM is in.
    if sp <= target::SRAM_BASE || sp > target::SRAM_BASE + target::MAX_SRAM_SIZE {
        return Err(ImageError::BadStackPointer { sp });
    }
    let end = target::FLASH_BASE.saturating_add(u32::try_from(data.len()).unwrap_or(u32::MAX));
    let entry = pc & !1;
    if pc & 1 == 0 || entry < target::FLASH_BASE || entry >= end {
        return Err(ImageError::BadResetVector { pc, end });
    }
    Ok(())
}

/// Load a raw `.bin` firmware image and [`validate`] it.
///
/// Returns [`crate::error::Error`] like every other library entry point:
/// [`Error::FirmwareFormat`] for a `.hex`/`.elf` extension *or* ELF / Intel HEX content (with an
/// objcopy hint), [`Error::FirmwareEmpty`] for a zero-length file, [`Error::FirmwareTooLarge`] for
/// a file larger than [`target::MAX_FLASH_SIZE`], and [`Error::Io`] for a read failure, a path
/// that isn't a regular file (a device such as `/dev/zero`, a FIFO, a directory), or an image whose
/// vector table fails [`validate`] (kind [`io::ErrorKind::InvalidData`]). The path is preserved in
/// every message.
///
/// The read is **bounded** to `MAX_FLASH_SIZE + 1` bytes, so an oversized file is rejected without
/// being slurped into memory.
pub fn load(path: &Path) -> Result<Vec<u8>> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if ext == "hex" || ext == "elf" {
            return Err(Error::FirmwareFormat {
                path: path.to_path_buf(),
                ext,
            });
        }
    }
    let data = read_bounded(path)?;
    if data.is_empty() {
        return Err(Error::FirmwareEmpty {
            path: path.to_path_buf(),
        });
    }
    validate(&data).map_err(|e| match e {
        ImageError::Elf => Error::FirmwareFormat {
            path: path.to_path_buf(),
            ext: "elf".to_string(),
        },
        ImageError::IntelHex => Error::FirmwareFormat {
            path: path.to_path_buf(),
            ext: "hex".to_string(),
        },
        other => Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a flashable raw STM32L0 image: {other}", path.display()),
        )),
    })?;
    Ok(data)
}

/// Read at most `MAX_FLASH_SIZE + 1` bytes of a **regular** file.
///
/// The regular-file check comes first: `/dev/zero` would otherwise be read until memory runs out,
/// and opening a FIFO blocks until a writer appears. `metadata` follows symlinks, so a symlink to
/// a real file is fine.
fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    // Preserve the path in the surfaced message.
    let io_err = |e: io::Error| {
        Error::Io(io::Error::new(
            e.kind(),
            format!("reading firmware file {}: {e}", path.display()),
        ))
    };
    let meta = std::fs::metadata(path).map_err(io_err)?;
    if !meta.is_file() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "firmware file {} is not a regular file (a device, FIFO, or directory?)",
                path.display()
            ),
        )));
    }
    let limit = u64::from(target::MAX_FLASH_SIZE) + 1;
    let mut data = Vec::with_capacity(meta.len().min(limit) as usize);
    File::open(path)
        .and_then(|f| f.take(limit).read_to_end(&mut data))
        .map_err(io_err)?;
    if data.len() as u64 > u64::from(target::MAX_FLASH_SIZE) {
        // Only MAX + 1 bytes were read; the real size is at least that.
        return Err(Error::FirmwareTooLarge {
            size: meta.len().max(data.len() as u64) as usize,
            max: target::MAX_FLASH_SIZE,
            max_kib: target::MAX_FLASH_SIZE / 1024,
        });
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `bytes` to a uniquely-named temp file with `name` and return its path.
    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        // Make the name unique per-process/per-test to avoid collisions.
        let unique = format!("tower-flasher-test-{}-{}", std::process::id(), name);
        dir.push(unique);
        let mut f = std::fs::File::create(&dir).unwrap();
        f.write_all(bytes).unwrap();
        dir
    }

    /// A minimal valid image of `len` (> 9) bytes: SP at the top of the L083's 20 KiB SRAM, reset
    /// vector = Thumb address of byte 8 (inside the image), filler after.
    fn image(len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        v.extend_from_slice(&(target::SRAM_BASE + target::MAX_SRAM_SIZE).to_le_bytes());
        v.extend_from_slice(&(target::FLASH_BASE + 8 + 1).to_le_bytes());
        v.resize(len, 0xA5);
        v
    }

    #[test]
    fn hex_rejected_case_insensitively_with_hint() {
        for name in ["x.hex", "X.HEX"] {
            let p = temp_file(name, b"\x00\x01\x02");
            let err = load(&p).unwrap_err();
            let msg = err.to_string();
            match err {
                Error::FirmwareFormat { ext, .. } => assert_eq!(ext, "hex"),
                other => panic!("expected FirmwareFormat, got {other:?}"),
            }
            // The Display carries the objcopy conversion hint.
            assert!(msg.contains("objcopy"), "hint missing from: {msg}");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn elf_rejected() {
        let p = temp_file("x.elf", b"\x7fELF");
        assert!(matches!(load(&p).unwrap_err(), Error::FirmwareFormat { .. }));
        let _ = std::fs::remove_file(&p);
    }

    /// J3: an ELF is caught by its magic even without an `.elf` extension (a stripped
    /// `cargo build` artefact passed straight to `flash`).
    #[test]
    fn extensionless_elf_rejected_by_content() {
        let mut bytes = b"\x7fELF\x01\x01\x01".to_vec();
        bytes.extend(image(64)); // a plausible tail must not rescue it
        for name in ["firmware", "firmware.bin"] {
            let p = temp_file(&format!("elfcontent-{name}"), &bytes);
            let err = load(&p).unwrap_err();
            assert!(err.to_string().contains("objcopy"), "hint missing: {err}");
            match err {
                Error::FirmwareFormat { ext, .. } => assert_eq!(ext, "elf"),
                other => panic!("expected FirmwareFormat, got {other:?}"),
            }
            let _ = std::fs::remove_file(&p);
        }
    }

    /// J3: Intel HEX content in a `.bin` is caught by its record header.
    #[test]
    fn intel_hex_content_rejected() {
        let p = temp_file(
            "hexcontent.bin",
            b":020000040800F2\n:10000000985000200101000800000000000000002E\n",
        );
        match load(&p).unwrap_err() {
            Error::FirmwareFormat { ext, .. } => assert_eq!(ext, "hex"),
            other => panic!("expected FirmwareFormat, got {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_bin_rejected() {
        let p = temp_file("empty.bin", b"");
        assert!(matches!(load(&p).unwrap_err(), Error::FirmwareEmpty { .. }));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_path_error_mentions_the_path() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tower-flasher-test-{}-does-not-exist.bin",
            std::process::id()
        ));
        let err = load(&dir).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains(&dir.display().to_string()));
    }

    #[test]
    fn small_bin_loads_byte_exact() {
        let img = image(12);
        let p = temp_file("twelve.bin", &img);
        assert_eq!(load(&p).unwrap(), img);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn extensionless_file_loads() {
        let img = image(16);
        let p = temp_file("noext", &img);
        assert_eq!(load(&p).unwrap(), img);
        let _ = std::fs::remove_file(&p);
    }

    /// J3: a blob without a Cortex-M vector table is refused (InvalidData, path named) instead of
    /// being flashed and "verified".
    #[test]
    fn non_image_blob_rejected_with_path() {
        let p = temp_file("zeros.bin", &[0u8; 64]);
        let err = load(&p).unwrap_err();
        let msg = err.to_string();
        match &err {
            Error::Io(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidData),
            other => panic!("expected Io(InvalidData), got {other:?}"),
        }
        assert!(msg.contains(&p.display().to_string()), "path missing: {msg}");
        assert!(msg.contains("stack pointer"), "reason missing: {msg}");
        let _ = std::fs::remove_file(&p);
    }

    /// J3: the read is bounded: a file one byte over the family maximum is FirmwareTooLarge.
    #[test]
    fn oversize_file_rejected() {
        let p = temp_file("big.bin", &image(target::MAX_FLASH_SIZE as usize + 1));
        assert!(matches!(
            load(&p).unwrap_err(),
            Error::FirmwareTooLarge { max, .. } if max == target::MAX_FLASH_SIZE
        ));
        let _ = std::fs::remove_file(&p);
        // Exactly the maximum still loads.
        let p = temp_file("max.bin", &image(target::MAX_FLASH_SIZE as usize));
        assert_eq!(load(&p).unwrap().len(), target::MAX_FLASH_SIZE as usize);
        let _ = std::fs::remove_file(&p);
    }

    /// J3: a directory (and, on Unix, an endless character device) is rejected before any read.
    #[test]
    fn non_regular_files_rejected() {
        let err = load(&std::env::temp_dir()).unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == io::ErrorKind::InvalidInput),
            "got {err:?}"
        );
        #[cfg(unix)]
        {
            let err = load(Path::new("/dev/zero")).unwrap_err();
            assert!(err.to_string().contains("not a regular file"), "got {err}");
        }
    }

    #[test]
    fn validate_vector_table_bounds() {
        assert_eq!(validate(&image(12)), Ok(()));
        assert_eq!(validate(&[0u8; 7]), Err(ImageError::TooShort { len: 7 }));

        // SP: SRAM_BASE itself and anything past the 20 KiB top are out; the top is in.
        let top = target::SRAM_BASE + target::MAX_SRAM_SIZE;
        for (sp, ok) in [
            (target::SRAM_BASE, false),
            (target::SRAM_BASE + 4, true),
            (top, true),
            (top + 4, false),
            (0x0800_0000, false),
        ] {
            let mut img = image(32);
            img[..4].copy_from_slice(&sp.to_le_bytes());
            assert_eq!(validate(&img).is_ok(), ok, "sp 0x{sp:08X}");
        }

        // Reset vector: must be odd (Thumb) and inside [FLASH_BASE, FLASH_BASE + len).
        let len = 32u32;
        for (pc, ok) in [
            (target::FLASH_BASE + 9, true),
            (target::FLASH_BASE + 8, false),         // even: not Thumb
            (target::FLASH_BASE + len - 1, true),    // last byte (odd) still inside
            (target::FLASH_BASE + len + 1, false),   // past the image
            (target::FLASH_BASE - 0x100 + 1, false), // below flash
        ] {
            let mut img = image(len as usize);
            img[4..8].copy_from_slice(&pc.to_le_bytes());
            assert_eq!(validate(&img).is_ok(), ok, "pc 0x{pc:08X}");
        }
    }

    /// A real TOWER image header (SP from flip-link's bottom-of-RAM stack, reset handler early
    /// in flash) passes.
    #[test]
    fn validate_accepts_a_real_tower_header() {
        let mut img = vec![0x98, 0x23, 0x00, 0x20, 0xC1, 0x00, 0x00, 0x08];
        img.resize(1024, 0);
        assert_eq!(validate(&img), Ok(()));
    }
}
