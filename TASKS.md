# TASKS

The one list of TOWER work: numbered **decisions** waiting for the maintainer and numbered
**tasks**. It replaced `TODO.md` and `firmware/TODO.md` (2026-11-09).

Every working session keeps this file current, so the next one can resume from it alone
(AGENTS.md, "Tasks and sessions"):

- **Before work begins**, the task is added here, or an existing one set `in progress`, with its
  plan as a checklist. A question the work needs answered becomes a new decision.
- **During the work**, the checklist is ticked as steps finish.
- **Once resolved**, the task is marked `done` with the date and its commits and moves to *Done*;
  an answered decision gets the answer and its date and moves to *Decided*.
- Numbers are never reused or renumbered: `D5` and `T7` mean the same item forever.

Task status: `open`, `in progress`, `waiting (Dn)` (blocked on a decision), `done`.

## In progress

(None.)

## Decisions

### D6. Report the embassy-stm32 EEPROM error-flag bug upstream?

embassy-stm32 0.6 on the L0 checks `FLASH_SR` after each EEPROM program but clears the error flags
only on success, and `unlock` never clears them: one failed program makes every later EEPROM write
fail until reset. On the bench it silently stopped settings, strip state and replay lanes from
saving after 18 minutes. Our storage works around it (8a24fe5). Options: an issue, an issue with
a fix (clear the error flags before programming), or nothing.

### D7. The next CLI release: when, and after a dry run?

`main` carries taf, the five node apps, `radio_gateway` and a firmware bundle that now ships every
node app; `cli/Cargo.toml` is still 1.1.0. AGENTS.md asks for a dry run of `release-cli.yml` before
tagging (the bundle step changed). Decide the version and when to tag; the dry run can go first
at any time.

## Tasks

### T22. The LCD thermostat: what the bench left open

Status: open

- [ ] **The pairing SAS on the screen.** With the LCD Module the thermostat pairs from its menu,
  but the SAS to compare shows only on the USB console (`net: pairing: SAS n`) and on the host. A
  hook from the runtime (the SAS, pairing done or failed) would let the screen show it.
- [ ] **PPK2**: the idle floor with the module attached, a VCOM toggle, a page-mode show (its
  drawing runs sixteen times per show; estimated a few ms of CPU at 16 MHz).
- [x] **The `button simulate` command** stayed mounted on the thermostat with the module, where
  it did nothing (the LCD driver owns the button). The runtime now records that it started
  without the Core button, and the command refuses there (result 1) and names `lcd press core`.
  Bench: refused on the thermostat with the module, `lcd press core` works there; on
  `radio_control` a simulated click still reaches MQTT. 2026-11-09.

### T3. Listening node on US 915

Status: open (D4: later)

**Why.** The listening class (`radio_control`; docs/firmware/radio.md, "Node classes") runs on EU
868 only. A node that keeps its receiver on would have to follow the gateway's hop schedule all the
time, and one fixed channel at full power is not allowed on US 915 (FCC §15.247), so a listening
kind refuses `us915` for now (no pairing, no provisioning, radio off; docs/firmware/taf.md).

**What it takes.** The network layer (`firmware/src/radio/net`, `crates/net`): a listening node
that tracks the hop schedule between its own sends (a sleeping node acquires it with each uplink
and re-acquires by prediction after a sleep); the gateway (`radio_gateway`) sending a listening
node's downlink at once in the current slot, which today it does on EU 868 only.

**Done when.** `radio_control` on `us915` pairs, a `set` over MQTT is confirmed within the
listening budget (`hil/tests/listening.rs` on `us915`), and the per-channel airtime in `gateway
status` stays even.

### T4. Battery Module: battery voltage, power numbers, a long soak

Status: open

**Why.** The taf apps have run on USB power only. The sleeping apps are meant for batteries.

**What it takes.** A Battery Module driver and the battery voltage as a common property of every
sleeping app; the PPK2 power numbers of each sleeping app (idle floor, energy per report and per
event); a long soak on batteries.

**Done when.** Every sleeping app reports its battery voltage over MQTT, docs/firmware/taf.md
lists the measured power per app, and a soak of a day or more on batteries passes.

### T5. Climate Module sensors for `radio_climate`

Status: open

**Why.** `radio_climate` reports only the Core Module's TMP112.

**What it takes.** Drivers for the Climate Module's sensors (humidity, illuminance, pressure,
temperature) and their properties in the `climate` kind.

