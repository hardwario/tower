//! `kv_store`: the power-loss-safe EEPROM key-value codec (`crates/kv`).
//!
//! The codec runs over an in-memory EEPROM (6 KiB, the STM32L083CZ data EEPROM, or a 512-byte
//! region whose 244-byte halves make every input cross many compaction flips). The input picks a
//! **starting image** and then a sequence of operations:
//!
//! * starting images: blank (a fresh part), a raw byte image (bytes in any other layout, which
//!   `init` must format empty), or a store built through the codec and then corrupted byte-wise;
//! * operations: `set` / `delete` / `compact` (plain and the `_with` variants that share an
//!   incremental-flip plan), `maintain` slices, `wipe`, reads, clean reboots, EEPROM corruption,
//!   and **power cuts**: an armed cut makes the Nth EEPROM write from now land only a prefix of its
//!   bytes (optionally with one garbage byte, a torn word), fail, and every later write fail too,
//!   until the simulated reboot (RAM state dropped, `init` re-run, as the firmware boots).
//!
//! The oracle is an independent reader of the documented on-EEPROM format (superblocks with
//! `magic ‖ generation ‖ crc`, the highest valid generation is active, records
//! `[tag | len | crc | value]` scanned up to the first blank or corrupt one, latest record per key
//! wins, a zero-length record is a delete tombstone, an uninitialized region holds nothing: `init`
//! formats it empty). Before and after every operation the harness reads the image with it and
//! asserts the transition the API documents:
//!
//! * a completed `set(k, v)` changes exactly `k` to `v` (a tombstone for an empty `v`), a completed
//!   `delete(k)` removes exactly `k`, `compact` / `maintain` / `init` / a reboot change nothing,
//!   `wipe` empties the store; a refused operation changes nothing;
//! * an operation cut by power loss leaves either the state before it or the state after it, for
//!   every key at once (a wipe is all-or-nothing): no torn mix, no resurrected deleted or wiped
//!   value, no phantom key;
//! * the codec's own reads (`get_bytes`, `free_bytes`, `live_bytes`, `generation`,
//!   `dead_half_blank`) agree with the reference reader, and it never addresses past `capacity`;
//! * wear rules: deleting an absent key writes nothing, a `maintain` slice that does not commit a
//!   flip programs at most its word budget;
//! * on a store that started blank and was never corrupted (where the documented capacity limits
//!   are exact), `Full` / `TooManyKeys` are only returned when the live set really does not fit or
//!   the half really holds more than `MAX_KEYS` distinct keys, and the generation (the wear gauge)
//!   only grows, by one per committed flip.
#![no_main]

use std::collections::{BTreeMap, BTreeSet};

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tower_kv::{self as kv, Eeprom, KV_HEADER, KvError, MAX_KEYS, MAX_VALUE, MaintState};
use tower_protocol::crc::crc32_ieee;

/// The STM32L083CZ data EEPROM: the capacity the firmware hands the codec.
const CAP_6K: u32 = 6144;
/// A small region (two 244-byte halves) so that one input crosses many flips.
const CAP_SMALL: u32 = 512;
const SUPER_LEN: u32 = 12;
const SUPER_MAGIC: &[u8; 4] = b"TKV1";
/// Largest number of operations one input runs (bounds the per-input time).
const MAX_OPS: usize = 96;

// ---------------------------------------------------------------------------------------------
// The EEPROM: RAM with power-cut injection.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    PowerLost,
}

#[derive(Debug, Clone, Copy)]
struct Cut {
    /// Index (over the whole run) of the write call that tears.
    at: usize,
    /// Bytes of that write that reach the cells before the power dies.
    landed: usize,
    /// The cell after the landed prefix is left holding this (a torn word), if set.
    garbage: Option<u8>,
}

struct Ram {
    buf: Vec<u8>,
    /// Write calls so far (the cut's clock).
    writes: usize,
    /// Word programs so far, counted like the codec's budget (`ceil(len / 4)` per write call).
    words: usize,
    cut: Option<Cut>,
    /// Power is gone: every write fails without effect until the reboot.
    dead: bool,
}

impl Eeprom for Ram {
    type Error = Fault;

