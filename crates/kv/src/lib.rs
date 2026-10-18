//! tower-kv (`crates/kv`): a hardware-free, `no_std` power-loss-safe key-value codec over a
//! byte-addressable EEPROM, the storage engine that backs `tower::storage::Kv` in the firmware
//! SDK. It is not the EEPROM driver: the SDK's `tower::storage::Storage` implements this
//! crate's [`Eeprom`] trait over the STM32L0 data EEPROM.
//!
//! It lives in its own crate for one reason: the firmware is `no_std` and targets thumbv6m, so
//! it cannot `cargo test`. The codec, by contrast, is pure logic over a tiny [`Eeprom`] byte-store
//! trait. The firmware implements it over the L0 data EEPROM (`tower::storage::Storage`); the
//! tests here implement it over a RAM array, so the layout, the compaction commit, the
//! power-loss edges, and the first-boot format can all be exercised on the host.
//!
//! # Why double-buffered
//!
//! The catastrophic failure for this SDK is losing (or lowering) the radio TX-counter watermark:
//! the counter feeds the AES-CCM nonce, so a reset/rollback of it reuses a nonce. A single-region
//! append-log cannot survive a power-loss mid-write without risking exactly that (a torn in-place
//! update or a torn in-place compaction can orphan or resurface a record). So the region is split
//! into **two halves** plus two **superblocks** (`magic ‖ generation ‖ crc`):
//!
//! ```text
//!   [ half 0 records ][ half 1 records ][ super0 ][ super1 ]
//!    0              H  H             2H   top-24    top-12
//! ```
//!
//! - The **active** half is the one whose superblock is valid and has the highest generation.
//! - **Updates are append-only** within the active half: a torn write only ever corrupts the
//!   *tail* record, so `scan` stops there and every committed record before it survives. That
//!   holds because no write ever touches a word that holds committed bytes (next section).
//! - **Compaction is a flip:** blank the inactive half, write the live set packed into it, then
//!   write *its* superblock at `generation + 1`. That single CRC'd superblock write is the atomic
//!   commit: torn before it, the old half is still active (no data lost); torn during it, the
//!   superblock fails its CRC and the old half still wins.
//!
//! A record inside a half is `[tag(2) | len(2) | crc(4) | value | pad]`, little-endian, CRC over
//! `tag‖len‖value`: `len` is the true value length, and `pad` is 0 to 3 zero bytes (outside the
//! CRC) that round the record up to whole 32-bit words, so it occupies `8 + round_up(len, 4)`
//! bytes. A region that holds no valid superblock (a blank part, or bytes in any other layout,
//! including the unpadded `TKV1` layout before this one) is formatted empty by [`init`]; there
//! is no migration of other layouts.
//!
//! **Deletion** rides the same append-only log: [`delete`] writes a **tombstone** (a zero-length
//! record, `len == 0`), so the key reads back absent ([`get_bytes`]), and the next flip drops any
//! key whose latest record is a tombstone, reclaiming both the value and the tombstone. `len == 0`
//! is therefore reserved as the tombstone marker (no caller stores a genuinely empty value).
//!
//! **Factory reset** ([`wipe`]) is a flip to an *empty* live set (blank the dead half, commit
//! its superblock, then scrub the old half), so it is as power-loss-safe as any flip and keeps
//! the generation (the wear gauge) counting.
//!
//! # Whole words only
//!
//! The STM32L0 data EEPROM programs, and keeps its ECC, per aligned 32-bit word: a byte or
//! half-word program is an erase plus a program of the **whole** word that holds it, so a power
//! cut during one can leave that whole word erased or undefined, including the bytes *before*
//! the written offset. The firmware's backend (embassy-stm32 `eeprom_write_slice`) programs an
//! unaligned head and tail of a write as single-byte programs. So the codec issues **only
//! whole-word writes at word-aligned offsets**: records are padded to whole words, the halves
//! and the superblocks start on word boundaries (`half_len` is rounded down to whole words),
//! and the log-end terminator (`append_end`) is a whole zero word. A cut then damages only the
//! word being programmed, and that word never holds a byte of a committed record or of the
//! active superblock. The unpadded layout broke this: an append's first byte programs rewrote
//! the word holding the previous record's last bytes, so a cut there failed that record's CRC
//! and its key read an older value (a TX-counter watermark rollback, i.e. CCM nonce reuse) or
//! vanished; and every boundary word was erased and reprogrammed up to four times per pass
//! through its half instead of once (the wear the firmware's `FLIP_BUDGET` rests on).
//!
//! The order of an append's writes matters as much, because a torn word may hold anything, and
//! a cut between two programs leaves the next word as it was: `append_end` first clears the log
//! end if it holds an unreadable record (so nothing later written into its remains can revive
//! it), then zeroes the word past the new record, then writes the record's body, and its header
//! word (`tag | len`) **last**. Until that word lands the scan stops where it did, so a cut
//! anywhere in an append leaves the state before it, whatever the cells under the new record
//! held (even an old record with the same tag and length at the same offset). The host tests
//! cut the power inside every single word program of every path, with the word left as it
//! was, erased or garbled, and check that every key reads its value from before or after the
//! interrupted operation.
//!
//! # Incremental maintenance
//!
//! Because every pre-commit flip write is invisible until the superblock lands, the *same* flip
//! can also run in bounded slices from a background task: [`flip_start`] (RAM plan) →
//! [`flip_step`] (a few word-programs at a time) → [`flip_commit`] (tail catch-up + the atomic
//! superblock write), with [`blank_dead_step`] pre-blanking the dead half between flips and
//! [`maintain`] as the policy driver. All state is RAM-only; a reboot mid-flip restarts from
//! scratch with nothing lost. The synchronous [`compact`]/in-line flip remains as the fallback,
//! semantics unchanged. Motivation: on the STM32L0 each EEPROM word program stalls the CPU
//! ~3.4 ms, so a synchronous full-half flip freezes the chip for seconds (firmware/docs/storage.md).

#![cfg_attr(not(test), no_std)]

/// Largest value (bytes) one record can hold.
pub const MAX_VALUE: usize = 256;
/// Maximum number of distinct keys [`compact`] can track in one pass.
pub const MAX_KEYS: usize = 64;
/// Record header: tag(2) + value-len(2) + crc32(4). Value bytes follow, padded with zeros to
/// whole words (see [`record_size`]).
pub const KV_HEADER: usize = 8;

/// The program (and ECC) granularity of the STM32L0 data EEPROM: one aligned 32-bit word. Every
/// codec write is a whole number of these at a multiple of it (module docs, "Whole words only").
const WORD: u32 = 4;

/// Superblock magic: with the CRC, what tells an initialized double-buffered region from a blank
/// part or bytes in any other layout (formatted empty by [`init`]). `TKV2` is the word-padded
/// record layout; a `TKV1` store (unpadded records) is another layout, formatted empty.
const SUPER_MAGIC: [u8; 4] = *b"TKV2";
/// Superblock size: magic(4) + generation(4 LE) + crc(4 LE, over magic‖generation).
const SUPER_LEN: u32 = 12;

// Everything the codec writes is whole words: the header, every padded record (MAX_VALUE is
// whole words, so a maximal record needs no padding and fits `KV_HEADER + MAX_VALUE` buffers),
// the superblock, and the halves of the firmware's 6 KiB region.
const _: () = assert!((KV_HEADER as u32).is_multiple_of(WORD));
const _: () = assert!((MAX_VALUE as u32).is_multiple_of(WORD));
const _: () = assert!(SUPER_LEN.is_multiple_of(WORD));
const _: () = assert!(half_len(6144) == 3060 && half_len(6144).is_multiple_of(WORD));

/// Bytes a record with a `len`-byte value occupies in a half: the header plus the value rounded
/// up to whole words. The pad (0 to 3 zero bytes) is outside the CRC; `len` stays the true length.
#[must_use]
pub const fn record_size(len: usize) -> u32 {
    KV_HEADER as u32 + (len as u32).next_multiple_of(WORD)
}

/// A byte-addressable store the codec reads and writes. The firmware implements it over the
/// STM32L0 data EEPROM; host tests implement it over a RAM array. All offsets are bounded by the
/// `capacity` passed to the codec functions (the codec never addresses past it).
pub trait Eeprom {
    /// Backend error (e.g. the HAL flash error in the firmware; `()` in tests).
    type Error;
    /// Read `buf.len()` bytes starting at `off`.
    fn read(&self, off: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
    /// Write `data` starting at `off`.
    fn write(&mut self, off: u32, data: &[u8]) -> Result<(), Self::Error>;
}

/// A key-value codec error. `Backend` wraps the [`Eeprom`] error so the firmware can preserve its
/// HAL flash error; the rest are codec-level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvError<E> {
    /// The underlying EEPROM read/write failed (e.g. out of bounds).
    Backend(E),
    /// A value exceeded [`MAX_VALUE`] bytes.
    ValueTooLarge,
    /// The store (one half) is full even after a compaction flip.
    Full,
    /// Key `0` is reserved (it marks the end of the record log).
    InvalidKey,
    /// More distinct keys than [`MAX_KEYS`] (only hit during compaction).
    TooManyKeys,
    /// A [`FlipState`] no longer matches the store (another flip committed underneath it).
    /// Returned by [`flip_step`]/[`flip_commit`] so a stale RAM plan can never be committed;
    /// the caller drops the state and starts over (see [`maintain`], which does exactly that).
    Stale,
}

impl<E: core::fmt::Display> core::fmt::Display for KvError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KvError::Backend(e) => write!(f, "backend error: {e}"),
            KvError::ValueTooLarge => f.write_str("value exceeds MAX_VALUE"),
            KvError::Full => f.write_str("store full"),
            KvError::InvalidKey => f.write_str("invalid key (0 is reserved)"),
            KvError::TooManyKeys => f.write_str("too many distinct keys"),
            KvError::Stale => f.write_str("stale flip state (store changed underneath)"),
        }
    }
}

/// CRC-32 (IEEE 802.3) over the record header bytes (`tag ‖ len`) followed by the value; the
/// `crc` field itself is excluded. Byte-for-byte the firmware's framing, via the shared primitive.
#[must_use]
pub fn entry_crc(hdr4: &[u8], value: &[u8]) -> u32 {
    let crc = tower_protocol::crc::crc32_update(0xFFFF_FFFF, hdr4);
    !tower_protocol::crc::crc32_update(crc, value)
}

// --- layout -------------------------------------------------------------------------------------

/// The whole-word part of the region: an unaligned `capacity` leaves its top 1 to 3 bytes unused,
/// so the superblocks below its top stay word-aligned (the firmware's 6144 is whole words).
const fn region(capacity: u32) -> u32 {
    capacity / WORD * WORD
}

/// Record-area size of one half, given the total region `capacity`. The two superblocks sit in the
/// top `2 * SUPER_LEN` bytes; the rest splits evenly into the two halves, each rounded down to
/// whole words so half 1 (and every record in it) starts on a word boundary.
const fn half_len(capacity: u32) -> u32 {
    region(capacity).saturating_sub(2 * SUPER_LEN) / 2 / WORD * WORD
}

/// Record-area base offset of half `h` (0 or 1).
fn half_base(capacity: u32, h: u8) -> u32 {
    h as u32 * half_len(capacity)
}

/// Offset of half `h`'s superblock (top of the region).
fn super_off(capacity: u32, h: u8) -> u32 {
    region(capacity) - 2 * SUPER_LEN + h as u32 * SUPER_LEN
}

/// Read half `h`'s superblock generation, or `None` if it is absent / corrupt (bad magic or CRC).
fn read_gen<S: Eeprom>(store: &S, capacity: u32, h: u8) -> Result<Option<u32>, KvError<S::Error>> {
    let mut b = [0u8; SUPER_LEN as usize];
    store
        .read(super_off(capacity, h), &mut b)
        .map_err(KvError::Backend)?;
    if b[0..4] != SUPER_MAGIC {
        return Ok(None);
    }
    let g = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let crc = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    if super_crc(&b[0..8]) != crc {
        return Ok(None);
    }
    Ok(Some(g))
}

/// Write half `h`'s superblock with `generation`: the atomic commit point of a flip. The CRC
/// makes even a torn superblock write fail closed (it won't validate, so the other half wins).
fn write_gen<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    h: u8,
    generation: u32,
) -> Result<(), KvError<S::Error>> {
    let mut b = [0u8; SUPER_LEN as usize];
    b[0..4].copy_from_slice(&SUPER_MAGIC);
    b[4..8].copy_from_slice(&generation.to_le_bytes());
    let crc = super_crc(&b[0..8]);
    b[8..12].copy_from_slice(&crc.to_le_bytes());
    store.write(super_off(capacity, h), &b).map_err(KvError::Backend)
}

fn super_crc(b: &[u8]) -> u32 {
    !tower_protocol::crc::crc32_update(0xFFFF_FFFF, b)
}

/// The active half `(h, generation)`: the valid superblock with the highest generation, or `None`
/// if neither half is initialized yet (a fresh, not yet formatted region).
fn active<S: Eeprom>(store: &S, capacity: u32) -> Result<Option<(u8, u32)>, KvError<S::Error>> {
    let g0 = read_gen(store, capacity, 0)?;
    let g1 = read_gen(store, capacity, 1)?;
    Ok(match (g0, g1) {
        (Some(a), Some(b)) => Some(if a >= b { (0, a) } else { (1, b) }),
        (Some(a), None) => Some((0, a)),
        (None, Some(b)) => Some((1, b)),
        (None, None) => None,
    })
}

// --- record scan within a half ------------------------------------------------------------------

/// Result of a [`scan_half`]: `(free_rel, latest_record_for_target)`, offsets **relative** to the
/// half's record base, where a record is `(rel_offset, value_len)`.
type HalfScan = (u32, Option<(u32, u16)>);

/// Scan one half's record area `[base, base + area)`: returns `(free_rel, latest(rel,len))`. Stops
/// at the first blank (`tag == 0`) or corrupt record: the end of a clean log, and where a torn
/// (tail) append would sit. Records advance by their padded [`record_size`], so every offset it
/// visits (and the `free_rel` it returns) is word-aligned.
fn scan_half<S: Eeprom>(store: &S, base: u32, area: u32, target: u16) -> Result<HalfScan, KvError<S::Error>> {
    let mut o = 0u32;
    let mut found = None;
    let mut val = [0u8; MAX_VALUE];
    loop {
        if o + KV_HEADER as u32 > area {
            break;
        }
        let mut hdr = [0u8; KV_HEADER];
        store.read(base + o, &mut hdr).map_err(KvError::Backend)?;
        let tag = u16::from_le_bytes([hdr[0], hdr[1]]);
        if tag == 0 {
            break;
        }
        let len = u16::from_le_bytes([hdr[2], hdr[3]]);
        let crc = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if len as usize > MAX_VALUE || o + record_size(len as usize) > area {
            break;
        }
        store
            .read(base + o + KV_HEADER as u32, &mut val[..len as usize])
            .map_err(KvError::Backend)?;
        if entry_crc(&hdr[0..4], &val[..len as usize]) != crc {
            break;
        }
        if tag == target {
            found = Some((o, len));
        }
        o += record_size(len as usize);
    }
    Ok((o, found))
}

/// Write one `[tag|len|crc|value|pad]` record at absolute (word-aligned) offset `at`, whose header
/// word reads zero (see [`append_end`]): the body (`crc | value | pad`, the pad zero) first, then
/// the header word (`tag | len`) LAST, as the record's own commit. Until that one word lands, the
/// scan still stops at `at`, so a cut anywhere in the body leaves no record at all, whatever the
/// cells under it held before. With the header first, a cut right after it left the old CRC and
/// value behind it, and an old record at the same offset with the same tag and length (the old
/// layout's first record, under the first append after a format) read back.
fn append<S: Eeprom>(store: &mut S, at: u32, key: u16, value: &[u8]) -> Result<(), KvError<S::Error>> {
    let len = value.len() as u16;
    let mut buf = [0u8; KV_HEADER + MAX_VALUE];
    buf[0..2].copy_from_slice(&key.to_le_bytes());
    buf[2..4].copy_from_slice(&len.to_le_bytes());
    let crc = entry_crc(&buf[0..4], value);
    buf[4..8].copy_from_slice(&crc.to_le_bytes());
    buf[8..8 + value.len()].copy_from_slice(value);
    let w = WORD as usize;
    store
        .write(at + WORD, &buf[w..record_size(value.len()) as usize])
        .map_err(KvError::Backend)?;
    store.write(at, &buf[..w]).map_err(KvError::Backend)
}

/// Zero the word at absolute offset `at`, read-first: no program (and no wear) when it is zero.
fn clear_word<S: Eeprom>(store: &mut S, at: u32) -> Result<(), KvError<S::Error>> {
    let mut word = [0u8; WORD as usize];
    store.read(at, &mut word).map_err(KvError::Backend)?;
    if word != [0; WORD as usize] {
        store.write(at, &[0; WORD as usize]).map_err(KvError::Backend)?;
    }
    Ok(())
}