**Done when.** `radio_climate` with a Climate Module reports them over MQTT, with a measured stack
peak under the RAM floor.

### T6. Relay Module for `radio_control`

Status: open (not scheduled)

**Why.** `radio_control` switches the Power Module's relay. The Relay Module is the other relay
of the kit.

**Done when.** `radio_control` drives the Relay Module, its relay state reported and settable over
MQTT like the Power Module's.

### T7. HIL: OTA pairing of a taf node and a sleeping-node `set`

Status: open

**Why.** The HIL bench covers the listening node (`hil/tests/listening.rs`) and the gateway with
cable provisioning; taf's OTA pairing and a `set` delivered on a sleeping node's next wake are
checked by hand only.

**Done when.** A HIL test pairs a taf node over the air and confirms a `set` on a sleeping node
within one report period.

### T8. Root cause of the first failed EEPROM program

Status: open (waiting for the bench to catch one)

**Why.** 8a24fe5 clears stale flash error flags before each EEPROM program and retries once, so
one failure no longer wedges storage (see D6). What made the first program fail on the bench is
unknown.

**What it takes.** Watch for `storage: EEPROM program at ... failed: ... (SR ...)`; the logged
`FLASH_SR` names the error (write protection, alignment, size), then find its source.

**Done when.** The cause is known and fixed, or shown harmless and documented in
docs/firmware/storage.md.

### T9. Two-board verification of the 2026-10-17 batch

Status: waiting (a PPK2): everything else re-run and passed on the taf firmware
2026-11-09 (docs/firmware/results/2026-11-09.md)

**Why.** The night of 2026-10-17 changed pairing (the node commits its key only on an ACKed
uplink, the 12 s re-pair hold, the SAS in `Paired`/`Joined`, the window rule), replay and nonce
safety (counter floors on moved keys, per-accept node lanes, host uplink dedup), the push
button's backoff and the gateway's request pacing. Everything that needs only one board was
checked on hardware (KATs, watchdog, gateway end to end, key floors, a join with no gateway in
range); nothing that needs a node and a gateway on air was.

**What to run** (a Core Module with `radio_button`, a Radio Dongle with `radio_gateway`, `tower`
from the same commit, EEPROM wiped first):
- OTA pairing: `tower nodes add --ota`, hold 1 s; the node's `net: pairing: SAS`, the CLI's
  `paired ... (SAS n)` and the gateway log show the same six digits; `node: paired with gateway
  ...` follows on the node and its NodeInfo names it.
- Lost confirm (a temporary patch that skips the JOIN_CONFIRM transmit): the node discards the
  key after its commit round and pairs again on the next hold.
- Re-pair: a 12 s hold on a paired node is refused while it is registered; after `tower nodes
  remove` it pairs again with a new SAS.
- Window end: `--window 5` with a late hold is refused, never committed behind the host's back.
- Backoff: with the gateway off, the node's periodic uplinks space out up to the heartbeat and
  come back on a press.
- Replay: reboot the dongle under a running `tower gateway`; no event is published twice.
- Key move: `tower nodes show --keys`, wipe the gateway, `node_add` with the floors; the node's
  uplinks and remote-shell downlinks keep working.
- The HIL gateway group (`just hil bench`), and a PPK2 look at the node's idle floor with the new
  watchdog pet interval (~7.4 s).

**Done when.** Each item passes on the bench and the results are noted in
`docs/firmware/test-plan.md`.

**Status 2026-10-18** (three-board bench, results in
[docs/firmware/results/2026-10-18.md](docs/firmware/results/2026-10-18.md)):
OTA pairing, lost confirm, re-pair, window end, backoff, key move and the HIL gateway group
pass. Open: the replay item (nothing can reboot the dongle under a running `tower gateway`; D8)
and the PPK2 look at the idle floor (no PPK2 on the bench).

**Re-run 2026-11-09** (since 10-18 the node side is taf: the power-up gesture instead of the
holds, the `Info` retry; `radio_button` on the motion board, the HIL [core] slot, EEPROM wiped):
- [x] OTA pairing with the gesture: one SAS on the node, the CLI and the gateway; named on its
      first `Info`
- [x] Re-pair: refused while registered (the old pairing keeps working), a new SAS after removal
- [x] Window end: a join in a window's last seconds refused, nothing committed
- [x] Lost confirm (a node build that skips its confirm, not committed): key discarded, a
      second gesture pairs after the removal
