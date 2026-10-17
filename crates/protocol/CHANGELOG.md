# Changelog: tower-protocol

All notable changes to the wire format and the crate API. The wire format is postcard
(not self-describing): every entry that says **wire change** implies a `PROTOCOL_VERSION` (or,
for the radio schema, `RADIO_SCHEMA_VERSION`) bump. Since the monorepo, the firmware, the
`tower` CLI and the HIL harness build this crate by path, so they move in the same commit;
the entries from v1.5.1 down describe the earlier multi-repo era, when each consumer re-pinned a
git tag.

## Unreleased

**Moved into the [hardwario/tower](https://github.com/hardwario/tower) monorepo** as
`crates/protocol`. No wire change and no API change (`PROTOCOL_VERSION` stays 3,
`RADIO_SCHEMA_VERSION` stays 2, every golden vector is byte-identical).

- Consumed **by path**: the firmware SDK (`firmware/`), the `tower` CLI (`cli/`), the HIL
  harness (`hil/`) and `tower-kv` (`crates/kv`) all build this source tree, so the
  wire-format lockstep is a compile-time fact; the tag pins and their checkers are gone.
- Release tags are renamed `protocol-vX.Y.Z` (the imported `v1.0.0` ... `v1.5.1` are now
  `protocol-v1.0.0` ... `protocol-v1.5.1`); `tools/check_wire_bump.py` takes its baseline from
  the latest `protocol-v*` tag. No GitHub Release or auto-tag job any more: the crate is an
  internal path dependency.
- The crate joins the repo-wide rustfmt gate (one root `rustfmt.toml`, max_width 110); the
  reformat is whitespace-only, which the guard's per-golden fingerprints ignore.

## v1.5.1 (2026-10-17)

**Hardening, wire UNCHANGED** (`PROTOCOL_VERSION` stays 3, `RADIO_SCHEMA_VERSION` stays 2;
every pre-existing golden vector is byte-identical, confirmed by the reworked
`wire-bump-guard` against v1.5.0). No API signature changes and no new `Error` variant; the
behaviour changes are all on the receive side:

- **`decode_frame` checks the CRC before the version and type.** It used to check the version
  first, so a corrupt or partial frame whose first byte's top 3 bits weren't
  `PROTOCOL_VERSION` surfaced as `BadVersion` instead of `BadCrc`, and consumers treat a single
  `BadVersion` as a lockstep mismatch (tower-cli's mismatch banner / exit code). The order is
  now `TooShort`/`TooLong` → `BadCrc` → `BadVersion` → `BadType`, so `BadVersion` really does
  mean "CRC-valid frame from a different protocol version". The radio envelope's version check
  is unchanged: it has no CRC of its own, because AES-CCM in the net layer already
  authenticated it. *A test that forges a foreign-version frame must now give it a valid CRC,
  or it reads as `BadCrc`.*
- **Trailing bytes are rejected.** `decode_msg`, `radio::decode_node_msg` and
  `radio::decode_node_cmd` now return `Error::Malformed` when a body deserializes but leaves
  bytes behind. `postcard::from_bytes` silently ignored them, which half-read a producer whose
  schema had grown. `Malformed`'s doc and `Display` text say so.
- **Golden coverage** (tests only): byte goldens for every console message type (`Print`,
  `Event`, `ShellCommand`, `ShellComplete`, `ShellCompletions`/`Candidate`,
  `RadioStat::Channel`, plus `Tx` without an ACK) and every `MgmtOp` variant (including both
  arms of `Provision.addr` with distinct `band`/`channel`). There are distinct-value companions
  for the vectors where swapping two fields left the bytes unchanged (`ShellResponse`,
  `MgmtResponse`, `DeviceInfo`, radio `NodeShellChunk`), plus `NodeInfo` with
  `Some(battery_mv)`. Variant-order pins are now `(Variant, 0x..)` hex tables the guard can see
  (they used to be `i as u8`). New tests also cover the CRC-first order and trailing-byte
  rejection.
- **`tools/check_wire_bump.py` rewritten.** It now compares each golden fn's fingerprint
  (its literals plus `Type::Variant` paths, ignoring comments, formatting, assertion messages
  and field names) instead of "the old hex is still a subsequence of the file". These used to
  pass and now fail unless the version strictly increased: appending a field, a new sibling
  repeating a modified vector's old bytes, a reorder hidden by editing the test array, and a
  renamed or removed golden or golden file. A version decrease always fails. Dropping an entry
  from `SURFACES` doesn't help, because the baseline's own list is checked too. New flags:
  `--base REF` and `--self-test` (20 scenarios). The default CI invocation is unchanged.
- Docs: README pin example → `v1.5.1`, version table completed (v1.4.0 to v1.5.1), "What's in it"
  covers the gateway types and the `mgmt` / `radio` modules, and the "never a silent mis-decode"
  claim is corrected. A mismatch is only visible if the version was bumped; a schema change under
  the same version silently mis-decodes, which is why the bump is mandatory.
- Toolchain: `rust-version` 1.85 → 1.99.1 and `rust-toolchain.toml` pinned to 1.99.1 (was
  `stable`).

## v1.5.0 (2026-07-12)

**API rename, wire UNCHANGED** (postcard positional; `PROTOCOL_VERSION` stays 3,
golden vectors byte-identical): unit suffixes dropped from field names. The unit
stays documented on the field, it just no longer rides in the name.

- `msg::Uplink.rssi_dbm` → `rssi`; `msg::RadioStat::Channel.rssi_dbm` → `rssi`,
  `msg::RadioStat::Tx.ack_rssi_dbm` → `ack_rssi`.
- `mgmt::NodeEntry`: `last_seen_s` → `last_seen`, `rssi_dbm` → `rssi`.
- `mgmt::MgmtOp`: `PairingOpen`/`JoinOpen` `window_s` → `window`;
  `QueuePush.ttl_s` → `ttl`.
- `mgmt::QueueEntry`: `age_s` → `age`, `ttl_s` → `ttl`.

`LAST_SEEN_NEVER` and the constants are unchanged.

**Radio schema change: `RADIO_SCHEMA_VERSION` 1 → 2** (the independently-versioned
`src/radio.rs` surface, *not* `PROTOCOL_VERSION`; `radio_golden.rs` regenerated).
`radio::NodeCmd::Shell` gains an `epoch: u32` field. The node de-duplicates the
gateway's at-least-once downlink re-deliveries on `(epoch, cmd_id)`; the host stamps a
random `epoch` per process, so a freshly-restarted host reusing a low `cmd_id` can no
longer be mistaken for a re-delivery a node already ran (the "empty checkmark" reply).
The reply (`NodeMsg::Shell`) is unchanged; correlation stays within one host process.
A **push-button node reflash + `tower-cli` rebuild** are required to interoperate (the
gateway firmware is a transparent bridge for `NodeCmd` and needs no change).

Both surfaces move together → one minor bump + the usual coordinated re-pin of all
three consumers (`tower-firmware`, `tower-cli`, `tower-hil`).

## v1.4.0 (2026-07-12)

**API rename, wire UNCHANGED** (`PROTOCOL_VERSION` stays 3; postcard is positional, so
these field renames do not move a single byte: the golden vectors are byte-identical and
the `wire-bump-guard` confirms it). This is a **source-breaking** change for consumers, hence
a minor bump and a coordinated re-pin of all three (`tower-firmware`, `tower-cli`, `tower-hil`).

Unifies the radio-address terminology across the ecosystem (no more `_id` for an address):

- `mgmt::Provision`: `my_id` → `addr`, `gw_id` → `gw_addr`.
- `mgmt::DeviceInfo`: `net_id` → `addr`, `gw_id` → `gw_addr`.
- `mgmt::MgmtOp::{NodeAdd,NodeRemove,NodeUpdate,NodeRevealKey}`: field `id` → `addr`.
- `mgmt::{NodeEntry,NodeKey}`: `id` → `addr`. `mgmt::Paired`: `node_id` → `addr`.
- `mgmt::ProvisionAck`: `id` → `addr`. `mgmt::Joined`: `gw_id` → `gw_addr`.
- `mgmt::MgmtOp::{QueuePush,QueueList,QueueDrop}` + `mgmt::QueueEntry`: `node` → `node_addr`
  (the node is a *target* here, not the record's subject).

Rule applied: a field's plain name is `addr` when its enclosing type already names the entity
(`NodeAdd`, `DeviceInfo`-self, …); it is qualified (`gw_addr`, `node_addr`) only to disambiguate.
Idiomatic frame fields `src`/`dest`, the queue-item `item`, and the non-address ids
(`req_id`, `cmd_id`, `session_id`) are unchanged.

## v1.3.0 (2026-07-11)

**Wire change** (`PROTOCOL_VERSION` 2 → 3): the gateway link.

- New frame types: `Uplink` (a decrypted radio uplink forwarded verbatim by the
  gateway: src, counter, rssi/lqi, opaque payload), `MgmtRequest` / `MgmtResponse`
  (req_id-correlated management ops with `ShellResponse`-style chunked replies),
  and `RadioStat` (ambient channel-RSSI samples + per-TX outcome reports).
- New `mgmt` module: the complete management op/record schema: device describe
  (role probing), registry CRUD + key reveal, OTA pairing window, downlink queue
  (push/list/drop), stats cadence, node provisioning over the cable, and the
  result/TX-outcome code sets. Designed complete for the gateway feature set;
  appending an op later is a wire change by the usual rules.
- New `radio` module: the **radio application schema** node apps put inside
  encrypted radio frames (`NodeMsg`: info/button/temperature/accel/shell-chunk;
  `NodeCmd`: shell) with its own leading version byte
  (`RADIO_SCHEMA_VERSION = 1`). Versioned independently of the console framing:
  it rides opaquely through the gateway, so only node firmware + the host CLI
  re-pin on a radio-schema change. Golden vectors live in
  `tests/radio_golden.rs`; `tools/check_wire_bump.py` now guards both surfaces,
  each against its own version constant.
- Golden vectors regenerated for wire v3 (header version bits changed); new
  vectors + variant-order pins + MTU/frame-budget capacity tests for every new
  type.

## v1.2.1 (2026-07-05)

**No wire change.** Dependency + metadata freshening (patch):

- `cobs` 0.3 → 0.5 (latest): drop-in; the golden vectors and the corruption/arbitrary-byte
  fuzz suites verify the wire stays byte-identical.
- Declared `rust-version = "1.85"` (edition 2024 floor) and pinned the toolchain channel
  (`rust-toolchain.toml`, stable + thumbv6m target).

## v1.2.0 (2026-07-05)

**No wire change**: `PROTOCOL_VERSION` stays 2; frames are byte-identical to v1.1.0.
API + hardening release (source-breaking for `Error` matches, hence the minor bump):

- Exported `MAX_PAYLOAD`, the per-frame postcard payload budget
  (`MAX_FRAME − header − CRC`) every consumer previously re-derived by hand.
- `Error` is now `#[non_exhaustive]` (add a `_` arm): new failure modes can land
  without a source break. The message enums stay exhaustive on purpose: under
  lockstep, a new message variant *should* break consumer matches at compile time.
- Documented the `seq` semantics and the schema-evolution rules in the crate rustdoc
  (previously only in dev-internal notes).
- Tests: `Error::Malformed` path covered; `Level` variant order pinned (all five
  indices); a 200k-byte arbitrary-input decoder fuzz (no-panic guarantee for
  `FrameDecoder` + `decode_msg` under attacker-controlled bytes).
- `tools/check_wire_bump.py` accepts additive golden coverage (subsequence rule)
  while still failing any change to existing vectors without a version bump.

## v1.1.0 (2026-07-04)

**Wire change**: `PROTOCOL_VERSION` 1 → 2.

- `Hello` extended: `{ protocol_version, firmware_name, firmware_version, session_id: u32 }`,
  so the host banner can now show what firmware it is talking to and detect device reboots
  (`session_id` changes).
- Added `decode_msg`: typed one-call decode of an inner frame into the `Msg` enum.
- Added `Error::Malformed` (CRC-valid frame whose body fails to deserialize).
- Golden vectors regenerated for the new schema.

## v1.0.0 (2026-07-02)

Initial (re-baselined) release.

- COBS framing with `0x00` delimiters + CRC-32 over the inner frame.
- Postcard message schema: `Hello`, `Log`, `Print`, `Event`, `Dropped`, `ShellCommand`,
  `ShellResponse`, `ShellComplete`, `ShellCompletions`.
- Codec hardening: over-long frames rejected on both encode and decode, actionable
  version-mismatch error, golden wire-byte vectors + CRC check-vector tests.
- Note: the FOTA subsystem was removed and the crate re-baselined at this version; the
  previous `v1.0.0`/`v1.1.0` tags from the FOTA era were deleted and `v1.0.0` re-cut
  (2026-07-02). Anything resolving the old SHAs must re-pin.
