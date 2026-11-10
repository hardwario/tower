# Test strategy: the layers to add

What TOWER tests today, and the layers still missing for product quality, in the order they pay
off. Written 2026-10-18 after a three-board bench campaign; each item says why it matters (most
cite a bug that the missing layer would have caught), what to set up, what to automate and when
it passes. The per-test catalogue of the firmware stays in
[docs/firmware/test-plan.md](firmware/test-plan.md); bench results go to
`firmware/tools/hwtest/RESULTS-<date>.md`.

Updated 2026-11-10: the layers that need no hardware are in place, mutation testing (6) and
the host side's MQTT contract and TUI snapshots (8). Each of those sections now says what
exists and what is still to do; the others wait for the bench (a self-hosted runner, a
power-switching USB hub, the PPK2, a board for the rogue firmware).

## What exists

| Layer | Where | Runs |
|-------|-------|------|
| Unit and host tests of every crate, the CLI and the gateway engine | `just test` | CI, every push |
| The MQTT API page held against the bridge: topics, retain flags, payload fields and types, the RPC ops, error texts, property tables (layer 8) | `cli/src/gateway/engine/tests/contract.rs`, in `just test` | CI, every push |
| Frame snapshots of both TUIs at three terminal sizes: the screen, its colours and attributes, the cursor (layer 8) | `cli/src/snapshots/`, `cli/src/gateway/snapshots/`, in `just test` | CI, every push |
| Fuzzing of the pure kernels (frames, radio envelopes, KV store, radio and shell kernels) | `fuzz/`, `just fuzz` | CI, weekly (nightly toolchain) |
| Mutation testing of the shared kernels (`tower-net`, `tower-kv`, `tower-protocol`, `tower-gateway`, `tower-radio`, `tower-shell`, `tower-taf`, `tower-modules`): no mutant survives unexplained (layer 6) | `just mutants`, `.cargo/mutants.toml` | CI, weekly |
| Firmware gates: MCU build, RAM budget, clippy over the role variants, rustdoc | `just fw ci` | CI, every push |
| Supply chain: `cargo audit` on every lockfile, pinned actions | `.github/workflows` | CI |
| HIL bench: smoke, KATs, two-board radio and network, gateway end to end, stack high-water, a listening node's downlinks (also on US 915), OTA pairing and a sleeping node's `set`, the Climate, CO2 and Relay Modules on the Core | `just hil bench` | by hand |
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

**A first step (2026-11-10).** `firmware/tools/hwtest/soak.py` does the accounting an MQTT
recording allows: uplinks and the longest silence per node, events lost (by their running
count), every state's reports and range, the downlinks' outcomes, the gateway's status changes.
Still missing: the script that drives clicks and remote shell commands, resets and the crash
record, the stack high-water mark, the EEPROM flips, the host process's memory.

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

**Mutation testing is in place** (2026-11-10) for the eight hardware-free crates the firmware
and the bridge decide with. `tower-net`, `tower-kv` and `tower-protocol` came first,
the crates that hold the security decisions: the replay rule, the TX-counter watermark, the ACK
and beacon-epoch rules, pairing, the EEPROM store, the framing and the schema tags.
[cargo-mutants](https://mutants.rs) changes a crate's code one small piece at a time (an
operator, a constant in place of a result) and runs the crate's tests on each change. A mutant
no test fails on is a decision the tests do not hold.