    fn read(&self, off: u32, buf: &mut [u8]) -> Result<(), Fault> {
        let s = off as usize;
        let src = self
            .buf
            .get(s..s + buf.len())
            .unwrap_or_else(|| panic!("codec read past capacity: {off}+{}", buf.len()));
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write(&mut self, off: u32, data: &[u8]) -> Result<(), Fault> {
        let s = off as usize;
        assert!(
            s + data.len() <= self.buf.len(),
            "codec wrote past capacity: {off}+{}",
            data.len()
        );
        if self.dead {
            return Err(Fault::PowerLost);
        }
        let n = self.writes;
        self.writes += 1;
        if let Some(c) = self.cut.filter(|c| c.at == n) {
            let landed = c.landed.min(data.len());
            self.buf[s..s + landed].copy_from_slice(&data[..landed]);
            if let Some(g) = c.garbage.filter(|_| landed < data.len()) {
                self.buf[s + landed] = g;
            }
            self.cut = None;
            self.dead = true;
            return Err(Fault::PowerLost);
        }
        self.buf[s..s + data.len()].copy_from_slice(data);
        self.words += data.len().div_ceil(4);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The reference reader of the documented on-EEPROM format.

type View = BTreeMap<u16, Vec<u8>>;

fn half_len(cap: u32) -> u32 {
    (cap - 2 * SUPER_LEN) / 2
}

fn superblock(buf: &[u8], cap: u32, h: u8) -> Option<u32> {
    let off = (cap - 2 * SUPER_LEN + h as u32 * SUPER_LEN) as usize;
    let b = &buf[off..off + SUPER_LEN as usize];
    let generation = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let crc = u32::from_le_bytes(b[8..12].try_into().unwrap());
    (&b[0..4] == SUPER_MAGIC && crc32_ieee(&b[0..8]) == crc).then_some(generation)
}

/// The active half: the valid superblock with the highest generation (half 0 on a tie).
fn active(buf: &[u8], cap: u32) -> Option<(u8, u32)> {
    match (superblock(buf, cap, 0), superblock(buf, cap, 1)) {
        (Some(a), Some(b)) => Some(if a >= b { (0, a) } else { (1, b) }),
        (Some(a), None) => Some((0, a)),
        (None, Some(b)) => Some((1, b)),
        (None, None) => None,
    }
}

/// Records of the log at `[base, base + area)`, up to the first blank (tag 0), oversized,
/// overrunning or CRC-failing record. Returns the records and the log end (relative).
fn scan(buf: &[u8], base: u32, area: u32) -> (Vec<(u16, Vec<u8>)>, u32) {
    let mut out = Vec::new();
    let mut o = 0u32;
    while o + KV_HEADER as u32 <= area {
        let at = (base + o) as usize;
        let hdr = &buf[at..at + KV_HEADER];
        let tag = u16::from_le_bytes([hdr[0], hdr[1]]);
        let len = u16::from_le_bytes([hdr[2], hdr[3]]) as u32;
        let crc = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        if tag == 0 || len as usize > MAX_VALUE || o + KV_HEADER as u32 + len > area {
            break;
        }
        let value = &buf[at + KV_HEADER..at + KV_HEADER + len as usize];
        let mut covered = hdr[0..4].to_vec();
        covered.extend_from_slice(value);
        if crc32_ieee(&covered) != crc {
            break;
        }
        out.push((tag, value.to_vec()));
        o += KV_HEADER as u32 + len;
    }
    (out, o)
}

/// What the store holds, read independently of the codec.
#[derive(Clone)]
struct Snapshot {
    active: Option<(u8, u32)>,
    /// Latest value per key, tombstoned keys removed. Empty for an uninitialized region (`init`
    /// formats it empty, whatever bytes it holds).
    view: View,
    /// Log end of the active half (relative to its base).
    end: u32,
    /// Distinct keys in the active half's log (superseded and tombstoned ones included): what a
    /// compaction pass has to track, against `MAX_KEYS`.
    tags: usize,
    /// The inactive half's record area is all zero.
    dead_blank: bool,
}

fn snapshot(buf: &[u8], cap: u32) -> Snapshot {
    let half = half_len(cap);
    let act = active(buf, cap);
    let (records, end, dead_blank) = match act {
        Some((h, _)) => {
            let (r, e) = scan(buf, h as u32 * half, half);
            let dead = ((1 - h) as u32 * half) as usize;
            (r, e, buf[dead..dead + half as usize].iter().all(|&b| b == 0))
        }
        None => (Vec::new(), 0, true),
    };
    let mut view = View::new();
    let mut tags = BTreeSet::new();
    for (tag, value) in records {
        tags.insert(tag);
        if value.is_empty() {
            view.remove(&tag);
        } else {
            view.insert(tag, value);
        }
    }
    Snapshot {
        active: act,
        view,
        end,
        tags: tags.len(),
        dead_blank,
    }
}

/// Packed size of a live set: what a compaction flip leaves in the fresh half.
fn live(view: &View) -> u32 {
    view.values().map(|v| (KV_HEADER + v.len()) as u32).sum()
}

// ---------------------------------------------------------------------------------------------
// Input.

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Key {
    /// One of 8 keys, so operations collide.
    Small(u8),
    /// Any key, including the reserved 0 and enough distinct ones to reach `MAX_KEYS`.
    Any(u16),
}

impl Key {
    fn get(self) -> u16 {
        match self {
            Key::Small(k) => 1 + (k % 8) as u16,
            Key::Any(k) => k,
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Value {
    Bytes(Vec<u8>),
    /// `len % 300`: up to past `MAX_VALUE`.
    Fill {
        byte: u8,
        len: u16,
    },
    /// A value that embeds a complete, CRC-valid record for another key after `pad` bytes. Apps
    /// store arbitrary bytes (settings strings, keys), so this is content a torn or leftover
    /// write could expose to the scan as a phantom record.
    Embedded {
        pad: u8,
        key: Key,
        inner: Vec<u8>,
        tail: Vec<u8>,
    },
}

impl Value {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Value::Bytes(b) => b.clone(),
            Value::Fill { byte, len } => vec![*byte; *len as usize % 300],
            Value::Embedded {
                pad,
                key,
                inner,
                tail,
            } => {
                let inner = &inner[..inner.len().min(64)];
                let mut v = vec![*pad; *pad as usize % 32];
                let mut rec = key.get().to_le_bytes().to_vec();
                rec.extend_from_slice(&(inner.len() as u16).to_le_bytes());
                let crc = kv::entry_crc(&rec, inner);
                rec.extend_from_slice(&crc.to_le_bytes());
                rec.extend_from_slice(inner);
                v.extend_from_slice(&rec);
                v.extend_from_slice(tail);
                v
            }
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Op {
    Set {
        key: Key,
        value: Value,
        with: bool,
    },
    Delete {
        key: Key,
        with: bool,
    },
    Get {
        key: Key,
    },
    Compact {
        with: bool,
    },
    Wipe,
    Maintain {
        budget: u8,
        threshold: u16,
        slices: u8,
    },
    /// Arm a power cut at the `after`-th write call from now.
    Arm {
        after: u8,
        landed: u16,
        garbage: Option<u8>,
    },
    Reboot,
    /// EEPROM corruption (a byte changed in place), noticed at the next boot.
    Corrupt {
        off: u16,
        xor: u8,
    },
}

#[derive(Arbitrary, Debug)]
enum Start<'a> {
    Blank,
    Raw {
        at: u16,
        bytes: &'a [u8],
    },
    Patched {
        sets: Vec<(Key, Value)>,
        patches: Vec<(u16, u8)>,
    },
}

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    small: bool,
    start: Start<'a>,
    /// A power cut armed before the first boot's `init` (e.g. mid format).
    boot_cut: Option<(u8, u16, Option<u8>)>,
    ops: Vec<Op>,
}

// ---------------------------------------------------------------------------------------------
// The harness.

struct Harness {
    cap: u32,
    ram: Ram,
    maint: MaintState,
    /// The documented capacity limits are exact (blank start, never corrupted): check liveness
    /// (`Full` / `TooManyKeys` justified) and generation monotonicity too.
    strict: bool,
    /// Highest committed generation seen (strict mode).
    max_gen: u32,
    /// Every key the run touched or saw, for the full read-back checks.
    keys: BTreeSet<u16>,
}

/// Which keys one operation may change, for the before-or-after check of a power cut.
enum Effect {
    /// The state after = the state before.
    Nothing,
    /// The state after = `view` (computed from the state before).
    To(View),
}

impl Harness {
    fn snap(&self) -> Snapshot {
        snapshot(&self.ram.buf, self.cap)
    }

    fn half(&self) -> u32 {
        half_len(self.cap)
    }

    fn get(&self, key: u16) -> Option<Vec<u8>> {
        let mut out = [0u8; MAX_VALUE];
        let n = kv::get_bytes(&self.ram, self.cap, key, &mut out).expect("a read never fails");
        n.map(|n| out[..n].to_vec())
    }

    /// The codec's reads agree with the reference reader, for `keys` and the aggregates.
    fn cross_check(&mut self, s: &Snapshot, keys: &[u16]) {
        assert!(s.active.is_some(), "store left uninitialized");
        for &k in keys {
            assert_eq!(
                self.get(k).as_ref(),
                s.view.get(&k),
                "get_bytes({k}) disagrees with the image"
            );
        }
        assert_eq!(kv::free_bytes(&self.ram, self.cap), Ok(self.half() - s.end));
        match kv::live_bytes(&self.ram, self.cap) {
            Ok(n) => assert_eq!(n, live(&s.view), "live_bytes disagrees with the image"),
            Err(KvError::TooManyKeys) => assert!(s.tags > MAX_KEYS),
            Err(e) => panic!("live_bytes failed: {e:?}"),
        }
        assert_eq!(
            kv::generation(&self.ram, self.cap),
            Ok(s.active.map_or(0, |a| a.1))
        );
        assert_eq!(kv::dead_half_blank(&self.ram, self.cap), Ok(s.dead_blank));
        if self.strict {
            let g = s.active.map_or(0, |a| a.1);
            assert!(g >= self.max_gen, "the generation went backwards");
            self.max_gen = g;
        }
        self.keys.extend(s.view.keys().copied());
    }

    fn all_keys(&self) -> Vec<u16> {
        self.keys.iter().copied().collect()
    }

    /// Boot: drop all RAM state and run `init`, as the firmware's `Kv::new` does. `init` must
    /// mount an initialized store untouched and format anything else empty (the reference reader
    /// already reads an uninitialized region as empty), in at most the head tag and one
    /// superblock, even when itself cut by a power loss (an armed cut may fire inside it; the
    /// device then boots again).
    fn boot(&mut self) {
        loop {
            self.ram.dead = false;
            self.maint = MaintState::new();
            let before = self.snap();
            let (w0, words0) = (self.ram.writes, self.ram.words);
            let r = kv::init(&mut self.ram, self.cap);
            let cut = self.ram.dead;
            let after = self.snap();
            assert_eq!(
                after.view, before.view,
                "init (cut: {cut}) changed the stored content"
            );
            if before.active.is_some() {
                assert_eq!(r, Ok(()));
                assert_eq!(self.ram.writes, w0, "init of an initialized store must not write");
            } else {
                // The format: the 2-byte head tag (one word) and the 12-byte superblock (three).
                assert!(
                    self.ram.words - words0 <= 4,
                    "the format programmed more than it needs"
                );
            }
            if cut {
                assert_eq!(r, Err(KvError::Backend(Fault::PowerLost)));
                continue;
            }
            match r {
                Ok(()) => {
                    let keys = self.all_keys();
                    self.cross_check(&after, &keys);
                    return;
                }
                Err(e) => panic!("init failed: {e:?}"),
            }
        }
    }

    /// Settle an operation: the transition it documents (or, cut by a power loss, the state
    /// before or the state after it), then the reboot a power loss implies. `needed` is the
    /// append the operation makes (`Full` / `TooManyKeys` liveness in strict mode). Returns
    /// whether the run continues.
    fn settle(
        &mut self,
        what: &str,
        r: Result<(), KvError<Fault>>,
        before: &Snapshot,
        effect: Effect,
        needed: u32,
    ) -> bool {
        let after = self.snap();
        let done = match &effect {
            Effect::Nothing => &before.view,
            Effect::To(v) => v,
        };
        if self.ram.dead {
            assert_eq!(
                r,
                Err(KvError::Backend(Fault::PowerLost)),
                "{what}: a lost write must surface"
            );
            assert!(
                after.view == before.view || after.view == *done,
                "{what} cut by power loss left neither the state before nor after it:\n \
                 before {:?}\n after  {:?}\n got    {:?}",
                before.view,
                done,
                after.view
            );
            self.boot();
            return true;
        }
        match r {
            Ok(()) => assert_eq!(
                after.view, *done,
                "{what} completed but the store does not hold its result"
            ),
            Err(e) => {
                assert_eq!(
                    after.view, before.view,
                    "{what} failed ({e:?}) but changed the store"
                );
                match e {
                    KvError::Full => assert!(
                        !self.strict || live(&before.view) + needed > self.half(),
                        "{what}: Full, but the live set ({} B) + {needed} B fits a {} B half",
                        live(&before.view),
                        self.half()
                    ),
                    KvError::TooManyKeys => assert!(
                        !self.strict || before.tags > MAX_KEYS,
                        "{what}: TooManyKeys with only {} distinct keys",
                        before.tags
                    ),
                    other => panic!("{what}: unexpected error {other:?}"),
                }
            }
        }
        if self.strict {
            // One flip at most, except a write that finished a pending incremental flip and then
            // had to re-flip it (`set_bytes_with`: the finished flip carries a verbatim tail).
            let (g0, g1) = (before.active.map_or(0, |a| a.1), after.active.map_or(0, |a| a.1));
            assert!(g0 <= g1 && g1 <= g0 + 2, "{what}: generation {g0} -> {g1}");
        }
        true
    }

    fn run(&mut self, op: &Op) -> bool {
        let before = self.snap();
        assert!(before.active.is_some());
        let w0 = self.ram.writes;
        match op {
            Op::Set { key, value, with } => {
                let (k, v) = (key.get(), value.bytes());
                self.keys.insert(k);
                let r = if *with {
                    kv::set_bytes_with(&mut self.ram, self.cap, k, &v, self.maint.pending())
                } else {
                    kv::set_bytes(&mut self.ram, self.cap, k, &v)
                };
                if k == 0 || v.len() > MAX_VALUE {
                    let e = if k == 0 {
                        KvError::InvalidKey
                    } else {
                        KvError::ValueTooLarge
                    };
                    assert_eq!(r, Err(e));
                    assert_eq!(self.ram.writes, w0, "a refused set must not write");
                    return true;
                }
                let mut done = before.view.clone();
                if v.is_empty() {
                    done.remove(&k); // an empty value is a delete tombstone
                } else {
                    done.insert(k, v.clone());
                }
                let needed = (KV_HEADER + v.len()) as u32;
                let go = self.settle("set", r, &before, Effect::To(done), needed);
                if go && !self.ram.dead {
                    let s = self.snap();
                    self.cross_check(&s, &[k]);
                }
                go
            }
            Op::Delete { key, with } => {
                let k = key.get();
                self.keys.insert(k);
                let r = if *with {
                    kv::delete_with(&mut self.ram, self.cap, k, self.maint.pending())
                } else {
                    kv::delete(&mut self.ram, self.cap, k)
                };
                if k == 0 {
                    assert_eq!(r, Err(KvError::InvalidKey));
                    return true;
                }
                if !before.view.contains_key(&k) && !self.ram.dead {
                    assert_eq!(r, Ok(()));
                    assert_eq!(self.ram.writes, w0, "deleting an absent key must not write");
                }
                let mut done = before.view.clone();
                done.remove(&k);
                let go = self.settle("delete", r, &before, Effect::To(done), KV_HEADER as u32);
                if go && !self.ram.dead {
                    let s = self.snap();
                    self.cross_check(&s, &[k]);
                }
                go
            }
            Op::Get { key } => {
                let keys: Vec<u16> = std::iter::once(key.get())
                    .chain(self.keys.iter().copied())
                    .collect();
                self.cross_check(&before, &keys);
                true
            }
            Op::Compact { with } => {
                let r = if *with {
                    kv::compact_with(&mut self.ram, self.cap, self.maint.pending())
                } else {
                    kv::compact(&mut self.ram, self.cap)
                };
                let ok = r.is_ok();
                let go = self.settle("compact", r, &before, Effect::Nothing, 0);
                if go && ok && self.strict {
                    let g0 = before.active.unwrap().1;
                    assert_eq!(self.snap().active.unwrap().1, g0 + 1, "a compaction is one flip");
                }
                go
            }
            Op::Wipe => {
                let r = kv::wipe(&mut self.ram, self.cap);
                let ok = r.is_ok();
                let go = self.settle("wipe", r, &before, Effect::To(View::new()), 0);
                if go && ok {
                    let s = self.snap();
                    let keys = self.all_keys();
                    self.cross_check(&s, &keys);
                }
                go
            }
            Op::Maintain {
                budget,
                threshold,
                slices,
            } => {
                let mut r = Ok(());
                for _ in 0..=(slices % 8) {
                    let g0 = self.snap().active.map(|a| a.1);
                    let words0 = self.ram.words;
                    match kv::maintain(
                        &mut self.ram,
                        self.cap,
                        &mut self.maint,
                        *budget as u32,
                        *threshold as u32,
                    ) {
                        Ok(more) => {
                            if self.snap().active.map(|a| a.1) == g0 {
                                assert!(
                                    self.ram.words - words0 <= *budget as usize,
                                    "a maintain slice exceeded its word budget"
                                );
                            }
                            if !more {
                                break;
                            }
                        }
                        Err(e) => {
                            r = Err(e);
                            break;
                        }
                    }
                }
                self.settle("maintain", r, &before, Effect::Nothing, 0)
            }
            Op::Arm {
                after,
                landed,
                garbage,
            } => {
                self.ram.cut = Some(Cut {
                    at: self.ram.writes + *after as usize,
                    landed: *landed as usize,
                    garbage: *garbage,
                });
                true
            }
            Op::Reboot => {
                self.boot();
                true
            }
            Op::Corrupt { off, xor } => {
                let i = *off as usize % self.ram.buf.len();
                self.ram.buf[i] ^= xor;
                self.strict = false;
                self.boot();
                true
            }
        }
    }
}

fn build_start(start: &Start<'_>, cap: u32) -> Vec<u8> {
    let mut buf = vec![0u8; cap as usize];
    match start {
        Start::Blank => {}
        Start::Raw { at, bytes } => {
            let at = *at as usize % buf.len();
            let n = bytes.len().min(buf.len() - at);
            buf[at..at + n].copy_from_slice(&bytes[..n]);
        }
        Start::Patched { sets, patches } => {
            let mut ram = Ram {
                buf,
                writes: 0,
                words: 0,
                cut: None,
                dead: false,
            };
            for (k, v) in sets.iter().take(MAX_OPS) {
                let _ = kv::set_bytes(&mut ram, cap, k.get(), &v.bytes());
            }
            buf = ram.buf;
            for &(off, val) in patches {
                let i = off as usize % buf.len();
                buf[i] = val;
            }
        }
    }
    buf
}

fuzz_target!(|input: Input<'_>| {
    let cap = if input.small { CAP_SMALL } else { CAP_6K };
    let mut h = Harness {
        cap,
        ram: Ram {
            buf: build_start(&input.start, cap),
            writes: 0,
            words: 0,
            cut: None,
            dead: false,
        },
        maint: MaintState::new(),
        strict: matches!(input.start, Start::Blank),
        max_gen: 0,
        keys: BTreeSet::new(),
    };
    if let Some((after, landed, garbage)) = input.boot_cut {
        h.ram.cut = Some(Cut {
            at: after as usize,
            landed: landed as usize,
            garbage,
        });
    }
    h.boot();
    for op in input.ops.iter().take(MAX_OPS) {
        if !h.run(op) {
            return;
        }
    }
    // Final full read-back.
    let s = h.snap();
    let keys = h.all_keys();
    h.cross_check(&s, &keys);
});
