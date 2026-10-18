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
//!   *tail* record, so `scan` stops there and every committed record before it survives.
//! - **Compaction is a flip:** blank the inactive half, write the live set packed into it, then
//!   write *its* superblock at `generation + 1`. That single CRC'd superblock write is the atomic
//!   commit: torn before it, the old half is still active (no data lost); torn during it, the
//!   superblock fails its CRC and the old half still wins.
//!
//! A record inside a half is `[tag(2) | len(2) | crc(4) | value]`, little-endian, CRC over
//! `tag‖len‖value`. A region that holds no valid superblock (a blank part, or bytes in any other
//! layout) is formatted empty by [`init`]; there is no migration of other layouts.
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
/// Record header: tag(2) + value-len(2) + crc32(4). Value bytes follow.
pub const KV_HEADER: usize = 8;

/// Superblock magic: with the CRC, what tells an initialized double-buffered region from a blank
/// part or bytes in any other layout (formatted empty by [`init`]).
const SUPER_MAGIC: [u8; 4] = *b"TKV1";
/// Superblock size: magic(4) + generation(4 LE) + crc(4 LE, over magic‖generation).
const SUPER_LEN: u32 = 12;

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

/// Record-area size of one half, given the total region `capacity`. The two superblocks sit in the
/// top `2 * SUPER_LEN` bytes; the rest splits evenly into the two halves.
fn half_len(capacity: u32) -> u32 {
    capacity.saturating_sub(2 * SUPER_LEN) / 2
}

/// Record-area base offset of half `h` (0 or 1).
fn half_base(capacity: u32, h: u8) -> u32 {
    h as u32 * half_len(capacity)
}

/// Offset of half `h`'s superblock (top of the region).
fn super_off(capacity: u32, h: u8) -> u32 {
    capacity - 2 * SUPER_LEN + h as u32 * SUPER_LEN
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
/// (tail) append would sit.
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
        if len as usize > MAX_VALUE || o + KV_HEADER as u32 + len as u32 > area {
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
        o += KV_HEADER as u32 + len as u32;
    }
    Ok((o, found))
}

/// Append one `[tag|len|crc|value]` record at absolute offset `at`.
fn append<S: Eeprom>(store: &mut S, at: u32, key: u16, value: &[u8]) -> Result<(), KvError<S::Error>> {
    let len = value.len() as u16;
    let mut buf = [0u8; KV_HEADER + MAX_VALUE];
    buf[0..2].copy_from_slice(&key.to_le_bytes());
    buf[2..4].copy_from_slice(&len.to_le_bytes());
    let crc = entry_crc(&buf[0..4], value);
    buf[4..8].copy_from_slice(&crc.to_le_bytes());
    buf[8..8 + value.len()].copy_from_slice(value);
    store
        .write(at, &buf[..KV_HEADER + value.len()])
        .map_err(KvError::Backend)
}

