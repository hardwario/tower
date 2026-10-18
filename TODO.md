# TODO

Cross-cutting work that is decided but not scheduled yet. Component-specific items live next to
the code (e.g. [firmware/TODO.md](firmware/TODO.md): FHSS in the product apps, a provisioned
ATSHA204A).

## Two-board verification of the 2026-10-17 batch

**Why.** The night of 2026-10-17 changed pairing (the node commits its key only on an ACKed
uplink, the 12 s re-pair hold, the SAS in `Paired`/`Joined`, the window rule), replay and nonce
safety (counter floors on moved keys, per-accept node lanes, host uplink dedup), the push
button's backoff and the gateway's request pacing. Everything that needs only one board was
checked on hardware (KATs, watchdog, gateway end to end, key floors, a join with no gateway in
range); nothing that needs a node and a gateway on air was.

**What to run** (a Core Module with `radio_push_button`, a Radio Dongle with
`radio_dongle_gateway`, `tower` from the same commit, EEPROM wiped first):
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
- The HIL gateway group (`just hil hil`), and a PPK2 look at the node's idle floor with the new
  watchdog pet interval (~7.4 s).

**Done when.** Each item passes on the bench and the results are noted in
`firmware/docs/test-plan.md`.

**Status 2026-10-18** (three-board bench, results in
[firmware/tools/hwtest/RESULTS-2026-10-18.md](firmware/tools/hwtest/RESULTS-2026-10-18.md)):
OTA pairing, lost confirm, re-pair, window end, backoff, key move and the HIL gateway group
pass. Open: the replay item (nothing can reboot the dongle under a running `tower gateway`; a
reboot RPC would make it testable) and the PPK2 look at the idle floor (no PPK2 on the bench).

## Test layers to add

[docs/test-strategy.md](docs/test-strategy.md): nightly HIL, long soak, power loss and brown-out,
energy per event, an attacker board, mutation testing, latency budgets, host-side snapshots and
contract tests, radio pre-compliance, environment, production test, release qualification.
Needs bench setup first (a self-hosted runner, a power-switching USB hub, the PPK2, a board
for the rogue firmware). Open findings to run down with it: a remote shell reply chunk lost when two nodes answer
at once (1 in 8 on the bench), and `radio_afa` delivering 14 of 19 confirmed sends.

## Compatibility gate (before the first users)

**Why.** Until TOWER has users, AGENTS.md says "no backward compatibility anywhere": formats and
protocols change freely and stale peers are only *detected* (schema tags), never supported. That
is right for development and wrong for a fleet in the field: sleeping nodes update slowly, a
gateway and its nodes will run different firmware during a rollout, and integrations read the
MQTT payloads. Decided 2026-10-17: keep compatibility out until then, and gate the first users on
this checklist.

**When.** Before the first external user, the first product shipment, or any public release
announcement, whichever comes first.

**Decide and reintroduce:**
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

## Retire the previous TOWER generation

**Why.** The new generation (this repository) replaces the C-SDK TOWER line, which still lives in
40+ repositories in the hardwario organization: `twr-sdk`, the `twr-radio-*` / `twr-lora-*`
application firmwares, `twr-gateway-radio-dongle`, `bch-firmware-tool`, `hardwario-playground`,
`hardwario-code`, `hardwario-tower-vscode-extension`, `twr-zephyr`, `twr-skeleton` and others.
Decided 2026-10-17: they will be made obsolete later; until then, work only on the new
generation and leave them untouched.

**When.** Not before the new generation covers what users of the old line rely on (at least the
product apps they deploy) and the compatibility gate above is done.

**What it takes.** An inventory of which old repositories have users and what replaces each; a
migration path for deployed devices (re-flash and re-pair, or a documented end of support);
README notices pointing to hardwario/tower; then archive (not delete) the old repositories so
existing links and releases keep working. Keep `twr-hardware` and `twr-enclosures` if the new
generation still targets the same modules.