/// Append one record at the log end `free` (relative) of the half at `base` with record area
/// `area`, in an order that keeps a power cut at any point on the near side of the append (the
/// state before it), until the header word (written last by [`append`]) commits the record.
///
/// 1. **Clear the log end.** The scan ends the log at the first blank or unreadable record, and
///    appends land exactly there. Normally that is a zero word (the terminator of step 2, or the
///    one [`init`] writes); after a torn write or EEPROM corruption it is an unreadable record.
///    That record's header word is zeroed first (read-first: a 4-byte read and no write in the
///    normal case), so nothing later written into its remains can make it readable again: a
///    torn word can hold any bytes, and one that happened to restore a corrupted record would
///    have revived it and the stale records behind it.
/// 2. **Terminate past the new record.** In a healthy half the log end is followed by blank cells,
///    but not after a torn append or a corrupted mid-log record: there, a new record SHORTER than
///    the bad one leaves the bad one's remaining bytes right behind it, and a same-sized one lands
///    exactly on the intact records that followed it. Either way the next scan walked on past the
///    new record into those leftovers: a superseded value resurrected over the one just written
///    (a TX-counter watermark rollback, i.e. CCM nonce reuse), or bytes from inside a value
///    surfaced as a phantom key. So the tag slot just past the new record is zeroed before the
///    record is written (read-first again). It is zeroed as the **whole word** that holds it
///    (tag + len), never as a 2-byte write: on the STM32L0 a partial program is an erase plus a
///    program of the whole word, so a cut in it could take neighbouring bytes with it (module
///    docs, "Whole words only"); with padded records that word lies wholly past the new record.
///    By induction, every completed append is followed by a scan stop. Where no header fits after
///    the record the scan stops on its own, so there is nothing to zero.
/// 3. **The record**, header word last ([`append`]).
///
/// Every write here lies at or past the log end, in words that hold no committed byte, so a cut
/// in any of them leaves every committed record intact.
fn append_end<S: Eeprom>(
    store: &mut S,
    base: u32,
    area: u32,
    free: u32,
    key: u16,
    value: &[u8],
) -> Result<(), KvError<S::Error>> {
    clear_word(store, base + free)?;
    let next = free + record_size(value.len());
    if next + KV_HEADER as u32 <= area {
        clear_word(store, base + next)?;
    }
    append(store, base + free, key, value)
}

/// Blank `len` bytes from `at` (so a half's record scan terminates at a clean end).
///
/// **Read-first**: only words that are actually nonzero get programmed, so blanking an
/// already-blank span (e.g. a dead half pre-blanked by [`blank_dead_step`]) costs reads only:
/// no wear, and on the STM32L0 no CPU-stalling word programs. This is what shrinks the in-line
/// fallback flip from "blank + copy" to "copy" once maintenance has pre-blanked (firmware/docs/storage.md).
fn blank<S: Eeprom>(store: &mut S, at: u32, len: u32) -> Result<(), KvError<S::Error>> {
    let mut unbounded = u32::MAX;
    blank_step_span(store, at, len, 0, &mut unbounded).map(|_| ())
}

/// Read-first blanking of `[base+from, base+len)`, programming at most `budget` zero-words
/// (already-zero words are skipped with reads only; idempotent, wear-free on re-runs).
/// Returns the new progress offset; `len` means the span is fully blank. The workhorse of
/// [`blank`] (unbounded) and [`blank_dead_step`]/[`flip_step`] (budgeted). Every span it is given
/// (a half, a superblock) is word-aligned and whole words long, so each program is one whole word.
fn blank_step_span<S: Eeprom>(
    store: &mut S,
    base: u32,
    len: u32,
    from: u32,
    budget: &mut u32,
) -> Result<u32, KvError<S::Error>> {
    let zeros = [0u8; 4];
    let mut chunk = [0u8; 64];
    let mut o = from;
    while o < len {
        let n = ((len - o) as usize).min(chunk.len());
        store.read(base + o, &mut chunk[..n]).map_err(KvError::Backend)?;
        let mut w = 0usize;
        while w < n {
            let wn = (n - w).min(4);
            if chunk[w..w + wn].iter().any(|&b| b != 0) {
                if *budget == 0 {
                    return Ok(o + w as u32);
                }
                store
                    .write(base + o + w as u32, &zeros[..wn])
                    .map_err(KvError::Backend)?;
                *budget -= 1;
            }
            w += wn;
        }
        o += n as u32;
    }
    Ok(len)
}

// --- public API ---------------------------------------------------------------------------------

/// Mount the region, formatting it first if needed. Idempotent: an initialized double-buffered
/// region (a valid superblock) mounts as it is, untouched. Anything else, a blank part or bytes
/// in any other layout, is **formatted empty**: nothing in it is adopted, migrated or kept. Call
/// once before first use (the firmware's `Kv::new` does, best-effort).
///
/// The format is two writes: the word holding the tag slot at the head of half 0 is cleared, so
/// its record scan stops at offset 0 (an empty log), then half 0's superblock is committed at
/// generation 1. The rest of the region is left as it is (no seconds of NVM stall on the first
/// boot): whatever sits further in half 0 can never be read as records, because every append
/// first clears the word holding the tag slot just past the record it writes and writes its own
/// header word last (`append_end`), so the scan always stops at the log end, even under a torn
/// append over an old record with the same header, and a flip blanks its destination half before
/// writing it. Power loss before the commit leaves the region unformatted, and the next `init`
/// formats it again; a torn commit fails its CRC the same way (the magic is part of it, so an old
/// layout's superblock under a torn one never validates).
pub fn init<S: Eeprom>(store: &mut S, capacity: u32) -> Result<(), KvError<S::Error>> {
    if active(store, capacity)?.is_some() {
        return Ok(()); // already double-buffered
    }
    if KV_HEADER as u32 <= half_len(capacity) {
        clear_word(store, half_base(capacity, 0))?;
    }
    write_gen(store, capacity, 0, 1)
}

/// Store raw `value` bytes under `key`. Append-only within the active half (a torn write only ever
/// corrupts the tail); when the half is full, a compaction flip frees space and the append retries
/// in the freshly-packed other half.
///
/// An **empty** `value` writes a delete tombstone (the key reads back absent); prefer the named
/// [`delete`], which additionally skips the write when the key is already absent.
pub fn set_bytes<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    key: u16,
    value: &[u8],
) -> Result<(), KvError<S::Error>> {
    set_bytes_with(store, capacity, key, value, &mut None)
}

/// [`set_bytes`], aware of an in-progress incremental flip (`pending`, shared with [`maintain`]).
///
/// Appends keep going to the **source** (active) half while a flip is pending; [`flip_commit`]'s
/// tail catch-up carries them over. If the source fills anyway, the pending flip is **finished
/// synchronously** (bounded: only its remaining steps + tail, not a from-scratch flip) and the
/// append retries in the fresh half; this path never fails where plain [`set_bytes`] would have
/// succeeded. A stale `pending` (a synchronous flip got there first) is simply dropped.
pub fn set_bytes_with<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    key: u16,
    value: &[u8],
    pending: &mut Option<FlipState>,
) -> Result<(), KvError<S::Error>> {
    if key == 0 {
        return Err(KvError::InvalidKey);
    }
    if value.len() > MAX_VALUE {
        return Err(KvError::ValueTooLarge);
    }
    let (h, g) = match active(store, capacity)? {
        Some(x) => x,
        None => {
            init(store, capacity)?;
            (0, 1)
        }
    };
    let half = half_len(capacity);
    let needed = record_size(value.len());

    let base = half_base(capacity, h);
    let (free, _) = scan_half(store, base, half, key)?;
    if free + needed <= half {
        return append_end(store, base, half, free, key, value);
    }

    // Active half full → flip-compact into the other half, then append there. Prefer finishing
    // a pending incremental flip (its blank/copy work is already partly done); otherwise the
    // synchronous in-line flip, exactly as before.
    let finished_pending = match pending.take() {
        Some(mut st) if (st.src_h, st.src_gen) == (h, g) => {
            flip_commit(store, capacity, &mut st)?;
            true
        }
        _ => {
            flip(store, capacity, h, g)?;
            false
        }
    };
    let (mut h2, g2) = active(store, capacity)?.ok_or(KvError::Full)?;
    let (mut free2, _) = scan_half(store, half_base(capacity, h2), half, key)?;
    if free2 + needed > half && finished_pending {
        // A finished incremental flip carries the source tail over VERBATIM (every record appended
        // while it ran, superseded ones included; see `flip_commit`), so it can pack looser than a
        // fresh flip. Failing `Full` here would refuse a write plain `set_bytes` accepts (the
        // promise above; found while building the `kv_store` fuzz target), so re-flip the fresh half once:
        // that packs exactly the live set. Rare (only a near-full live set with a superseded tail).
        flip(store, capacity, h2, g2)?;
        h2 = active(store, capacity)?.ok_or(KvError::Full)?.0;
        free2 = scan_half(store, half_base(capacity, h2), half, key)?.0;
    }
    if free2 + needed > half {
        return Err(KvError::Full);
    }
    append_end(store, half_base(capacity, h2), half, free2, key, value)
}

/// Read the raw bytes stored under `key` into `out`; returns the value's true length (which may
/// exceed `out.len()`; only `out.len()` bytes are copied), or `None` if the key is absent.
pub fn get_bytes<S: Eeprom>(
    store: &S,
    capacity: u32,
    key: u16,
    out: &mut [u8],
) -> Result<Option<usize>, KvError<S::Error>> {
    if key == 0 {
        return Ok(None);
    }
    let Some((h, _)) = active(store, capacity)? else {
        return Ok(None); // un-initialized region → empty
    };
    let base = half_base(capacity, h);
    match scan_half(store, base, half_len(capacity), key)?.1 {
        // A latest record of length 0 is a delete **tombstone** (see [`delete`]): the key was
        // removed, so it reads back absent exactly like one never set. The tombstone bytes stay
        // on disk until the next flip drops them.
        Some((_, 0)) | None => Ok(None),
        Some((off, len)) => {
            let n = (len as usize).min(out.len());
            store
                .read(base + off + KV_HEADER as u32, &mut out[..n])
                .map_err(KvError::Backend)?;
            Ok(Some(len as usize))
        }
    }
}

/// Delete `key`: append a **tombstone** (a zero-length record) so the key reads back absent.
/// Append-only and power-loss-safe like any [`set_bytes`]: a torn tombstone write only corrupts
/// the tail. The key's old value and the tombstone are both reclaimed by the next compaction
/// `flip` (which drops any key whose latest record is a tombstone); until then they occupy
/// space. A no-op (reads only, no write, no wear) if the key is already absent or tombstoned, so
/// deleting a missing key never grows the log.
pub fn delete<S: Eeprom>(store: &mut S, capacity: u32, key: u16) -> Result<(), KvError<S::Error>> {
    delete_with(store, capacity, key, &mut None)
}

/// [`delete`], aware of an in-progress incremental flip (`pending`, shared with [`maintain`]):
/// the tombstone append follows the same source-half / finish-pending-flip rules as
/// [`set_bytes_with`].
pub fn delete_with<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    key: u16,
    pending: &mut Option<FlipState>,
) -> Result<(), KvError<S::Error>> {
    if key == 0 {
        return Err(KvError::InvalidKey);
    }
    // Skip the write when there is nothing to delete: an un-initialized region, a key never set,
    // or one whose latest record is already a tombstone. Keeps a redundant delete free of wear.
    match active(store, capacity)? {
        None => return Ok(()),
        Some((h, _)) => {
            let base = half_base(capacity, h);
            match scan_half(store, base, half_len(capacity), key)?.1 {
                Some((_, len)) if len > 0 => {} // a live value: fall through to tombstone it
                _ => return Ok(()),             // absent or already tombstoned
            }
        }
    }
    // A zero-length value IS the tombstone; reuse the append path (incl. flip-on-full). If the
    // half is full, the flip first packs the live set (this key's value included), then the
    // tombstone lands in the fresh half and supersedes it; correct, self-healing on the next flip.
    set_bytes_with(store, capacity, key, &[], pending)
}

/// Reclaim space taken by superseded records via a power-safe **flip**: the live set is packed
/// into the inactive half and committed by one superblock write. A no-op on an un-initialized
/// region.
pub fn compact<S: Eeprom>(store: &mut S, capacity: u32) -> Result<(), KvError<S::Error>> {
    if let Some((h, g)) = active(store, capacity)? {
        flip(store, capacity, h, g)?;
    }
    Ok(())
}

/// [`compact`], aware of an in-progress incremental flip (`pending`, shared with [`maintain`]):
/// a fresh pending flip is finished (that *is* the requested compaction, cheaper than starting
/// over); a stale or absent one falls back to the synchronous flip. Consumes `pending` either way.
pub fn compact_with<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    pending: &mut Option<FlipState>,
) -> Result<(), KvError<S::Error>> {
    match pending.take() {
        Some(mut st) => match flip_commit(store, capacity, &mut st) {
            Err(KvError::Stale) => compact(store, capacity),
            r => r,
        },
        None => compact(store, capacity),
    }
}

/// Factory reset as a **flip to an empty store**: power-loss-safe at every step, wear-gauge
/// preserving.
///
/// The naive reset (zero the region front-to-back, superblocks last) is neither atomic nor
/// safe: interrupted part-way, the still-valid superblock sits over an active half whose prefix
/// is zeroed but whose tail survives. The record scan then reads the half as empty, new appends
/// start at offset 0, and the moment the new log end lands on an old record boundary the old
/// tail is scanned again: the old records **resurrect**, and (later in the log, latest-wins)
/// override the newer values. It also reset the generation (the lifetime-flip wear gauge) to 0.
///
/// This instead reuses the flip's own commit discipline, with an empty live set:
///
/// 1. **Blank the dead half** (read-first; free when maintenance pre-blanked it). Invisible:
///    the old store stays active.
/// 2. **Commit** the dead half's superblock at `generation + 1`: the single CRC'd atomic write.
///    Torn before/during it, the old store is still active and intact (nothing was wiped);
///    landed, the store reads **empty** from then on. The committed superblock is never zeroed,
///    and the generation carries on, so the wear gauge keeps counting (a wipe costs one flip).
/// 3. **Scrub the old half's records** so the wiped keys are not left physically readable.
///    Invisible: that half is no longer active, and the next flip into it blanks it first
///    anyway (its superblock is left as an ordinary superseded one, exactly like after a flip).
///
/// So a reset at any point leaves either the complete old store or an empty one, never a mix,
/// and never a half whose tail can resurface. An un-initialized region (fresh, or bytes in any
/// other layout) is committed empty in half 1 at generation 1 and then scrubbed (half 0 +
/// superblock slot 0, which holds no valid superblock).
///
/// Cost: one half of word-programs (~2.6 s of NVM stall on the STM32L0) when the dead half was
/// pre-blanked, two halves (~5.2 s) otherwise. A caller holding RAM state derived from the store
/// must stop writing and reboot afterwards (the firmware's `Kv` freezes itself for that).
pub fn wipe<S: Eeprom>(store: &mut S, capacity: u32) -> Result<(), KvError<S::Error>> {
    let half = half_len(capacity);
    match active(store, capacity)? {
        Some((h, g)) => {
            let dst = 1 - h;
            blank(store, half_base(capacity, dst), half)?; // (1) invisible
            write_gen(store, capacity, dst, g.wrapping_add(1))?; // (2) the commit: empty from here
            blank(store, half_base(capacity, h), half) // (3) scrub: invisible
        }
        None => {
            blank(store, half_base(capacity, 1), half)?;
            write_gen(store, capacity, 1, 1)?;
            blank(store, half_base(capacity, 0), half)?;
            blank(store, super_off(capacity, 0), SUPER_LEN)
        }
    }
}

/// The active half's **generation**: a monotonic counter bumped once per compaction `flip`
/// (its atomic commit), so it doubles as the store's **lifetime flip count**: the load-bearing
/// input to an EEPROM-wear estimate, since each flip cycles ~one half's cells. `0` on an
/// un-initialized region. A pure read (two superblock reads, no write), so polling it for
/// telemetry never itself contributes to wear.
pub fn generation<S: Eeprom>(store: &S, capacity: u32) -> Result<u32, KvError<S::Error>> {
    Ok(active(store, capacity)?.map(|(_, g)| g).unwrap_or(0))
}

