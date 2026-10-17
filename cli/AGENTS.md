# tower-cli: working notes for coding agents

The `tower` host CLI: decodes the framed serial link from the TOWER firmware (logs / events /
shell / TUI) and flashes/erases/resets over the UART bootloader (via the `jolt` engine). It is
also the gateway/MQTT host: `tower gateway` bridges a Radio Dongle's network to MQTT (embedded
`rumqttd` broker or external, TUI + `--service`), and `tower nodes` / `tower net` are the MQTT
client commands (`src/gateway/`, `src/client/`).

## Shared wire protocol (`tower-protocol`): keep it in lockstep

The wire format lives in a **separate repo**, github.com/hardwario/tower-protocol, pinned here by
git tag in `Cargo.toml`. The firmware (`tower-firmware`, in **two** manifests) and the HIL bench
harness (`tower-hil`) pin the **same tag**; all three consumers MUST move together, because
postcard isn't self-describing (mismatched versions silently mis-decode).

**To bump tower-protocol** (after a change is released there; see that repo's `AGENTS.md`):

```sh
# set tag = "vX.Y.Z" in Cargo.toml, then:
# ONLY if a local paths override exists, move it aside so it doesn't shadow the git source
# (it's gitignored and only present during local co-dev; see "Local dev override" below):
[ -f .cargo/config.toml ] && mv .cargo/config.toml .cargo/config.toml.bak
cargo update -p tower-protocol                  # re-resolve the lock to the new tag
[ -f .cargo/config.toml.bak ] && mv .cargo/config.toml.bak .cargo/config.toml
cargo build
```

…and bump **tower-firmware** (both of its manifests) and **tower-hil** to the same tag in the
same change-set.

## Local dev override

`.cargo/config.toml` is **gitignored** and holds a `paths` override to side-by-side checkouts so
local edits are picked up without re-tagging. The paths are relative to this repo's root (the
directory holding `.cargo/`): in the TOWER control-plane checkout (`cli/` next to `protocol/` and
`jolt/`) they are `../protocol` and `../jolt`; with standalone sibling clones (`tower-cli/` next
to `tower-protocol/` and `jolt/`) they are `../tower-protocol` and `../jolt`. A standalone clone
without the file builds from the pinned tags. The override does **not** modify
`Cargo.lock`, and (as above) must be moved aside when you actually want to re-resolve the lock.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.