```sh
cargo install --locked cargo-mutants   # 27.1.0
just mutants                           # the eight crates: about an hour at 4 jobs
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
| `tower-net`, with the hop slot plan and the hop-clock rule (T3) | 279 | 244 | 2 | 33 | 0 |
| `tower-gateway`, first run | 102 | 89 | 1 | 9 | 3 |
| `tower-gateway`, now | 102 | 92 | 1 | 9 | 0 |
| `tower-radio`, first run | 113 | 87 | 2 | 6 | 18 |
| `tower-radio`, now | 108 | 100 | 2 | 6 | 0 |
| `tower-shell`, first run | 105 | 73 | 19 | 0 | 13 |
| `tower-shell`, now | 103 | 84 | 19 | 0 | 0 |
| `tower-taf`, first run | 237 | 214 | 0 | 10 | 13 |
| `tower-taf`, now | 235 | 225 | 0 | 10 | 0 |
| `tower-modules`, first run | 248 | 221 | 0 | 7 | 20 |
| `tower-modules`, now | 234 | 227 | 0 | 7 | 0 |

The other five followed the same day. `tower-gateway` (the registry codec and the downlink
queue) had three survivors: nothing held that a queue id still in use is skipped after the counter wraps
and that a freed id is not handed out again at once (the host tracks items by id), that
`is_empty` is false with an item queued, and that a stored bucket longer than a bucket can be
is unreadable rather than a slice past the buffer. `tower-radio` had 18: twelve in the hop permutation, where the tests
held that it is a permutation and the same every cycle, which any shuffle is, but not which one
(its sequence is pinned by value now: both ends of a US 915 link compute it); one in CCM's AAD
length, which RFC 3610's vector #1 with its 8-byte header could not tell from a longer one
(vector #4, a 12-byte header, does); five equivalent. `tower-shell` had 13: the completion budget's byte counts (its
9-byte base, an exact fit, the first candidate's prefix share) were held only by "it fits one
frame", the address parser took nine hex digits when the value still fit, and the xorshift
was pinned for one seed that no `|` in place of `^` changes. `tower-taf` had 13: the weekly
program looked back over the days in a test that could not tell back from forward, "strictly
after" a switch point was never asked on one, and `Mode::from_index`, `Demand::is_on` and
`Reporter::last` had no test at all. `tower-modules` (the module drivers'
arithmetic and frames, written the same day): one constant nothing pinned (the Relay Module's red LED bit:
`1 << 1` could have become 0), an untested period constant and error text, and 14 equivalent
mutants (`|` against `^` of masks that share no bit), listed in `.cargo/mutants.toml`.

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

- **TUI snapshots: in place** (2026-11-10). Both TUIs are drawn into ratatui's test backend from
  canned states (no device, no broker) and compared with committed `insta` snapshots
  (`cli/src/snapshot.rs`): 12 frames of the console TUI at 80x24, 120x40 and 60x16 (the split
  layout, the zoomed shell with a long `export`, the zoomed log paused, the reconnect header),
  26 of the gateway TUI at 100x30, 120x36 and 140x45 (empty, busy, a pairing window, the zoomed
  panes, the modals). A snapshot is the screen as text, a map of every cell's colours and
  attributes with its legend (the lit zoom, pause and filter chips, the focus border, the
  severity tints), and the cursor. A frame that changes fails its test and leaves a `.snap.new`
  to compare; `INSTA_UPDATE=always cargo test -p tower-cli` accepts, and the `git diff` of the
  `.snap` files is the review. The margins, the wrapped `export` and the chip contrast are held
  this way. The first frames showed defects nobody had reported. Two are fixed, each fix a
  reviewed change of the `.snap` files: the console footer's clock was drawn over the key hints
  without a gap and left a letter of one after it at 80 columns; and a pane lost its newest
  rows below its bottom edge when a line wrapped (in the gateway TUI's shell pane the prompt
  itself, in its MQTT and log panes the newest messages, in the console TUI the tail of a
  wrapped response), because the panes counted lines, or estimated rows from the text's width,
  where ratatui wraps at words. The frames hold the screens as they are, so the rest is still
  in them:
  - console: the footer's key hints run under the clock on any terminal narrower than about 140
    columns, `<F10>/<^C> Quit` first;
  - console: the prompt does not scroll, so a command longer than the shell pane is cut at its
    right edge with the cursor pinned there (22 characters at 80 columns);
  - gateway: the footer's twelve chips need 129 columns, so at 100 (the width the node table is
    laid out for) `F8 Clear`, `F9 Mouse` and `F10 Quit` are cut off;
  - gateway: below 100 columns the node table does not fit, the address loses its last digits,
    and ratatui resolves the squeeze differently from run to run (which is why no gateway frame
    narrower than 100 columns is held).

  **Still to do:** those four; frames of the two modals that count down on the wall clock (the
  key view, the cable catch), which need the clock passed in.
- **MQTT contract: in place** (2026-11-10). [mqtt-api.md](cli/mqtt-api.md) is read as data by
  the CLI's tests (`cli/src/gateway/engine/tests/contract.rs`, 19 tests, no broker): its tables
  and examples are parsed out of the Markdown and compared with a scripted session of the
  bridge's engine. Held both ways (a row without code fails, code without a row fails): every
  topic the bridge publishes with its retain flag and its diagnostic class, the topics it
  subscribes to (a retained request is cleared, not run), every payload's keys and JSON types
  against its field table, every JSON example against the typed payload, the last will, the
  RPC ops with their parameters, defaults, limits and `data` shapes, the common error texts,
  the terminal errors of a remote shell dialog, the name rules, the access table and the
  property tables against `tower_protocol::props`, the forms a `set` takes, and the limits the
  prose states (id length, payload cap, timeouts) against the constants. Plain assertions on
  `serde_json::Value`, no schema language: the page is the schema. Not held: the prose on
  behavior over time, and the numbers that live in the firmware (registry and queue sizes).
  **Still to do:** a CI job against a real mosquitto with TLS, logins and a broker restart (the
  CLI's own tests cover the embedded broker and its TLS front end, not an external one).
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
