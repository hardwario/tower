# tower-protocol

Shared wire-format crate for the [HARDWARIO TOWER](https://www.hardwario.com/) link:
the single source of truth that the **firmware SDK** ([`firmware/`](../../firmware)), the
**host CLI** ([`cli/`](../../cli), the `tower` binary), and the **HIL bench harness**
([`hil/`](../../hil)) all build, so no end can drift on the bytes they exchange. It lives in
the [hardwario/tower](https://github.com/hardwario/tower) monorepo as `crates/protocol`.

`no_std`, no-alloc, builds for `thumbv6m-none-eabi` (the Cortex-M0+ target) as well as the host.

## What's in it

- **Console framing**: COBS frame sync (`0x00` delimiter) + a trailing CRC-32 over the inner
  frame (`crc.rs`), and `encode_frame` / `FrameDecoder` / `decode_frame` / `decode_msg`
  (`lib.rs`). The decoder checks the CRC *before* the version and type, so a corrupt frame is
  always `BadCrc` and `BadVersion` only ever means a CRC-valid frame from another protocol
  version; `decode_msg` also rejects a body with trailing bytes (`Malformed`). The link is
  *always* framed, so a raw serial monitor shows binary; decode with `tower logs`.
- **Console message schema** (`msg.rs`, `MsgType`): the `Hello` handshake (firmware name,
  version, session id, wire fingerprint), logs, prints, structured events, the interactive-shell messages
  (`ShellCommand` / `ShellResponse` / `ShellComplete` / `ShellCompletions`), and the gateway
  link: `Uplink` (a decrypted radio uplink forwarded verbatim), `MgmtRequest` / `MgmtResponse`
  (req_id-correlated management ops with chunked replies) and `RadioStat` (channel RSSI + TX
  outcomes). Serialized with **postcard**, which is *not* self-describing. That is the whole
  reason this crate is shared: all ends must hold the exact same struct definitions.
- **Management schema** (`mgmt.rs`): the `MgmtOp` set a host sends in `MgmtRequest`
  (describe / role probing, node registry CRUD + key reveal, OTA pairing, downlink queue,
  stats cadence, cable provisioning, join) and the typed reply records streamed in
  `MgmtResponse.data`, plus the result / TX-outcome code sets.
- **Radio application schema** (`radio.rs`): what node apps put inside their encrypted radio
  frames (`NodeMsg`: info / button / temperature / accel / shell chunk; `NodeCmd`: shell). The
  gateway forwards it opaquely, so it is versioned on its own: every envelope leads with
  `RADIO_SCHEMA_VERSION`, independent of the console `PROTOCOL_VERSION`.
- **Wire fingerprint** (`fingerprint.rs`, `WIRE_FINGERPRINT`): a compile-time structural hash of
  all of the above (every `MsgType` value with its payload schema, the mgmt records, the radio
  schema). Names are ignored, so a rename keeps it; field / variant order, shapes, integer widths
  and nesting are not. Devices announce it in `Hello` and in the `Describe` reply (`DeviceInfo`);
  a host compares it with its own and treats a difference like a version mismatch. The module
  docs list exactly what is and is not covered (heapless capacities, constant values and the
  framing are not).

## Use it

Every consumer in the monorepo depends on it **by path** (no git tag, no crates.io publish).
The host crates inherit it from the root workspace; the firmware workspace points at it
directly:

```toml
# cli/Cargo.toml, hil/Cargo.toml, crates/kv/Cargo.toml (members of the root host workspace)
tower-protocol.workspace = true

# firmware/Cargo.toml (its own workspace)
tower-protocol = { path = "../crates/protocol" }
```

Because postcard isn't self-describing, **all ends must build the same schema**. With path
dependencies that is a compile-time fact: the firmware, the `tower` CLI and the HIL harness
compile this one source tree, so a schema edit reaches all of them in the same commit. What a
build cannot catch is an *installed* artifact built from an older tree: a `tower` binary or a
flashed firmware from before a wire change. After a wire change, reflash the device and rebuild
the CLI (`just install-cli`, or `cargo install --locked --path cli` at the repo root); the
`PROTOCOL_VERSION` bump turns the mismatch into a clean `BadVersion` instead of a silent
mis-decode, and `WIRE_FINGERPRINT` catches the case where two trees differ in schema under the
same `PROTOCOL_VERSION` (`tower` shows the same mismatch banner and exit code).

Checks, from the repo root:

```bash
cargo test -p tower-protocol
cargo clippy -p tower-protocol --all-targets -- -D warnings
cargo build -p tower-protocol --target thumbv6m-none-eabi     # stays no_std / MCU-buildable
python3 crates/protocol/tools/check_wire_bump.py              # wire-bump guard
python3 crates/protocol/tools/check_wire_bump.py --self-test
```

## Evolving the schema (read before changing `msg.rs`, `mgmt.rs` or `radio.rs`)

postcard encodes structs by **field order** and enums by **variant index**; names never hit the
wire. The rules, enforced by the golden-vector tests (`tests/golden.rs` for the console,
`tests/radio_golden.rs` for the radio schema) and the CI wire-bump guard
(`tools/check_wire_bump.py`, which fails any edited, renamed or removed golden unless the
surface's version constant strictly increased; `--self-test` exercises it):

1. **Never reorder or remove** fields of an existing struct or variants of an existing enum.
2. **Appending** a field to a struct, a variant to an enum, or a new `MsgType` is still a wire
   change: old decoders cannot read the new bytes.
3. **Any wire change bumps `PROTOCOL_VERSION`** (`src/lib.rs`; for `radio.rs`,
   `RADIO_SCHEMA_VERSION` instead) and regenerates the golden vectors. Decoders hard-reject frames from
   a different version (`Error::BadVersion`), but that only protects you if the bump happened:
   postcard itself detects nothing, and two builds whose schemas differ under the *same* version
   silently mis-decode (wrong field values, no error). That is why the bump is mandatory, why
   the goldens + guard make it mechanical, and why every consumer builds this one tree.
4. Renaming a field or variant is wire-transparent (names aren't encoded): no version bump, but
   it is a source-breaking change for the consumers, so fix them in the same commit (the path
   dependencies make the build fail until you do) and give it a new crate version + tag.
   `WIRE_FINGERPRINT` ignores names, so a rename leaves it unchanged.
5. **Any other schema change moves the fingerprint.** Its two halves are pinned as goldens
   (`fingerprint::CONSOLE_HASH` in `tests/golden.rs`, `fingerprint::RADIO_HASH` in
   `tests/radio_golden.rs`), so the guard demands the matching version bump even for a change no
   byte vector exercises (e.g. widening a `u8` field whose golden value still encodes the same).
   Never add a serde attribute that changes the encoding (`skip`, `with`, `flatten`, ...): the
   `Schema` derive cannot see it, and the fingerprint would no longer describe the bytes.
6. Ship the change with the runbook in `AGENTS.md`: one commit covering the protocol and every
   consumer it touches, then the `protocol-vX.Y.Z` tag.

## Versioning

`protocol-vMAJOR.MINOR.PATCH` git tags (see `CHANGELOG.md`; before the monorepo they were plain
`vX.Y.Z` tags in the old tower-protocol repository, renamed on import). The tags mark protocol
releases and are the wire-bump guard's baseline; nothing is published from them. The console `PROTOCOL_VERSION` (a 3-bit field
in every frame header) gates console wire compatibility independently of the crate version; a
decoder rejects CRC-valid frames whose `PROTOCOL_VERSION` or `MsgType` it doesn't know. The radio
application schema carries its own `RADIO_SCHEMA_VERSION` byte in every envelope.

| Tag | `PROTOCOL_VERSION` | Contents |
|---|---|---|
| `protocol-v1.6.0` | 4 | the wire fingerprint: `WIRE_FINGERPRINT` (compile-time structural schema hash, names ignored); `Hello` gains `wire_fingerprint`; `DeviceInfo` gains `wire_fingerprint` + `firmware_version`; radio schema unchanged (`RADIO_SCHEMA_VERSION = 2`); first release built in the monorepo |
| `protocol-v1.5.1` | 3 | hardening, wire byte-identical: CRC checked before version/type (`BadVersion` = genuine mismatch), trailing bytes rejected (`Malformed`), full golden coverage, per-golden wire-bump guard |
| `protocol-v1.5.0` | 3 | unit-suffix field renames (console wire unchanged); radio schema v2 (`RADIO_SCHEMA_VERSION = 2`): `NodeCmd::Shell` gains `epoch` (downlink re-delivery dedup) |
| `protocol-v1.4.0` | 3 | API rename to `addr` / `gw_addr` / `node_addr` terminology (wire unchanged) |
| `protocol-v1.3.0` | 3 | the gateway link: `Uplink` / `MgmtRequest` / `MgmtResponse` / `RadioStat` + the `mgmt` op schema; new `radio` application schema (own `RADIO_SCHEMA_VERSION = 1`, guarded independently by `tests/radio_golden.rs`) |
| `protocol-v1.2.1` | 2 | deps: cobs 0.5, MSRV declared (wire byte-identical) |
| `protocol-v1.2.0` | 2 | crate API: exported `MAX_PAYLOAD`, non_exhaustive `Error`, doc'd evolution rules (frames byte-identical to v1.1.0) |
| `protocol-v1.1.0` | 2 | `Hello` carries `firmware_name` / `firmware_version` / `session_id`; adds typed one-call `decode_msg` + `Error::Malformed` |
| `protocol-v1.0.0` | 1 | initial release: console framing (COBS + CRC-32) + the postcard message schema |

## License

MIT © 2026 HARDWARIO a.s.