/// Append one record at the log end `free` (relative) of the half at `base` with record area
/// `area`, first making sure the scan will **stop right after it**.
///
/// The scan ends the log at the first blank or unreadable record, and appends land exactly there.
/// In a healthy half that spot is followed by blank cells, but not after a torn append (power
/// loss: only a prefix landed) or a corrupted mid-log record: there, a new record SHORTER than the
/// bad one leaves the bad one's remaining bytes right behind it, and a same-sized one lands
/// exactly on the intact records that followed it. Either way the next scan walked on past the new
/// record into those leftovers: a superseded value resurrected over the one just written (a
/// TX-counter watermark rollback, i.e. CCM nonce reuse), or bytes from inside a value surfaced as a
/// phantom key. So the tag slot just past the new record is zeroed FIRST (read-first: a 2-byte read
/// and no write in the normal, already-blank case). Power loss between the two writes is harmless:
/// the zeroed bytes lie beyond the log end, and a torn append is cut at its CRC as before. By
/// induction, every completed append is followed by a scan stop. Where no header fits after the
/// record the scan stops on its own, so there is nothing to zero.
fn append_end<S: Eeprom>(
    store: &mut S,
    base: u32,
    area: u32,
    free: u32,
    key: u16,
    value: &[u8],
) -> Result<(), KvError<S::Error>> {
    let next = free + KV_HEADER as u32 + value.len() as u32;
    if next + KV_HEADER as u32 <= area {
        let mut tag = [0u8; 2];
        store.read(base + next, &mut tag).map_err(KvError::Backend)?;
        if tag != [0, 0] {
            store.write(base + next, &[0, 0]).map_err(KvError::Backend)?;
        }
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
/// [`blank`] (unbounded) and [`blank_dead_step`]/[`flip_step`] (budgeted).
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
/// The format is two writes: the tag slot at the head of half 0 is cleared, so its record scan
/// stops at offset 0 (an empty log), then half 0's superblock is committed at generation 1. The
/// rest of the region is left as it is (no seconds of NVM stall on the first boot): whatever
/// sits further in half 0 can never be read as records, because every append first clears the
/// tag slot just past the record it writes (`append_end`), so the scan always stops at the log
/// end, and a flip blanks its destination half before writing it. Power loss before the commit
/// leaves the region unformatted, and the next `init` formats it again; a torn commit fails its
/// CRC the same way.
pub fn init<S: Eeprom>(store: &mut S, capacity: u32) -> Result<(), KvError<S::Error>> {
    if active(store, capacity)?.is_some() {
        return Ok(()); // already double-buffered
    }
    if KV_HEADER as u32 <= half_len(capacity) {
        let at = half_base(capacity, 0);
        let mut tag = [0u8; 2];
        store.read(at, &mut tag).map_err(KvError::Backend)?;
        if tag != [0, 0] {
            store.write(at, &[0, 0]).map_err(KvError::Backend)?;
        }
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
    let needed = KV_HEADER as u32 + value.len() as u32;

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
        let size = KV_HEADER as u32 + len as u32;
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
        if len as usize > MAX_VALUE || o + KV_HEADER as u32 + len as u32 > area {
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
        o += KV_HEADER as u32 + len as u32;
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
/// commit. Two worst-case records (`2 × (KV_HEADER + MAX_VALUE)` = 528 B) cover the append that
/// crossed the threshold plus a maximal burst record during the drain. That is generous, given the
/// maintenance task is woken by that very write and drains the flip in back-to-back slices,
/// while real SDK records are 4-8 byte scalars. And the guarantee is soft by design: if a
/// pathological burst fills the source anyway, [`set_bytes_with`] finishes the pending flip
/// synchronously (bounded by its *remaining* steps): never a failure, just latency.
pub const FLIP_THRESHOLD: u32 = 2 * (KV_HEADER + MAX_VALUE) as u32;

/// Cost of writing `len` bytes, in EEPROM word-programs: the unit of every maintenance budget
/// (each word program stalls the STM32L0 ~3.4 ms). Exact for word-aligned spans (the blank
/// passes). An **unaligned** span costs more: the HAL (`eeprom_write_slice`) programs its
/// unaligned head and tail as up to **3 single-byte programs each** (each as slow as a word
/// program), so one write call can cost up to 4 programs over this estimate, e.g. 6 bytes at
/// `offset % 4 == 1` = 3 + 3 byte programs vs `words(6)` = 2. Record copies land at arbitrary
/// offsets, so a copy slice that touches several records can overshoot its budget by a few
/// programs per record (tens of ms, never seconds).
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
            total += KV_HEADER as u32 + len as u32;
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
    // record is harmless: the destination is uncommitted until `flip_commit`.
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
        let size = KV_HEADER as u32 + len as u32;
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
        if len as usize > MAX_VALUE || o + KV_HEADER as u32 + len as u32 > half {
            break;
        }
        let size = KV_HEADER as u32 + len as u32;
        store
            .read(
                src_base + o + KV_HEADER as u32,
                &mut tmp[KV_HEADER..size as usize],
            )
            .map_err(KvError::Backend)?;
        if entry_crc(&tmp[0..4], &tmp[KV_HEADER..size as usize]) != crc {
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

    /// RAM-backed [`Eeprom`]; `write` programs bytes verbatim (byte-addressable, like the L0 data
    /// EEPROM). `torn_write` simulates a power-loss mid-program (only a prefix lands).
    /// `words_written` mirrors the codec's own budget accounting (`words()` per write call), so
    /// the maintenance tests can assert budgets and skip-if-zero wear-freedom.
    struct Ram {
        buf: Vec<u8>,
        words_written: usize,
    }
    impl Ram {
        fn new(cap: usize) -> Self {
            Self {
                buf: vec![0u8; cap],
                words_written: 0,
            }
        }
        /// Simulate a torn write: only the first `landed` bytes of `data` are programmed.
        fn torn_write(&mut self, off: u32, data: &[u8], landed: usize) {
            let n = landed.min(data.len());
            self.buf[off as usize..off as usize + n].copy_from_slice(&data[..n]);
        }
        /// Write a well-formed record at absolute `off` with no superblock anywhere, returning the
        /// next offset: bytes in another layout (e.g. the old single-region log) for the format
        /// tests, which must never be read back as records.
        fn raw_put(&mut self, off: u32, key: u16, value: &[u8]) -> u32 {
            append(self, off, key, value).unwrap();
            off + KV_HEADER as u32 + value.len() as u32
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
            self.buf
                .get_mut(s..s + data.len())
                .ok_or(())?
                .copy_from_slice(data);
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
        let mut ram = Ram::new(CAP as usize);
        let mut o = 0u32;
        o = ram.raw_put(o, 0x5201, &7u32.to_le_bytes()); // e.g. an old TX-counter watermark
        o = ram.raw_put(o, 0x5202, &3u32.to_le_bytes());
        let mut n = 0u32;
        while o + (KV_HEADER as u32 + 4) <= CAP - 2 * SUPER_LEN {
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
                    ram.torn_write(head, &[0, 0], landed.min(1));
                } else {
                    ram.buf[head as usize..head as usize + 2].copy_from_slice(&[0, 0]);
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
        set_bytes_with(&mut ram, CAP, 2, &[2u8; 14], &mut pending).unwrap(); // tail: 22 B
        set_bytes_with(&mut ram, CAP, 2, &[3u8; 14], &mut pending).unwrap(); // tail: 44 B, full
        // Live is 100 + 22 = 122 B, so a 108-byte record fits one fresh flip (230 <= 244), while
        // the finished flip alone holds plan + tail = 144 B (only 100 B free).
        set_bytes_with(&mut ram, CAP, 3, &[4u8; 100], &mut pending).unwrap();
        assert!(pending.is_none());
        assert_eq!(get_vec(&ram, 1).unwrap(), [1u8; 92]);
        assert_eq!(get_vec(&ram, 2).unwrap(), [3u8; 14]);
        assert_eq!(get_vec(&ram, 3).unwrap(), [4u8; 100]);
        let (_, g1) = active(&ram, CAP).unwrap().unwrap();
        assert_eq!(g1, g0 + 2, "the finished flip plus exactly one re-flip");

        // Plain set_bytes on the same history agrees (one flip).
        let mut ram = build();
        set(&mut ram, 2, &[2u8; 14]).unwrap();
        set(&mut ram, 2, &[3u8; 14]).unwrap();
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
        // Each record is KV_HEADER + len; live counts only the latest per key.
        set(&mut ram, 1, &[1, 2, 3, 4]).unwrap(); // 12 B
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half - 12);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 12);
        set(&mut ram, 1, &[5, 6, 7, 8]).unwrap(); // supersedes: used 24, live still 12
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half - 24);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 12);
        set(&mut ram, 2, &[9u8; 6]).unwrap(); // 14 B
        assert_eq!(free_bytes(&ram, CAP).unwrap(), half - 38);
        assert_eq!(live_bytes(&ram, CAP).unwrap(), 26);
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
}
