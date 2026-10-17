# tower-protocol

Shared wire-format crate for the [HARDWARIO TOWER](https://www.hardwario.com/) link:
the single source of truth that the **firmware SDK** ([`firmware/`](../../firmware)), the
**host CLI** ([`cli/`](../../cli), the `tower` binary), and the **HIL bench harness**
([`hil/`](../../hil)) all build, so no end can drift on the bytes they exchange. It lives in
the [hardwario/tower](https://github.com/hardwario/tower) monorepo as `crates/protocol`.

`no_std`, no-alloc, builds for `thumbv6m-none-eabi` (the Cortex-M0+ target) as well as the host.

## What's in it

- **Console framing**: COBS frame sync (`0x00` delimiter) + a trailing CRC-32 over the inner
  frame (`crc.rs`), a 5-byte header `type(1) ‖ tag(2, LE) ‖ seq(2, LE)`, and `encode_frame` /
  `FrameDecoder` / `decode_frame` / `decode_msg` (`lib.rs`). The decoder checks the CRC *before*
  the schema tag and the type, so a corrupt frame is always `BadCrc` and `SchemaMismatch` only
  ever means a CRC-valid frame from a peer built from another checkout; `decode_msg` also rejects
  a body with trailing bytes (`Malformed`). The link is *always* framed, so a raw serial monitor
  shows binary; decode with `tower logs`.
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
  gateway forwards it opaquely, so it is tagged on its own: every envelope leads with
  `RADIO_TAG` (2 bytes, LE), independent of the console's `CONSOLE_TAG`.
- **Schema tags** (`fingerprint.rs`): a compile-time structural hash of each surface (every
  `MsgType` value with its payload schema and the mgmt records for the console; `NodeMsg` /
  `NodeCmd` for the radio), folded to the 16-bit `CONSOLE_TAG` / `RADIO_TAG`. Names are ignored,
  so a rename keeps them; field / variant order, shapes, integer widths and nesting are not. The
  module docs list exactly what is and is not covered (heapless capacities, constant values and
  the framing are not).

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
build cannot catch is an *installed* artifact built from another tree: a `tower` binary or a
flashed firmware from before a schema change. The schema tags turn that into a clean
`SchemaMismatch` instead of a silent mis-decode (`tower` shows a mismatch banner and exits 125);
the remedy is to rebuild both from the same checkout (`just install-cli`, reflash the board).

Checks, from the repo root:

```bash
cargo test -p tower-protocol
cargo clippy -p tower-protocol --all-targets -- -D warnings
cargo build -p tower-protocol --target thumbv6m-none-eabi     # stays no_std / MCU-buildable
```

## Changing the schema (read before changing `msg.rs`, `mgmt.rs` or `radio.rs`)

postcard encodes structs by **field order** and enums by **variant index**; names never hit the
wire. To change the wire, change the types, fix every caller the compiler points at (firmware,
cli, hil, kv) in the same commit, and rebuild every end from that checkout. There is nothing to
bump: the tags are derived from the schema, so any change that reaches the bytes moves the
affected tag, and a stale peer is rejected by its tag. There is no compatibility with older
peers and no version negotiation.

1. A rename is wire-transparent (names aren't encoded) and leaves the tags as they are.
2. Never add a serde attribute that changes the encoding (`skip`, `with`, `flatten`, ...): the
   `Schema` derive cannot see it, and the tags would no longer describe the bytes.
3. A framing change (the header, COBS, the CRC) is not covered by the tags: frames from the other
   side then fail their CRC or carry their tag elsewhere. `tower` reports a stream where every
   frame fails its CRC as a likely framing difference; keep the CRC trailer as it is so a
   foreign *schema* stays recognizable.
4. Tests keep the framing pinned (`tests/framing.rs`, byte goldens built with an explicit tag so
   they never move with the schema), every message, op and record round-trips, and the hasher's
   properties are pinned (`tests/fingerprint.rs`); there are no per-message byte goldens.

## History

Releases used to be tagged `protocol-vX.Y.Z` with hand-bumped version constants (a 3-bit header
version, a radio schema version byte, a 32-bit fingerprint in `Hello`). Those tags stay in the
repository as history; no new ones are made. The version constants were replaced by the schema
tags, and the crate is versioned `0.0.0` (internal, never published).

## License

MIT © 2026 HARDWARIO a.s.
