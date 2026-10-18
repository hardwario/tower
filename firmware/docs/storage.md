# TOWER Storage: EEPROM key-value store & wear

The SDK persists small amounts of state (radio counters, per-boot session id, shell
settings, and whatever an app wants) in the STM32L083's **6 KiB data EEPROM**, through a
single shared key-value store (`storage::Nv`, codec in the `tower-kv` crate). This guide
covers the layout, how it wears, what the SDK writes and how often, the tuning knobs, and the
rules an app should follow so the EEPROM lasts the product's life.

> **Status: implemented and hardware-verified.** The store, the wear telemetry
> (`system eeprom print`), and the boot-loop backoff all run on the Core Module. The
> background maintenance task (incremental compaction, below) is implemented and host-tested;
> its stall-slicing numbers are datasheet/bench-derived but not yet re-verified on the bench.

## The store

- One store over the whole data EEPROM, namespaced so subsystems can't collide: `NS_NET`
  (radio), `NS_SYS` (console session), `NS_SHELL` (settings), `NS_APP` (yours). Take a
  scoped view with `b.kv.scope(NS_APP)`; every get/set is then keyed by an 8-bit local.
- Records are `[tag(2) | len(2) | crc(4) | value]`; values up to `MAX_VALUE` (256 B).
- Power-loss-safe: a torn write never corrupts a prior key (see the `tower-kv` module docs).
- **Delete** with `kv.delete(key)`: it appends a zero-length **tombstone** record (`len == 0`),
  so the key reads back absent (`get` → `None`) and the next compaction flip drops both the value
  and the tombstone, so the live set can **shrink**, not only grow. Append-only and power-loss-safe
  like any write; a redundant delete of an absent key is a wear-free no-op. (`len == 0` is therefore
  reserved as the tombstone marker: no key stores a genuinely empty value.)
- **Mount or format** (`tower_kv::init`, run by `Kv::new` on every boot). A region with a valid
  superblock mounts as it is, untouched. Anything else, a blank part or bytes in any other layout
  (such as the single-region log of older firmware), is **formatted empty**: nothing is adopted,
  migrated or kept. The format is two writes: the tag slot at the head of half 0 is cleared, then
  half 0's superblock is committed at generation 1. The rest of the region is not scrubbed (no
  multi-second stall on the first boot; the next point is why its old bytes stay unreadable). A
  power loss before the commit leaves the region unformatted, and the next boot formats it again.
  A board flashed over a store in another layout therefore starts empty: re-pair or re-provision
  it.
- **Every log end is terminated.** An append first clears the 2-byte tag slot just past the
  record it is about to write (read-first: no write when the slot is already blank), then writes
  the record. Stale bytes past the log end (what a torn append left, the tail of a corrupted
  mid-log record, the untouched rest of a freshly formatted half) can therefore never be scanned
  as records. Without it, a new record shorter than a torn or corrupted one left that one's
  remaining bytes right behind it, and the next scan read on into them: a superseded TX-counter
  watermark resurrected over the one just written (CCM nonce reuse) or bytes from inside a value
  surfaced as a phantom key (found by the `kv_store` fuzz target, host-tested by
  `torn_tail_then_shorter_append_exposes_no_phantom` and
  `corrupt_mid_log_record_never_resurrects_the_tail`). A power loss between the two writes is
  harmless: the cleared slot lies beyond the log end, and a torn append is cut at its CRC as
  before.

## How it wears: it wear-levels

The 6144 bytes are **two 3060-byte halves** plus two 12-byte superblocks at the top.

- Writes **append** to the active half (each record at a fresh, advancing offset; no cell is
  rewritten in place). The free offset is derived by scanning, so an append touches **only**
  the record it writes.
- When the active half fills, `compact` **flips**: it packs the live set (latest record per
  key) into the *other* half and commits by writing that half's superblock with an incremented
  **generation**. The two halves alternate as active, so wear spreads across the whole region.
- The superblocks are the only fixed-location writes, but each is written once every *other*
  flip (the same rate the data cells are cycled), so there is **no disproportionate hot cell**.

