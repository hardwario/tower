# tower-protocol

Shared wire-format crate for the [HARDWARIO TOWER](https://www.hardwario.com/) link:
the single source of truth that the **firmware**
([`tower-firmware`](https://github.com/hardwario/tower-firmware)), the **host CLI**
([`tower-cli`](https://github.com/hardwario/tower-cli)), and the **HIL bench harness**
([`tower-hil`](https://github.com/hardwario/tower-hil)) all depend on, so no end
can drift on the bytes they exchange.

`no_std`, no-alloc, builds for `thumbv6m-none-eabi` (the Cortex-M0+ target) as well as the host.

## What's in it

- **Console framing**: COBS frame sync (`0x00` delimiter) + a trailing CRC-32 over the inner
  frame (`crc.rs`), and `encode_frame` / `FrameDecoder` / `decode_frame` / `decode_msg`
  (`lib.rs`). The decoder checks the CRC *before* the version and type, so a corrupt frame is
  always `BadCrc` and `BadVersion` only ever means a CRC-valid frame from another protocol
  version; `decode_msg` also rejects a body with trailing bytes (`Malformed`). The link is
  *always* framed, so a raw serial monitor shows binary; decode with `tower logs`.
- **Console message schema** (`msg.rs`, `MsgType`): the `Hello` handshake (firmware name,
  version, session id), logs, prints, structured events, the interactive-shell messages
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

## Use it

It is consumed as a **pinned git dependency** (no crates.io publish):

```toml
[dependencies]
tower-protocol = { git = "https://github.com/hardwario/tower-protocol", tag = "v1.5.1" }
```

Because postcard isn't self-describing, **all ends must build the same version**: pin the same
tag in the firmware, the host CLI, and the HIL harness, and bump it in lockstep with any wire
change. A tag mismatch is not guaranteed to error: two tags with the same `PROTOCOL_VERSION`
but a different schema would *silently* mis-decode.

### Developing it alongside a consumer

To hack on the protocol and have a consumer pick up your local edits without re-tagging, add a
machine-local [`paths` override](https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html#paths-overrides)
(it is *not* committed). `tower-cli` keeps it in a gitignored `.cargo/config.toml`; for
`tower-firmware` (whose `.cargo/config.toml` is committed for the build target) put it in your
`~/.cargo/config.toml` instead:

```toml
paths = ["/absolute/path/to/tower-protocol"]
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
   the goldens + guard make it mechanical, and why all consumers pin the same tag.
4. Renaming a field or variant is wire-transparent (names aren't encoded): no version bump, but
   it is a source-breaking change for consumers, so it still needs a new crate version + tag.
5. Ship the change with the full lockstep runbook in `AGENTS.md`: new tag, then re-pin
   `tower-firmware` (two manifests), `tower-cli`, and `tower-hil` in the same change-set.

## Versioning

`vMAJOR.MINOR.PATCH` git tags (see `CHANGELOG.md`). The console `PROTOCOL_VERSION` (a 3-bit field
in every frame header) gates console wire compatibility independently of the crate version; a
decoder rejects CRC-valid frames whose `PROTOCOL_VERSION` or `MsgType` it doesn't know. The radio
application schema carries its own `RADIO_SCHEMA_VERSION` byte in every envelope.

| Tag | `PROTOCOL_VERSION` | Contents |
|---|---|---|
| `v1.5.1` | 3 | hardening, wire byte-identical: CRC checked before version/type (`BadVersion` = genuine mismatch), trailing bytes rejected (`Malformed`), full golden coverage, per-golden wire-bump guard |
| `v1.5.0` | 3 | unit-suffix field renames (console wire unchanged); radio schema v2 (`RADIO_SCHEMA_VERSION = 2`): `NodeCmd::Shell` gains `epoch` (downlink re-delivery dedup) |
| `v1.4.0` | 3 | API rename to `addr` / `gw_addr` / `node_addr` terminology (wire unchanged) |
| `v1.3.0` | 3 | the gateway link: `Uplink` / `MgmtRequest` / `MgmtResponse` / `RadioStat` + the `mgmt` op schema; new `radio` application schema (own `RADIO_SCHEMA_VERSION = 1`, guarded independently by `tests/radio_golden.rs`) |
| `v1.2.1` | 2 | deps: cobs 0.5, MSRV declared (wire byte-identical) |
| `v1.2.0` | 2 | crate API: exported `MAX_PAYLOAD`, non_exhaustive `Error`, doc'd evolution rules (frames byte-identical to v1.1.0) |
| `v1.1.0` | 2 | `Hello` carries `firmware_name` / `firmware_version` / `session_id`; adds typed one-call `decode_msg` + `Error::Malformed` |
| `v1.0.0` | 1 | initial release: console framing (COBS + CRC-32) + the postcard message schema |

## License

MIT © 2026 HARDWARIO a.s.
