# tower-protocol

Shared wire-format crate for the [HARDWARIO TOWER](https://www.hardwario.com/) link —
the single source of truth that the **firmware**
([`tower-firmware`](https://github.com/hardwario/tower-firmware)), the **host CLI**
([`tower-cli`](https://github.com/hardwario/tower-cli)), and the **HIL bench harness**
([`tower-hil`](https://github.com/hardwario/tower-hil)) all depend on, so no end
can drift on the bytes they exchange.

`no_std`, no-alloc, builds for `thumbv6m-none-eabi` (the Cortex-M0+ target) as well as the host.

## What's in it

- **Console framing** — COBS frame sync (`0x00` delimiter) + a trailing CRC over the inner
  frame (`crc.rs`), and `encode_frame` / `FrameDecoder` / `decode_frame` / `decode_msg`
  (`lib.rs`). The link is *always* framed, so a raw serial monitor shows binary — decode with
  `tower logs`.
- **Console message schema** (`msg.rs`, `MsgType`) — the `Hello` handshake (firmware name,
  version, session id), logs, structured events, and the interactive-shell messages
  (`ShellCommand` / `ShellResponse` / `ShellComplete` / `ShellCompletions`). Serialized with
  **postcard**, which is *not* self-describing — which is the whole reason this crate is shared:
  all ends must hold the exact same struct definitions.

## Use it

It is consumed as a **pinned git dependency** (no crates.io publish):

```toml
[dependencies]
tower-protocol = { git = "https://github.com/hardwario/tower-protocol", tag = "v1.3.0" }
```

Because postcard isn't self-describing, **all ends must build the same version** — pin the same
tag in the firmware, the host CLI, and the HIL harness, and bump it in lockstep with any wire
change.

### Developing it alongside a consumer

To hack on the protocol and have a consumer pick up your local edits without re-tagging, add a
machine-local [`paths` override](https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html#paths-overrides)
(it is *not* committed). `tower-cli` keeps it in a gitignored `.cargo/config.toml`; for
`tower-firmware` (whose `.cargo/config.toml` is committed for the build target) put it in your
`~/.cargo/config.toml` instead:

```toml
paths = ["/absolute/path/to/tower-protocol"]
```

## Evolving the schema (read before changing `msg.rs`)

postcard encodes structs by **field order** and enums by **variant index** — names never hit the
wire. The rules, enforced by the golden-vector tests (`tests/golden.rs`) and the CI wire-bump
guard (`tools/check_wire_bump.py`):

1. **Never reorder or remove** fields of an existing struct or variants of an existing enum.
2. **Appending** a field to a struct, a variant to an enum, or a new `MsgType` is still a wire
   change — old decoders cannot read the new bytes.
3. **Any wire change bumps `PROTOCOL_VERSION`** (`src/lib.rs`) and regenerates the golden
   vectors. Decoders hard-reject frames from a different `PROTOCOL_VERSION`, so a mismatch is a
   visible error, never a silent mis-decode.
4. Renaming a field or variant is wire-transparent (names aren't encoded) — no version bump, but
   it is a source-breaking change for consumers, so it still needs a new crate version + tag.
5. Ship the change with the full lockstep runbook in `CLAUDE.md`: new tag, then re-pin
   `tower-firmware` (two manifests), `tower-cli`, and `tower-hil` in the same change-set.

## Versioning

`vMAJOR.MINOR.PATCH` git tags (see `CHANGELOG.md`). The console `PROTOCOL_VERSION` (a 3-bit field
in every frame header) gates console wire compatibility independently of the crate version; a
decoder rejects frames whose `PROTOCOL_VERSION` or `MsgType` it doesn't know.

| Tag | `PROTOCOL_VERSION` | Contents |
|---|---|---|
| `v1.3.0` | 3 | the gateway link: `Uplink` / `MgmtRequest` / `MgmtResponse` / `RadioStat` + the `mgmt` op schema; new `radio` application schema (own `RADIO_SCHEMA_VERSION = 1`, guarded independently by `tests/radio_golden.rs`) |
| `v1.2.1` | 2 | deps: cobs 0.5, MSRV declared (wire byte-identical) |
| `v1.2.0` | 2 | crate API: exported `MAX_PAYLOAD`, non_exhaustive `Error`, doc'd evolution rules (frames byte-identical to v1.1.0) |
| `v1.1.0` | 2 | `Hello` carries `firmware_name` / `firmware_version` / `session_id`; adds typed one-call `decode_msg` + `Error::Malformed` |
| `v1.0.0` | 1 | initial release: console framing (COBS + CRC-32) + the postcard message schema |

## License

MIT © 2026 HARDWARIO a.s.
