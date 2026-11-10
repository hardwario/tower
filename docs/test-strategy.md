# Test strategy: the layers to add

What TOWER tests today, and the layers still missing for product quality, in the order they pay
off. Written 2026-10-18 after a three-board bench campaign; each item says why it matters (most
cite a bug that the missing layer would have caught), what to set up, what to automate and when
it passes. The per-test catalogue of the firmware stays in
[docs/firmware/test-plan.md](firmware/test-plan.md); bench results go to
`firmware/tools/hwtest/RESULTS-<date>.md`.

## What exists

| Layer | Where | Runs |
|-------|-------|------|
| Unit and host tests of every crate, the CLI and the gateway engine | `just test` | CI, every push |
| Fuzzing of the pure kernels (frames, radio envelopes, KV store, radio and shell kernels) | `fuzz/`, `just fuzz` | CI, weekly (nightly toolchain) |
| Mutation testing of the security kernels (`tower-net`, `tower-kv`, `tower-protocol`): no mutant survives unexplained (layer 6) | `just mutants`, `.cargo/mutants.toml` | CI, weekly |
| Firmware gates: MCU build, RAM budget, clippy over the role variants, rustdoc | `just fw ci` | CI, every push |
| Supply chain: `cargo audit` on every lockfile, pinned actions | `.github/workflows` | CI |
| HIL bench: smoke, KATs, two-board radio and network, gateway end to end, stack high-water, a listening node's downlinks, OTA pairing and a sleeping node's `set` | `just hil bench` | by hand |
| HIL power group (PPK2 + J-Link) | `just hil bench-power` | by hand |
| Manual product checks and the remaining examples | `docs/firmware/test-plan.md`, `tools/hwtest` | by hand |

The weak spot is the last three rows: everything that needs hardware runs only when someone
starts it, briefly, on one bench.

## Highest value

### 1. Nightly HIL

**Why.** Every defect the 2026-10-18 campaign found (a name chosen at pairing overwritten by
auto-naming, summary verdicts lost to the console queue, a join refusal logged every 1.5 s,
`\x0d` in remote shell output) was found only because the bench was run by hand.
**Setup.** A self-hosted GitHub Actions runner on the bench machine, the bench left cabled
(`hil/hil.toml` pins the ports).
**Automate.** `just hil bench` nightly on `main` and on demand; results attached to the run.
**Passes when.** The suite is green for a week of nights with no retries.

### 2. Long soak with failure accounting

**Why.** Some failures need hours: the dongle's console wedge (a lost TC flag parking the writer)
appeared only after hours of traffic.
**Setup.** The gateway service, two or more nodes, a script driving clicks and remote shell
commands, an MQTT recorder.
**Automate.** 24 to 72 h runs that count: delivery ratio per node, resets (the boot session
counter, the crash record), the stack high-water mark, EEPROM flips per day projected to a
wear-out date, the host process's memory.
**Passes when.** No unexplained loss or reset, wear-out projected past the product lifetime.

### 3. Power loss and brown-out

**Why.** The KV store's torn-write handling is tested on the host only. A TX counter that goes
backward after a power cut is a CCM nonce reuse: the most security-relevant gap.
**Setup.** Switchable power: a USB hub with per-port power switching (`uhubctl`) for the cuts;
later the PPK2's source mode for slow voltage ramps.
**Automate.** Cut power at random points during settings writes, gateway registry commits and
TX watermark persists; after each cut check that the store opens intact and that the first
counter sent is above every counter sent before it (the gateway's replay lane sees both). Ramp
the supply down to check brown-out reset, radio and EEPROM behavior at low battery.
**Passes when.** Thousands of cuts with no corrupt store and no counter regression.

### 4. Energy per event, tracked

**Why.** The STOP floor alone misses an energy regression such as a retry storm or a timer that
wakes too often.
**Setup.** The PPK2 bench (`just hil bench-power`).
**Automate.** Energy of a click uplink, a temperature report, a heartbeat, a failed uplink with
its retries and a pairing; a battery-life projection from them; fail the run when a figure gets
more than 10 % worse than its recorded baseline.
**Passes when.** Baselines are recorded and the gate runs with the power group.

### 5. An attacker on the bench

**Why.** The fuzzers cover the pure kernels, not the device's receive path as a whole (SPIRIT1
driver, network layer, app); the security model's claims (replay, forgery, join-as-victim,
duty-budget exhaustion) are tested only as unit logic.
**Setup.** A third board in the HIL roster with a "rogue" firmware: capture and replay, forged,
truncated and oversized frames, ACK floods, JOIN requests as a registered address, random frames.
**Automate.** A HIL group running each attack against the product gateway and node.
**Passes when.** No replay is delivered, nothing crashes or wedges, the duty budget survives,
and every refusal is logged within its rate limit.