Net: **wear is proportional to the flip rate, which is proportional to total bytes written.**
There is no per-write hot spot; the design spreads writes across all 6 KiB.

## The compaction CPU stall: bounded by background maintenance

A compaction is not just wear; it is **time**: on the STM32L0, every EEPROM word write
stalls the CPU (NVM-stall: instruction fetches from flash halt, so interrupt handlers
effectively halt with them). Re-packing a full half synchronously is on the order of 1,500
word writes at ~3.4 ms each ≈ **5.2 s of the whole chip frozen** (bench-measured 2026-07-05:
5.16-5.20 s, flip-counter 1:1 with the stalls). Historically, whichever append happened to
fill the active half paid for the whole compaction, with the console and radio dead for the
duration: the boot-time session bump (a ~5 s "hung" boot every Nth reboot), a `settings set`,
a radio watermark persist.

The SDK now compacts **incrementally, in the background**. A maintenance task (`storage::maintenance`, spawned by
default by the `app!` macro) runs bounded slices of at most
`MAINT_BUDGET_WORDS` = 4 word-programs ≈ **14 ms** of stall each, yielding to the other tasks
between slices. It is **event-driven**: woken by KV writes (and once at boot), it drains all
work and then sleeps on a signal; an idle node takes zero extra wakeups and STOP stays
reachable. The two-half + superblock-commit design is what makes this safe: every pre-commit
step is invisible (the source half stays active until the final CRC'd superblock write), so a
reboot mid-maintenance loses only RAM progress and the flip restarts from scratch.

What the task does, in priority order:

1. **Advance a pending incremental flip** (`tower-kv` `flip_start`/`flip_step`/`flip_commit`):
   pass-1 scan into a RAM plan, then blank + copy a few words per slice, then one commit slice
   (tail catch-up of appends that landed during the flip + the superblock write). Appends keep
   going to the source half throughout; if it fills anyway, the write itself finishes the
   flip's *remaining* steps synchronously (never a failure, just latency).
2. **Start a flip proactively** once free space in the active half drops below
   `FLIP_THRESHOLD` = 528 B (two worst-case records: headroom for the appends that land
   between trigger and commit), and only when flipping would restore that headroom.
3. **Pre-blank the dead half** (read-first, skipping already-zero words; idempotent and
   wear-free on re-runs), so the next flip's blank pass costs nothing.

Residual stall budget:

| Path | Worst-case stall |
|---|---|
| Steady state (default `app!`: maintenance task running) | ~14 ms slices; commit slice ~tens of ms |
| Sync fallback, dead half pre-blanked (store filled mid-flip / no proactive room) | ≤ ~2.6 s (copy pass only; the blank pass reads-and-skips) |
| Sync fallback, maintenance never ran (custom entry without the task) | ~5.2 s (blank + copy), as before |
| Factory wipe (`system eeprom wipe confirm`, `examples/kv_wipe.rs`) | ~2.6 s (dead half pre-blanked) / ~5.2 s |

**Watchdog across a stall.** The executor (and with it the IWDG feeder task) is frozen for
the whole of a synchronous run, so the storage layer reloads an armed IWDG after every EEPROM
program (`watchdog::pet_during_stall`, called from `Storage`'s codec write). It is bounded: it
pets only while the app's liveness check-ins (if configured) are fresh and only within
`feed interval + 10 s` of the feeder's last pet, so code that wedges *inside* an EEPROM loop
still gets reset. That is what keeps short timeouts (the `watchdog` example's 8 s) safe across a
~5.2 s wipe.

The boot-time session bump still appends one small record per boot, but in steady state it can
no longer land on a full half (maintenance keeps free space above the threshold), so the ~5 s
"hung boot" is gone. Products that want a full compaction at a moment they control (e.g. during
provisioning) can call `Nv::compact_now()`.

Pre-blanking adds **no extra wear**: it programs exactly the nonzero words the next flip's
blank pass would have programmed (which then skips them), just earlier and in slices. Note the
per-boot session id and TX watermark deliberately stay ordinary KV records: the KV region
occupies the entire 6 KiB EEPROM (3060 + 3060 + 2×12 superblocks; no reserved raw area), so
relocating them to dedicated raw cells would shrink the region and change the layout (every
store formatted empty); bounded maintenance removes the motive.

## Write budget

The STM32L083CZ data EEPROM endurance is **100,000 erase/write cycles per cell**
(`EEPROM_ENDURANCE_CYCLES`, datasheet-confirmed). One "cycle" is one erase plus one program of a
cell; every figure here scales with it.

Each compaction flip erases and reprograms the store's most-written cells: the re-packed
live-set prefix at each half's start, and the committed superblock. The gauge is kept
**conservative**: it charges one full erase/write cycle of the worst cell *per flip*, so

```
FLIP_BUDGET = endurance = 100,000 flips
```

In reality the store alternates two halves, so any given cell is the flip target only every
*other* flip; true life is closer to ~200,000 flips. The gauge therefore reports **more** wear
than real, never less. Either way a flip absorbs roughly one half (~3 KB) of appends, so total
write capacity is on the order of:

```
100,000 flips × ~3 KB ≈ 3 × 10⁸ bytes ≈ tens of millions of small records
```

Treat **~10⁷ writes** as the working budget. `system eeprom print` reports the live flip count
against `FLIP_BUDGET`.

## What the SDK writes, and how often

The SDK is deliberately frugal (nothing writes per radio message):

| State (namespace) | When it writes | Frequency |
|---|---|---|
| TX-counter watermark (`NS_NET`) | reserve-ahead: once per `RESERVE` = 1024 sends, and once at boot | amortized to ~nothing |
| Per-lane last-seen (`NS_NET`) | lazy: once per `P` = 32 *accepted receives* on a lane (every accept on a sleeping node: `set_lane_persist_period(1)`) | **∝ RX traffic** |
| Registry format marker (`NS_APP`, gateway) | once, on a fresh store (never rewritten per boot) | once |
| Session id (`NS_SYS`) | once per boot (suppressed while boot-looping) | ∝ boots |
| FHSS epoch (`NS_NET`) | once per gateway (master) boot | ∝ gateway boots |
| Settings (`NS_SHELL`) | on an explicit setting change | user-driven, rare |

For a typical sensor node this is a handful of writes per day → decades. The one path that
scales with load is a **busy gateway's last-seen** persistence (see `P` below).

## Tuning knobs (wear vs. behaviour)

Both live in the host-tested `tower-net` decision crate (`crates/net`: `txctr::RESERVE`,
`replay::P`), used by `src/radio/net/mod.rs`:

- **`RESERVE`** (watermark reserve block, default 1024): larger = fewer watermark writes, at the
  cost of a bigger counter jump per boot (counter space, not wear). Rarely needs changing.
- **`P`** (last-seen lazy-persist period, default 32): the main gateway wear knob. Larger `P` =
  fewer last-seen writes, but a **larger replay window across a reboot**: after a reset a
  receiver can accept up to `P` already-seen counters before the persisted last-seen catches up.
  Smaller `P` tightens that window at the cost of more writes. It is a security/wear trade-off,
  not a free dial, and it is per `Net` (`set_lane_persist_period`): the push-button persists
  on every accept (it hears a few downlinks per boot, so a lazy lane was never written and a
  recorded downlink replayed after every reboot), the gateway keeps 32 (its host drops replayed
  uplinks by counter).

`tower-kv` appends a record on every write, identical value or not (no read-compare skip): a
value that rarely changes is written only when it does (the gateway's registry format marker is
stamped once on a fresh store, not on every boot).

## Wear telemetry

`system eeprom print` (or `Nv::flip_generation()` / `free_bytes()` / `live_bytes()` /
`dead_half_blank()`):

```
eeprom: 6 KiB data EEPROM
flips: 110 / 100000 (0.1%)
live: 84 B
free: 2620 B
dead-half blanked: yes
resets: 1
```

`flips` is the store's lifetime compaction count (the persisted superblock generation) against
`FLIP_BUDGET`; proactive background flips count here too. They trigger at the 528 B threshold
rather than at full, so each flip absorbs ~2.5 KB of appends instead of ~3 KB: a ~20 % higher
flip rate, well inside the conservative budget (the gauge already under-reports true life ~2×). `live` is the packed size of the
latest record per key; `free` the appendable room left in the active half (a flip reclaims
`used − live`); `dead-half blanked` reports whether maintenance has pre-blanked the inactive
half (the next flip then skips its blank pass). `resets` is the current consecutive-fast-reset
run (see below). Reading it is a pure EEPROM **read**; polling telemetry adds no wear.

## Factory reset (wipe)

`system eeprom wipe confirm` (USB console only; refused over the radio remote shell with
`R_DENIED`) and the `kv_wipe` example drop **every** key: pairing keys, peer tables, replay
lanes, the TX watermark, settings, the session counter. (To remove one key, use
`kv.delete(key)`; the wipe is the whole-device reset.)

It is a **flip to an empty live set** (`tower_kv::wipe` / `storage::factory_wipe`), using the
compaction flip's own commit discipline:

1. blank the dead half (read-first; free if maintenance pre-blanked it); invisible, the old
   store stays active;
2. commit the dead half's superblock at `generation + 1` (the single CRC'd atomic write); from
   here the store reads empty;
3. scrub the old half's records so the wiped values aren't left physically readable; invisible
   (that half is no longer active, and the next flip into it blanks it first anyway).

A power loss, brown-out or reset at **any** point leaves either the complete old store or an
empty one. (The old reset zeroed the region front-to-back with the superblocks last: interrupted,
it left the valid superblock over a half whose prefix read empty while its tail survived; new
appends restarted at offset 0, and once the log end lined up with an old record boundary the old
tail **resurfaced** and, being later in the log, overrode the newer values. Host-tested now by
cutting power at every write of the wipe and churning the rebooted store through several flips.)
The generation keeps counting, so the wear gauge is not reset; a wipe costs one flip.

After a wipe the in-RAM `Kv` is **frozen**: every write fails with `Error::Frozen` until the
reboot the caller owes, also when the wipe itself failed part-way (the store is still
consistent, old or empty). The shell reboots in both cases, after flushing its reply (~1 s); the
freeze is what stops other tasks (the gateway loop, the radio watermark / replay-lane persists)
from writing their pre-wipe RAM state back into the fresh store in that window.

## Boot-loop backoff

A unit stuck in a reset loop (a persistent hang caught by the watchdog, or a brown-out cycle)
would otherwise rewrite per-boot state on every reset and grind the store. `bootguard` counts
consecutive resets that never reached a **30 s healthy uptime** in a reset-surviving `.uninit`
RAM word (retained across a warm reset, never persisted → **zero EEPROM wear**). Once the run
reaches `BOOT_LOOP_THRESHOLD` (8), the SDK stops persisting the session counter (it reports a
RAM-derived id instead), so a wedged node can't wear the EEPROM one record per reset.

The **watermark is never suppressed** this way: it is the CCM nonce anti-reuse guarantee and must
keep failing closed even in a loop. Only wear-only, non-security per-boot state backs off.

## App guidance: the EEPROM is not RAM

The store is for configuration and slowly-changing state, not a scratchpad or a log:

- **Coalesce.** Keep hot values in RAM; persist only on a meaningful change, periodically at a
  low rate, or at shutdown, not every sample. One write per second exhausts the store in about
  a year; the budget is ~10⁷ writes, not unlimited.
- **Keep the live set small.** Storing many or large distinct keys that together approach the
  ~3 KB half size forces a flip on nearly every write (a flip storm). A handful of small keys
  leaves hundreds of appends between flips.
- **Batch related fields** into one record rather than writing several keys in a burst.
- Check `system eeprom print` during development to see how fast a workload is spending flips.
