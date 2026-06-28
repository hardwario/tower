# tower-protocol

Shared wire-format crate for the [HARDWARIO TOWER](https://www.hardwario.com/) link —
the single source of truth that the **firmware**
([`tower-firmware`](https://github.com/hardwario/tower-firmware)) and the **host CLI**
([`tower-cli`](https://github.com/hardwario/tower-cli)) both depend on, so the two ends
can never drift on the bytes they exchange.

`no_std`, no-alloc, builds for `thumbv6m-none-eabi` (the Cortex-M0+ target) as well as the host.

## What's in it

- **Console framing** — COBS frame sync (`0x00` delimiter) + a trailing CRC over the inner
  frame (`crc.rs`), and `encode_frame` / `FrameDecoder` / `decode_frame` (`lib.rs`). The link is
  *always* framed, so a raw serial monitor shows binary — decode with `tower logs`.
- **Console message schema** (`msg.rs`, `MsgType`) — logs, structured events, and the
  interactive-shell messages (`ShellCommand` / `ShellResponse` / `ShellComplete` /
  `ShellCompletions`). Serialized with **postcard**, which is *not* self-describing — which is
  the whole reason this crate is shared: both ends must hold the exact same struct definitions.
- **FOTA transport messages** (`FotaReq` / `FotaData`, raw payloads via `encode_frame_raw`) —
  the host-proxy image link used by `tower fota serve`.
- **Signed FOTA manifest** (`fota.rs`, `Manifest`) — a fixed 52-byte little-endian layout +
  64-byte Ed25519 signature (116-byte signed blob), plus `verify_signed` (Ed25519 via
  [`salty`](https://crates.io/crates/salty)) behind the **`verify`** feature. The device
  verifies a signed manifest in the bootloader before arming an A/B swap; the matching host
  signer is `tools/fota-sign` in the firmware repo.

The `verify` feature is **off by default** so pure-wire consumers (the host CLI's parsing path,
the firmware app) take no crypto dependency; the bootloader turns it on.

## Use it

It is consumed as a **pinned git dependency** (no crates.io publish):

```toml
[dependencies]
tower-protocol = { git = "https://github.com/hardwario/tower-protocol", tag = "v0.2.0" }
# the bootloader/verifier adds:
# tower-protocol = { git = "...", tag = "v0.2.0", features = ["verify"] }
```

Because postcard isn't self-describing, **both ends must build the same version** — pin the same
tag in the firmware and the host CLI, and bump it in lockstep with any wire change.

### Developing it alongside a consumer

To hack on the protocol and have a consumer pick up your local edits without re-tagging, add a
machine-local [`paths` override](https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html#paths-overrides)
(it is *not* committed). `tower-cli` keeps it in a gitignored `.cargo/config.toml`; for
`tower-firmware` (whose `.cargo/config.toml` is committed for the build target) put it in your
`~/.cargo/config.toml` instead:

```toml
paths = ["/absolute/path/to/tower-protocol"]
```

## Versioning

`vMAJOR.MINOR.PATCH` git tags. The console `PROTOCOL_VERSION` (a byte in every frame) gates
console wire compatibility independently of the crate version; a decoder rejects frames whose
`PROTOCOL_VERSION` or `MsgType` it doesn't know.

| Tag | Adds |
|---|---|
| `v0.1.0` | console framing + message schema |
| `v0.2.0` | FOTA `FotaReq`/`FotaData` messages + the signed `Manifest` (+ `verify` feature) |

## License

MIT © HARDWARIO a.s.
