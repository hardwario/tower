# TODO

Cross-cutting work that is decided but not scheduled yet. Component-specific items live next to
the code (e.g. [firmware/TODO.md](firmware/TODO.md): FHSS in the product apps, a provisioned
ATSHA204A).

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