/// Flip: pack the live set of the active half `src_h` (generation `src_gen`) into the inactive
/// half, then commit by writing the inactive half's superblock at `src_gen + 1`.
fn flip<S: Eeprom>(store: &mut S, capacity: u32, src_h: u8, src_gen: u32) -> Result<(), KvError<S::Error>> {
    let half = half_len(capacity);
    let src_base = half_base(capacity, src_h);
    let dst_h = 1 - src_h;
    let dst_base = half_base(capacity, dst_h);

    // Pass 1: latest offset per tag in the source half (+ the source log's end).
    // `tmp` is reused as pass 1's CRC-check scratch and pass 2's record-copy buffer; the two
    // uses are strictly sequential, so one buffer serves both. This keeps a second MAX_VALUE
    // array off the compaction stack: flip runs synchronously inside the executor poll (via
    // set_bytes ← Nv::with ← net.send), the deepest stack path in a radio app, where peak
    // headroom is tight (see firmware/docs/radio.md: no stack guard on this target).
    let mut latest: [(u16, u32); MAX_KEYS] = [(0, 0); MAX_KEYS];
    let mut nkeys = 0usize;
    let mut tmp = [0u8; KV_HEADER + MAX_VALUE];
    let src_free = walk_latest(store, src_base, half, &mut latest, &mut nkeys, &mut tmp)?;

    // Blank the destination half so its scan will terminate cleanly (it may hold stale records
    // from when it was active two generations ago). Its superblock is untouched here, so the
    // source half stays active throughout; the blank/copy are not yet committed.
    blank(store, dst_base, half)?;

    // Pass 2: copy each latest record forward into the destination, packed.
    let mut wr = 0u32;
    let mut o = 0u32;
    while o < src_free {
        let mut hdr = [0u8; KV_HEADER];
        store.read(src_base + o, &mut hdr).map_err(KvError::Backend)?;
        let tag = u16::from_le_bytes([hdr[0], hdr[1]]);
        if tag == 0 {
            break;
        }
        let len = u16::from_le_bytes([hdr[2], hdr[3]]);
        let size = record_size(len as usize);
        // Copy a record only if it is the latest for its tag AND not a delete tombstone
        // (`len == 0`): a tombstoned key is dropped here, reclaiming both its value and the
        // tombstone. Superseded records (older than the latest) fall through uncopied as before.
        if len != 0 && latest[..nkeys].iter().any(|&(t, off)| t == tag && off == o) {
            if wr + size > half {
                return Err(KvError::Full); // live set doesn't fit a half
            }
            store
                .read(src_base + o, &mut tmp[..size as usize])
                .map_err(KvError::Backend)?;
            store
                .write(dst_base + wr, &tmp[..size as usize])
                .map_err(KvError::Backend)?;
            wr += size;
        }
        o += size;
    }

    // Commit: the destination becomes active the instant this CRC'd superblock lands.
    write_gen(store, capacity, dst_h, src_gen.wrapping_add(1))
}

/// Walk a half's records recording the latest offset per tag; returns the log's end offset (rel).
fn walk_latest<S: Eeprom>(
    store: &S,
    base: u32,
    area: u32,
    latest: &mut [(u16, u32); MAX_KEYS],
    nkeys: &mut usize,
    val: &mut [u8],
) -> Result<u32, KvError<S::Error>> {
    let mut o = 0u32;
    loop {
        if o + KV_HEADER as u32 > area {
            break;
        }
        let mut hdr = [0u8; KV_HEADER];
        store.read(base + o, &mut hdr).map_err(KvError::Backend)?;
        let tag = u16::from_le_bytes([hdr[0], hdr[1]]);
        if tag == 0 {
            break;
        }
        let len = u16::from_le_bytes([hdr[2], hdr[3]]);
        let crc = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if len as usize > MAX_VALUE || o + record_size(len as usize) > area {
            break;
        }
        store
            .read(base + o + KV_HEADER as u32, &mut val[..len as usize])
            .map_err(KvError::Backend)?;
        if entry_crc(&hdr[0..4], &val[..len as usize]) != crc {
            break;
        }
        match latest[..*nkeys].iter().position(|&(t, _)| t == tag) {
            Some(i) => latest[i].1 = o,
            None => {
                if *nkeys >= MAX_KEYS {
                    return Err(KvError::TooManyKeys);
                }
                latest[*nkeys] = (tag, o);
                *nkeys += 1;
            }
        }
        o += record_size(len as usize);
    }
    Ok(o)
}

// --- incremental maintenance ---------------------------------------------------------------------
//
// A synchronous flip costs *time*, not just wear: on the STM32L0 every EEPROM word program stalls
// the whole CPU ~3.4 ms (instruction fetch halts, ISRs freeze), so blanking + re-packing a full
// half is seconds of chip freeze (firmware/docs/storage.md, bench-measured). The pieces below split the
// same work into bounded, RAM-tracked steps a background task can run a few word-programs at a
// time. Power-loss safety is inherited from the two-half + superblock-commit design: nothing
// before [`flip_commit`]'s single superblock write changes the active half, so a reboot anywhere
// mid-maintenance just forgets the RAM state and restarts from scratch: no on-EEPROM state
// machine, no recovery path.

/// Proactive-flip threshold for [`maintain`]: start an incremental flip once the active half's
/// free space drops below this many bytes.
///
/// Rationale: appends during an incremental flip keep landing in the *source* half, so the
/// trigger must leave room for the writes that arrive between crossing the threshold and the
/// commit. Two worst-case records (`2 × (KV_HEADER + MAX_VALUE)` = 528 B; `MAX_VALUE` is whole
/// words, so that is also the padded [`record_size`] of a maximal record) cover the append that
/// crossed the threshold plus a maximal burst record during the drain. That is generous, given the
/// maintenance task is woken by that very write and drains the flip in back-to-back slices,
/// while real SDK records are 4-8 byte scalars. And the guarantee is soft by design: if a
/// pathological burst fills the source anyway, [`set_bytes_with`] finishes the pending flip
/// synchronously (bounded by its *remaining* steps): never a failure, just latency.
pub const FLIP_THRESHOLD: u32 = 2 * (KV_HEADER + MAX_VALUE) as u32;

/// Cost of writing `len` bytes, in EEPROM word-programs: the unit of every maintenance budget
/// (each word program stalls the STM32L0 ~3.4 ms). **Exact** for every codec write: all of them
/// are whole words at word-aligned offsets (module docs, "Whole words only"), so the HAL
/// (`eeprom_write_slice`) programs each as `len / 4` word programs and never splits off the
/// single-byte head/tail programs an unaligned span costs (up to 3 each, each as slow as a word
/// program, and each an erase of the whole word holding it).
fn words(len: u32) -> u32 {
    len.div_ceil(4)
}

/// Free (appendable) bytes remaining in the active half, a pure read (one record scan). On an
/// un-initialized region, reports one full half (what [`init`] would leave). Feeds the
/// [`maintain`] threshold check and `system eeprom print`.
pub fn free_bytes<S: Eeprom>(store: &S, capacity: u32) -> Result<u32, KvError<S::Error>> {
    let half = half_len(capacity);
    match active(store, capacity)? {
        Some((h, _)) => {
            let (end, _) = scan_half(store, half_base(capacity, h), half, 0)?;
            Ok(half - end)
        }
        None => Ok(half),
    }
}

/// Bytes the live set (latest record per key) occupies: the packed size a flip would leave.
/// A pure read; `0` on an un-initialized region. `free_bytes + (used − live_bytes)` is the
/// space one flip reclaims.
pub fn live_bytes<S: Eeprom>(store: &S, capacity: u32) -> Result<u32, KvError<S::Error>> {
    let Some((h, _)) = active(store, capacity)? else {
        return Ok(0);
    };
    let mut latest: [(u16, u32); MAX_KEYS] = [(0, 0); MAX_KEYS];
    let mut nkeys = 0usize;
    let mut tmp = [0u8; KV_HEADER + MAX_VALUE];
    let base = half_base(capacity, h);
    walk_latest(store, base, half_len(capacity), &mut latest, &mut nkeys, &mut tmp)?;
    let mut total = 0u32;
    for &(_, off) in &latest[..nkeys] {
        let mut hdr = [0u8; KV_HEADER];
        store.read(base + off, &mut hdr).map_err(KvError::Backend)?;
        let len = u16::from_le_bytes([hdr[2], hdr[3]]);
        // A tombstoned key (latest len 0) leaves nothing behind after a flip; exclude it so
        // this stays an exact "packed size a flip would leave".
        if len != 0 {
            total += record_size(len as usize);
        }
    }
    Ok(total)
}

/// Whether the dead (inactive) half is fully blank, a pure read; `true` on an un-initialized
/// region. Exactly the condition under which the next flip's blank pass costs nothing.
pub fn dead_half_blank<S: Eeprom>(store: &S, capacity: u32) -> Result<bool, KvError<S::Error>> {
    let Some((h, _)) = active(store, capacity)? else {
        return Ok(true);
    };
    let base = half_base(capacity, 1 - h);
    let half = half_len(capacity);
    let mut chunk = [0u8; 64];
    let mut o = 0u32;
    while o < half {
        let n = ((half - o) as usize).min(chunk.len());
        store.read(base + o, &mut chunk[..n]).map_err(KvError::Backend)?;
        if chunk[..n].iter().any(|&b| b != 0) {
            return Ok(false);
        }
        o += n as u32;
    }
    Ok(true)
}

/// RAM-only progress cursor for [`blank_dead_step`]. A fresh cursor (or a reboot, which loses
/// it) restarts from the top of the dead half (near-free, because already-blank words are
/// skipped with reads only). The recorded generation makes it self-healing: when a flip changes
/// the active half, the cursor rewinds automatically.
#[derive(Clone, Copy, Default)]
pub struct BlankCursor {
    generation: u32,
    off: u32,
}

impl BlankCursor {
    /// A cursor starting at the top of the (current) dead half.
    pub const fn new() -> Self {
        Self {
            generation: 0,
            off: 0,
        }
    }
}

/// Incrementally blank the **dead** (inactive) half ("pre-blanking"), run any time after a flip
/// commits, so the *next* flip (incremental or the in-line fallback) skips its blank pass.
/// Programs at most `budget_words` zero-words per call; already-zero words are skipped with
/// reads only (idempotent, no wear on blank cells). Returns whether blanking work remains.
pub fn blank_dead_step<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    cur: &mut BlankCursor,
    budget_words: u32,
) -> Result<bool, KvError<S::Error>> {
    let Some((h, g)) = active(store, capacity)? else {
        return Ok(false); // un-initialized region: no dead half yet
    };
    if cur.generation != g {
        *cur = BlankCursor {
            generation: g,
            off: 0,
        }; // active half changed → rewind
    }
    let half = half_len(capacity);
    if cur.off >= half {
        return Ok(false);
    }
    let mut budget = budget_words;
    cur.off = blank_step_span(store, half_base(capacity, 1 - h), half, cur.off, &mut budget)?;
    Ok(cur.off < half)
}

/// RAM-only state of an in-progress **incremental** flip ([`flip_start`] → [`flip_step`]* →
/// [`flip_commit`]). Reboot-safe by construction: the source half stays active until the
/// commit's superblock write, so losing this state (power loss, task restart) leaves the store
/// exactly as committed and the flip simply restarts from scratch.
pub struct FlipState {
    src_h: u8,
    src_gen: u32,
    /// Source log end at [`flip_start`]. Appends may land past it (they keep going to the
    /// source half); [`flip_commit`] re-walks that tail.
    src_end: u32,
    /// Pass-1 plan: latest record offset per tag within `[0, src_end)`.
    latest: [(u16, u32); MAX_KEYS],
    nkeys: usize,
    /// Destination blank progress (phase A of [`flip_step`]).
    blank_off: u32,
    /// Source scan progress of the copy (phase B).
    read_off: u32,
    /// Bytes of planned records fully copied: the destination write position.
    write_off: u32,
    /// Bytes of the record at `read_off` already copied (mid-record resume between steps).
    rec_copied: u32,
}

/// Begin an incremental flip: pass-1 scan of the active half into a RAM plan (reads only: no
/// EEPROM writes, no stall). `None` on an un-initialized region.
pub fn flip_start<S: Eeprom>(store: &S, capacity: u32) -> Result<Option<FlipState>, KvError<S::Error>> {
    let Some((h, g)) = active(store, capacity)? else {
        return Ok(None);
    };
    let mut latest: [(u16, u32); MAX_KEYS] = [(0, 0); MAX_KEYS];
    let mut nkeys = 0usize;
    let mut tmp = [0u8; KV_HEADER + MAX_VALUE];
    let src_end = walk_latest(
        store,
        half_base(capacity, h),
        half_len(capacity),
        &mut latest,
        &mut nkeys,
        &mut tmp,
    )?;
    Ok(Some(FlipState {
        src_h: h,
        src_gen: g,
        src_end,
        latest,
        nkeys,
        blank_off: 0,
        read_off: 0,
        write_off: 0,
        rec_copied: 0,
    }))
}

/// Whether `st` still describes the store (no other flip committed since [`flip_start`]).
fn flip_fresh<S: Eeprom>(store: &S, capacity: u32, st: &FlipState) -> Result<bool, KvError<S::Error>> {
    Ok(active(store, capacity)? == Some((st.src_h, st.src_gen)))
}

/// One bounded slice of flip work: blank the destination if still needed (read-first; free when
/// pre-blanked by [`blank_dead_step`]), then copy the planned live set, programming at most
/// `budget_words` words. Returns whether pre-commit work remains (`false` = ready for
/// [`flip_commit`]). [`KvError::Stale`] if another flip committed underneath.
pub fn flip_step<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    st: &mut FlipState,
    budget_words: u32,
) -> Result<bool, KvError<S::Error>> {
    if !flip_fresh(store, capacity, st)? {
        return Err(KvError::Stale);
    }
    let half = half_len(capacity);
    let src_base = half_base(capacity, st.src_h);
    let dst_base = half_base(capacity, 1 - st.src_h);
    let mut budget = budget_words;

    // Phase A: blank the destination so stale records from two generations ago can't survive
    // past the copied set (scan would trust a stale-but-valid record; see `flip`).
    if st.blank_off < half {
        st.blank_off = blank_step_span(store, dst_base, half, st.blank_off, &mut budget)?;
        if st.blank_off < half {
            return Ok(true); // budget exhausted mid-blank
        }
    }

    // Phase B: copy the planned records forward, packed; resumable mid-record, so one step
    // never programs more than the budget even for a MAX_VALUE record. A partially-copied
    // record is harmless: the destination is uncommitted until `flip_commit`. Records (padded)
    // and chunks (`budget * 4`) are whole words, so every slice is aligned word programs.
    let mut tmp = [0u8; KV_HEADER + MAX_VALUE];
    while st.read_off < st.src_end {
        let mut hdr = [0u8; KV_HEADER];
        store
            .read(src_base + st.read_off, &mut hdr)
            .map_err(KvError::Backend)?;
        let tag = u16::from_le_bytes([hdr[0], hdr[1]]);
        if tag == 0 {
            break; // defensive: walk_latest validated [0, src_end)
        }
        let len = u16::from_le_bytes([hdr[2], hdr[3]]);
        let size = record_size(len as usize);
        let planned = st.latest[..st.nkeys]
            .iter()
            .any(|&(t, off)| t == tag && off == st.read_off);
        if !planned || len == 0 {
            // superseded, or a delete tombstone (len 0) whose key is dropped: skip either way
            st.read_off += size;
            continue;
        }
        if st.write_off + size > half {
            return Err(KvError::Full); // unreachable (live ⊆ source), kept defensive
        }
        while st.rec_copied < size {
            if budget == 0 {
                return Ok(true);
            }
            let chunk = (size - st.rec_copied).min(budget.saturating_mul(4));
            store
                .read(src_base + st.read_off + st.rec_copied, &mut tmp[..chunk as usize])
                .map_err(KvError::Backend)?;
            store
                .write(dst_base + st.write_off + st.rec_copied, &tmp[..chunk as usize])
                .map_err(KvError::Backend)?;
            budget = budget.saturating_sub(words(chunk));
            st.rec_copied += chunk;
        }
        st.rec_copied = 0;
        st.read_off += size;
        st.write_off += size;
    }
    Ok(false)
}

