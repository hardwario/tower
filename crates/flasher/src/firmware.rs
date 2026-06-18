//! Loading the raw firmware image from disk.

use std::path::Path;

use anyhow::{Context, Result, bail};

/// Load a raw `.bin` firmware image. `.hex`/`.elf` are rejected with a hint.
pub fn load(path: &Path) -> Result<Vec<u8>> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if ext == "hex" || ext == "elf" {
            bail!(
                "{} looks like a .{ext} file; jolt flashes raw binaries.\n\
                 Convert it first, e.g.: arm-none-eabi-objcopy -O binary in.{ext} out.bin",
                path.display()
            );
        }
    }
    let data = fs_read(path)?;
    if data.is_empty() {
        bail!("firmware file {} is empty", path.display());
    }
    Ok(data)
}

fn fs_read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading firmware file {}", path.display()))
}
