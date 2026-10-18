# tower-protocol: working notes for coding agents

The shared wire-format crate (`crates/protocol` in the hardwario/tower monorepo; read the root
`AGENTS.md` first). Every end of the TOWER link builds this one source tree **by path**:

- **the firmware SDK** (`firmware/Cargo.toml`, `tower-protocol = { path = "../crates/protocol" }`),
  and through it every example and product app;
- **`tower-kv`** (`crates/kv`), which shares the CRC primitive so EEPROM records stay
  byte-compatible with the firmware;
- **the `tower` CLI** (`cli/`) and **the HIL bench harness** (`hil/`), which decodes the framed
  console natively.

**Cardinal rule:** postcard is *not* self-describing: a producer and a consumer built against
different schemas silently mis-decode. With path dependencies every *build* agrees by
construction; what can still disagree is an *installed* artifact from another tree (a `tower`
binary, a flashed board). That is what the schema tags are for: `CONSOLE_TAG` in every console
frame header and `RADIO_TAG` at the head of every radio envelope, both derived at compile time
from the schema's structural hash (`src/fingerprint.rs`), so the stale side fails loudly with
`SchemaMismatch` instead of decoding garbage. Nothing is bumped by hand.

## Changing the wire

1. **Change the types** (`src/msg.rs`, `src/mgmt.rs`: console; `src/radio.rs`: the radio
   application schema) and fix every caller in `firmware/`, `cli/`, `hil/` (and `crates/kv` if
   it is touched) **in the same commit**; the path dependencies make the build fail until you do.
2. **Rebuild every end from that checkout**: reflash the boards and reinstall the CLI (`just
   install-cli`). A board or a `tower` binary from another checkout is rejected by its tag
   (`tower` prints the mismatch banner and exits 125).

That is all: no version constant, no changelog, no golden bytes to update, no tag to cut. Things
to keep in mind:

- A rename keeps the tags (names are not hashed). Never use a serde attribute that changes the
  encoding (`skip`, `with`, `flatten`, ...): the `Schema` derive cannot see it.
- `postcard-schema` / `postcard-derive` are pinned with `=` on purpose (the host and firmware
  workspaces each recompute the tags from their own lockfile); bumping them may move the tags,
  which is harmless but makes every installed artifact stale.
- The framing (header, COBS, CRC) is not hashed: change it deliberately, keep the CRC trailer
  (it is what makes a foreign tag readable), and update `tests/framing.rs`, whose byte goldens
  pin it with an explicit tag.
- Size budgets: `MAX_FRAME` (256) and `MAX_PAYLOAD` (247) are what the firmware console, the
  shell's `CompletionBudget`, the CLI and the harness size against; the capacity tests in
  `tests/framing.rs` and `tests/radio.rs` hold the largest real payloads to them.

## Tests / checks

From the repo root (`just check` runs them all, with every other gate):

```sh
cargo test -p tower-protocol
cargo clippy -p tower-protocol --all-targets -- -D warnings
cargo build -p tower-protocol --target thumbv6m-none-eabi
```

The crate is formatted by the repo-wide `cargo fmt` gate (the root `rustfmt.toml`).

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- Part of the hardwario/tower monorepo, developed straight on `main`; no feature branches or PRs
  unless the user asks.
- Commit and push only when the user requests it.