/// Finish an incremental flip: drive any remaining [`flip_step`] work, **catch up the source
/// tail** (records appended past `src_end` since [`flip_start`], copied verbatim in append order;
/// a tail record superseding a planned or earlier-tail one lands *later* in the destination,
/// so latest-wins scan semantics resolve it), then commit by writing the destination superblock
/// at `generation + 1`: the same single CRC'd atomic commit as the synchronous flip.
/// [`KvError::Stale`] if another flip committed underneath (nothing is written in that case).
pub fn flip_commit<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    st: &mut FlipState,
) -> Result<(), KvError<S::Error>> {
    // Freshness is checked inside flip_step (first call), so a stale plan can't reach the
    // superblock write below.
    while flip_step(store, capacity, st, u32::MAX)? {}

    let half = half_len(capacity);
    let src_base = half_base(capacity, st.src_h);
    let dst_base = half_base(capacity, 1 - st.src_h);

    // Tail catch-up: same validated walk as `scan_half`, copying every intact record whole.
    // Total destination usage = packed live set (≤ src_end) + tail (= src_now − src_end)
    // ≤ src_now ≤ half, so this always fits; the check stays as a defensive invariant.
    let mut tmp = [0u8; KV_HEADER + MAX_VALUE];
    let mut o = st.src_end;
    loop {
        if o + KV_HEADER as u32 > half {
            break;
        }
        store
            .read(src_base + o, &mut tmp[..KV_HEADER])
            .map_err(KvError::Backend)?;
        let tag = u16::from_le_bytes([tmp[0], tmp[1]]);
        if tag == 0 {
            break;
        }
        let len = u16::from_le_bytes([tmp[2], tmp[3]]);
        let crc = u32::from_le_bytes([tmp[4], tmp[5], tmp[6], tmp[7]]);
        if len as usize > MAX_VALUE || o + record_size(len as usize) > half {
            break;
        }
        // The whole padded record is copied (value + pad), the CRC checked over the value only.
        let size = record_size(len as usize);
        store
            .read(
                src_base + o + KV_HEADER as u32,
                &mut tmp[KV_HEADER..size as usize],
            )
            .map_err(KvError::Backend)?;
        if entry_crc(&tmp[0..4], &tmp[KV_HEADER..KV_HEADER + len as usize]) != crc {
            break; // torn tail append: end of the clean log, drop it as scan would
        }
        if st.write_off + size > half {
            return Err(KvError::Full);
        }
        store
            .write(dst_base + st.write_off, &tmp[..size as usize])
            .map_err(KvError::Backend)?;
        st.write_off += size;
        o += size;
    }

    // Commit: the destination becomes active the instant this CRC'd superblock lands.
    write_gen(store, capacity, 1 - st.src_h, st.src_gen.wrapping_add(1))
}

/// RAM-only maintenance state: the pre-blanking cursor plus at most one pending incremental
/// flip. Keep it next to the store handle and share [`pending`](Self::pending) with
/// [`set_bytes_with`]/[`compact_with`], so a store that fills mid-flip *finishes* the flip
/// instead of restarting one. Losing it (reboot) is always safe; see [`FlipState`].
pub struct MaintState {
    blank: BlankCursor,
    flip: Option<FlipState>,
}

impl MaintState {
    /// Fresh state: nothing pending, blanking restarts from the top (near-free if already blank).
    pub const fn new() -> Self {
        Self {
            blank: BlankCursor::new(),
            flip: None,
        }
    }

    /// The pending incremental-flip slot, for [`set_bytes_with`]/[`compact_with`].
    pub fn pending(&mut self) -> &mut Option<FlipState> {
        &mut self.flip
    }
}

impl Default for MaintState {
    fn default() -> Self {
        Self::new()
    }
}