- [x] Backoff: the gateway held in its bootloader, the node's uplinks space out; a click ends it.
      First run FAILED (a taf regression): an undelivered report stays due and every sample
      retried it, an uplink every `report-period` forever; fixed (reports wait out the backoff),
      re-run: attempts 31, 61, 121 s apart, a click delivered at once with the held report
- [x] Key move: keys exported, the gateway wiped, every node re-added with its floors; uplinks
      and remote-shell downlinks keep working
- [x] The HIL gateway group (`just hil bench`)
- [x] Replay: with the `reboot` RPC (T21), under traffic: no event published twice
- [ ] The PPK2 idle floor: needs a PPK2
- [x] The bench restored (`radio_motion` back on its board, every node paired and named);
      results in docs/firmware/results/2026-11-09.md, test-plan.md points to them; commit

### T10. FHSS in the product apps (US 915): remaining verification

Status: open (needs the PPK2 and a lab, not the code)

**Done 2026-10-18.** `radio_gateway` and `radio_button` hop on `us915` (FCC §15.247): the gateway
is the hop master, the node follows the schedule with every send, pairs on the rendezvous carrier
at low power (§15.249) and re-acquires after a sleep by prediction on the crystal clock
(`clock::now_ms`; embassy's `Instant` runs on HSI16, ±1 %). docs/firmware/radio.md
(*Spectrum-access modes*, "In the product apps") and docs/firmware/gateway.md (*US 915*) describe
it. Bench (Radio Dongle + two Core Modules, `us915`): OTA pairing of both nodes at low power;
clicks delivered within one slot; remote shell downlink in the uplink's slot, the reply one chunk
per slot; both nodes booting at once (a collision, then the random retry backoff); the gateway
beaconing every slot (all 80 channels, even per-channel airtime in `gateway status`).

**Still to verify:**
- The receive cost of re-acquisition (a predicted beacon catch after a sleep, the 15 min resync,
  the 24-48 s rescan after pairing or a reboot) and the STOP floor with the RTC sub-second counter
  at 1024 Hz (`board::init`; the asynchronous prescaler dropped from 127 to 31).
- The EIRP of `config::PA_INDEX_LOW` (about -6 dBm estimated from the PA ramp) against §15.249,
  with the product antenna.
- The §15.247 numbers (channel count, dwell, bandwidth) against the current FCC KDB, with an
  accredited lab, before any product claim. Until then: no US deployment claims.

### T11. STOP-mode regulator bits after every wake (power)

Status: open

**Why.** `board::apply_stop_tuning` sets `PWR_CR.LPSDSR` (low-power regulator in deep sleep) and
`ULP` (VREFINT off) for the measured ~5 µA STOP floor. embassy's L0 `exit_stop` re-inits the
RCC on every wake from STOP, and `rcc/l.rs` writes the whole `PWR_CR` (`write`, not `modify`),
which clears both bits. Only the console manager's 500 ms VBUS poll re-applies them, so after
any other wake (watchdog feeder, temperature read, LED pattern, the 5 ms button poll, a radio
IRQ) the next STOP runs on the main regulator until the next poll: about 2 % of the idle time on
a quiet node, nearly all of it while an LED pattern runs. (Review of 2026-10-17; the share is
arithmetic, not a measurement.)

**Why it is not a one-line fix.** A hook that re-applies the bits before every sleep (embassy-
executor's `trace` feature calls `_embassy_trace_executor_idle` right before the low-power
executor sleeps) would also set LPSDSR when embassy decides on plain Sleep instead of STOP (a
peripheral holds STOP off, e.g. the console UART with USB attached). LPSDSR also puts the
regulator in low-power mode in Sleep (RM0367 PWR_CR), which at the 16 MHz HSI16 clock is out of
spec. The same applies, rarely, to the current poll-based scheme: after a poll wake, an idle
pass that cannot STOP sleeps with LPSDSR set. ULP without FWU also lengthens the wake by the
VREFINT start-up; ST's low-power examples pair ULP with FWU.

**What's needed.** Set the bits on the STOP path only: upstream in embassy (L0 `enter_stop` sets
LPSDSR + ULP + FWU and `exit_stop` clears LPSDSR; `rcc/l.rs` modifies `PWR_CR` instead of
writing it), carried here with a `[patch.crates-io]` until it is released; then drop the console
manager's re-apply.

