# tower-protocol: rules for agents

The wire contract. Every end builds this one source tree by path: the firmware SDK (and through
it every example and app), `tower-kv` (`crates/kv`, which shares the CRC so EEPROM records stay
byte-compatible), the `tower` CLI and the HIL harness. How it works and how to change it:
[docs/protocol.md](../../docs/protocol.md), *Changing the schema*; the repo-wide runbook is the
root `AGENTS.md`, *Changing the wire format*.

## Rules

- **postcard is not self-describing:** a wire change fixes every caller (`firmware/`, `cli/`,
  `hil/`, and `crates/kv` if touched) in the same commit, and every end is rebuilt from that
  checkout (reflash the boards, `just install-cli`). Nothing is bumped by hand: the schema tags
  move by themselves.
- Never use a serde attribute that changes the encoding (`skip`, `with`, `flatten`, ...): the
  `Schema` derive cannot see it.
- `postcard-schema` / `postcard-derive` are pinned with `=` on purpose (the host and firmware
  workspaces each recompute the tags from their own lockfile); bumping them may move the tags,
  which is harmless but makes every installed artifact stale.
- The framing (header, COBS, CRC) is not hashed: change it only deliberately, keep the CRC
  trailer (it is what makes a foreign tag readable), and update the byte goldens in
  `tests/framing.rs`.
- Keep the largest real payloads within `MAX_FRAME` / `MAX_PAYLOAD` (docs/protocol.md, *Changing
  the schema*); the capacity tests hold them to it.

## Checks

From the repo root (`just check` runs them all, with every other gate):

```sh
cargo test -p tower-protocol
cargo clippy -p tower-protocol --all-targets -- -D warnings
cargo build -p tower-protocol --target thumbv6m-none-eabi
```