/// One bounded maintenance slice (at most `budget_words` EEPROM word-programs). In priority
/// order: (1) advance/commit a pending incremental flip, (2) start one when the active half's
/// free space is below `threshold` (see [`FLIP_THRESHOLD`]) *and* a flip would restore it,
/// (3) pre-blank the dead half. Returns whether work remains; call again (after yielding to
/// other tasks) until `false`, then sleep until the next store write.
///
/// The commit slice is the one call that can exceed the budget: it also copies the tail of
/// appends that landed during the flip (bounded by `threshold`, typically zero or one small
/// record) plus the 12-byte superblock.
pub fn maintain<S: Eeprom>(
    store: &mut S,
    capacity: u32,
    m: &mut MaintState,
    budget_words: u32,
    threshold: u32,
) -> Result<bool, KvError<S::Error>> {
    let Some((h, g)) = active(store, capacity)? else {
        return Ok(false); // un-initialized: nothing to maintain until the first write
    };

    // (1) Drive a pending flip; drop it if stale (a synchronous flip committed underneath).
    if m.flip.as_ref().is_some_and(|st| (st.src_h, st.src_gen) != (h, g)) {
        m.flip = None;
    }
    if let Some(st) = &mut m.flip {
        if flip_step(store, capacity, st, budget_words)? {
            return Ok(true);
        }
        let mut st = m.flip.take().unwrap();
        flip_commit(store, capacity, &mut st)?;
        return Ok(true); // the old half is now dead + dirty → pre-blanking work remains
    }

    // (2) Start a flip below the threshold, but only if it would lift free space back over it
    // (live + threshold ≤ half). Otherwise the live set is simply too big for proactive help:
    // flipping would burn wear without restoring headroom, so leave it to the in-line fallback
    // (whose blank pass the pre-blanked dead half already halves).
    let half = half_len(capacity);
    let (end, _) = scan_half(store, half_base(capacity, h), half, 0)?;
    if half - end < threshold && live_bytes(store, capacity)? + threshold <= half {
        m.flip = flip_start(store, capacity)?;
        return Ok(true); // pass-1 was reads only; the next slice starts programming
    }

    // (3) Pre-blank the dead half so the next flip skips its blank pass.
    blank_dead_step(store, capacity, &mut m.blank, budget_words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const CAP: u32 = 512; // small enough to force flips quickly in tests

    /// How a power cut leaves the word whose program it interrupts. The STM32L0 data EEPROM
    /// programs (and keeps ECC) per aligned 32-bit word, so either way the WHOLE word is lost,
    /// including bytes of it the write never addressed.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Tear {
        /// The power died before the program began: the word keeps its old contents (a cut
        /// between two programs).
        Untouched,
        /// The erase phase finished, the program did not: the word reads erased (all zero).
        Erased,
        /// Undefined contents: here the word as the program meant to leave it, with the low bit
        /// of every byte flipped, so a tag or length in it stays plausible and only a CRC (or a
        /// superblock magic) catches it.
        Garbled,
    }

    /// RAM-backed [`Eeprom`] that programs like the firmware's backend (embassy-stm32
    /// `eeprom_write_slice` over the STM32L0 data EEPROM): a write's word-aligned middle as word
    /// programs, an unaligned head and tail as single-byte programs, each of which is an erase plus
    /// a program of the whole aligned word holding it. Without a cut armed it simply programs the
    /// bytes. With `cut` armed, the power dies inside the program with that index (`programs`
    /// clock): the programs before it landed, the whole word of the torn one is erased or
    /// garbled ([`Tear`]), and every later write fails without effect until [`Ram::revive`]
    /// (the reboot). `torn_write` is the older, byte-granular model (only a prefix lands), kept
    /// for the tests that hand-place a torn record.
    ///
    /// Bookkeeping: `words_written` mirrors the codec's own budget accounting (`words()` per
    /// write call), so the maintenance tests can assert budgets and skip-if-zero wear-freedom;
    /// `cycles` counts erase/write cycles per word (a program over a word that is not already
    /// erased auto-erases it first, the endurance the firmware's `FLIP_BUDGET` is about);
    /// `unaligned` counts write calls that were not whole words at a word-aligned offset;
    /// `trace` is the word address of every program, in order.
    struct Ram {
        buf: Vec<u8>,
        words_written: usize,
        programs: usize,
        cut: Option<(usize, Tear)>,
        dead: bool,
        cycles: Vec<u32>,
        unaligned: usize,
        trace: Vec<usize>,
    }
    impl Ram {
        fn new(cap: usize) -> Self {
            Self {
                buf: vec![0u8; cap],
                words_written: 0,
                programs: 0,
                cut: None,
                dead: false,
                cycles: vec![0; cap.div_ceil(4)],
                unaligned: 0,
                trace: Vec::new(),
            }
        }
        /// Arm a power cut `after` programs from now.
        fn arm(&mut self, after: usize, tear: Tear) {
            self.cut = Some((self.programs + after, tear));
        }
        /// The reboot: power is back, nothing armed. What reached the cells is all that survives.
        fn revive(&mut self) {
            self.dead = false;
            self.cut = None;
        }
        /// Simulate a torn write: only the first `landed` bytes of `data` are programmed.
        fn torn_write(&mut self, off: u32, data: &[u8], landed: usize) {
            let n = landed.min(data.len());
            self.buf[off as usize..off as usize + n].copy_from_slice(&data[..n]);
        }
        /// Write a well-formed record of the old, unpadded layout at absolute `off` with no
        /// superblock anywhere, returning the next offset: bytes in another layout (e.g. the old
        /// single-region log) for the format tests, which must never be read back as records.
        /// Placed straight into the cells (it is a fixture, not a codec write).
        fn raw_put(&mut self, off: u32, key: u16, value: &[u8]) -> u32 {
            let mut rec = key.to_le_bytes().to_vec();
            rec.extend_from_slice(&(value.len() as u16).to_le_bytes());
            let crc = entry_crc(&rec, value);
            rec.extend_from_slice(&crc.to_le_bytes());
            rec.extend_from_slice(value);
            self.buf[off as usize..off as usize + rec.len()].copy_from_slice(&rec);
            off + rec.len() as u32
        }
        /// One HAL program of `bytes` (a whole aligned word, or one byte) at `at`.
        fn program(&mut self, at: usize, bytes: &[u8]) -> Result<(), ()> {
            if self.dead {
                return Err(());
            }
            let w = at & !3;
            let n = self.programs;
            self.programs += 1;
            self.trace.push(w);
            if let Some((_, tear)) = self.cut.filter(|&(c, _)| c == n) {
                let word = &mut self.buf[w..w + 4];
                match tear {
                    Tear::Untouched => {}
                    Tear::Erased => word.fill(0),
                    Tear::Garbled => {
                        let mut meant = [word[0], word[1], word[2], word[3]];
                        meant[at - w..at - w + bytes.len()].copy_from_slice(bytes);
                        for (cell, m) in word.iter_mut().zip(meant) {
                            *cell = m ^ 0x01;
                        }
                    }
                }
                self.dead = true;
                return Err(());
            }
            if self.buf[w..w + 4].iter().any(|&b| b != 0) {
                self.cycles[w / 4] += 1;
            }
            self.buf[at..at + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
    }
    impl Eeprom for Ram {
        type Error = ();
        fn read(&self, off: u32, buf: &mut [u8]) -> Result<(), ()> {
            buf.copy_from_slice(self.buf.get(off as usize..off as usize + buf.len()).ok_or(())?);
            Ok(())
        }
        fn write(&mut self, off: u32, data: &[u8]) -> Result<(), ()> {
            let s = off as usize;
            if s + data.len() > self.buf.len() {
                return Err(());
            }
            if !s.is_multiple_of(4) || !data.len().is_multiple_of(4) {
                self.unaligned += 1;
            }
            // The embassy-stm32 `eeprom_write_slice` split: byte programs up to the first word
            // boundary, word programs, byte programs for the tail.
            let head = if s.is_multiple_of(4) {
                0
            } else {
                (4 - s % 4).min(data.len())
            };
            let body = (data.len() - head) / 4 * 4;
            for i in 0..head {
                self.program(s + i, &data[i..i + 1])?;
            }
            for k in (head..head + body).step_by(4) {
                self.program(s + k, &data[k..k + 4])?;
            }
            for i in head + body..data.len() {
                self.program(s + i, &data[i..i + 1])?;
            }
            self.words_written += data.len().div_ceil(4);
            Ok(())
        }
    }

    fn set(ram: &mut Ram, key: u16, value: &[u8]) -> Result<(), KvError<()>> {
        set_bytes(ram, CAP, key, value)
    }
    fn get_vec(ram: &Ram, key: u16) -> Option<Vec<u8>> {
        let mut out = [0u8; MAX_VALUE];
        get_bytes(ram, CAP, key, &mut out)
            .unwrap()
            .map(|n| out[..n].to_vec())
    }

    #[test]
    fn roundtrip_and_absent() {
        let mut ram = Ram::new(CAP as usize);
        assert_eq!(get_vec(&ram, 1), None);
        set(&mut ram, 1, b"hello").unwrap();
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"hello"[..]));
        assert_eq!(get_vec(&ram, 2), None);
    }

    #[test]
    fn key_zero_reserved_and_value_too_large() {
        let mut ram = Ram::new(CAP as usize);
        assert_eq!(set(&mut ram, 0, b"x"), Err(KvError::InvalidKey));
        assert_eq!(get_bytes(&ram, CAP, 0, &mut [0u8; 4]).unwrap(), None);
        assert_eq!(
            set(&mut ram, 1, &[0u8; MAX_VALUE + 1]),
            Err(KvError::ValueTooLarge)
        );
    }

    #[test]
    fn append_only_latest_wins() {
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, &[1, 2, 3, 4]).unwrap();
        set(&mut ram, 1, &[9, 9, 9, 9]).unwrap(); // same length still appends (no in-place)
        set(&mut ram, 1, &[5, 6]).unwrap(); // different length appends
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&[5, 6][..]));
    }

    #[test]
    fn multiple_keys_independent_across_flips() {
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 0x5202, b"keepme").unwrap();
        // Churn key 0x5201 enough to force several compaction flips; 0x5202 must survive each.
        for i in 0..400u32 {
            set(&mut ram, 0x5201, &i.to_le_bytes()).unwrap();
            assert_eq!(
                get_vec(&ram, 0x5202).as_deref(),
                Some(&b"keepme"[..]),
                "lost key across flip at i={i}"
            );
        }
        assert_eq!(get_vec(&ram, 0x5201).unwrap(), 399u32.to_le_bytes());
    }

    #[test]
    fn flip_alternates_active_half_and_bumps_generation() {
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, b"v").unwrap();
        let (h0, g0) = active(&ram, CAP).unwrap().unwrap();
        compact(&mut ram, CAP).unwrap();
        let (h1, g1) = active(&ram, CAP).unwrap().unwrap();
        assert_ne!(h0, h1, "flip must switch the active half");
        assert_eq!(g1, g0 + 1, "flip must bump the generation");
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn full_when_value_cannot_fit_a_half() {
        let mut ram = Ram::new(CAP as usize);
        let half = half_len(CAP) as usize;
        // A value larger than a half (minus header) can never fit, even after a flip.
        assert_eq!(set(&mut ram, 1, &vec![7u8; half]), Err(KvError::Full));
    }

    #[test]
    fn torn_append_preserves_prior_keys() {
        // A torn append corrupts only the tail record; scan stops there and prior keys survive.
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, b"alpha").unwrap();
        set(&mut ram, 2, b"bravo").unwrap();
        let (h, _) = active(&ram, CAP).unwrap().unwrap();
        let base = half_base(CAP, h);
        let (free, _) = scan_half(&ram, base, half_len(CAP), 1).unwrap();
        let mut rec = [0u8; KV_HEADER + 4];
        rec[0..2].copy_from_slice(&3u16.to_le_bytes());
        rec[2..4].copy_from_slice(&4u16.to_le_bytes());
        let crc = entry_crc(&rec[0..4], &[7, 7, 7, 7]);
        rec[4..8].copy_from_slice(&crc.to_le_bytes());
        rec[8..12].copy_from_slice(&[7, 7, 7, 7]);
        ram.torn_write(base + free, &rec, 6); // only 6 of 12 bytes land
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"alpha"[..]));
        assert_eq!(get_vec(&ram, 2).as_deref(), Some(&b"bravo"[..]));
        assert_eq!(get_vec(&ram, 3), None, "the torn in-flight record is dropped");
    }

    #[test]
    fn torn_flip_keeps_old_half_active() {
        // Power-safety of compaction: a flip that writes the destination half but loses power
        // before the superblock commit must leave the OLD half active with the last good values,
        // and the next successful set must still see them.
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, b"committed-1").unwrap();
        set(&mut ram, 2, b"committed-2").unwrap();
        let (src_h, src_gen) = active(&ram, CAP).unwrap().unwrap();

        // Mimic flip()'s body but DROP the final superblock commit (the power-loss point).
        let half = half_len(CAP);
        let dst_h = 1 - src_h;
        let dst_base = half_base(CAP, dst_h);
        blank(&mut ram, dst_base, half).unwrap();
        // (write some plausible-but-uncommitted records into the destination half)
        append(&mut ram, dst_base, 1, b"GARBAGE").unwrap();
        // ... no write_gen(): power lost here.

        // The old half is still active and intact.
        let (now_h, now_gen) = active(&ram, CAP).unwrap().unwrap();
        assert_eq!(
            (now_h, now_gen),
            (src_h, src_gen),
            "old half must remain active after a torn flip"
        );
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"committed-1"[..]));
        assert_eq!(get_vec(&ram, 2).as_deref(), Some(&b"committed-2"[..]));
    }

    #[test]
    fn torn_superblock_commit_fails_closed() {
        // If the superblock write itself tears (CRC won't match), that half is treated as invalid
        // and the other half wins; never a half-committed generation.
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, b"good").unwrap();
        let (src_h, src_gen) = active(&ram, CAP).unwrap().unwrap();
        let dst_h = 1 - src_h;
        // Build a valid superblock for dst at gen+1, then tear it (only the magic + 2 bytes land).
        let mut sb = [0u8; SUPER_LEN as usize];
        sb[0..4].copy_from_slice(&SUPER_MAGIC);
        sb[4..8].copy_from_slice(&(src_gen + 1).to_le_bytes());
        let crc = super_crc(&sb[0..8]);
        sb[8..12].copy_from_slice(&crc.to_le_bytes());
        ram.torn_write(super_off(CAP, dst_h), &sb, 6);
        assert_eq!(
            active(&ram, CAP).unwrap(),
            Some((src_h, src_gen)),
            "torn superblock must not win"
        );
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"good"[..]));
    }

    /// A region holding valid-looking records but no superblock (the old single-region layout).
    fn foreign_region() -> Ram {
        foreign_region_at(CAP)
    }

    fn foreign_region_at(cap: u32) -> Ram {
        let mut ram = Ram::new(cap as usize);
        let mut o = 0u32;
        o = ram.raw_put(o, 0x5201, &7u32.to_le_bytes()); // e.g. an old TX-counter watermark
        o = ram.raw_put(o, 0x5202, &3u32.to_le_bytes());
        let mut n = 0u32;
        while o + (KV_HEADER as u32 + 4) <= cap - 2 * SUPER_LEN {
            n += 1;
            o = ram.raw_put(o, 0x5300, &n.to_le_bytes()); // through both halves
        }
        ram
    }

    #[test]
    fn init_formats_a_foreign_region_empty() {
        // Anything without a valid superblock is formatted empty: nothing is adopted or migrated,
        // in two writes (the head tag of half 0, then its superblock), not a scrub of the region.
        let mut ram = foreign_region();
        assert!(active(&ram, CAP).unwrap().is_none());
        ram.words_written = 0;
        init(&mut ram, CAP).unwrap();
        assert_eq!(active(&ram, CAP).unwrap(), Some((0, 1)));
        assert_eq!(ram.words_written, 1 + SUPER_LEN as usize / 4);
        for key in [0x5201, 0x5202, 0x5300] {
            assert_eq!(get_vec(&ram, key), None, "key {key:#x} adopted");
        }
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half_len(CAP));
        // The old records further in the half never resurface: appends of every size, through
        // flips, read back exactly what was written and nothing else.
        for i in 0..200u32 {
            let len = (i % 9) as usize + 1;
            let v: Vec<u8> = (0..len).map(|j| (i as u8).wrapping_add(j as u8) | 1).collect();
            set(&mut ram, 1 + (i % 3) as u16, &v).unwrap();
            assert_eq!(get_vec(&ram, 1 + (i % 3) as u16), Some(v));
            for key in [0x5201, 0x5202, 0x5300] {
                assert_eq!(get_vec(&ram, key), None, "key {key:#x} resurfaced at step {i}");
            }
        }
        assert!(generation(&ram, CAP).unwrap() > 1, "the churn must have flipped");
    }

    #[test]
    fn init_fresh_region_is_empty_then_usable() {
        let mut ram = Ram::new(CAP as usize);
        init(&mut ram, CAP).unwrap();
        assert_eq!(active(&ram, CAP).unwrap(), Some((0, 1)));
        assert_eq!(get_vec(&ram, 1), None);
        set(&mut ram, 1, b"x").unwrap();
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn init_mounts_an_initialized_region_untouched() {
        // A valid superblock: init is a pure read, whichever half is active.
        let mut ram = flip_fixture();
        let before = ram.buf.clone();
        ram.words_written = 0;
        init(&mut ram, CAP).unwrap();
        assert_eq!(ram.words_written, 0);
        assert_eq!(ram.buf, before);
        assert_fixture_live(&ram);
    }

    #[test]
    fn init_is_power_loss_safe() {
        // Cut the format at each of its two writes, landing none, part or all but one byte of it:
        // the region stays unformatted (no valid superblock), and the next init formats it.
        for cut in 0..2 {
            for landed in [0usize, 1, 5, 11] {
                let mut ram = foreign_region();
                // Replay init's writes by hand, tearing the `cut`-th one.
                let head = half_base(CAP, 0);
                if cut == 0 {
                    ram.torn_write(head, &[0; 4], landed.min(3));
                } else {
                    ram.buf[head as usize..head as usize + 4].copy_from_slice(&[0; 4]);
                    let mut sb = [0u8; SUPER_LEN as usize];
                    sb[0..4].copy_from_slice(&SUPER_MAGIC);
                    sb[4..8].copy_from_slice(&1u32.to_le_bytes());
                    let crc = super_crc(&sb[0..8]);
                    sb[8..12].copy_from_slice(&crc.to_le_bytes());
                    ram.torn_write(super_off(CAP, 0), &sb, landed);
                }
                assert!(active(&ram, CAP).unwrap().is_none(), "cut {cut} landed {landed}");
                init(&mut ram, CAP).unwrap();
                assert_eq!(active(&ram, CAP).unwrap(), Some((0, 1)));
                assert_eq!(get_vec(&ram, 0x5201), None);
                set(&mut ram, 7, b"ok").unwrap();
                assert_eq!(get_vec(&ram, 7).as_deref(), Some(&b"ok"[..]));
            }
        }
    }

    // --- incremental maintenance ------------------------------------------------------------

    /// Fixture for the maintenance tests: one committed flip (so the dead half holds stale
    /// records, a dirty blank target), then superseded + live records in the active half.
    fn flip_fixture() -> Ram {
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, b"one-v1").unwrap();
        set(&mut ram, 2, b"two").unwrap();
        compact(&mut ram, CAP).unwrap(); // gen 2: the old half is now dead and dirty
        set(&mut ram, 1, b"one-v2").unwrap(); // supersedes the packed copy
        set(&mut ram, 3, &[7u8; 40]).unwrap();
        ram
    }

    fn assert_fixture_live(ram: &Ram) {
        assert_eq!(get_vec(ram, 1).as_deref(), Some(&b"one-v2"[..]));
        assert_eq!(get_vec(ram, 2).as_deref(), Some(&b"two"[..]));
        assert_eq!(get_vec(ram, 3).as_deref(), Some(&[7u8; 40][..]));
    }

    #[test]
    fn preblank_skips_zero_and_is_idempotent() {
        let mut ram = flip_fixture();
        assert!(
            !dead_half_blank(&ram, CAP).unwrap(),
            "fixture's dead half must be dirty"
        );
        // Blank in budget-2 slices; every slice must respect its budget.
        let mut cur = BlankCursor::new();
        loop {
            let before = ram.words_written;
            let more = blank_dead_step(&mut ram, CAP, &mut cur, 2).unwrap();
            assert!(ram.words_written - before <= 2, "slice exceeded its word budget");
            if !more {
                break;
            }
        }
        assert!(dead_half_blank(&ram, CAP).unwrap());
        // Re-run with a fresh cursor (a "reboot"): every word is already zero, so the pass is
        // pure reads (zero programs, zero wear) and completes in one call.
        let w = ram.words_written;
        let mut cur = BlankCursor::new();
        assert!(!blank_dead_step(&mut ram, CAP, &mut cur, 2).unwrap());
        assert_eq!(ram.words_written, w, "re-blank after reboot must be write-free");
        assert_fixture_live(&ram); // the active half was never touched
    }

    #[test]
    fn preblank_restarts_after_mid_blank_reboot() {
        let mut ram = flip_fixture();
        let mut cur = BlankCursor::new();
        assert!(blank_dead_step(&mut ram, CAP, &mut cur, 1).unwrap()); // one word, then "reboot"
        let mut cur = BlankCursor::new(); // the RAM cursor is lost; restart from the top
        while blank_dead_step(&mut ram, CAP, &mut cur, 4).unwrap() {}
        assert!(dead_half_blank(&ram, CAP).unwrap());
        assert_fixture_live(&ram);
        // With the dead half pre-blanked, the in-line fallback flip must skip its blank pass:
        // it programs only the packed live set + the superblock.
        let live = live_bytes(&ram, CAP).unwrap();
        let w = ram.words_written;
        compact(&mut ram, CAP).unwrap();
        assert!(
            (ram.words_written - w) as u32 <= words(live) + 3,
            "pre-blanked in-line flip must not re-blank"
        );
        assert_fixture_live(&ram);
    }

    #[test]
    fn incremental_flip_interruptible_at_every_boundary() {
        // Probe: how many budget-1 slices a full flip of the fixture takes.
        let mut probe = flip_fixture();
        let mut st = flip_start(&probe, CAP).unwrap().unwrap();
        let mut total = 0usize;
        while flip_step(&mut probe, CAP, &mut st, 1).unwrap() {
            total += 1;
        }
        assert!(total > 4, "fixture too small to exercise both phases");

        // Interrupt after flip_start (k = 0), after every step (k = 1..total), and with all
        // steps done but the commit never written (k = total): in every case the old
        // generation stays active with the committed values, and a from-scratch restart
        // converges cleanly.
        for k in 0..=total {
            let mut ram = flip_fixture();
            let (h0, g0) = active(&ram, CAP).unwrap().unwrap();
            let mut st = flip_start(&ram, CAP).unwrap().unwrap();
            for _ in 0..k {
                assert!(flip_step(&mut ram, CAP, &mut st, 1).unwrap());
            }
            // `FlipState` has no Drop impl; the explicit drop is the narrative: it ends `st` here.
            #[allow(clippy::drop_non_drop)]
            drop(st); // ← the "reboot": RAM-only state lost, nothing committed
            assert_eq!(
                active(&ram, CAP).unwrap(),
                Some((h0, g0)),
                "old generation must stay active (interrupted at k={k})"
            );
            assert_fixture_live(&ram);
            // Restart from scratch and complete.
            let mut st = flip_start(&ram, CAP).unwrap().unwrap();
            while flip_step(&mut ram, CAP, &mut st, 1).unwrap() {}
            flip_commit(&mut ram, CAP, &mut st).unwrap();
            let (h1, g1) = active(&ram, CAP).unwrap().unwrap();
            assert_ne!(h0, h1, "flip must switch the active half (k={k})");
            assert_eq!(g1, g0 + 1, "flip must bump the generation (k={k})");
            assert_fixture_live(&ram);
        }
    }

    #[test]
    fn appends_during_flip_are_caught_up_at_commit() {
        let mut ram = flip_fixture();
        let (h0, g0) = active(&ram, CAP).unwrap().unwrap();
        let mut st = flip_start(&ram, CAP).unwrap().unwrap();
        for _ in 0..2 {
            assert!(flip_step(&mut ram, CAP, &mut st, 1).unwrap());
        }
        // Appends land in the SOURCE half while the flip is in progress (active is unchanged),
        // including a key superseded twice in the tail and a brand-new key.
        set(&mut ram, 2, b"tail-1").unwrap();
        set(&mut ram, 2, b"tail-2").unwrap();
        set(&mut ram, 9, b"new").unwrap();
        assert_eq!(
            active(&ram, CAP).unwrap(),
            Some((h0, g0)),
            "appends must not flip"
        );
        assert_eq!(get_vec(&ram, 2).as_deref(), Some(&b"tail-2"[..]));
        while flip_step(&mut ram, CAP, &mut st, 4).unwrap() {}
        flip_commit(&mut ram, CAP, &mut st).unwrap();
        let (h1, g1) = active(&ram, CAP).unwrap().unwrap();
        assert_ne!(h0, h1);
        assert_eq!(g1, g0 + 1);
        // Latest-wins across plan + tail: the twice-superseded key resolves to its last value.
        assert_eq!(get_vec(&ram, 2).as_deref(), Some(&b"tail-2"[..]));
        assert_eq!(get_vec(&ram, 9).as_deref(), Some(&b"new"[..]));
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"one-v2"[..]));
        assert_eq!(get_vec(&ram, 3).as_deref(), Some(&[7u8; 40][..]));
        // And the store keeps working in the new half.
        set(&mut ram, 2, b"post").unwrap();
        assert_eq!(get_vec(&ram, 2).as_deref(), Some(&b"post"[..]));
    }

    #[test]
    fn maintain_flips_below_threshold_only() {
        const T: u32 = 64; // test threshold (FLIP_THRESHOLD exceeds a whole test half)
        const BUDGET: u32 = 4;
        let mut ram = Ram::new(CAP as usize);
        let mut m = MaintState::new();
        set(&mut ram, 1, &[1u8; 20]).unwrap();

        // Plenty of free space: maintenance must quiesce without flipping.
        let g0 = generation(&ram, CAP).unwrap();
        while maintain(&mut ram, CAP, &mut m, BUDGET, T).unwrap() {}
        assert_eq!(generation(&ram, CAP).unwrap(), g0, "no flip above the threshold");

        // Churn until free space drops below the threshold, never filling the half, so no
        // in-line flip fires either.
        while free_bytes(&ram, CAP).unwrap() >= T {
            set(&mut ram, 1, &[9u8; 20]).unwrap();
        }
        assert_eq!(
            generation(&ram, CAP).unwrap(),
            g0,
            "churn must not flip synchronously"
        );

        // Below the threshold: maintenance starts and completes exactly one incremental flip,
        // then pre-blanks the old half and quiesces. Every slice stays within its budget
        // (+3 words for the commit slice's superblock).
        let mut worked = false;
        loop {
            let before = ram.words_written;
            let more = maintain(&mut ram, CAP, &mut m, BUDGET, T).unwrap();
            assert!(
                (ram.words_written - before) as u32 <= BUDGET + 3,
                "slice exceeded budget"
            );
            if !more {
                break;
            }
            worked = true;
        }
        assert!(worked);
        assert_eq!(
            generation(&ram, CAP).unwrap(),
            g0 + 1,
            "exactly one proactive flip"
        );
        assert!(free_bytes(&ram, CAP).unwrap() >= T, "flip must restore headroom");
        assert!(
            dead_half_blank(&ram, CAP).unwrap(),
            "maintenance ends with a blank dead half"
        );
        assert_eq!(get_vec(&ram, 1).unwrap(), [9u8; 20]);
    }

    #[test]
    fn set_bytes_finishes_pending_flip_when_source_fills() {
        let mut ram = Ram::new(CAP as usize);
        for i in 0..5u8 {
            set(&mut ram, 1, &[i; 40]).unwrap(); // 5 × 48 B = 240 of 244, nearly full
        }
        let (h0, g0) = active(&ram, CAP).unwrap().unwrap();
        let mut pending = flip_start(&ram, CAP).unwrap();
        assert!(flip_step(&mut ram, CAP, pending.as_mut().unwrap(), 1).unwrap()); // barely begun
        // This append cannot fit the source half: set_bytes_with must FINISH the pending flip
        // (not start a synchronous one from scratch) and then succeed, never failing where
        // plain set_bytes would have succeeded.
        set_bytes_with(&mut ram, CAP, 2, &[5u8; 100], &mut pending).unwrap();
        assert!(pending.is_none(), "the pending flip must be consumed");
        let (h1, g1) = active(&ram, CAP).unwrap().unwrap();
        assert_ne!(h0, h1);
        assert_eq!(g1, g0 + 1, "exactly one flip");
        assert_eq!(get_vec(&ram, 1).unwrap(), [4u8; 40]);
        assert_eq!(get_vec(&ram, 2).unwrap(), [5u8; 100]);
    }

    /// Regression (found while building the `kv_store` fuzz target): with a pending incremental
    /// flip, a full source made `set_bytes_with` finish that flip, which carries the source tail
    /// over verbatim (superseded records included), and then fail `Full` for a record a plain
    /// `set_bytes` (one fresh flip of the true live set) accepts. It must re-flip instead.
    #[test]
    fn set_with_pending_flip_never_fails_where_plain_set_fits() {
        let build = || {
            let mut ram = Ram::new(CAP as usize);
            set(&mut ram, 1, &[0u8; 92]).unwrap();
            set(&mut ram, 1, &[1u8; 92]).unwrap(); // 200 of 244 B used, 100 B live
            ram
        };
        let mut ram = build();
        let (_, g0) = active(&ram, CAP).unwrap().unwrap();
        let mut pending = flip_start(&ram, CAP).unwrap(); // plan: key 1 (100 B)
        set_bytes_with(&mut ram, CAP, 2, &[2u8; 12], &mut pending).unwrap(); // tail: 20 B
        set_bytes_with(&mut ram, CAP, 2, &[3u8; 12], &mut pending).unwrap(); // tail: 40 B, full
        // Live is 100 + 20 = 120 B, so a 108-byte record fits one fresh flip (228 <= 244), while
        // the finished flip alone holds plan + tail = 140 B (only 104 B free).
        set_bytes_with(&mut ram, CAP, 3, &[4u8; 100], &mut pending).unwrap();
        assert!(pending.is_none());
        assert_eq!(get_vec(&ram, 1).unwrap(), [1u8; 92]);
        assert_eq!(get_vec(&ram, 2).unwrap(), [3u8; 12]);
        assert_eq!(get_vec(&ram, 3).unwrap(), [4u8; 100]);
        let (_, g1) = active(&ram, CAP).unwrap().unwrap();
        assert_eq!(g1, g0 + 2, "the finished flip plus exactly one re-flip");

        // Plain set_bytes on the same history agrees (one flip).
        let mut ram = build();
        set(&mut ram, 2, &[2u8; 12]).unwrap();
        set(&mut ram, 2, &[3u8; 12]).unwrap();
        set(&mut ram, 3, &[4u8; 100]).unwrap();
        assert_eq!(get_vec(&ram, 3).unwrap(), [4u8; 100]);
    }

    #[test]
    fn free_and_live_bytes_match_hand_layout() {
        let mut ram = Ram::new(CAP as usize);
        init(&mut ram, CAP).unwrap();
        let half = half_len(CAP);
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 0);
        // Each record is KV_HEADER + len rounded up to whole words; live counts only the latest
        // per key.
        set(&mut ram, 1, &[1, 2, 3, 4]).unwrap(); // 12 B
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half - 12);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 12);
        set(&mut ram, 1, &[5, 6, 7, 8]).unwrap(); // supersedes: used 24, live still 12
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half - 24);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 12);
        set(&mut ram, 2, &[9u8; 6]).unwrap(); // 8 + 6 + 2 pad = 16 B
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half - 40);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 28);
        set(&mut ram, 3, &[9u8; 1]).unwrap(); // 8 + 1 + 3 pad = 12 B
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half - 52);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 40);
        assert_eq!(
            get_vec(&ram, 2).unwrap(),
            [9u8; 6],
            "the pad is not part of the value"
        );
        assert_eq!(get_vec(&ram, 3).unwrap(), [9u8; 1]);
        assert!(dead_half_blank(&ram, CAP).unwrap());
    }

    // --- deletion (tombstones) ------------------------------------------------------------------

    fn del(ram: &mut Ram, key: u16) -> Result<(), KvError<()>> {
        delete(ram, CAP, key)
    }

    #[test]
    fn delete_makes_key_absent_then_resurrectable() {
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, b"hello").unwrap();
        set(&mut ram, 2, b"keep").unwrap();
        del(&mut ram, 1).unwrap();
        assert_eq!(get_vec(&ram, 1), None, "deleted key reads absent");
        assert_eq!(
            get_vec(&ram, 2).as_deref(),
            Some(&b"keep"[..]),
            "sibling untouched"
        );
        // A later set resurrects the key (the new record supersedes the tombstone).
        set(&mut ram, 1, b"back").unwrap();
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"back"[..]));
        del(&mut ram, 1).unwrap();
        assert_eq!(get_vec(&ram, 1), None);
    }

    #[test]
    fn delete_missing_key_is_a_wear_free_noop() {
        let mut ram = Ram::new(CAP as usize);
        assert_eq!(del(&mut ram, 0), Err(KvError::InvalidKey), "key 0 reserved");
        // Never-set key on an un-initialized region: no write at all.
        del(&mut ram, 7).unwrap();
        assert_eq!(
            ram.words_written, 0,
            "deleting a missing key must not program a word"
        );
        set(&mut ram, 7, b"x").unwrap();
        del(&mut ram, 7).unwrap();
        let after_first = ram.words_written;
        del(&mut ram, 7).unwrap(); // already tombstoned → no-op
        assert_eq!(ram.words_written, after_first, "redundant delete is wear-free");
        assert_eq!(get_vec(&ram, 7), None);
    }

    #[test]
    fn flip_reclaims_deleted_keys() {
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, &[1u8; 40]).unwrap();
        set(&mut ram, 2, b"survivor").unwrap();
        del(&mut ram, 1).unwrap();
        // Before the flip, the tombstone leaves nothing live for key 1…
        assert_eq!(
            live_bytes(&ram, CAP).unwrap(),
            KV_HEADER as u32 + 8,
            "only key 2 is live"
        );
        compact(&mut ram, CAP).unwrap();
        // …and the flip physically drops both key 1's value and its tombstone.
        assert_eq!(get_vec(&ram, 1), None);
        assert_eq!(get_vec(&ram, 2).as_deref(), Some(&b"survivor"[..]));
        assert_eq!(
            free_bytes(&ram, CAP).unwrap(),
            half_len(CAP) - (KV_HEADER as u32 + 8),
            "packed half holds only the survivor"
        );
    }

    #[test]
    fn delete_survives_many_flips() {
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 0x10, b"gone").unwrap();
        del(&mut ram, 0x10).unwrap();
        // Churn another key hard enough to force many flips; the deleted key must never reappear.
        for i in 0..400u32 {
            set(&mut ram, 0x11, &i.to_le_bytes()).unwrap();
            assert_eq!(get_vec(&ram, 0x10), None, "deleted key resurfaced at i={i}");
        }
    }

    #[test]
    fn delete_that_triggers_a_flip_still_deletes() {
        // Fill the half so the tombstone append itself must flip; the key must end up absent.
        let mut ram = Ram::new(CAP as usize);
        for i in 0..5u8 {
            set(&mut ram, 1, &[i; 40]).unwrap(); // ~240 of 244 B, nearly full
        }
        let (_, g0) = active(&ram, CAP).unwrap().unwrap();
        del(&mut ram, 1).unwrap(); // no room for the 8-B tombstone → flips, then tombstones
        let (_, g1) = active(&ram, CAP).unwrap().unwrap();
        assert_eq!(g1, g0 + 1, "the delete drove exactly one flip");
        assert_eq!(get_vec(&ram, 1), None);
        // A second flip now reclaims the value+tombstone the first flip carried over.
        compact(&mut ram, CAP).unwrap();
        assert_eq!(get_vec(&ram, 1), None);
        assert_eq!(
            live_bytes(&ram, CAP).unwrap(),
            0,
            "store empty after the delete is reclaimed"
        );
    }

    #[test]
    fn tombstone_in_the_tail_supersedes_a_planned_value() {
        // A delete that lands AFTER flip_start's snapshot (in the source tail) must still win:
        // flip_commit copies the tail tombstone in after the planned value, so latest-wins holds.
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, b"planned").unwrap();
        set(&mut ram, 2, b"keep").unwrap();
        let mut st = flip_start(&ram, CAP).unwrap().unwrap();
        flip_step(&mut ram, CAP, &mut st, u32::MAX).unwrap(); // copies the planned live set
        del(&mut ram, 1).unwrap(); // tombstone appended to the source tail, past the snapshot
        flip_commit(&mut ram, CAP, &mut st).unwrap(); // tail catch-up carries the tombstone over
        assert_eq!(get_vec(&ram, 1), None, "tail tombstone won");
        assert_eq!(get_vec(&ram, 2).as_deref(), Some(&b"keep"[..]));
        // The carried value+tombstone are dead weight the NEXT flip reclaims.
        compact(&mut ram, CAP).unwrap();
        assert_eq!(get_vec(&ram, 1), None);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), KV_HEADER as u32 + 4);
    }

    // --- leftovers past the log end (found by the `kv_store` fuzz target) -----------------------

    /// `inner` as a complete, CRC-valid record for `key`: apps store arbitrary bytes, so a value
    /// can contain one.
    fn embedded_record(key: u16, inner: &[u8]) -> Vec<u8> {
        let mut rec = key.to_le_bytes().to_vec();
        rec.extend_from_slice(&(inner.len() as u16).to_le_bytes());
        let crc = entry_crc(&rec, inner);
        rec.extend_from_slice(&crc.to_le_bytes());
        rec.extend_from_slice(inner);
        rec
    }

    /// The scan ends the log at the first unreadable record, and the next append lands there.
    /// When that record is torn and the new one is SHORTER, the torn record's bytes left past the
    /// new one used to be scanned on as records: a value embedding a valid record surfaced as a
    /// phantom key. Power loss mid-append, then the next boot's ordinary write, is the path.
    #[test]
    fn torn_tail_then_shorter_append_exposes_no_phantom() {
        let mut ram = Ram::new(CAP as usize);
        set(&mut ram, 1, b"alpha").unwrap();
        let (h, _) = active(&ram, CAP).unwrap().unwrap();
        let base = half_base(CAP, h);
        let (free, _) = scan_half(&ram, base, half_len(CAP), 1).unwrap();
        // The in-flight record for key 2 carries a valid record for key 3 at exactly the offset
        // where a 4-byte record ends; power is lost after that part landed (the last byte did not).
        let mut value = vec![0xAA; 4];
        value.extend_from_slice(&embedded_record(3, b"phantom"));
        value.extend_from_slice(&[0xBB; 4]);
        let mut rec = vec![0u8; KV_HEADER + value.len()];
        rec[0..2].copy_from_slice(&2u16.to_le_bytes());
        rec[2..4].copy_from_slice(&(value.len() as u16).to_le_bytes());
        let crc = entry_crc(&rec[0..4], &value);
        rec[4..8].copy_from_slice(&crc.to_le_bytes());
        rec[8..].copy_from_slice(&value);
        ram.torn_write(base + free, &rec, rec.len() - 1);
        assert_eq!(get_vec(&ram, 2), None, "the torn record is not part of the log");
        // Reboot; an ordinary 4-byte write lands on the torn record's offset.
        set(&mut ram, 4, &[4, 4, 4, 4]).unwrap();
        assert_eq!(get_vec(&ram, 4).as_deref(), Some(&[4u8, 4, 4, 4][..]));
        assert_eq!(
            get_vec(&ram, 3),
            None,
            "leftover bytes of a torn record surfaced as a key"
        );
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"alpha"[..]));
    }

    /// The same hazard with no crafted content: a bit flip makes a mid-log record unreadable (the
    /// scan ends there, the later records drop out of view), then the next same-sized append
    /// lands exactly over it. The scan used to walk on into the intact records past it, so a
    /// superseded value resurfaced OVER the one just written: for the TX-counter watermark, a
    /// rollback, i.e. CCM nonce reuse.
    #[test]
    fn corrupt_mid_log_record_never_resurrects_the_tail() {
        let mut ram = Ram::new(CAP as usize);
        for v in [100u32, 200, 300] {
            set(&mut ram, 1, &v.to_le_bytes()).unwrap();
        }
        let (h, _) = active(&ram, CAP).unwrap().unwrap();
        let rec = KV_HEADER + 4;
        ram.buf[half_base(CAP, h) as usize + rec + KV_HEADER] ^= 0x01; // the 200 record fails its CRC
        assert_eq!(
            get_vec(&ram, 1).unwrap(),
            100u32.to_le_bytes(),
            "the log ends at the bad record"
        );
        set(&mut ram, 1, &1124u32.to_le_bytes()).unwrap(); // lands exactly over the bad record
        assert_eq!(
            get_vec(&ram, 1).unwrap(),
            1124u32.to_le_bytes(),
            "a superseded value resurfaced over the one just written"
        );
    }

    // --- factory reset (wipe) -------------------------------------------------------------------

    /// A [`Ram`] that loses power at write call `cut` (0-based): that call lands only its first
    /// `torn` bytes and fails, and every later call fails without programming anything; the
    /// device is dead until the "reboot" (the test reads the surviving buffer back).
    struct CutRam {
        ram: Ram,
        calls: usize,
        cut: usize,
        torn: usize,
    }
    impl Eeprom for CutRam {
        type Error = ();
        fn read(&self, off: u32, buf: &mut [u8]) -> Result<(), ()> {
            self.ram.read(off, buf)
        }
        fn write(&mut self, off: u32, data: &[u8]) -> Result<(), ()> {
            let n = self.calls;
            self.calls += 1;
            if n < self.cut {
                self.ram.write(off, data)
            } else {
                if n == self.cut {
                    self.ram.torn_write(off, data, self.torn);
                }
                Err(())
            }
        }
    }

    /// Fixture keys after the store was wiped: all absent, whatever was written before.
    fn assert_fixture_gone(ram: &Ram) {
        for k in [1, 2, 3] {
            assert_eq!(get_vec(ram, k), None, "wiped key {k} resurrected");
        }
    }

    #[test]
    fn wipe_empties_scrubs_and_keeps_the_generation() {
        let mut ram = flip_fixture(); // dead half dirty, both superblocks valid
        let (h0, g0) = active(&ram, CAP).unwrap().unwrap();
        wipe(&mut ram, CAP).unwrap();
        let (h1, g1) = active(&ram, CAP).unwrap().unwrap();
        assert_ne!(h0, h1, "the wipe is a flip");
        assert_eq!(g1, g0 + 1, "the wear gauge keeps counting (no reset to 0)");
        assert_fixture_gone(&ram);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 0);
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half_len(CAP));
        // Both record areas are physically zero: no wiped value left readable.
        assert!(
            ram.buf[..2 * half_len(CAP) as usize].iter().all(|&b| b == 0),
            "record areas must be scrubbed"
        );
        // And the store keeps working (flips included) without ever resurfacing old keys.
        for i in 0..200u32 {
            set(&mut ram, 9, &i.to_le_bytes()).unwrap();
            assert_fixture_gone(&ram);
        }
        assert_eq!(get_vec(&ram, 9).unwrap(), 199u32.to_le_bytes());
    }

    #[test]
    fn wipe_of_an_uninitialized_region() {
        // Bytes in another layout (no superblock): the wipe commits an empty store and scrubs them.
        let mut ram = Ram::new(CAP as usize);
        let o = ram.raw_put(0, 1, b"foreign-one");
        let _ = ram.raw_put(o, 2, b"foreign-two");
        wipe(&mut ram, CAP).unwrap();
        assert_eq!(active(&ram, CAP).unwrap(), Some((1, 1)));
        assert_fixture_gone(&ram);
        // Everything except the committed superblock (slot 1) is zero.
        let s1 = super_off(CAP, 1) as usize;
        assert!(
            ram.buf[..s1].iter().all(|&b| b == 0),
            "foreign residue must be scrubbed"
        );
        set(&mut ram, 1, b"new").unwrap();
        assert_eq!(get_vec(&ram, 1).as_deref(), Some(&b"new"[..]));
    }

    /// The finding this guards against: an interrupted reset must never leave a half whose
    /// prefix reads empty while its tail survives (new appends would then re-expose the tail).
    /// Cut power at EVERY write call of the wipe (with the call landing none, part, or all-but-
    /// one of its bytes), reboot, and require either the complete old store or an empty one;
    /// then churn the rebooted store through several flips and require that no wiped key ever
    /// reappears (and that an un-wiped store keeps every value).
    #[test]
    fn wipe_is_power_loss_safe_at_every_write() {
        for preblanked in [false, true] {
            let fixture = || {
                let mut ram = flip_fixture();
                if preblanked {
                    let mut cur = BlankCursor::new();
                    while blank_dead_step(&mut ram, CAP, &mut cur, 4).unwrap() {}
                }
                ram
            };
            // Probe: the number of write calls a clean wipe makes.
            let mut probe = CutRam {
                ram: fixture(),
                calls: 0,
                cut: usize::MAX,
                torn: 0,
            };
            wipe(&mut probe, CAP).unwrap();
            let total = probe.calls;
            assert!(total > 2, "wipe must program the old half + the superblock");

            let (_, g0) = active(&fixture(), CAP).unwrap().unwrap();
            let mut saw_old = false;
            let mut saw_empty = false;
            for cut in 0..=total {
                for torn in [0usize, 1, 2, 3, 6, 11] {
                    let mut c = CutRam {
                        ram: fixture(),
                        calls: 0,
                        cut,
                        torn,
                    };
                    let r = wipe(&mut c, CAP);
                    assert_eq!(r.is_ok(), cut >= total, "cut={cut} torn={torn}");
                    let mut ram = c.ram; // ← the reboot: only what reached the EEPROM survives
                    let wiped = match generation(&ram, CAP).unwrap() {
                        g if g == g0 => {
                            assert_fixture_live(&ram);
                            saw_old = true;
                            false
                        }
                        g if g == g0 + 1 => {
                            assert_fixture_gone(&ram);
                            saw_empty = true;
                            true
                        }
                        g => panic!("unexpected generation {g} (cut={cut} torn={torn})"),
                    };
                    // Life goes on: churn through several flips, checking every step.
                    for i in 0..120u32 {
                        set(&mut ram, 9, &i.to_le_bytes()).unwrap();
                        if wiped {
                            assert_fixture_gone(&ram);
                        } else {
                            assert_fixture_live(&ram);
                        }
                    }
                    assert!(
                        generation(&ram, CAP).unwrap() > g0 + 1,
                        "churn must have flipped (cut={cut} torn={torn})"
                    );
                }
            }
            assert!(saw_old && saw_empty, "the sweep must cover both outcomes");
        }
    }

    // --- whole words: the word-granular tear model ---------------------------------------------
    //
    // The STM32L0 data EEPROM programs and ECC-protects per aligned 32-bit word, and the firmware's
    // backend splits an unaligned write into byte programs, each an erase + program of its whole
    // word ([`Ram`]). With the unpadded `TKV1` layout an append's first byte programs rewrote the
    // word holding the previous record's tail, so a cut there corrupted a COMMITTED record. These
    // tests cut power inside every single program of the real code paths and require that the
    // codec only ever programs whole aligned words, and that no cut reaches committed data.

    const TEARS: [Tear; 3] = [Tear::Untouched, Tear::Erased, Tear::Garbled];

    /// The provisioning record of the radio apps (`NS_APP`), postcard-encoded: odd lengths.
    const PROV: u16 = 0x6001;
    /// The radio TX-counter watermark (`NS_NET`): a u32, the CCM nonce's anti-reuse guarantee.
    const WM: u16 = 0x5200;
    /// The per-boot session counter (`NS_SYS`): one small append per boot.
    const SESSION: u16 = 0x5300;
    const CAP6K: u32 = 6144;

    fn get_at(ram: &Ram, cap: u32, key: u16) -> Option<Vec<u8>> {
        let mut out = [0u8; MAX_VALUE];
        get_bytes(ram, cap, key, &mut out)
            .unwrap()
            .map(|n| out[..n].to_vec())
    }

    #[test]
    fn layout_is_whole_words_for_any_capacity() {
        for cap in [64u32, 512, 513, 514, 515, 1000, 1002, 6143, 6144, 6146] {
            let half = half_len(cap);
            assert_eq!(half % WORD, 0, "cap {cap}: half length");
            for h in 0..2 {
                assert_eq!(half_base(cap, h) % WORD, 0, "cap {cap}: half {h} base");
                assert_eq!(super_off(cap, h) % WORD, 0, "cap {cap}: superblock {h}");
            }
            assert!(
                2 * half <= super_off(cap, 0),
                "cap {cap}: halves overlap a superblock"
            );
            assert!(
                super_off(cap, 1) + SUPER_LEN <= cap,
                "cap {cap}: superblock past the top"
            );
        }
        for len in 0..=MAX_VALUE {
            assert_eq!(record_size(len) % WORD, 0);
            assert!(record_size(len) as usize - KV_HEADER - len < WORD as usize);
        }
        assert_eq!(record_size(MAX_VALUE), (KV_HEADER + MAX_VALUE) as u32);
    }

    /// Every write the codec issues, over every path (all value lengths, tombstones, in-line and
    /// incremental flips with appends during them, maintenance, compaction, wipe, the format of a
    /// foreign region), is whole words at a word-aligned offset: the HAL never byte-programs.
    #[test]
    fn every_codec_write_is_whole_aligned_words() {
        for cap in [CAP, CAP6K] {
            let mut ram = foreign_region_at(cap);
            init(&mut ram, cap).unwrap();
            let mut m = MaintState::new();
            // Five keys of up to `max` bytes always fit a half (the 512 B one: 24 B values).
            let max = if cap == CAP { 24 } else { MAX_VALUE };
            for len in 0..=max {
                let key = 1 + (len % 5) as u16;
                set_bytes_with(&mut ram, cap, key, &vec![len as u8 | 1; len], m.pending()).unwrap();
                if len % 7 == 0 {
                    delete_with(&mut ram, cap, key, m.pending()).unwrap();
                }
                if len % 3 == 0 {
                    maintain(&mut ram, cap, &mut m, 3, 64).unwrap();
                }
                if len % 50 == 0 {
                    compact_with(&mut ram, cap, m.pending()).unwrap();
                }
            }
            while maintain(&mut ram, cap, &mut m, 5, 64).unwrap() {}
            wipe(&mut ram, cap).unwrap();
            set_bytes(&mut ram, cap, 1, b"after").unwrap();
            assert!(
                generation(&ram, cap).unwrap() > 3,
                "the workload must have flipped"
            );
            assert_eq!(
                ram.unaligned, 0,
                "cap {cap}: a codec write was not whole aligned words"
            );
        }
    }

    /// The finding, as the review reproduced it (the 6 KiB firmware region): boot N committed the
    /// TX watermark 2049 after a 23-byte provisioning record; on boot N+1 the session-counter
    /// append is cut in its very first program. In the unpadded layout that program rewrote the
    /// word holding the watermark's last byte, the watermark failed its CRC and the store read the
    /// superseded 1025 back: the next boot resumed below counters already sent (CCM nonce reuse).
    #[test]
    fn word_tear_never_rolls_back_the_tx_watermark() {
        for tear in TEARS {
            let mut ram = Ram::new(CAP6K as usize);
            init(&mut ram, CAP6K).unwrap();
            set_bytes(&mut ram, CAP6K, PROV, &[0x5A; 15]).unwrap();
            set_bytes(&mut ram, CAP6K, WM, &1025u32.to_le_bytes()).unwrap(); // boot N-1
            set_bytes(&mut ram, CAP6K, WM, &2049u32.to_le_bytes()).unwrap(); // boot N
            ram.arm(0, tear); // boot N+1: the session append dies in its first program
            assert!(set_bytes(&mut ram, CAP6K, SESSION, &7u32.to_le_bytes()).is_err());
            ram.revive();
            init(&mut ram, CAP6K).unwrap();
            assert_eq!(
                get_at(&ram, CAP6K, WM).unwrap(),
                2049u32.to_le_bytes(),
                "a torn append of ANOTHER key rolled the committed TX watermark back ({tear:?})"
            );
            assert_eq!(get_at(&ram, CAP6K, PROV).unwrap(), [0x5A; 15]);
        }
    }

    /// The other face of the finding: a flip packs the live set back to back, so the first append
    /// after it shared a word with the last packed record, and a cut there lost that key outright
    /// (here the pairing/provisioning record).
    #[test]
    fn word_tear_after_flip_never_loses_an_unrelated_key() {
        for tear in TEARS {
            let mut ram = Ram::new(CAP6K as usize);
            init(&mut ram, CAP6K).unwrap();
            set_bytes(&mut ram, CAP6K, WM, &5121u32.to_le_bytes()).unwrap();
            set_bytes(&mut ram, CAP6K, PROV, &[0x5A; 14]).unwrap();
            compact(&mut ram, CAP6K).unwrap();
            ram.arm(0, tear);
            assert!(set_bytes(&mut ram, CAP6K, SESSION, &1u32.to_le_bytes()).is_err());
            ram.revive();
            init(&mut ram, CAP6K).unwrap();
            assert_eq!(
                get_at(&ram, CAP6K, PROV),
                Some(vec![0x5A; 14]),
                "pairing record lost"
            );
            assert_eq!(get_at(&ram, CAP6K, WM).unwrap(), 5121u32.to_le_bytes());
        }
    }

    /// A cut anywhere in the append of key B never changes key A, whatever offset A's record
    /// ended at in the unpadded layout: A (the TX watermark, an older value of it earlier in the
    /// log) follows a provisioning record of 12 to 15 bytes and is itself 1 to 8 bytes long, so
    /// its record ended at every offset mod 4. B's append is cut at each of its programs (the
    /// log-end terminator, then the record) with the word erased or garbled.
    #[test]
    fn torn_append_never_changes_an_earlier_key() {
        let mut cuts = 0;
        for plen in 12..=15usize {
            for alen in 1..=8usize {
                let build = || {
                    let mut ram = Ram::new(CAP as usize);
                    init(&mut ram, CAP).unwrap();
                    set(&mut ram, PROV, &vec![0x5A; plen]).unwrap();
                    set(&mut ram, WM, &vec![0x11; alen]).unwrap(); // the superseded watermark
                    set(&mut ram, WM, &vec![0x22; alen]).unwrap(); // the committed one
                    ram
                };
                let mut probe = build();
                let p0 = probe.programs;
                set(&mut probe, SESSION, &7u32.to_le_bytes()).unwrap();
                let programs = probe.programs - p0;
                assert!(programs >= 3, "B's record is three words");
                for cut in 0..programs {
                    for tear in TEARS {
                        let what = format!("P {plen} B, A {alen} B, cut {cut} ({tear:?})");
                        let mut ram = build();
                        ram.arm(cut, tear);
                        assert!(set(&mut ram, SESSION, &7u32.to_le_bytes()).is_err(), "{what}");
                        ram.revive();
                        init(&mut ram, CAP).unwrap();
                        assert_eq!(get_vec(&ram, WM), Some(vec![0x22; alen]), "{what}: A changed");
                        assert_eq!(get_vec(&ram, PROV), Some(vec![0x5A; plen]), "{what}: P changed");
                        let b = get_vec(&ram, SESSION);
                        assert!(b.is_none() || b == Some(7u32.to_le_bytes().to_vec()), "{what}: B");
                        // Life goes on: the retried append lands, and still changes nothing else.
                        set(&mut ram, SESSION, &8u32.to_le_bytes()).unwrap();
                        assert_eq!(get_vec(&ram, SESSION).unwrap(), 8u32.to_le_bytes(), "{what}");
                        assert_eq!(get_vec(&ram, WM), Some(vec![0x22; alen]), "{what}: A changed");
                        assert_eq!(get_vec(&ram, PROV), Some(vec![0x5A; plen]), "{what}: P changed");
                        cuts += 1;
                    }
                }
            }
        }
        assert!(cuts >= 4 * 8 * 3 * 2);
    }

    // --- the exhaustive cut sweep ----------------------------------------------------------------

    type View = std::collections::BTreeMap<u16, Vec<u8>>;

    /// One operation of a sweep scenario, over the real public API.
    #[derive(Clone, Debug)]
    enum Step {
        Set(u16, Vec<u8>),
        SetWith(u16, Vec<u8>),
        Delete(u16),
        DeleteWith(u16),
        /// Begin an incremental flip in the shared [`MaintState`] slot.
        FlipStart,
        /// One [`flip_step`] slice of the pending flip (none pending, or stale: dropped, no-op).
        FlipSlice(u32),
        /// [`flip_commit`] the pending flip (none pending, or stale: no-op).
        FlipCommit,
        Compact,
        CompactWith,
        /// One [`maintain`] slice.
        Maintain {
            budget: u32,
            threshold: u32,
        },
        Wipe,
        Init,
    }

    impl Step {
        /// The content after the step completes, from the content before it.
        fn effect(&self, before: &View) -> View {
            let mut v = before.clone();
            match self {
                Step::Set(k, val) | Step::SetWith(k, val) if !val.is_empty() => {
                    v.insert(*k, val.clone());
                }
                Step::Set(k, _) | Step::SetWith(k, _) | Step::Delete(k) | Step::DeleteWith(k) => {
                    v.remove(k);
                }
                Step::Wipe => v.clear(),
                _ => {}
            }
            v
        }

        fn run(&self, ram: &mut Ram, cap: u32, m: &mut MaintState) -> Result<(), KvError<()>> {
            match self {
                Step::Set(k, v) => set_bytes(ram, cap, *k, v),
                Step::SetWith(k, v) => set_bytes_with(ram, cap, *k, v, m.pending()),
                Step::Delete(k) => delete(ram, cap, *k),
                Step::DeleteWith(k) => delete_with(ram, cap, *k, m.pending()),
                Step::FlipStart => {
                    m.flip = flip_start(ram, cap)?;
                    Ok(())
                }
                Step::FlipSlice(budget) => match m.flip.as_mut().map(|st| flip_step(ram, cap, st, *budget)) {
                    Some(Err(KvError::Stale)) => {
                        m.flip = None;
                        Ok(())
                    }
                    Some(r) => r.map(|_| ()),
                    None => Ok(()),
                },
                Step::FlipCommit => match m.flip.take().map(|mut st| flip_commit(ram, cap, &mut st)) {
                    Some(Err(KvError::Stale)) | None => Ok(()),
                    Some(r) => r,
                },
                Step::Compact => compact(ram, cap),
                Step::CompactWith => compact_with(ram, cap, m.pending()),
                Step::Maintain { budget, threshold } => {
                    maintain(ram, cap, m, *budget, *threshold).map(|_| ())
                }
                Step::Wipe => wipe(ram, cap),
                Step::Init => init(ram, cap),
            }
        }
    }

    /// Run `step` after a reboot, where the content may differ from the scenario's clean run (a
    /// cut left the state before an earlier step): it must succeed, or, for a write, fail `Full`
    /// with nothing changed when the live set plus its record really does not fit a half.
    /// Returns the content it must leave (the caller checks it).
    fn run_after(
        step: &Step,
        ram: &mut Ram,
        cap: u32,
        m: &mut MaintState,
        watch: &[u16],
        model: View,
        what: &str,
    ) -> View {
        let needed = match step {
            Step::Set(_, v) | Step::SetWith(_, v) => record_size(v.len()),
            _ => KV_HEADER as u32,
        };
        match step.run(ram, cap, m) {
            Ok(()) => step.effect(&model),
            Err(KvError::Full) => {
                let live: u32 = model.values().map(|v| record_size(v.len())).sum();
                assert!(
                    live + needed > half_len(cap),
                    "{what}: {step:?} Full, but it fits"
                );
                assert_holds(ram, cap, watch, &model, || format!("{what}: {step:?} refused"));
                model
            }
            Err(e) => panic!("{what}: {step:?} failed: {e:?}"),
        }
    }

    fn view_of(ram: &Ram, cap: u32, watch: &[u16]) -> View {
        watch
            .iter()
            .filter_map(|&k| get_at(ram, cap, k).map(|v| (k, v)))
            .collect()
    }

    /// The content reads exactly `model`: every watched key, and (through `live_bytes`, which
    /// sums the live set of EVERY tag) no phantom key outside the watch list either. `what` names
    /// the check (built only on a failure: the sweeps run this hundreds of thousands of times).
    fn assert_holds(ram: &Ram, cap: u32, watch: &[u16], model: &View, what: impl Fn() -> String) {
        let got = view_of(ram, cap, watch);
        assert!(&got == model, "{}:\n got  {got:?}\n want {model:?}", what());
        let live: u32 = model.values().map(|v| record_size(v.len())).sum();
        let read = live_bytes(ram, cap).unwrap();
        assert!(
            read == live,
            "{}: {read} B live, want {live} B (a phantom key)",
            what()
        );
    }

    /// What one sweep covered.
    #[derive(Debug, Default)]
    struct Swept {
        /// Cut points (program index x tear mode).
        cuts: usize,
        /// Cuts that left the state before the interrupted step / after it (a step that changes
        /// no content counts as neither). An append's record is its last write, so a cut inside
        /// it always leaves the state before; "after" needs writes past the commit (a wipe's
        /// scrub).
        before: usize,
        after: usize,
        /// The scenario's programs on a clean run, and the flips it committed.
        programs: usize,
        flips: u32,
    }

    /// Cut the power inside EVERY program the scenario `steps` makes (from the store `setup`
    /// builds), with the word erased and garbled, then reboot (`init`, RAM state lost) and check:
    /// the content is exactly the one before the interrupted step or the one after it (every key
    /// at once: no unrelated key changes, nothing resurfaces, no phantom), `init` keeps it, and
    /// life goes on: the rest of the scenario and then a churn of `CHURN` through several more
    /// flips each have exactly their effect. Also checks the scenario's clean run and that every
    /// codec write was whole aligned words.
    fn sweep(cap: u32, setup: &dyn Fn() -> Ram, steps: &[Step], watch: &[u16]) -> Swept {
        const CHURN: u16 = 0x7E;
        let mut watch = watch.to_vec();
        watch.push(CHURN);
        let watch = &watch[..];
        let start = view_of(&setup(), cap, watch);

        // Clean run: the expected content before each step, the program count, the flips.
        let mut ram = setup();
        let (p0, g0) = (ram.programs, generation(&ram, cap).unwrap());
        let mut m = MaintState::new();
        let mut model = start.clone();
        for s in steps {
            s.run(&mut ram, cap, &mut m)
                .unwrap_or_else(|e| panic!("{s:?} failed on a clean run: {e:?}"));
            model = s.effect(&model);
            assert_holds(&ram, cap, watch, &model, || format!("{s:?} on a clean run"));
        }
        assert_eq!(ram.unaligned, 0, "a codec write was not whole aligned words");
        let mut swept = Swept {
            programs: ram.programs - p0,
            flips: generation(&ram, cap).unwrap().wrapping_sub(g0),
            ..Swept::default()
        };

        for cut in 0..swept.programs {
            for tear in TEARS {
                let mut ram = setup();
                ram.arm(cut, tear);
                let mut m = MaintState::new();
                let mut model = start.clone();
                let mut i = 0;
                let done = loop {
                    let s = &steps[i];
                    let done = s.effect(&model);
                    let r = s.run(&mut ram, cap, &mut m);
                    if ram.dead {
                        assert_eq!(r, Err(KvError::Backend(())), "a lost write must surface");
                        break done;
                    }
                    r.unwrap_or_else(|e| panic!("{s:?} failed before the cut: {e:?}"));
                    model = done;
                    i += 1;
                };
                let what = format!("cut at program {cut} ({tear:?}) in step {i} {:?}", steps[i]);

                // The reboot: only what reached the cells survives.
                ram.revive();
                let got = view_of(&ram, cap, watch);
                assert!(
                    got == model || got == done,
                    "{what}: neither the state before nor after it\n before {model:?}\n after  \
                     {done:?}\n got    {got:?}"
                );
                if model != done {
                    if got == model {
                        swept.before += 1;
                    } else {
                        swept.after += 1;
                    }
                }
                init(&mut ram, cap).unwrap();
                assert_holds(&ram, cap, watch, &got, || {
                    format!("{what}: init after the reboot")
                });

                // Life goes on: the rest of the scenario, then a churn through several flips.
                let mut model = got;
                let mut m = MaintState::new();
                for s in &steps[i + 1..] {
                    model = run_after(s, &mut ram, cap, &mut m, watch, model, &what);
                    assert_holds(&ram, cap, watch, &model, || format!("{what}: later {s:?}"));
                }
                let g = generation(&ram, cap).unwrap();
                for j in 0..48u16 {
                    let v: Vec<u8> = (0..1 + j % 9).map(|b| (j as u8) ^ (b as u8) | 0x80).collect();
                    model = run_after(
                        &Step::SetWith(CHURN, v),
                        &mut ram,
                        cap,
                        &mut m,
                        watch,
                        model,
                        &what,
                    );
                    if j % 4 == 3 {
                        assert_holds(&ram, cap, watch, &model, || format!("{what}: churn {j}"));
                        maintain(&mut ram, cap, &mut m, 4, 64).unwrap();
                    }
                }
                assert!(generation(&ram, cap).unwrap() > g, "{what}: the churn must flip");
                assert_holds(&ram, cap, watch, &model, || format!("{what}: after the churn"));
                swept.cuts += 1;
            }
        }
        println!("{swept:?}");
        swept
    }

    /// A value of `len` bytes, distinct per `seed`, never all zero.
    fn val(seed: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|j| (seed as u8).wrapping_mul(31).wrapping_add(j as u8) | 0x40)
            .collect()
    }

    fn blank_store() -> Ram {
        let mut ram = Ram::new(CAP as usize);
        init(&mut ram, CAP).unwrap();
        ram
    }

    /// `set_bytes` (appends of every length mod 4, and appends that fill the half and flip in
    /// line), `delete` and `delete_with` (tombstones, one of which fills the half and flips).
    #[test]
    fn word_tear_sweep_set_and_delete() {
        let lens = [15, 4, 4, 7, 13, 1, 2, 3, 9, 6, 5, 11];
        let mut steps = Vec::new();
        for i in 0..30 {
            let k = 1 + (i % 5) as u16;
            match i % 7 {
                5 => steps.push(Step::Delete(k)),
                6 => steps.push(Step::DeleteWith(k)),
                _ => steps.push(Step::Set(k, val(i, lens[i % lens.len()]))),
            }
        }
        // Then fill the half to within a tombstone of full with two values of key 6 (the first
        // superseded, so the flip frees room), and delete it: the tombstone has to flip to land.
        steps.push(Step::Compact);
        let mut probe = blank_store();
        let mut m = MaintState::new();
        for st in &steps {
            st.run(&mut probe, CAP, &mut m).unwrap();
        }
        let live = live_bytes(&probe, CAP).unwrap();
        let rec = (half_len(CAP) - live) / 2 / WORD * WORD; // live + 2 rec within 4 B of full
        let n = rec as usize - KV_HEADER - 3; // an odd length, padded to `rec`
        assert_eq!(record_size(n), rec);
        steps.push(Step::Set(6, val(90, n)));
        steps.push(Step::Set(6, val(91, n)));
        steps.push(Step::Delete(6));
        let s = sweep(CAP, &blank_store, &steps, &[1, 2, 3, 4, 5, 6]);
        assert!(s.flips >= 4, "{s:?}");
        assert!(s.before > 0, "{s:?}");
        // The last step really is a delete that flips: on a clean run it commits one.
        let mut ram = blank_store();
        let mut m = MaintState::new();
        for st in &steps[..steps.len() - 1] {
            st.run(&mut ram, CAP, &mut m).unwrap();
        }
        let g = generation(&ram, CAP).unwrap();
        delete(&mut ram, CAP, 6).unwrap();
        assert_eq!(
            generation(&ram, CAP).unwrap(),
            g + 1,
            "the tombstone must flip to land"
        );
    }

    /// The incremental flip by hand: `flip_start` / `flip_step` slices with `set_bytes_with`,
    /// `delete_with` and a plain `set_bytes` landing in the source tail meanwhile, `flip_commit`
    /// (tail catch-up + commit), a source that fills mid-flip (`set_bytes_with` finishes the
    /// pending flip, then has to re-flip it), `compact_with` on a pending flip, and `compact`.
    #[test]
    fn word_tear_sweep_incremental_flip() {
        let setup = || {
            let mut ram = blank_store();
            set(&mut ram, 1, &[0u8; 92]).unwrap();
            set(&mut ram, 1, &[1u8; 92]).unwrap(); // 200 of 244 B used, 100 B live
            ram
        };
        let steps = vec![
            // The `set_with_pending_flip_never_fails_where_plain_set_fits` path: finish + re-flip.
            Step::FlipStart,
            Step::SetWith(2, val(1, 12)),
            Step::SetWith(2, val(2, 12)),
            Step::SetWith(3, val(3, 100)),
            Step::Delete(3),
            Step::Compact,
            // Slices with appends of every kind in the source tail meanwhile, then the commit.
            Step::Set(4, val(4, 7)),
            Step::FlipStart,
            Step::FlipSlice(1),
            Step::FlipSlice(2),
            Step::SetWith(4, val(5, 5)),
            Step::FlipSlice(3),
            Step::DeleteWith(2),
            Step::Set(5, val(6, 13)),
            Step::FlipSlice(u32::MAX),
            Step::SetWith(5, val(7, 2)),
            Step::FlipCommit,
            // A source that fills while a flip is pending: the write finishes it.
            Step::FlipStart,
            Step::FlipSlice(2),
            Step::SetWith(1, val(8, 90)),
            Step::SetWith(2, val(9, 9)),
            // compact_with on a pending flip finishes that flip.
            Step::FlipStart,
            Step::FlipSlice(4),
            Step::CompactWith,
            Step::Delete(4),
            Step::Compact,
        ];
        let s = sweep(CAP, &setup, &steps, &[1, 2, 3, 4, 5]);
        assert!(s.flips >= 6, "{s:?}");
        assert!(s.before > 0, "{s:?}");
    }

    /// `maintain` as the firmware drives it (between writes, a few word programs per slice): the
    /// proactive incremental flip below the threshold, its commit slice, and pre-blanking.
    #[test]
    fn word_tear_sweep_maintain() {
        let mut steps = Vec::new();
        for i in 0..26 {
            steps.push(Step::SetWith(1 + (i % 4) as u16, val(i, [5, 15, 4, 10][i % 4])));
            for _ in 0..4 {
                steps.push(Step::Maintain {
                    budget: 4,
                    threshold: 64,
                });
            }
        }
        let s = sweep(CAP, &blank_store, &steps, &[1, 2, 3, 4]);
        assert!(s.flips >= 2, "{s:?}");
        assert!(s.before > 0, "{s:?}");
    }

    /// `wipe` over a dirty dead half and over a pre-blanked one, then writes after it: all or
    /// nothing at every cut.
    #[test]
    fn word_tear_sweep_wipe() {
        let steps = vec![
            Step::Wipe,
            Step::Set(1, val(1, 3)),
            Step::Set(2, val(2, 14)),
            Step::Compact,
            Step::Set(3, val(3, 5)),
            Step::Maintain {
                budget: 200,
                threshold: 0,
            },
            Step::Wipe,
            Step::Set(4, val(4, 1)),
        ];
        let s = sweep(CAP, &flip_fixture, &steps, &[1, 2, 3, 4]);
        assert!(s.before > 0 && s.after > 0, "{s:?}");
    }

    /// `init` (the format) over a blank part, a foreign region and an old-layout `TKV1` store,
    /// then the first writes: a cut anywhere leaves it unformatted or formatted empty, and the
    /// old bytes never surface as keys (`assert_holds` also counts every live tag).
    #[test]
    fn word_tear_sweep_init() {
        let steps = vec![
            Step::Init,
            Step::Set(1, val(1, 3)),
            Step::Set(0x5201, val(2, 4)),
            Step::Set(2, val(3, 9)),
        ];
        let watch = [1, 2, 0x5201, 0x5202, 0x5300];
        let setups: [&dyn Fn() -> Ram; 3] = [&|| Ram::new(CAP as usize), &|| foreign_region_at(CAP), &|| {
            tkv1_store(CAP)
        }];
        for setup in setups {
            assert!(
                active(&setup(), CAP).unwrap().is_none(),
                "the setup must be unformatted"
            );
            let s = sweep(CAP, setup, &steps, &watch);
            assert!(s.cuts > 0 && s.before > 0, "{s:?}");
        }
    }

    /// A store in the unpadded `TKV1` layout (valid magic and CRC of its own): another layout, so
    /// `init` formats it empty instead of misreading its unpadded records.
    fn tkv1_store(cap: u32) -> Ram {
        let mut ram = foreign_region_at(cap);
        let mut sb = [0u8; SUPER_LEN as usize];
        sb[0..4].copy_from_slice(b"TKV1");
        sb[4..8].copy_from_slice(&5u32.to_le_bytes());
        let crc = super_crc(&sb[0..8]);
        sb[8..12].copy_from_slice(&crc.to_le_bytes());
        let at = super_off(cap, 0) as usize;
        ram.buf[at..at + SUPER_LEN as usize].copy_from_slice(&sb);
        ram
    }

    /// The first append after a format lands where the old layout's first record still sits. If
    /// it has the same tag and length (the same key written first again, e.g. after a firmware
    /// update over a `TKV1` store), a cut right after the header word lands leaves the old CRC and
    /// value behind it: the old record would read back, a value the formatted store never held.
    /// The header word is therefore written last.
    #[test]
    fn torn_first_append_never_adopts_an_old_record() {
        for old in [foreign_region_at(CAP), tkv1_store(CAP)] {
            let build = || {
                let mut ram = Ram::new(CAP as usize);
                ram.buf.copy_from_slice(&old.buf);
                init(&mut ram, CAP).unwrap();
                ram
            };
            let mut probe = build();
            let p0 = probe.programs;
            set(&mut probe, 0x5201, &9u32.to_le_bytes()).unwrap(); // the old record: 7u32
            for cut in 0..probe.programs - p0 {
                for tear in TEARS {
                    let mut ram = build();
                    ram.arm(cut, tear);
                    assert!(set(&mut ram, 0x5201, &9u32.to_le_bytes()).is_err());
                    ram.revive();
                    init(&mut ram, CAP).unwrap();
                    let got = get_vec(&ram, 0x5201);
                    assert!(
                        got.is_none() || got == Some(9u32.to_le_bytes().to_vec()),
                        "cut {cut} ({tear:?}): the old record read back: {got:?}"
                    );
                    assert_eq!(
                        live_bytes(&ram, CAP).unwrap(),
                        got.map_or(0, |v| record_size(v.len()))
                    );
                }
            }
        }
    }

    /// Found by the `kv_store` fuzz target once it tore whole words: a bit flip ends the log at
    /// record A (its CRC fails) with record B intact past it. The next append's terminator lands
    /// inside A, and a torn word there can hold any bytes, A's original ones included: that
    /// revived A, and B with it, values the store no longer held. The log end's header word is
    /// therefore cleared before anything is written into the record it begins.
    #[test]
    fn torn_word_in_a_corrupt_log_end_never_revives_it() {
        let build = || {
            let mut ram = blank_store();
            set(&mut ram, 1, &[0x11; 40]).unwrap();
            set(&mut ram, 2, &[0x22; 8]).unwrap();
            ram.buf[KV_HEADER + 2] ^= 0x80; // half 0, A's value (word 8): A fails its CRC
            ram
        };
        assert_eq!((get_vec(&build(), 1), get_vec(&build(), 2)), (None, None));
        // A tombstone (8 B) for another key: its terminator is the word holding the bad byte.
        let mut probe = build();
        let p0 = probe.programs;
        set(&mut probe, 3, &[]).unwrap();
        let into_a = p0 + probe.trace[p0..].iter().position(|&w| w == KV_HEADER).unwrap();
        for cut in 0..probe.programs - p0 {
            for tear in TEARS {
                let mut ram = build();
                ram.arm(cut, tear);
                assert!(set(&mut ram, 3, &[]).is_err());
                ram.revive();
                if p0 + cut >= into_a {
                    // The worst a torn (or later) program of that word can leave: A's own bytes.
                    ram.buf[KV_HEADER..KV_HEADER + 4].copy_from_slice(&[0x11; 4]);
                }
                init(&mut ram, CAP).unwrap();
                let what = format!("cut {cut} ({tear:?})");
                assert_eq!(get_vec(&ram, 1), None, "{what}: A revived");
                assert_eq!(get_vec(&ram, 2), None, "{what}: B revived");
                assert_eq!(live_bytes(&ram, CAP).unwrap(), 0, "{what}");
            }
        }
    }

    #[test]
    fn init_formats_an_old_layout_store_empty() {
        let mut ram = tkv1_store(CAP);
        assert!(
            active(&ram, CAP).unwrap().is_none(),
            "a TKV1 superblock is not ours"
        );
        init(&mut ram, CAP).unwrap();
        assert_eq!(active(&ram, CAP).unwrap(), Some((0, 1)));
        for key in [0x5201, 0x5202, 0x5300] {
            assert_eq!(get_vec(&ram, key), None, "key {key:#x} adopted");
        }
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 0);
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half_len(CAP));
    }

    // --- wear -------------------------------------------------------------------------------------

    /// Erase/write cycles of the most-cycled word over a run, against the flips it committed: the
    /// figure the firmware's `FLIP_BUDGET` rests on. Each word of a half is erased once per pass
    /// through that half (blanked before the half is written again) and programmed once on the
    /// erased word; each superblock once per flip into its half. The halves alternate, so no word
    /// may cycle more than once every other flip: at most `ceil(flips / 2)`, i.e. 0.5 cycle per
    /// flip (the pre-blanking runs right after a flip, one flip ahead of the next pass, hence the
    /// rounding up). The unpadded layout cycled its record-boundary words ~2 times per flip. The
    /// workload is the SDK's (docs/storage.md, "What the SDK writes"): an odd-length provisioning
    /// record, the u32 TX watermark, 12-byte replay lanes, the per-boot session counter and a
    /// 7-byte setting, with and without the background maintenance task (in-line flips only).
    #[test]
    fn wear_is_one_cycle_per_word_every_other_flip() {
        for background in [true, false] {
            let mut ram = Ram::new(CAP6K as usize);
            init(&mut ram, CAP6K).unwrap();
            let mut m = MaintState::new();
            set_bytes(&mut ram, CAP6K, PROV, &[0x5A; 15]).unwrap();
            for i in 0..9000u32 {
                let (key, v) = match i % 8 {
                    0 | 4 => (WM, i.to_le_bytes().to_vec()),
                    1 | 3 | 5 => (0x5210 + (i % 3) as u16, val(i as usize, 12)),
                    2 | 6 => (SESSION, (i / 8).to_le_bytes().to_vec()),
                    _ => (0x5501, val(i as usize, 7)),
                };
                set_bytes_with(&mut ram, CAP6K, key, &v, m.pending()).unwrap();
                if background {
                    while maintain(&mut ram, CAP6K, &mut m, 4, FLIP_THRESHOLD).unwrap() {}
                }
            }
            let flips = generation(&ram, CAP6K).unwrap() - 1;
            let worst = *ram.cycles.iter().max().unwrap();
            let at = ram.cycles.iter().position(|&c| c == worst).unwrap() * 4;
            println!(
                "background maintenance {background}: {flips} flips, worst word (byte {at}) {worst} \
                 cycles = {:.3} per flip",
                worst as f64 / flips as f64
            );
            assert!(flips >= 30, "the workload must flip often enough to measure");
            assert!(
                2 * worst <= flips + 1,
                "a word cycled {worst} times in {flips} flips: more than once every other flip"
            );
            assert_eq!(ram.unaligned, 0);
        }
    }
}