**Done when.** PPK2: a node with an LED background pattern and a 1 s timer task idles at the
same floor as `lowpower`; with USB attached the board runs with LPSDSR clear; the radio KATs and
a two-board confirmed uplink with a downlink still pass.

### T12. Test layers to add

Status: open (needs bench setup first)

[docs/test-strategy.md](docs/test-strategy.md): nightly HIL, long soak, power loss and brown-out,
energy per event, an attacker board, mutation testing, latency budgets, host-side snapshots and
contract tests, radio pre-compliance, environment, production test, release qualification.
Needs bench setup first (a self-hosted runner, a power-switching USB hub, the PPK2, a board
for the rogue firmware). Open findings to run down with it: a remote shell reply chunk lost when
two nodes answer at once (1 in 8 on the bench before the 2026-10-18 fixes, 0 in 48 in the soak
after them), and `radio_afa` delivering 14 of 19 confirmed sends.

### T13. Provisioned ATSHA204A as a secure element (manufacturing)

Status: open

**Why.** Every Core Module carries an ATSHA204A (I2C 0x64), which the SDK today only puts to
sleep (`board::atsha_sleep`, the `AtshaGuard` bus wrapper). Provisioned in production, it would
give the radio network two things the MCU alone can't:
- **Device attestation**: during the keyed join (E4, docs/firmware/radio.md), the gateway sends a
  random challenge and the node answers with the ATSHA's HMAC-SHA256 over it under a secret slot
  key that never leaves the chip, so the host knows it is talking to a genuine, provisioned
  board and not to a clone holding a copied link key.
- **Keys at rest**: the link keys (today plain records in the data EEPROM, protected only by the
  flash readout protection) stored in, or wrapped by, an ATSHA slot, so an EEPROM dump no longer
  yields them.

**What's needed.**
- A manufacturing step that writes a per-device secret into a slot (random per device, or
  diversified from a factory master key and the device identity) and records it host-side for
  the attestation check, then **locks the config zone and the data zone** (one-time, irreversible:
  slot policies must be final before the first production run).
- An SDK driver: wake / command / sleep over the shared I2C2 bus (the wake and sleep handling in
  `board.rs` already exists), the HMAC and nonce commands, CRC-16 framing.
- A keyed-join extension carrying the challenge and the HMAC answer, a host-side verifier, and a
  policy for unprovisioned boards (development kits).
- A power check on the PPK2: every ATSHA command wakes the chip (hundreds of µA while awake).

