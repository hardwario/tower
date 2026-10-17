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
construction; what can still disagree is an *installed* artifact from an older tree (a `tower`
binary, a flashed board). That is what the version constants are for: bump them on every wire
change so the stale side fails loudly with `BadVersion` instead of decoding garbage.

## Releasing a change

1. **Change the types.** Any postcard struct/enum change (field *or* variant **order** counts,
   and appending is a change too), the framing, or a `MsgType` discriminant.
2. **Bump the version constant of the surface you touched.** The console wire is
   `PROTOCOL_VERSION` in `src/lib.rs`. **The radio application schema (`src/radio.rs`) is a
   second, independently-versioned surface**: a change there bumps `RADIO_SCHEMA_VERSION` (not
   `PROTOCOL_VERSION`).
3. **Update the goldens** (`tests/golden.rs` for the console, `tests/radio_golden.rs` for the
   radio schema) and the variant-order fingerprint pins, if the change touches them. New
   coverage = a NEW golden fn, never an edit to an existing one. Variant-order pins are hex
   tables (`(Variant, 0x02)`), never `i as u8`.
4. **Bump `version` in `Cargo.toml`** (semver: wire/behaviour change: minor; fix: patch) and add
   a `CHANGELOG.md` entry. Fix every consumer the change breaks (firmware, cli, hil, kv) **in the
   same commit**; the path dependencies make the build fail until you do.
5. **One commit, then tag it** `protocol-vX.Y.Z`:
   ```sh
   git tag -a protocol-vX.Y.Z -m "tower-protocol X.Y.Z"
   ```
   The tag marks the release and is the next baseline of the wire-bump guard; nothing is
   published from it (no crates.io, no GitHub Release). Push only when the user asks.
6. Rebuild what is installed: reflash the boards and reinstall the CLI (`just install-cli`), or
   the old binary reports `BadVersion` against the new firmware.

CI enforces both pairings: the `host` job fails if the goldens no longer match the code, and the
`wire-guard` job (`tools/check_wire_bump.py`) checks each golden file against its own version
constant, failing if any pre-existing golden fn (one holding hex literals) was edited, renamed
or removed relative to the latest `protocol-v*` tag while that constant did not strictly
increase (a decrease always fails). The guard runs from the repo root or from this directory
(`python3 crates/protocol/tools/check_wire_bump.py`); `--base REF` compares against any ref
instead of the latest tag. Changing the guard? Run it with `--self-test`.

## Tests / checks

From the repo root (`just check` runs them all, with every other gate):

```sh
cargo test -p tower-protocol
cargo clippy -p tower-protocol --all-targets -- -D warnings
cargo build -p tower-protocol --target thumbv6m-none-eabi
python3 crates/protocol/tools/check_wire_bump.py
python3 crates/protocol/tools/check_wire_bump.py --self-test
```

The crate is formatted by the repo-wide `cargo fmt` gate (the root `rustfmt.toml`); it is no
longer hand-formatted. Reformatting never trips the wire-bump guard: its fingerprints ignore
whitespace, line wrapping and comments.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- Part of the hardwario/tower monorepo, developed straight on `main`; no feature branches or PRs
  unless the user asks.
- Commit, tag and push only when the user requests it.
