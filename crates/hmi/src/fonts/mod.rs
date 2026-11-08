//! The bundled fonts: [Spleen](https://github.com/fcambus/spleen) by Frederic Cambus
//! (BSD-2-Clause, see `crates/hmi/LICENSE-spleen`), a monospaced bitmap font in six sizes.
//!
//! | Font | Cell | On 128 x 128: columns x lines | Preset blocks | Preset flash |
//! |------|------|-------------------------------|---------------|--------------|
//! | [`SPLEEN_5X8`] | 5 x 8 | 25 x 16 | ASCII | 760 B |
//! | [`SPLEEN_6X12`] | 6 x 12 | 21 x 10 | ASCII, Latin-1 | 2.2 KiB |
//! | [`SPLEEN_8X16`] | 8 x 16 | 16 x 8 | ASCII, Latin-1 | 3.0 KiB |
//! | [`SPLEEN_12X24`] | 12 x 24 | 10 x 5 | ASCII, Latin-1 | 9.0 KiB |
//! | [`SPLEEN_16X32`] | 16 x 32 | 8 x 4 | ASCII, Latin-1 | 11.9 KiB |
//! | [`SPLEEN_32X64`] | 32 x 64 | 4 x 2 | ASCII, Latin-1 | 47.8 KiB |
//! | [`SPLEEN_16X32_NUMERIC`] | 16 x 32 | 8 x 4 | numeric | 1.2 KiB |
//! | [`SPLEEN_32X64_NUMERIC`] | 32 x 64 | 4 x 2 | numeric | 4.8 KiB |
//!
//! Latin-1 holds `°`, `µ`, `±` and the Western European letters; upstream Spleen has no Latin-1
//! in 5x8 (its code points are empty there), so that size is ASCII only. Each size module
//! (`spleen_8x16`, ...) also has the blocks one by one: `ASCII`, `LATIN1`, `LATIN_EXT_A` (Central
//! European letters: Czech, Slovak, Polish, ...; 8x16 and up), `SYMBOLS` (the arrows, block
//! elements for bar graphs, shapes and so on that the size has) and, for 16x32 and 32x64,
//! `NUMERIC` (space, digits, `%+,-./:` and `°`: a big readout for a tenth of the flash). Combine
//! blocks with the size module's `with`; the linker keeps only the blocks a used font lists. A
//! character no block has draws as `?`.
//!
//! The glyph data is generated from the upstream release by `crates/hmi/tools/spleen.py`.

mod spleen;

pub use spleen::*;