**Decided (2026-10-17).** The ATSHA204A's 72-bit serial number is **not** used as an identity:
anyone with the board can read it and it proves nothing without a provisioned secret. The device
identity is the MCU's factory unique ID (`board::unique_id`, shown as `uid:` in `system resource
print`).

**Done when.** A provisioned board keyed-joins with a verified attestation and an unprovisioned
or cloned one is refused (logged), the link keys survive a reboot without being readable from
the EEPROM, and the manufacturing procedure (provision, lock, record) is documented.

### T14. Compatibility gate (before the first users)

Status: open (not before the first users)

**Why.** Until TOWER has users, AGENTS.md says "no backward compatibility anywhere": formats and
protocols change freely and stale peers are only *detected* (schema tags), never supported. That
is right for development and wrong for a fleet in the field: sleeping nodes update slowly, a
gateway and its nodes will run different firmware during a rollout, and integrations read the
MQTT payloads. Decided 2026-10-17: keep compatibility out until then, and gate the first users on
this checklist.

**When.** Before the first external user, the first product shipment, or any public release
announcement, whichever comes first.

**Decide and reintroduce** (each becomes a decision here when the gate starts):
- **Console wire** (`crates/protocol`): is rejection by schema tag enough, or does the host need
  to decode older firmware (a negotiated version plus a support window)?
- **Radio protocol and pairing** (`crates/net`, `firmware/src/radio`): mixed fleets during a
  rollout. Nodes and gateways from adjacent releases must interoperate, or the rollout order and
  the failure mode must be defined and documented.
- **EEPROM layouts** (`crates/kv`, lane records, the gateway registry buckets, settings): a
  firmware update must migrate what the previous release stored (keys, TX watermarks, pairing),
  power-loss safely. Losing a TX watermark on update would reuse CCM nonces.
- **MQTT topics and payloads** (`cli/src/gateway`): version the schema for integrations (the
  `schema` field exists) and define a deprecation policy.
- **CLI surface**: flags, exit codes and output formats scripts rely on; semver for `tower` from
  then on.
- **Release process**: changelogs for user-facing artifacts, semver, a deprecation window, and a
  firmware update path that users can follow.
- **Remove** the "Compatibility: none" section from AGENTS.md and the matching "Don't" line.

**Done when.** Each bullet has a written decision (in the relevant docs) and the implementation
or test that backs it; AGENTS.md states the compatibility guarantees instead of their absence.

### T15. Retire the previous TOWER generation

Status: open (after T14)

**Why.** The new generation (this repository) replaces the C-SDK TOWER line, which still lives in
40+ repositories in the hardwario organization: `twr-sdk`, the `twr-radio-*` / `twr-lora-*`
application firmwares, `twr-gateway-radio-dongle`, `bch-firmware-tool`, `hardwario-playground`,
`hardwario-code`, `hardwario-tower-vscode-extension`, `twr-zephyr`, `twr-skeleton` and others.
Decided 2026-10-17: they will be made obsolete later; until then, work only on the new
generation and leave them untouched.

**When.** Not before the new generation covers what users of the old line rely on (at least the
product apps they deploy) and the compatibility gate (T14) is done.

**What it takes.** An inventory of which old repositories have users and what replaces each; a
migration path for deployed devices (re-flash and re-pair, or a documented end of support);
README notices pointing to hardwario/tower; then archive (not delete) the old repositories so
existing links and releases keep working. Keep `twr-hardware` and `twr-enclosures` if the new
generation still targets the same modules.

## Decided


- **D1. The LCD thermostat (RAM).** A page-mode LCD driver, later (T2). 2026-11-09.
- **D2. Push the overnight taf work.** Yes, pushed with the morning follow-ups (to 29fdf5f).
  2026-11-09.
- **D3. The new node apps in the release firmware bundle.** Yes (af0ea69). 2026-11-09.
- **D4. Listening nodes on US 915.** Later (T3). 2026-11-09.
- **D5. Where the bridge remembers node kinds across its own restart.** (a) The gateway
  registry: a kind byte per entry, read with the node list at start (T23). 2026-11-09.
- **D9. Nested `AGENTS.md`.** (a) Keep them, trimmed to the rules for working in that component;
  knowledge moves to `docs/`, the `CLAUDE.md` stubs stay (T16). 2026-11-09.
- **D10. Component READMEs.** (a) Their content moves to `docs/`; a short README per component
  stays (what it is, how to build or run it, links into `docs/`) (T16). 2026-11-09.
- **D11. The layout of `docs/`.** (a) By component, mirroring the code: `docs/firmware/`,
  `docs/cli/`, `docs/hil/`, `docs/protocol.md`, `docs/flasher.md`, `docs/fuzzing.md`, an index
  (T16). 2026-11-09.
- **D12. The power-up gesture and the BOOT button.** (a) The sequence starts right after
  power-up: the LED blinks once, the button pressed within 3 s and held runs the stages (T20).
  2026-11-09.
- **D13. A node's own button actions.** (a) Kept on top of the reported `button` events
  (`radio_control`'s click and hold) (T20). 2026-11-09.
- **D8. The replay check of the 2026-10-17 batch.** Add a reboot command to the bridge (T21),
  then run the check (T9). 2026-11-09.
- **D14. Flash for the LCD thermostat.** First answer: the `tower-sdk` package at `opt-level =
  "z"`, provided the LED strip passed on the bench. It did not: effects up to 2.2 times slower,
  1 to 3 underruns per sweep of the 18 effects (none at "s"), and keeping `tower-strip` at "s"
  doesn't help under fat LTO. Final answer: "z" for `radio_thermostat` only (cargo profile
  `release-z`, the justfile's `z_apps`), every other app at "s" as before (T2). The options
  passed over were "z" for every app despite the strip, and parking the screen. 2026-11-09.

## Done

(Newest first; one line each.)

- **T23. The bridge knows node kinds across its own restart** (D5 (a)): a stable `id` per kind
  in `props.rs` (`0` = unknown; pinned by test, not in the radio tag: no node sends one),
  `NodeEntry.kind` and `NodeUpdate.kind` (console tag 0x1fc6 to 0x2c25, radio tag unchanged), a
  kind byte in the gateway's registry records (`FORMAT` 2), stored by the bridge when a node's
  `NodeInfo` names another kind and read back with the node list at start. Bench: the gateway's
  three keys moved across the format change (exported with their floors, the gateway reflashed
  and wiped, `node_add` with `tx_floor` + 4096), both connected nodes heard and their kinds
  stored; after a bridge restart the list showed both kinds before any `NodeInfo`, and a `set`
  sent a second later was queued for both, delivered and confirmed by each node's report
  (control at once, r2 with its next heartbeat; docs/firmware/results/2026-11-09.md).
  `btn-r1` (r1, unplugged) is registered and gets its kind at its next wake. 2026-11-09.
- **T2. LCD thermostat**: `tower-hmi` canvas windows (bands; a host test checks bands against a
  frame in every rotation), `tower::lcd` page mode (`init_paged`, `Lcd<Paged>::show`, buttons
  only, band signatures, the frame linked only in frame mode, `lcd::probe`), the thermostat's
  screen (temperature, setpoint by the buttons, mode, clock, flame, a menu with mode, pairing,
  info, factory reset) in the regulator task, `taf::factory_reset` / `addr` / `gateway`,
  `Week::encode` / `decode` (no serde, 2.2 KB), `Override::until`; the thermostat built at
  `opt-level = "z"` (D14; `release-z`, the justfile's `z_apps`), r2 only (the r1 stays headless).
  Bench (LCD Module on the PIR board r2, now `radio_thermostat` "r2"): every page by `lcd dump`,
  setpoint by the buttons on MQTT, a mode set from MQTT on the screen, a remote `thermostat point`
  stored and read back, OTA pairing from the menu (SAS on both ends), stack peak 7.1 KB of 8.3 KB,
  4.5 KB of flash free; the strip at "s" unchanged (timings as before, 0 underruns). 2026-11-09.

- **T21. A reboot command for the gateway** (D8): the `reboot` RPC, the bridge pulses the
  dongle's reset line through the port it holds (no firmware or wire change, works on a wedged
  gateway); `tower net reboot` waits for the new session. Bench: back within a second, the resync
  logged, no event published twice under traffic (T9's replay check). 2026-11-09.
- **T19. A lost `Info` is resent only with the next heartbeat**: an undelivered `Info` (a
  heartbeat's or a `Describe`'s) is retried 30 s later, doubling, never later than the heartbeat.
  Bench: the gateway held in its ROM bootloader while the motion and r1 thermostat nodes booted
  (both boot `Info`s lost), released after 12 s: both retried 30 s later, the restarted bridge
  knew both kinds 32 s after boot and the thermostat synced its time. 2026-11-09.
- **T20. The Core button the same in every node app; pairing and factory reset at power-up**
  (D12 (a), D13 (a)): the gestures common and reported by the runtime, the power-up gesture
  (press within 3 s of the boot flash: 10 s pairs, 20 s resets, 25 s stuck), the old gestures
  gone. Bench: a click from every node; on the motion board 7 s (nothing), 27 s (stuck at 25 s),
  12 s (pairing), 22 s (factory reset, then paired again with the gesture: SAS 215391 on both
  ends); the stage timer bug it found fixed. 2026-11-09.
- **T18. Flash headroom**: 5.7 KB back in every app (no checked `str` slices: their panic linked
  the Unicode tables), `opt-level = "z"` measured and rejected (strip underruns), free flash and a
  2 KB warning in `just fw ci`: `radio_control` (r2) 6.3 KB, `radio_thermostat` (r1) 5.6 KB.
  2026-11-09.
- **T17. Thermometer and accelerometer in the taf runtime, for every node app**: `tilt` and
  `accel` common, the runtime's sensor task, `accel` applied at once. Bench (motion, control, r1
  thermostat): armed at boot, temperature on MQTT, an `accel` set re-armed in 70 ms, a tilt of
  each board on MQTT (the face, 10 s apart), stack peaks 5.2 KB on each. Flash headroom: T18; a
  lost boot `Info` found on the way: T19. 2026-11-09, 4d976ed.
- **T16. Repository topology; domain documentation into `docs/`** (D9, D10, D11): the docs
  moved to `docs/` by component with every reference rewritten, the nested `AGENTS.md` trimmed
  to their rules, short component READMEs. 2026-11-09, 102ff61 and the short READMEs after it.
- **T1. Merge TODO.md and firmware/TODO.md into TASKS.md**, with the session rule in AGENTS.md.
  2026-11-09, 2a5bfa1.
