//! Loading the raw firmware image from disk.

use std::path::Path;

use crate::error::{Error, Result};

/// Load a raw `.bin` firmware image. `.hex`/`.elf` are rejected with a hint.
///
/// Returns [`crate::error::Error`] like every other library entry point:
/// [`Error::FirmwareFormat`] for a `.hex`/`.elf` extension (with an objcopy
/// hint), [`Error::FirmwareEmpty`] for a zero-length file, and [`Error::Io`] for
/// a read failure (the path is preserved in the message).
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
    let data = fs_read(path)?;
    if data.is_empty() {
        return Err(Error::FirmwareEmpty {
            path: path.to_path_buf(),
        });
    }
    Ok(data)
}

fn fs_read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        // Preserve the path in the surfaced message.
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("reading firmware file {}: {e}", path.display()),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `bytes` to a uniquely-named temp file with `name` and return its path.
    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        // Make the name unique per-process/per-test to avoid collisions.
        let unique = format!("jolt-test-{}-{}", std::process::id(), name);
        dir.push(unique);
        let mut f = std::fs::File::create(&dir).unwrap();
        f.write_all(bytes).unwrap();
        dir
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
        assert!(matches!(
            load(&p).unwrap_err(),
            Error::FirmwareFormat { .. }
        ));
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
            "jolt-test-{}-does-not-exist.bin",
            std::process::id()
        ));
        let err = load(&dir).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains(&dir.display().to_string()));
    }

    #[test]
    fn small_bin_loads_byte_exact() {
        let p = temp_file("three.bin", &[0xDE, 0xAD, 0xBE]);
        assert_eq!(load(&p).unwrap(), vec![0xDE, 0xAD, 0xBE]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn extensionless_file_loads() {
        let p = temp_file("noext", &[0x01, 0x02]);
        assert_eq!(load(&p).unwrap(), vec![0x01, 0x02]);
        let _ = std::fs::remove_file(&p);
    }
}