## Making the existing tests stronger

### 6. Mutation testing, coverage, proofs

**Mutation testing is in place** (2026-11-10) for `tower-net`, `tower-kv` and `tower-protocol`,
the crates that hold the security decisions: the replay rule, the TX-counter watermark, the ACK
and beacon-epoch rules, pairing, the EEPROM store, the framing and the schema tags.
[cargo-mutants](https://mutants.rs) changes a crate's code one small piece at a time (an
operator, a constant in place of a result) and runs the crate's tests on each change. A mutant
no test fails on is a decision the tests do not hold.

```sh
cargo install --locked cargo-mutants   # 27.1.0
just mutants                           # the three crates: about half an hour at 4 jobs
just mutants kv                        # one of them
```

`just mutants` fails when a mutant survives and names it; cargo-mutants' own report is in
`target/mutants/<crate>/mutants.out/` (`missed.txt`, each mutant's change in `diff/`).
`CARGO_MUTANTS_JOBS` sets the parallel jobs (4 by default). It builds and tests every mutant, so
it is not part of `just check`: `.github/workflows/mutants.yml` runs it weekly and on demand.

| Crate | Mutants | Caught | Caught by the timeout | Unviable | Survived |
|-------|--------:|-------:|----------------------:|---------:|---------:|
| `tower-net`, first run | 246 | 203 | 2 | 33 | 8 |
| `tower-net`, now | 245 | 212 | 0 | 33 | 0 |
| `tower-kv`, first run | 395 | 246 | 73 | 32 | 44 |
| `tower-kv`, now | 381 | 276 | 73 | 32 | 0 |
| `tower-protocol`, first run | 392 | 276 | 0 | 72 | 44 |
| `tower-protocol`, now | 391 | 319 | 0 | 72 | 0 |

*Unviable*: the mutant does not compile (a const assertion fails, a type does not fit). *Caught
by the timeout*: the mutant makes the code itself loop for ever (a scan offset that stops
advancing), so cargo-mutants kills its test run; no test can fail faster. The timeout is five
times the unmutated test time.

None of the 96 survivors of the first run was a bug: the code was right and nothing held it.
43 tests were added, and what they hold now was open before:

- **`tower-net`.** `TxCounter::raise_to` with a floor exactly at the durable watermark: it must
  persist a higher watermark first, or the counter handed out next equals the one the next boot
  resumes at (a CCM nonce used twice). A node that lost sync re-locks to the *same* gateway
  session (only an older epoch is a replay). The all-zero check of the X25519 secret is an OR
  over its bytes, not an XOR (which refuses one honest pairing in 256). The ephemeral secret,
  the link key, the SAS and the confirmation tags are wiped on drop.
- **`tower-kv`.** A stale incremental-flip plan is refused before a single write (its destination
  is the active half by then). An append that fills its half writes no terminator into the
  other half or into superblock 0. The tail walk of a flip's commit reaches the last word of
  the source (a tombstone there keeps its key deleted). A record of exactly `MAX_VALUE` bytes
  reads back and survives both kinds of flip; a header claiming more ends the log without
  reading it. The exact-fit bounds after a flip, `compact_with`, the proactive threshold's
  boundary. The stored format is pinned byte for byte (the record, the superblock, CRC-32/IEEE).
- **`tower-protocol`.** That the schema tags cover what they claim: the console hash every
  message type and the management records, the radio hash both directions and the property
  tables, the hasher every tuple element, every field ahead of a nested type and every part of
  a property (replacing any of these by a constant passed every test before). The wire values
  the tags cannot see, pinned by number: the management and transmit codes, the registry
  flags, the message type bytes, the property ids (they are also the keys a node stores its
  settings under). The bounds of the framing to the byte: a frame of exactly `MAX_FRAME`, an
  output buffer of exactly the needed size, an envelope of exactly the radio MTU, an oversize
  or empty frame dropped by the deframer. `0` printed without a sign.

Of the 96, 84 are killed by those tests. Six change nothing and are excluded in
`.cargo/mutants.toml`, each with its reason: `1 << 0` against `1 >> 0` for bit 0 of a flags byte
(twice); in `tower-kv` the length of the last word of a span that is always whole words, a
blank pass entered once more when it has already finished, and two loops over the record log
whose bound may include the log end (no plan entry names that offset, so nothing more is
copied). The last six are gone at the source: `tower-kv` sized six record buffers
`KV_HEADER + MAX_VALUE` in place, where a larger buffer changes nothing a host test can see (and
costs stack the firmware does not have); the size is one named constant now, held by a const
assertion.

**Passes when** `just mutants` reports no survivor; a new one is killed by a test or added to
the exclusions with its reason. **Still to do:** `cargo-llvm-cov` for the untested branches;
Kani proofs of the counter and replay kernels, small and pure ("the TX counter never repeats",
"a replay is never fresh"); mutation testing of the other kernels (`tower-radio`,
`tower-gateway`, `tower-shell`, `tower-taf`).

### 7. Response-time budgets under load

**Why.** The button reacting slowly during pairing and TAB completion lagging were both latency
regressions found by hand.
**Automate.** Measure button press to uplink sent and console command to reply while pairing,
bulk transfers and radio statistics run; ideally drive the real button line from a bench GPIO so
debounce and EXTI are covered, not only `button simulate`. Fail on a budget overrun.

### 8. Host side

- **TUI snapshots:** ratatui's test backend with `insta`, both TUIs at a few terminal sizes. The
  truncated `/export`, the margins and the zoom/pause contrast were all visual bugs.
- **MQTT contract:** every topic and payload of [mqtt-api.md](cli/mqtt-api.md) checked against a
  JSON schema; a CI job against a real mosquitto with TLS, logins and a broker restart.
- **USB plug cycling:** a power-switching hub cycling a node's and the dongle's USB while the
  gateway streams, which exercises the console teardown and rebuild (the TX ring's drain and
  wakers).

## Before the first users

### 9. Radio pre-compliance and range

SDR captures of channel frequencies, hop distribution, occupied bandwidth and duty cycle against
EN 300 220 and FCC §15.247 (the rules the radio code cites), before the accredited lab run;
packet loss against path loss with step attenuators (a range curve); coexistence with a jammer
board (`radio_cw`). Note from 2026-10-18: `radio_afa` delivered 14 of 19 confirmed sends (an
unsynchronized 8 x 80 ms scanner against 6 reps); tune it before any product uses AFA.

### 10. Environment

A temperature chamber from -20 to +60 °C: the LSE start-up, the radio crystal's drift on the
narrowband channels, TMP112 accuracy, battery behavior in the cold. ESD on the button and USB.

### 11. End-of-line production test

A fixture flow driven by `tower`: flash, check RSSI and frequency offset against a reference
dongle, check the sensors, record the chip UID, provision. Every shipped board passes it.

### 12. Release qualification

Run the HIL suite and a short soak on the exact release artifacts (the `tower-firmware-cli-*.zip`
and the `tower` binaries from `release-cli.yml`), not development builds. With the compatibility
gate ([TASKS.md](../TASKS.md), T14), add upgrade tests: EEPROM layouts and the radio protocol across
adjacent releases.

## Order

Start with 1, 3 and 2: they need only the current bench plus a power-switching USB hub, and they
target the failures that hurt a fielded IoT product most (silent loss, nonce reuse, slow
degradation). Then 4 and 5 once the PPK2 and a third board are on the bench.
